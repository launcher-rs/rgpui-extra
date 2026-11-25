//! GitHub Release 更新组件（核心逻辑，不依赖任何 GUI）。
//!
//! 这是 `rgpui-extra` 扩展库的 `updater` 模块；启用 `updater` feature 即可使用。
//!
//! 职责边界：
//! - 提供「版本比较 → GitHub Release 查询 → 镜像回退 → 分块下载 → 解压出单文件 →
//!   落地为 `<exe>.new` → 替换旧程序并重启」的完整能力，但不主动发起检查；
//! - 替换由 [`install`] 模块的独立 helper 在调用方进程退出后完成（Windows 上
//!   运行中的 exe 无法自我覆盖），调用方负责决定何时触发与退出时机；
//! - 所有网络函数是 blocking，调用方必须放到后台线程（可用 [`run_blocking`] 桥接 async）；
//! - 网络层不认识具体程序，仓库、应用名、资产名全部由 [`Config`] 注入，
//!   因此 launcher-rs 的其他程序可直接复用；
//! - 支持 GitHub / Gitee / GitLab（含自托管）与兼容 GitHub API 的自定义服务，
//!   由 [`Provider`] 决定 API 地址与 JSON 解析；
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

/// 代码托管平台。决定 Release API / 发布页地址与 JSON 解析方式。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Provider {
    /// GitHub（默认）：`api.github.com` + `github.com`。
    #[default]
    GitHub,
    /// Gitee：`gitee.com/api/v5`，Release JSON 与 GitHub 同构。
    Gitee,
    /// GitLab：支持 gitlab.com 与自托管实例，`base_url` 为实例根地址。
    GitLab {
        /// 形如 `https://gitlab.com` 或 `https://git.example.com`。
        base_url: String,
    },
    /// 自定义：直接给出 API 与发布页地址，JSON 按 GitHub 结构解析。
    /// Gitea / Forgejo 等兼容 GitHub API 的服务可用此变体。
    Custom {
        /// 返回最新 Release JSON 的完整地址。
        api_url: String,
        /// 浏览器中打开的发布页地址。
        releases_page: String,
    },
}

impl Provider {
    /// Release API 地址。`repository` 形如 `owner/repo`。
    pub fn api_url(&self, repository: &str) -> String {
        match self {
            Self::GitHub => {
                format!("https://api.github.com/repos/{repository}/releases/latest")
            }
            Self::Gitee => {
                format!("https://gitee.com/api/v5/repos/{repository}/releases/latest")
            }
            Self::GitLab { base_url } => format!(
                "{}/api/v4/projects/{}/releases/permalink/latest",
                base_url.trim_end_matches('/'),
                encode_project_path(repository)
            ),
            Self::Custom { api_url, .. } => api_url.clone(),
        }
    }

    /// 浏览器中打开的发布页地址。
    pub fn releases_page(&self, repository: &str) -> String {
        match self {
            Self::GitHub => format!("https://github.com/{repository}/releases/latest"),
            Self::Gitee => format!("https://gitee.com/{repository}/releases/latest"),
            Self::GitLab { base_url } => {
                format!("{}/{repository}/-/releases", base_url.trim_end_matches('/'))
            }
            Self::Custom { releases_page, .. } => releases_page.clone(),
        }
    }
}

/// GitLab API 要求项目路径整体做 percent-encode（`owner/repo` → `owner%2Frepo`）。
fn encode_project_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 一个应用的更新配置。所有应用相关信息都通过这里注入，组件本身不作硬编码。
#[derive(Clone, Debug)]
pub struct Config {
    /// 仓库路径，形如 `launcher-rs/rumark`。
    pub repository: String,
    /// 代码托管平台；默认 GitHub。
    pub provider: Provider,
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
            provider: Provider::default(),
            app_name,
            executable_name,
            user_agent: "launcher-updater/1.0".to_string(),
            asset_name: None,
            custom_mirrors: Vec::new(),
            use_mirrors: false,
        }
    }

    /// 当前平台 Release API 的 latest 地址。
    pub fn api_url(&self) -> String {
        self.provider.api_url(&self.repository)
    }

    /// 当前平台浏览器中打开的发布页地址。
    pub fn releases_page(&self) -> String {
        self.provider.releases_page(&self.repository)
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

/// 解析 GitHub 风格 API 的 Release JSON（GitHub / Gitee / Gitea 等同构）。
pub fn parse_release(body: &str) -> Result<ReleaseInfo> {
    serde_json::from_str(body).context("解析 Release 响应失败")
}

impl ReleaseInfo {
    /// 按平台解析 Release JSON，统一成内部结构供 UI 与下载使用。
    pub fn parse_for(provider: &Provider, body: &str) -> Result<Self> {
        match provider {
            Provider::GitLab { .. } => parse_gitlab_release(body),
            Provider::Gitee => parse_gitee_release(body),
            _ => parse_release(body),
        }
    }
}

/// Gitee 与 GitHub 结构基本同构；较老实例可能没有 `published_at`，用 `created_at` 兜底。
fn parse_gitee_release(body: &str) -> Result<ReleaseInfo> {
    let mut release = parse_release(body)?;
    if release.published_at.is_empty() {
        #[derive(Deserialize)]
        struct CreatedAt {
            #[serde(default)]
            created_at: String,
        }
        if let Ok(extra) = serde_json::from_str::<CreatedAt>(body) {
            release.published_at = extra.created_at;
        }
    }
    Ok(release)
}

/// GitLab 的 Release JSON 与 GitHub 差异较大：描述字段叫 `description`、
/// 资产藏在 `assets.links`、日期用 `released_at`，下载链接优先 `direct_asset_url`。
fn parse_gitlab_release(body: &str) -> Result<ReleaseInfo> {
    #[derive(Deserialize)]
    struct Link {
        name: String,
        #[serde(default)]
        direct_asset_url: Option<String>,
        #[serde(default)]
        url: Option<String>,
    }
    #[derive(Deserialize, Default)]
    struct Assets {
        #[serde(default)]
        links: Vec<Link>,
    }
    #[derive(Deserialize)]
    struct Raw {
        tag_name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        released_at: Option<String>,
        #[serde(default)]
        created_at: Option<String>,
        #[serde(default)]
        assets: Assets,
    }

    let raw: Raw = serde_json::from_str(body).context("解析 GitLab Release 响应失败")?;
    let first_nonempty = |a: Option<String>, b: Option<String>| {
        a.filter(|s| !s.is_empty())
            .or_else(|| b.filter(|s| !s.is_empty()))
            .unwrap_or_default()
    };
    Ok(ReleaseInfo {
        tag_name: raw.tag_name,
        body: raw.description.unwrap_or_default(),
        published_at: first_nonempty(raw.released_at, raw.created_at),
        // GitLab 不提供资产大小，下载时按实际字节数与 200MB 上限校验。
        assets: raw
            .assets
            .links
            .into_iter()
            .map(|link| ReleaseAsset {
                name: link.name,
                size: 0,
                browser_download_url: first_nonempty(link.direct_asset_url, link.url),
            })
            .collect(),
    })
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
            Ok(body) => match ReleaseInfo::parse_for(&config.provider, &body) {
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

/// 一个已暂存、可直接安装的可执行文件及其版本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedUpdate {
    /// 已下载并解压好的可执行文件完整路径。
    pub path: PathBuf,
    /// 该更新对应的版本号（规范化为 `major.minor.patch`）。
    pub version: String,
}

/// 暂存可执行文件的文件名。
pub fn staged_file_name(executable_name: &str) -> String {
    format!("{executable_name}.new")
}

/// 与暂存文件同存的标记文件名，记录其对应版本，供后续复用判断。
fn staged_version_file_name(executable_name: &str) -> String {
    format!("{executable_name}.new.version")
}

/// 检查 `dest_dir` 下是否有对应 `version` 的已暂存更新。
///
/// 只有「可执行文件存在且非空」并且「版本标记文件存在且内容等于 version」时
/// 才返回 `Some`。任一条件不满足（文件被删、内容为空、版本标记缺失或不匹配）
/// 都视为无可用缓存，调用方据此决定是否重新下载。
pub fn load_staged_update(
    dest_dir: &Path,
    executable_name: &str,
    version: &str,
) -> Option<StagedUpdate> {
    // 先比对版本标记，避免无谓地读大文件的元数据。
    let marker =
        std::fs::read_to_string(dest_dir.join(staged_version_file_name(executable_name))).ok()?;
    if marker.trim() != version {
        return None;
    }

    let path = dest_dir.join(staged_file_name(executable_name));
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() == 0 {
        return None;
    }

    Some(StagedUpdate {
        path,
        version: version.to_string(),
    })
}

/// 下载资产、解压出可执行文件，写入 `dest_dir/<exe>.new` 并返回该路径。
///
/// 同时写入版本标记文件 `<exe>.new.version`，使下次检查更新时能识别这份
/// 缓存对应的版本并直接复用，避免用户点「稍后」后被迫重新下载。
/// `on_progress` 在后台线程被调用，调用方只做线程安全的数据投递（如 `mpsc::send`）。
/// 必须在后台线程调用。
pub fn download_and_stage<F>(
    config: &Config,
    asset: &ReleaseAsset,
    version: &str,
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
    let staged = dest_dir.join(staged_file_name(&config.executable_name));
    let mut out = std::fs::File::create(&staged)?;
    out.write_all(&executable)?;
    out.flush()?;

    // 版本标记与可执行文件一起落地：若这一步失败则整个下载失败，避免出现
    // 「有 .new 却不知道是哪个版本」的缓存，防止下次误装旧版本。
    let marker = dest_dir.join(staged_version_file_name(&config.executable_name));
    let mut marker_out = std::fs::File::create(&marker)?;
    marker_out.write_all(version.trim().as_bytes())?;
    marker_out.flush()?;

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

/// 单文件程序的替换 + 重启 helper（不依赖 GUI，随 `updater` feature 提供）。
pub mod install;
pub use install::{InstallOptions, spawn_install_helper};

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
    fn provider_builds_api_and_release_page_urls() {
        assert_eq!(
            Provider::GitHub.api_url("launcher-rs/rumark"),
            "https://api.github.com/repos/launcher-rs/rumark/releases/latest"
        );
        assert_eq!(
            Provider::GitHub.releases_page("launcher-rs/rumark"),
            "https://github.com/launcher-rs/rumark/releases/latest"
        );

        assert_eq!(
            Provider::Gitee.api_url("owner/repo"),
            "https://gitee.com/api/v5/repos/owner/repo/releases/latest"
        );
        assert_eq!(
            Provider::Gitee.releases_page("owner/repo"),
            "https://gitee.com/owner/repo/releases/latest"
        );

        let gitlab = Provider::GitLab {
            base_url: "https://gitlab.com/".into(),
        };
        assert_eq!(
            gitlab.api_url("owner/repo"),
            "https://gitlab.com/api/v4/projects/owner%2Frepo/releases/permalink/latest"
        );
        assert_eq!(
            gitlab.releases_page("owner/repo"),
            "https://gitlab.com/owner/repo/-/releases"
        );

        let custom = Provider::Custom {
            api_url: "https://git.example.com/api/latest".into(),
            releases_page: "https://git.example.com/owner/repo/releases".into(),
        };
        assert_eq!(
            custom.api_url("ignored"),
            "https://git.example.com/api/latest"
        );
        assert_eq!(
            custom.releases_page("ignored"),
            "https://git.example.com/owner/repo/releases"
        );
    }

    #[test]
    fn encode_project_path_handles_nested_groups_and_specials() {
        assert_eq!(encode_project_path("owner/repo"), "owner%2Frepo");
        assert_eq!(encode_project_path("group/sub/repo"), "group%2Fsub%2Frepo");
        assert_eq!(encode_project_path("a b+c"), "a%20b%2Bc");
    }

    #[test]
    fn config_api_url_follows_provider() {
        let mut config = config();
        assert!(config.api_url().contains("api.github.com"));
        config.provider = Provider::Gitee;
        assert!(config.api_url().contains("gitee.com/api/v5"));
        assert!(config.releases_page().contains("gitee.com"));
    }

    const GITEE_RELEASE: &str = r#"{
        "tag_name": "v0.3.0",
        "name": "v0.3.0",
        "body": "Gitee 更新说明",
        "created_at": "2026-11-20T10:00:00+08:00",
        "assets": [
            {"name": "demo-asset.zip", "browser_download_url": "https://gitee.com/owner/repo/releases/download/v0.3.0/demo-asset.zip", "size": 456}
        ]
    }"#;

    #[test]
    fn parses_gitee_release_with_created_at_fallback() {
        let release = ReleaseInfo::parse_for(&Provider::Gitee, GITEE_RELEASE).unwrap();
        assert_eq!(release.tag_name, "v0.3.0");
        assert_eq!(release.body, "Gitee 更新说明");
        // Gitee 较老实例没有 published_at，应回退到 created_at。
        assert_eq!(release.published_at, "2026-11-20T10:00:00+08:00");
        assert_eq!(release.assets[0].size, 456);
        assert!(release.assets[0].browser_download_url.contains("gitee.com"));
    }

    const GITLAB_RELEASE: &str = r#"{
        "tag_name": "v0.4.0",
        "description": "GitLab release notes",
        "released_at": "2026-11-22T08:00:00Z",
        "created_at": "2026-11-21T08:00:00Z",
        "assets": {
            "count": 2,
            "links": [
                {
                    "id": 1,
                    "name": "demo-asset.zip",
                    "url": "https://gitlab.com/owner/repo/-/releases/v0.4.0/downloads/demo-asset.zip",
                    "direct_asset_url": "https://gitlab.com/owner/repo/-/releases/v0.4.0/downloads/demo-asset.zip?direct=1",
                    "link_type": "other"
                },
                {"id": 2, "name": "docs", "url": "https://gitlab.com/owner/repo/-/blob/main/README.md"}
            ]
        }
    }"#;

    #[test]
    fn parses_gitlab_release_links_and_dates() {
        let provider = Provider::GitLab {
            base_url: "https://gitlab.com".into(),
        };
        let release = ReleaseInfo::parse_for(&provider, GITLAB_RELEASE).unwrap();
        assert_eq!(release.tag_name, "v0.4.0");
        assert_eq!(release.body, "GitLab release notes");
        // released_at 优先于 created_at。
        assert_eq!(release.published_at, "2026-11-22T08:00:00Z");
        assert_eq!(release.assets.len(), 2);
        assert_eq!(release.assets[0].name, "demo-asset.zip");
        // direct_asset_url 优先于 url。
        assert!(release.assets[0].browser_download_url.contains("direct=1"));
        // GitLab 不提供资产大小。
        assert_eq!(release.assets[0].size, 0);
        // 没有 direct_asset_url 时回退到 url。
        assert!(release.assets[1].browser_download_url.contains("README.md"));
    }

    #[test]
    fn gitlab_release_falls_back_to_created_at_and_plain_url() {
        let body = r#"{
            "tag_name": "v1.0.0",
            "created_at": "2026-01-01T00:00:00Z",
            "assets": {"links": [{"name": "demo.zip", "url": "https://example.com/demo.zip"}]}
        }"#;
        let provider = Provider::GitLab {
            base_url: "https://gitlab.com".into(),
        };
        let release = ReleaseInfo::parse_for(&provider, body).unwrap();
        assert_eq!(release.published_at, "2026-01-01T00:00:00Z");
        assert_eq!(
            release.assets[0].browser_download_url,
            "https://example.com/demo.zip"
        );
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

    #[test]
    fn staged_file_name_uses_exe_suffix() {
        assert_eq!(staged_file_name("rumark.exe"), "rumark.exe.new");
    }

    #[test]
    fn load_staged_update_requires_matching_marker_and_nonempty_file() {
        let dir = std::env::temp_dir().join(format!(
            "rgpui-extra-staged-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // 没有任何文件 → 无缓存。
        assert!(load_staged_update(&dir, "demo.exe", "0.2.4").is_none());

        // 版本标记不匹配 → 无缓存（防止误装旧版本）。
        std::fs::write(dir.join("demo.exe.new.version"), "0.2.3").unwrap();
        std::fs::write(dir.join("demo.exe.new"), b"binary").unwrap();
        assert!(load_staged_update(&dir, "demo.exe", "0.2.4").is_none());

        // 版本匹配（容忍首尾空白）且文件非空 → 命中缓存。
        std::fs::write(dir.join("demo.exe.new.version"), " 0.2.4\n").unwrap();
        let staged = load_staged_update(&dir, "demo.exe", "0.2.4").unwrap();
        assert_eq!(staged.version, "0.2.4");
        assert_eq!(staged.path, dir.join("demo.exe.new"));

        // 可执行文件为空 → 无缓存。
        std::fs::write(dir.join("demo.exe.new"), b"").unwrap();
        assert!(load_staged_update(&dir, "demo.exe", "0.2.4").is_none());

        // 可执行文件缺失 → 无缓存。
        std::fs::remove_file(dir.join("demo.exe.new")).unwrap();
        assert!(load_staged_update(&dir, "demo.exe", "0.2.4").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
