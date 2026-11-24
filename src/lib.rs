//! rgpui 生态的官方扩展库（长期存在，参照 egui_extras / iced_aw）。
//!
//! 这里收纳"有用但不是每个 GUI 程序都需要"的组件，以 feature + 模块形式提供，按需取用。
//! 绝大多数组件会一直留在本库；只有极少数基础组件可能回流主库。治理规则见仓库的 GOVERNANCE.md。
//!
//! # Feature
//!
//! - `updater`：GitHub Release 版本检测与分块下载组件，核心逻辑零 GUI 依赖。
//!   提供 `updater::{Config, Version, check, download_and_stage, ...}`。
//! - `ui`：在 `updater` 基础上加入 rgpui 对话框与状态栏进度模型，
//!   提供 `updater::ui::{render_update_dialog, UpdateStatus, ...}`。
//!   CLI 程序不要开启此 feature，即可完全不引入 rgpui。
//!
//! # 新增组件
//!
//! 1. 在 `src/` 下新建模块目录。
//! 2. 在 `Cargo.toml` 添加对应 feature 与可选依赖，并在本文件加 `#[cfg(feature = "...")]` 模块声明。
//! 3. 在 README 中登记组件状态（孵化中 / 稳定）。

#[cfg(feature = "updater")]
pub mod updater;
