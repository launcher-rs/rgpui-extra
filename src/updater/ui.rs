//! rgpui 的可选 UI 层：更新对话框与状态栏进度模型。
//!
//! 开启 `ui` feature 后可用。核心逻辑不依赖本模块，纯 CLI 程序无需引入 rgpui。

use rgpui::*;

use super::DownloadProgress;

/// 对话框文案。由使用方提供，组件本身不内置任何语言/品牌文本。
#[derive(Clone, Debug)]
pub struct UpdateDialogStrings {
    pub title: String,
    pub current_label: String,
    pub new_label: String,
    pub published_label: String,
    pub notes_label: String,
    pub download: String,
    pub later: String,
    pub open_release: String,
    /// 更新说明为空时的占位文本。
    pub empty_notes: String,
}

/// 用户在更新对话框中的选择，与按钮顺序 0/1/2 对应。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateChoice {
    Download = 0,
    Later = 1,
    OpenRelease = 2,
}

/// 更新进度阶段（状态栏展示用）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    Downloading,
}

/// 状态栏进度模型：纯数据，由使用方存进自己的实体并在进度回调里 `cx.notify()`。
#[derive(Clone, Debug, Default)]
pub struct UpdateStatus {
    pub phase: UpdatePhase,
    pub progress: Option<DownloadProgress>,
}

impl UpdateStatus {
    pub fn checking() -> Self {
        Self {
            phase: UpdatePhase::Checking,
            progress: None,
        }
    }

    pub fn downloading() -> Self {
        Self {
            phase: UpdatePhase::Downloading,
            progress: None,
        }
    }

    /// 进度条用的 0.0..=1.0 值；未知或空闲时为 `None`。
    pub fn fraction(&self) -> Option<f32> {
        match self.phase {
            UpdatePhase::Downloading => self.progress.and_then(|p| p.fraction()),
            _ => None,
        }
    }

    /// 状态栏文本：检查中显示固定文案，下载中附加百分比与字节数。
    pub fn label(&self, checking_label: &str, downloading_label: &str) -> Option<String> {
        match self.phase {
            UpdatePhase::Idle => None,
            UpdatePhase::Checking => Some(checking_label.to_string()),
            UpdatePhase::Downloading => {
                let mut label = downloading_label.to_string();
                if let Some(progress) = self.progress {
                    label.push(' ');
                    if let Some(percent) = progress.percent() {
                        label.push_str(&format!("{percent}% · "));
                    }
                    label.push_str(&progress.label());
                }
                Some(label)
            }
        }
    }
}

/// 对话框中展示的版本信息。打包成结构避免渲染函数参数过长。
#[derive(Clone, Debug)]
pub struct UpdateDialogContent {
    pub current: String,
    pub remote: String,
    pub published: String,
    pub notes: String,
    /// 更新说明区最大高度（像素），超出可滚动。
    pub notes_max_height: f32,
}

/// 渲染「发现新版本」对话框。说明区限制最大高度并支持滚动。
///
/// 返回的是可直接挂到编辑器渲染树上的覆盖层元素；`on_choice` 是按钮回调，
/// 使用方在其中继续下载 / 打开发布页 / 关闭对话框。
pub fn render_update_dialog<V: 'static>(
    cx: &mut Context<V>,
    strings: &UpdateDialogStrings,
    content: &UpdateDialogContent,
    on_choice: fn(&mut V, UpdateChoice, &mut Window, &mut Context<V>),
) -> AnyElement {
    let theme = cx.theme().clone();
    let strings = strings.clone();
    let current = content.current.clone();
    let remote = content.remote.clone();
    let published = content.published.clone();
    let notes_max_height = content.notes_max_height;
    let notes = if content.notes.trim().is_empty() {
        strings.empty_notes.clone()
    } else {
        content.notes.clone()
    };

    let content = Dialog::new(cx)
        .width(px(560.0))
        .overlay(false)
        .close_button(false)
        .keyboard(false)
        .title(strings.title.clone())
        .content(move |content, _window, cx| {
            let theme = cx.theme().clone();
            let row = |label: String, value: String| {
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        div()
                            .font_weight(FontWeight(600.0))
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .child(div().child(value))
            };
            content
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(row(strings.current_label.clone(), current.clone()))
                        .child(row(strings.new_label.clone(), remote.clone()))
                        .child(row(strings.published_label.clone(), published.clone())),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .mt(px(12.0))
                        .child(
                            div()
                                .font_weight(FontWeight(600.0))
                                .text_color(theme.muted_foreground)
                                .child(strings.notes_label.clone()),
                        )
                        // 关键：限制更新说明高度，内容多时滚动而不是撑满屏幕。
                        .child(
                            div()
                                .w_full()
                                .max_h(px(notes_max_height))
                                .overflow_y_scrollbar()
                                .p(px(10.0))
                                .rounded_md()
                                .bg(theme.input_background())
                                .child(notes.clone()),
                        ),
                )
        })
        .footer(
            div()
                .flex()
                .justify_end()
                .gap(px(8.0))
                .child(
                    Button::new("updater-dialog-later")
                        .label(strings.later.clone())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            on_choice(this, UpdateChoice::Later, window, cx)
                        })),
                )
                .child(
                    Button::new("updater-dialog-open-release")
                        .label(strings.open_release.clone())
                        .secondary()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            on_choice(this, UpdateChoice::OpenRelease, window, cx)
                        })),
                )
                .child(
                    Button::new("updater-dialog-download")
                        .label(strings.download.clone())
                        .primary()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            on_choice(this, UpdateChoice::Download, window, cx)
                        })),
                ),
        );

    div()
        .id("updater-dialog-overlay")
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .bg(theme.overlay)
        .child(content)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    // 不能用 `use super::*`：父模块的 `use rgpui::*` 会带来同名 `test` 宏导致递归展开。
    use super::{DownloadProgress, UpdateStatus};

    #[test]
    fn status_label_reports_phase_and_percentage() {
        let mut status = UpdateStatus::checking();
        assert_eq!(
            status.label("checking", "downloading").as_deref(),
            Some("checking")
        );
        status = UpdateStatus::downloading();
        status.progress = Some(DownloadProgress {
            downloaded: 1,
            total: Some(4),
        });
        let label = status.label("checking", "downloading").unwrap();
        assert!(label.starts_with("downloading 25%"), "got {label:?}");
        assert_eq!(status.fraction(), Some(0.25));
    }

    #[test]
    fn idle_status_has_no_label() {
        let status = UpdateStatus::default();
        assert_eq!(status.label("checking", "downloading"), None);
        assert_eq!(status.fraction(), None);
    }
}
