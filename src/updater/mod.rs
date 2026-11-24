//! GitHub Release 更新组件（核心逻辑，不依赖任何 GUI）。
//!
//! 这是 `rgpui-extra` 扩展库的 `updater` 模块；启用 `updater` feature 即可使用。
//!
//! 职责边界：
//! - 只做「版本比较 → GitHub Release 查询 → 镜像回退 → 分块下载 → 解压出单文件 →
//!   落地为 `<exe>.new`」，不自动检查、不替换正在运行的程序、不自动重启；
//! - 所有网络函数是 blocking，调用方必须放到后台线程（可用 [`run_blocking`] 桥接 async）；
//! - 网络层不认识具体程序，仓库、应用名、资产名全部由 [`Config`] 注入，
//!   因此 launcher-rs 的其他程序可直接复用；
//! - 需要 rgpui 对话框/进度视图时开启 `ui` feature（见 [`ui`] 模块）。

use std::io::{Read as _, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

/// 内置镜像前缀，按实测可用性排序。空串表示直连，永远作为兜底。
pub const DEFAULT_MIRRORS: &[&str] = &["https://ghfast.top", "https://ghproxy.net", ""];

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// 下载响应体上限 200MB，防止错配 URL 把超大内容读进内存。
const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;

/// 一个应用的更新配置。所有应用相关信息都通过这里注入，组件本身不作硬编码。
#[derive(Clone, Debug)]
pub struct Config {
    /// GitHub 仓库，形如 `launcher-rs/rumark`。
    pub repository: String,
    /// 应用名，用于拼默认资产名（如 `rumark-windows-x86_64.zip`）。
    pub app_name: String,
    /// 压缩包内可执行文件的名字（Windows 需带 `.exe`）。
    pub executable_name: String,
    /// HTTP User-Agent。
    pub user_agent: String,
    /// 覆盖默认资产名；`None` 时按平台与 `app_name` 推导。
    pub asset_name: Option<String>,
    /// 用户自定义的镜像前缀（每行一个由调用方切分好）。
    pub custom_mirrors: Vec<String>,
    /// 是否启用镜像中转；关闭时只走直连。默认关闭。
    pub use_mirrors: bool,
}

impl Config {
    /// 用仓库与应用名构造配置，其余字段取合理默认值。
    pub fn new(repository: impl Into<String>, app_name: impl Into<String>) -> Self {
        let app_name = app_name.into();
        let executable_name = if cfg!(target_os = "windows") {
            format!("{app_name}.exe")
        } else {
            app_name.clone()
        };
        Self {
            repository: repository.into(),
            app_name,
            executable_name,
            user_agent: "launcher-updater/1.0".to_string(),
            asset_name: None,
            custom_mirrors: Vec::new(),
            use_mirrors: false,
        }
    }

    /// GitHub Releases API 的 latest 地址。
    pub fn api_url(&self) -> String {
        format!(
            "https://api.github.com/repos/{}/releases/latest",
            self.repository
        )
    }

    /// 浏览器中打开的发布页地址。
    pub fn releases_page(&self) -> String {
        format!("https://github.com/{}/releases/latest", self.repository)
    }

    /// 当前平台对应的 Release 资产文件名。
    pub fn asset_name(&self) -> String {
        if let Some(name) = &self.asset_name {
            return name.clone();
        }
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        let suffix = "windows-x86_64.zip";
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        let suffix = "linux-x86_64.tar.gz";
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let suffix = "macos-aarch64.tar.gz";
        #[cfg(not(any(
            all(target_os = "windows", target_arch = "x86_64"),
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "macos", target_arch = "aarch64")
        )))]
        let suffix = "unknown.zip";
        format!("{}-{suffix}", self.app_name)
    }

    /// 本次要走的地址前缀列表。镜像关闭时只有直连；开启时自定义优先、
    /// 内置其次、直连兜底（去重）。
    pub fn candidate_mirrors(&self) -> Vec<String> {
        if !self.use_mirrors {
            return vec![String::new()];
        }
        let mut result: Vec<String> = Vec::new();
        for mirror in &self.custom_mirrors {
            // 统一去掉尾部斜杠，既便于去重，也让 mirror_url 拼接结果稳定。
            let m = mirror.trim().trim_end_matches('/');
            if !m.is_empty() && !result.iter().any(|x| x == m) {
                result.push(m.to_string());
            }
        }
        for &m in DEFAULT_MIRRORS {
            if !result.iter().any(|x| x == m) {
                result.push(m.to_string());
            }
        }
        result
    }
}

/// GitHub Release 的最小化结构（只解析用到的字段）。
#[derive(Deserialize, Debug, Clone)]
pub struct ReleaseInfo {
    pub tag_name: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub published_at: String,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ReleaseAsset {
    pub name: String,
    /// 资产大小，下载时按实际字节校验。
    #[serde(default)]
    pub size: u64,
    pub browser_download_url: String,
}

/// 语义版本（major.minor.patch），忽略预发布/构建后缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    pub fn version_string(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// 解析 `v0.2.5` / `0.2.5-beta.1` 这类版本串；失败返回 `None`。
pub fn parse_version(raw: &str) -> Option<Version> {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']);
    let core = trimmed.split(['-', '+']).next().unwrap_or("");
    let mut parts = core.split('.');
    Some(Version {
        major: parts.next()?.parse().ok()?,
        minor: parts.next()?.parse().ok()?,
        patch: parts.next()?.parse().ok()?,
    })
}

/// 远端版本是否比当前版本新。
pub fn is_newer(remote: &Version, current: &Version) -> bool {
    remote > current
}

/// 一次检测结果（是否已发现新版本由 [`UpdateCheck::newer`] 给出）。
#[derive(Debug, Clone)]
pub struct UpdateCheck {
    pub current: Version,
    pub remote: Version,
    pub release: ReleaseInfo,
    /// 命中的镜像前缀；空串为直连。
    pub mirror: String,
    /// 当前平台对应的资产（缺失时为 `None`，由调用方决定如何提示）。
    pub asset: Option<ReleaseAsset>,
    pub newer: bool,
}

/// 把镜像前缀套到原始 URL 外：`mirror` + `/` + `original`。
pub fn mirror_url(mirror: &str, original: &str) -> String {
    let mirror = mirror.trim();
    if mirror.is_empty() {
        return original.to_string();
    }
    format!("{}/{}", mirror.trim_end_matches('/'), original)
}

/// 在独立线程执行阻塞任务并把结果送回 async 上下文，不阻塞 UI 执行器。
pub async fn run_blocking<T, F>(job: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name("launcher-updater".to_string())
        .spawn(move || {
            let _ = tx.send(job());
        })
        .expect("spawn updater worker thread");
    rx.await
        .expect("updater worker thread finished without result")
}

fn blocking_client(config: &Config) -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(&config.user_agent)
        .build()?)
}

/// 解析 GitHub API 的 Release JSON。
pub fn parse_release(body: &str) -> Result<ReleaseInfo> {
    serde_json::from_str(body).context("解析 GitHub Release 响应失败")
}

/// 查询最新版本，依次尝试配置里的镜像，返回 (Release, 命中的镜像)。
/// 必须在后台线程调用。
pub fn fetch_latest_release(config: &Config) -> Result<(ReleaseInfo, String)> {
    let client = blocking_client(config)?;
    let mut attempts = Vec::new();
    for mirror in config.candidate_mirrors() {
        let url = mirror_url(&mirror, &config.api_url());
        match client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.text())
        {
            Ok(body) => match parse_release(&body) {
                Ok(release) => return Ok((release, mirror)),
                Err(err) => attempts.push(format!("{url}: {err}")),
            },
            Err(err) => attempts.push(format!("{url}: {err}")),
        }
    }
    bail!("检查更新失败，已尝试：\n{}", attempts.join("\n"))
}

/// 查询并比较版本。`current_version` 由调用方传入（通常是 `env!("CARGO_PKG_VERSION")`）。
pub fn check(config: &Config, current_version: &str) -> Result<UpdateCheck> {
    let (release, mirror) = fetch_latest_release(config)?;
    let remote = parse_version(&release.tag_name)
        .with_context(|| format!("无法解析版本号：{}", release.tag_name))?;
    let current = parse_version(current_version).unwrap_or_else(|| Version::new(0, 0, 0));
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == config.asset_name())
        .cloned();
    Ok(UpdateCheck {
        newer: is_newer(&remote, &current),
        current,
        remote,
        release,
        mirror,
        asset,
    })
}

/// 下载进度快照。`total` 为 `None` 表示服务端未给出 `Content-Length`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

impl DownloadProgress {
    /// 0.0..=1.0；无法计算时返回 `None`。
    pub fn fraction(&self) -> Option<f32> {
        match self.total {
            Some(total) if total > 0 => {
                Some((self.downloaded as f32 / total as f32).clamp(0.0, 1.0))
            }
            _ => None,
        }
    }

    /// 百分比（0..=100）；无法计算时返回 `None`。
    pub fn percent(&self) -> Option<u32> {
        self.fraction().map(|f| (f * 100.0).round() as u32)
    }

    /// 人类可读的 `已下载 / 总量` 文本，如 `3.2 MB / 11.2 MB`。
    pub fn label(&self) -> String {
        fn human(bytes: u64) -> String {
            const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
            let mut value = bytes as f64;
            let mut unit = 0;
            while value >= 1024.0 && unit + 1 < UNITS.len() {
                value /= 1024.0;
                unit += 1;
            }
            if unit == 0 {
                format!("{bytes} {}", UNITS[unit])
            } else {
                format!("{value:.1} {}", UNITS[unit])
            }
        }
        match self.total {
            Some(total) => format!("{} / {}", human(self.downloaded), human(total)),
            None => human(self.downloaded),
        }
    }
}

struct ProgressWriter<'a, F: Fn(DownloadProgress)> {
    payload: Vec<u8>,
    downloaded: u64,
    total: Option<u64>,
    reporter: &'a F,
}

impl<F: Fn(DownloadProgress)> Write for ProgressWriter<'_, F> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.downloaded.saturating_add(buf.len() as u64) > MAX_DOWNLOAD_BYTES {
            return Err(std::io::Error::other("更新包超过 200MB 上限"));
        }
        self.payload.extend_from_slice(buf);
        self.downloaded += buf.len() as u64;
        (self.reporter)(DownloadProgress {
            downloaded: self.downloaded,
            total: self.total,
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn extract_payload(bytes: &[u8], asset_name: &str, exe_name: &str) -> Result<Vec<u8>> {
    if asset_name.ends_with(".zip") {
        let cursor = std::io::Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(cursor).context("打开的更新包不是合法 zip")?;
        let mut file = archive
            .by_name(exe_name)
            .with_context(|| format!("更新包内未找到 {exe_name}"))?;
        let mut buf = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut buf)?;
        return Ok(buf);
    }

    if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
        let decoder = flate2::read::GzDecoder::new(bytes);
        let mut archive = tar::Archive::new(decoder);
        for entry in archive.entries().context("打开的更新包不是合法 tar.gz")? {
            let mut entry = entry?;
            let is_match = entry
                .path()
                .ok()
                .and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
                .as_deref()
                == Some(exe_name);
            if is_match {
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut buf)?;
                return Ok(buf);
            }
        }
        bail!("更新包内未找到 {exe_name}");
    }

    bail!("无法识别的更新包格式：{asset_name}")
}

/// 下载资产、解压出可执行文件，写入 `dest_dir/<exe>.new` 并返回该路径。
/// `on_progress` 在后台线程被调用，调用方只做线程安全的数据投递（如 `mpsc::send`）。
/// 必须在后台线程调用。
pub fn download_and_stage<F>(
    config: &Config,
    asset: &ReleaseAsset,
    dest_dir: &Path,
    on_progress: F,
) -> Result<PathBuf>
where
    F: Fn(DownloadProgress) + Send + Sync,
{
    let client = blocking_client(config)?;
    let mut attempts = Vec::new();
    let mut payload: Option<Vec<u8>> = None;

    for mirror in config.candidate_mirrors() {
        let url = mirror_url(&mirror, &asset.browser_download_url);
        let mut response = match client.get(&url).send().and_then(|r| r.error_for_status()) {
            Ok(response) => response,
            Err(err) => {
                attempts.push(format!("{url}: {err}"));
                continue;
            }
        };
        let total = response.content_length();
        on_progress(DownloadProgress {
            downloaded: 0,
            total,
        });
        let mut writer = ProgressWriter {
            payload: Vec::new(),
            downloaded: 0,
            total,
            reporter: &on_progress,
        };
        match response.copy_to(&mut writer) {
            Ok(_) if !writer.payload.is_empty() => {
                payload = Some(writer.payload);
                break;
            }
            Ok(_) => attempts.push(format!("{url}: 响应为空")),
            Err(err) => attempts.push(format!("{url}: {err}")),
        }
    }

    let bytes = payload.with_context(|| format!("下载失败，已尝试：\n{}", attempts.join("\n")))?;
    let executable = extract_payload(&bytes, &asset.name, &config.executable_name)?;
    if executable.is_empty() {
        bail!("更新包内的可执行文件为空");
    }

    std::fs::create_dir_all(dest_dir)?;
    let staged = dest_dir.join(format!("{}.new", config.executable_name));
    let mut out = std::fs::File::create(&staged)?;
    out.write_all(&executable)?;
    out.flush()?;
    Ok(staged)
}

/// 用系统默认浏览器打开发布页。
pub fn open_release_page(config: &Config) -> Result<()> {
    let url = config.releases_page();
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(&url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(&url).spawn()?;
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    let _ = url;
    Ok(())
}

#[cfg(feature = "ui")]
pub mod ui;

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::new("launcher-rs/demo", "demo")
    }

    #[test]
    fn parses_version_with_v_prefix_and_suffix() {
        assert_eq!(parse_version("v0.2.5"), Some(Version::new(0, 2, 5)));
        assert_eq!(parse_version("0.3.0-beta.1"), Some(Version::new(0, 3, 0)));
        assert_eq!(parse_version("1.2.3+build.9"), Some(Version::new(1, 2, 3)));
        assert_eq!(parse_version("garbage"), None);
        assert_eq!(parse_version("v1.2"), None);
    }

    #[test]
    fn compares_versions() {
        let cur = Version::new(0, 2, 4);
        assert!(is_newer(&Version::new(0, 2, 5), &cur));
        assert!(is_newer(&Version::new(1, 0, 0), &cur));
        assert!(!is_newer(&Version::new(0, 2, 4), &cur));
        assert!(!is_newer(&Version::new(0, 2, 3), &cur));
    }

    #[test]
    fn mirrors_are_disabled_by_default() {
        let mut config = config();
        assert!(!config.use_mirrors);
        assert_eq!(config.candidate_mirrors(), vec![String::new()]);
        config.use_mirrors = true;
        config.custom_mirrors = vec![" https://a.com/ ".into(), "".into(), "https://a.com".into()];
        let list = config.candidate_mirrors();
        assert_eq!(list[0], "https://a.com");
        assert_eq!(list.iter().filter(|x| *x == "https://a.com").count(), 1);
        assert!(list.contains(&"https://ghfast.top".to_string()));
        assert_eq!(list.last().map(String::as_str), Some(""));
    }

    #[test]
    fn mirror_url_normalizes_trailing_slash() {
        assert_eq!(
            mirror_url("https://ghfast.top/", "https://github.com/x"),
            "https://ghfast.top/https://github.com/x"
        );
        assert_eq!(
            mirror_url("", "https://github.com/x"),
            "https://github.com/x"
        );
    }

    #[test]
    fn config_builds_urls_and_asset_name() {
        let config = config();
        assert_eq!(
            config.api_url(),
            "https://api.github.com/repos/launcher-rs/demo/releases/latest"
        );
        assert_eq!(
            config.releases_page(),
            "https://github.com/launcher-rs/demo/releases/latest"
        );
        assert!(config.asset_name().starts_with("demo-"));
        let mut config = config;
        config.asset_name = Some("custom.zip".into());
        assert_eq!(config.asset_name(), "custom.zip");
    }

    const SAMPLE_RELEASE: &str = r#"{
        "tag_name": "v0.2.5",
        "name": "Demo v0.2.5",
        "body": "修复若干问题",
        "published_at": "2026-11-22T08:00:00Z",
        "assets": [
            {"name": "demo-asset.zip", "size": 123, "browser_download_url": "https://example.com/demo-asset.zip"}
        ]
    }"#;

    #[test]
    fn parses_release_asset() {
        let release = parse_release(SAMPLE_RELEASE).unwrap();
        assert_eq!(release.tag_name, "v0.2.5");
        assert_eq!(release.assets[0].size, 123);
    }

    #[test]
    fn progress_fraction_and_label() {
        let p = DownloadProgress {
            downloaded: 25,
            total: Some(100),
        };
        assert_eq!(p.fraction(), Some(0.25));
        assert_eq!(p.percent(), Some(25));
        assert!(p.label().contains("25 B"));
        let unknown = DownloadProgress {
            downloaded: 10,
            total: None,
        };
        assert_eq!(unknown.fraction(), None);
        assert_eq!(unknown.percent(), None);
    }
}
