# rgpui-extra

rgpui 生态的扩展组件库，按需启用 feature，不引入用不到的依赖。

当前组件：

| 组件 | Feature | 说明 |
| --- | --- | --- |
| `updater` | `updater` | 多平台 Release 版本检测与分块下载，核心逻辑零 GUI 依赖 |

## 安装

```toml
[dependencies]
rgpui-extra = { version = "0.1", features = ["updater"] }
```

GUI 程序如需现成的更新对话框与进度模型，启用 `ui` feature（`ui` 会自动带上 `updater`）：

```toml
[dependencies]
rgpui-extra = { version = "0.1", features = ["ui"] }
```

Feature 说明：

- `updater`：纯逻辑，不依赖 rgpui，CLI 与 GUI 程序都能用。
- `ui`：在 `updater` 之上加入 rgpui 对话框与状态栏进度模型。CLI 程序不要开启，即可完全不引入 rgpui。

## updater 用法

```rust
use rgpui_extra::updater::{Config, check, download_and_stage, run_blocking};

let config = Config::new("launcher-rs/demo", "demo");
// 网络请求是 blocking 的，必须放到后台线程，避免阻塞 UI。
let result = run_blocking(move || {
    let check = check(&config, env!("CARGO_PKG_VERSION"))?;
    if !check.newer {
        return Ok(None);
    }
    let asset = check.asset.context("当前平台没有可用更新包")?;
    let version = check.remote.version_string();
    let staged = download_and_stage(&config, &asset, &version, &config_updates_dir, |progress| {
        // 回调在后台线程执行；GUI 程序应在此只做线程安全的消息投递。
        println!("{}", progress.label());
    })?;
    Ok(Some(staged))
})
.await;
```

`Config` 可覆盖的字段：

| 字段 | 说明 |
| --- | --- |
| `repository` | 仓库路径，如 `launcher-rs/rumark` |
| `provider` | 发布平台，默认 `Provider::GitHub` |
| `app_name` | 应用名，用于推导默认资产名 |
| `executable_name` | 压缩包内可执行文件名称 |
| `user_agent` | HTTP User-Agent |
| `asset_name` | 覆盖默认资产名，跨平台统一命名时使用 |
| `custom_mirrors` | 用户自定义镜像前缀列表 |
| `use_mirrors` | 是否启用镜像中转，默认 `false` |

默认资产名按平台推导：`{app}-windows-x86_64.zip`、`{app}-linux-x86_64.tar.gz`、`{app}-macos-aarch64.tar.gz`。

### 发布平台

`provider` 决定 Release API / 发布页地址与 JSON 解析方式，默认 GitHub：

| 平台 | 写法 | API |
| --- | --- | --- |
| GitHub | `Provider::GitHub` | `api.github.com/repos/.../releases/latest` |
| Gitee | `Provider::Gitee` | `gitee.com/api/v5/repos/.../releases/latest` |
| GitLab.com / 自托管 | `Provider::GitLab { base_url }` | `<base>/api/v4/projects/<编码路径>/releases/permalink/latest` |
| Gitea / Forgejo 等 | `Provider::Custom { api_url, releases_page }` | 自定义地址，JSON 按 GitHub 结构解析 |

```rust
use rgpui_extra::updater::{Config, Provider};

let mut config = Config::new("owner/repo", "demo");
config.provider = Provider::Gitee;
// 自托管 GitLab：Provider::GitLab { base_url: "https://git.example.com".into() }
```

Gitee 的 Release JSON 与 GitHub 同构；GitLab 的 `assets.links` / `released_at` 会自动转换为统一结构，下载链接优先取 `direct_asset_url`。版本比较、镜像回退、暂存复用逻辑与平台无关。

### 镜像中转

默认只直连当前平台的 API 与资产域名（镜像主要面向 GitHub）。开启 `use_mirrors` 后，按以下顺序尝试并去重：

1. `custom_mirrors` 中的自定义前缀
2. 内置 `DEFAULT_MIRRORS`（`ghfast.top`、`ghproxy.net`）
3. 直连（空串，永远兜底）

镜像以前缀形式套在原始 URL 外：`<mirror>/<original-url>`。

### rgpui UI 层（`ui` feature）

- `UpdateDialogStrings` / `UpdateDialogContent`：由使用方提供文案与版本信息，组件不内置语言或品牌文本。
- `render_update_dialog()`：返回可挂到渲染树的覆盖层元素；更新说明区限高可滚动。
- `UpdateChoice`：对话框按钮选择（下载 / 稍后 / 打开发布页）。
- `UpdateStatus` / `UpdatePhase`：纯数据的进度模型，使用方存进自己的实体并在进度回调中 `cx.notify()`。

### 安全边界

检测到下载落地默认不替换正在运行的程序、不自动重启。如需单文件替换 + 重启，可使用 `InstallOptions` / `spawn_install_helper`：它会在进程退出后用新 exe 覆盖旧 exe 并重启（Windows 上运行中的 exe 无法自覆盖，故需要独立 helper）。触发时机仍由使用方决定。

## 许可

Apache-2.0，见 [LICENSE-APACHE](LICENSE-APACHE)。
