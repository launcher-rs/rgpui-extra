//! 单文件可执行程序的「替换 + 重启」helper。
//!
//! 正在运行的 exe 在 Windows 上无法被覆盖，因此不能在主进程里直接替换自己。
//! 这里把替换动作交给一个独立的后台 helper：主进程退出后，helper 等待其
//! 结束，再用新文件覆盖旧 exe，最后重新启动程序。helper 由主进程 spawn，
//! 不继承控制台、不使用 `cmd /C wait`（避免窗口闪烁与死锁）。

use std::path::PathBuf;

use anyhow::{Context as _, Result};

/// 一次替换安装所需的输入。
#[derive(Clone, Debug)]
pub struct InstallOptions {
    /// 当前正在运行的可执行文件完整路径（`std::env::current_exe()`）。
    pub current_exe: PathBuf,
    /// 已下载并解压好的新可执行文件（`download_and_stage` 的返回值）。
    pub staged_exe: PathBuf,
    /// 当前进程 ID，helper 用它判断何时可以安全替换。
    pub process_id: u32,
    /// 重启时携带的命令行参数（不含 argv[0]）。
    pub restart_args: Vec<String>,
}

/// 等待目标进程退出的最长秒数。
pub const WAIT_TIMEOUT_SECS: u64 = 60;

/// 确认 `staged_exe` 存在且非空，避免用坏文件覆盖正在使用的程序。
pub fn validate_staged(options: &InstallOptions) -> Result<()> {
    let meta = std::fs::metadata(&options.staged_exe)
        .with_context(|| format!("新可执行文件不存在：{}", options.staged_exe.display()))?;
    if !meta.is_file() {
        anyhow::bail!("新可执行文件路径不是文件：{}", options.staged_exe.display());
    }
    if meta.len() == 0 {
        anyhow::bail!("新可执行文件为空：{}", options.staged_exe.display());
    }
    // 与自身比对，拒绝用当前文件覆盖自己造成无意义重启。
    let current =
        std::fs::canonicalize(&options.current_exe).unwrap_or(options.current_exe.clone());
    let staged = std::fs::canonicalize(&options.staged_exe).unwrap_or(options.staged_exe.clone());
    if current == staged {
        anyhow::bail!("新可执行文件与当前程序是同一个文件");
    }
    Ok(())
}

/// 生成 Windows 平台的 `.cmd` helper 内容（纯函数，便于单测）。
///
/// helper 启动后循环等待 PID 退出，成功后用 `move /Y` 覆盖 exe 并重启；
/// 超时或替换失败就退出，绝不留下半替换的状态。
pub fn build_windows_script(options: &InstallOptions) -> String {
    let exe = windows_quote(&options.current_exe.to_string_lossy());
    let staged = windows_quote(&options.staged_exe.to_string_lossy());
    let args = join_windows_args(&options.restart_args);
    format!(
        "@echo off\r\n\
setlocal\r\n\
set \"PID={pid}\"\r\n\
set \"EXE={exe}\"\r\n\
set \"NEW={staged}\"\r\n\
set /a TRIES=0\r\n\
\r\n\
:wait\r\n\
tasklist /FI \"PID eq %PID%\" 2>NUL | findstr /I /C:\"%PID%\" >NUL\r\n\
if errorlevel 1 goto replace\r\n\
set /a TRIES+=1\r\n\
if %TRIES% GEQ {timeout} goto failed\r\n\
ping -n 2 127.0.0.1 >NUL\r\n\
goto wait\r\n\
\r\n\
:replace\r\n\
move /Y \"%NEW%\" \"%EXE%\" >NUL\r\n\
if errorlevel 1 goto failed\r\n\
start \"\" \"%EXE%\"{args}\r\n\
endlocal\r\n\
exit /b 0\r\n\
\r\n\
:failed\r\n\
endlocal\r\n\
exit /b 1\r\n",
        pid = options.process_id,
        timeout = WAIT_TIMEOUT_SECS / 2, // ping 约 0.5s/次
        exe = exe,
        staged = staged,
        args = args,
    )
}

/// 生成 Unix 平台的 `.sh` helper 内容（纯函数，便于单测）。
pub fn build_unix_script(options: &InstallOptions) -> String {
    let exe = shell_quote(&options.current_exe.to_string_lossy());
    let staged = shell_quote(&options.staged_exe.to_string_lossy());
    let args = join_unix_args(&options.restart_args);
    format!(
        "#!/bin/sh\n\
EXE={exe}\n\
NEW={staged}\n\
tries=0\n\
\n\
while kill -0 {pid} 2>/dev/null; do\n\
  tries=$((tries + 1))\n\
  if [ \"$tries\" -ge {timeout} ]; then\n\
    exit 1\n\
  fi\n\
  sleep 1\n\
done\n\
\n\
mv -f \"$NEW\" \"$EXE\" || exit 1\n\
\"$EXE\"{args} >/dev/null 2>&1 &\n\
exit 0\n",
        exe = exe,
        staged = staged,
        pid = options.process_id,
        timeout = WAIT_TIMEOUT_SECS,
        args = args,
    )
}

/// 根据目标平台生成 helper 脚本内容。
pub fn build_script(options: &InstallOptions) -> (String, &'static str) {
    #[cfg(target_os = "windows")]
    {
        (build_windows_script(options), "cmd")
    }
    #[cfg(not(target_os = "windows"))]
    {
        (build_unix_script(options), "sh")
    }
}

/// 写入 helper 脚本并以独立进程启动，返回脚本路径。
///
/// 调用方在成功后应尽快退出主程序；helper 会自行等待并替换。Unix 下用
/// `setsid` 让 helper 脱离当前会话，避免随主进程一同被终端信号终止。
pub fn spawn_install_helper(options: &InstallOptions) -> Result<PathBuf> {
    validate_staged(options)?;

    let (script_text, extension) = build_script(options);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "rgpui-updater-{}-{nonce}.{extension}",
        options.process_id
    ));
    std::fs::write(&path, &script_text)
        .with_context(|| format!("写入更新 helper 失败：{}", path.display()))?;

    #[cfg(target_os = "windows")]
    {
        // `start "" /B` 让 cmd 在后台运行且不弹窗；空的第一个参数是必需的
        // 窗口标题占位，否则 `cmd` 会被当成标题而不被执行。传空串时 Rust 会
        // 把它编码成 `""`，正是 cmd 需要的空标题。
        std::process::Command::new("cmd")
            .args(["/C", "start", "", "/B", "cmd", "/C"])
            .arg(&path)
            .spawn()
            .with_context(|| "启动更新 helper 失败")?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("/bin/sh");
        command.arg(&path).process_group(0);
        command.spawn().with_context(|| "启动更新 helper 失败")?;
    }

    Ok(path)
}

/// Windows `set "VAR=value"` 语境下的转义：百分号须双写，避免被当成变量展开。
fn windows_quote(value: &str) -> String {
    value.replace('%', "%%")
}

/// Windows 命令行参数拼接；空参数直接省略。
fn join_windows_args(args: &[String]) -> String {
    if args.is_empty() {
        return String::new();
    }
    let joined: Vec<String> = args
        .iter()
        .filter(|arg| !arg.is_empty())
        .map(|arg| {
            if arg.contains(' ') || arg.contains('"') {
                format!(" \"{}\"", arg.replace('"', "\"\""))
            } else {
                format!(" {arg}")
            }
        })
        .collect();
    joined.join("")
}

/// POSIX shell 单引号转义。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Unix 命令行参数拼接。
fn join_unix_args(args: &[String]) -> String {
    if args.is_empty() {
        return String::new();
    }
    let joined: Vec<String> = args
        .iter()
        .filter(|arg| !arg.is_empty())
        .map(|arg| format!(" {}", shell_quote(arg)))
        .collect();
    joined.join("")
}

#[cfg(test)]
mod tests {
    // 不能用 `use super::*`：父模块的 `use rgpui::*` 会带来同名 `test` 宏导致递归展开。
    use super::{
        InstallOptions, WAIT_TIMEOUT_SECS, build_unix_script, build_windows_script, shell_quote,
        validate_staged,
    };
    use std::path::PathBuf;

    fn options() -> InstallOptions {
        InstallOptions {
            current_exe: PathBuf::from("C:\\Program Files\\Rumark\\rumark.exe"),
            staged_exe: PathBuf::from("C:\\Users\\me\\AppData\\rumark.exe.new"),
            process_id: 4242,
            restart_args: vec!["note.md".into(), "my docs/today.md".into()],
        }
    }

    #[test]
    fn windows_script_waits_then_replaces_and_restarts() {
        let script = build_windows_script(&options());
        assert!(script.contains("PID eq %PID%"));
        assert!(script.contains("move /Y \"%NEW%\" \"%EXE%\""));
        assert!(script.contains("start \"\" \"%EXE%\""));
        assert!(script.contains("note.md"));
        assert!(script.contains("\"my docs/today.md\""));
        assert!(script.contains(&format!("GEQ {}", WAIT_TIMEOUT_SECS / 2)));
        // helper 必须区分成功与失败退出码。
        assert!(script.contains("exit /b 0"));
        assert!(script.contains("exit /b 1"));
    }

    #[test]
    fn unix_script_waits_then_replaces_and_restarts() {
        let script = build_unix_script(&options());
        assert!(script.contains("kill -0 4242"));
        assert!(script.contains("mv -f \"$NEW\" \"$EXE\""));
        assert!(script.contains("\"$EXE\""));
        assert!(script.contains("'my docs/today.md'"));
        assert!(script.contains("exit 0"));
        assert!(script.contains("exit 1"));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn validate_staged_rejects_missing_file() {
        let opts = InstallOptions {
            staged_exe: PathBuf::from("C:\\definitely-missing\\rumark.exe.new"),
            ..options()
        };
        assert!(validate_staged(&opts).is_err());
    }
}
