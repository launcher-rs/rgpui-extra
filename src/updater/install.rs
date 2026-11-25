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

/// 生成 Windows 平台的 PowerShell helper 内容。
///
/// 脚本以 UTF-16LE BOM 保存，避免 Windows 批处理代码页损坏可执行文件或文档中的中文路径。
pub fn build_windows_script(options: &InstallOptions) -> String {
    let exe = powershell_quote(&options.current_exe.to_string_lossy());
    let staged = powershell_quote(&options.staged_exe.to_string_lossy());
    let args = options
        .restart_args
        .iter()
        .map(|arg| format!("    {}", powershell_quote(arg)))
        .collect::<Vec<_>>()
        .join("\r\n");
    format!(
        "$ErrorActionPreference = 'Stop'\r\n\
$targetPid = {pid}\r\n\
$exe = {exe}\r\n\
$staged = {staged}\r\n\
$restartArgs = @(\r\n\
{args}\r\n\
)\r\n\
$deadline = [DateTime]::UtcNow.AddSeconds({timeout})\r\n\
while (Get-Process -Id $targetPid -ErrorAction SilentlyContinue) {{\r\n\
    if ([DateTime]::UtcNow -ge $deadline) {{ exit 1 }}\r\n\
    Start-Sleep -Milliseconds 500\r\n\
}}\r\n\
try {{ Move-Item -LiteralPath $staged -Destination $exe -Force -ErrorAction Stop }}\r\n\
catch {{ exit 1 }}\r\n\
function ConvertTo-WindowsCommandLineArgument([string]$value) {{\r\n\
    $builder = New-Object System.Text.StringBuilder\r\n\
    [void]$builder.Append([char]34)\r\n\
    $slashes = 0\r\n\
    foreach ($character in $value.ToCharArray()) {{\r\n\
        if ([int]$character -eq 92) {{ $slashes++; continue }}\r\n\
        if ([int]$character -eq 34) {{\r\n\
            for ($i = 0; $i -lt (2 * $slashes + 1); $i++) {{ [void]$builder.Append([char]92) }}\r\n\
            [void]$builder.Append([char]34)\r\n\
        }} else {{\r\n\
            for ($i = 0; $i -lt $slashes; $i++) {{ [void]$builder.Append([char]92) }}\r\n\
            [void]$builder.Append($character)\r\n\
        }}\r\n\
        $slashes = 0\r\n\
    }}\r\n\
    for ($i = 0; $i -lt (2 * $slashes); $i++) {{ [void]$builder.Append([char]92) }}\r\n\
    [void]$builder.Append([char]34)\r\n\
    return $builder.ToString()\r\n\
}}\r\n\
$startInfo = New-Object System.Diagnostics.ProcessStartInfo\r\n\
$startInfo.FileName = $exe\r\n\
$startInfo.WorkingDirectory = (Get-Location).Path\r\n\
$startInfo.Arguments = (($restartArgs | ForEach-Object {{ ConvertTo-WindowsCommandLineArgument $_ }}) -join ' ')\r\n\
$startInfo.UseShellExecute = $false\r\n\
try {{ [System.Diagnostics.Process]::Start($startInfo) | Out-Null }}\r\n\
catch {{ exit 1 }}\r\n\
exit 0\r\n",
        pid = options.process_id,
        timeout = WAIT_TIMEOUT_SECS,
        exe = exe,
        staged = staged,
        args = args,
    )
}

/// PowerShell 单引号字符串转义。
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
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
chmod +x \"$NEW\" || exit 1\n\
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
        (build_windows_script(options), "ps1")
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
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;

        // Windows PowerShell 5.1 读取带 BOM 的 UTF-16LE 脚本，可无损处理中文路径。
        let mut script_bytes = vec![0xFF, 0xFE];
        for code_unit in script_text.encode_utf16() {
            script_bytes.extend_from_slice(&code_unit.to_le_bytes());
        }
        std::fs::write(&path, script_bytes)
            .with_context(|| format!("写入更新 helper 失败：{}", path.display()))?;

        // PowerShell 在隐藏窗口中独立等待主程序退出，再替换并重启。
        std::process::Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&path)
            .creation_flags(0x08000000)
            .spawn()
            .with_context(|| "启动更新 helper 失败")?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::fs::write(&path, &script_text)
            .with_context(|| format!("写入更新 helper 失败：{}", path.display()))?;
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
        assert!(script.contains("Get-Process -Id $targetPid"));
        assert!(script.contains("Move-Item -LiteralPath $staged -Destination $exe"));
        assert!(script.contains("$startInfo.FileName = $exe"));
        assert!(script.contains("note.md"));
        assert!(script.contains("'my docs/today.md'"));
        assert!(script.contains(&format!("AddSeconds({WAIT_TIMEOUT_SECS})")));
        // helper 必须区分成功与失败退出码。
        assert!(script.contains("exit 0"));
        assert!(script.contains("exit 1"));
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

    /// 在 Windows 上真实运行 helper：覆盖中文目录中的程序，并验证中文参数能传给新进程。
    #[cfg(windows)]
    #[test]
    fn windows_helper_replaces_and_restarts_with_unicode_file_arguments() {
        use std::os::windows::process::CommandExt as _;
        use std::time::{Duration, Instant};

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rgpui-updater-中文-{nonce}"));
        let app_dir = root.join("应用程序");
        let cache_dir = root.join("更新缓存");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();

        let exe = app_dir.join("rumark.exe");
        let staged = cache_dir.join("新版本.exe");
        let marker = root.join("启动参数.txt");
        let source = root.join("启动探针.rs");
        let expected_argument = r"E:\桌面\Killer PDF\测试 文档.md";
        std::fs::copy(
            PathBuf::from(std::env::var_os("WINDIR").unwrap())
                .join("System32")
                .join("cmd.exe"),
            &exe,
        )
        .unwrap();
        let marker_literal = format!("{:?}", marker.to_string_lossy());
        std::fs::write(
            &source,
            format!(
                "fn main() {{{{\n    let args = std::env::args().skip(1).collect::<Vec<_>>();\n    std::fs::write({marker_literal}, args.join(\"\\n\")).unwrap();\n}}}}\n"
            ),
        )
        .unwrap();
        let status = std::process::Command::new("rustc")
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(&staged)
            .status()
            .unwrap();
        assert!(status.success(), "编译 Windows 启动探针失败");

        // 让 helper 等待一个独立进程，模拟 Rumark 关闭其窗口后释放 exe。
        let mut target = std::process::Command::new("ping")
            .args(["127.0.0.1", "-n", "3"])
            .creation_flags(0x08000000)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let options = InstallOptions {
            current_exe: exe.clone(),
            staged_exe: staged.clone(),
            process_id: target.id(),
            restart_args: vec![expected_argument.to_string()],
        };
        validate_staged(&options).unwrap();
        let script = build_windows_script(&options);
        let script_path = root.join("更新助手.ps1");
        let mut script_bytes = vec![0xFF, 0xFE];
        for code_unit in script.encode_utf16() {
            script_bytes.extend_from_slice(&code_unit.to_le_bytes());
        }
        std::fs::write(&script_path, script_bytes).unwrap();
        let mut helper = std::process::Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script_path)
            .creation_flags(0x08000000)
            .spawn()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(45);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let helper_status = helper.wait().unwrap();
        let _ = target.wait();
        assert!(helper_status.success(), "Windows 更新 helper 执行失败");
        assert!(marker.exists(), "更新后的程序未能从中文目录启动");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), expected_argument);
        assert!(!staged.exists(), "暂存文件应已替换到目标位置");

        std::fs::remove_dir_all(root).unwrap();
    }
}
