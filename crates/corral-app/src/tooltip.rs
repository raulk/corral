//! Hover popover for a tile.
//!
//! The strip window is sized exactly to its visible content, so an
//! in-window tooltip has nowhere to grow. Tooltips therefore open in their
//! own borderless pop-up window, anchored to the left of the strip at the
//! hovered tile's y coordinate. The strip's hover handlers drive both the
//! `open` and `close` paths; macOS does not need focus on the pop-up since
//! it is purely decorative.

use crate::theme;
use chrono::{DateTime, Utc};
use corral_core::agent::Tool;
use corral_core::proc::ProcessId;
use corral_core::status::AgentState;
use corral_core::text::truncate_end;
use gpui::{
    App, Bounds, Context, IntoElement, ParentElement, Pixels, Point, Render, Styled, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div, point,
    prelude::*, px, relative, rgb, size,
};
use std::path::PathBuf;
use uuid::Uuid;

const TOOLTIP_BG: u32 = 0x1c1c20;
const TOOLTIP_FG: u32 = 0xe7e7ea;
const TOOLTIP_FG_DIM: u32 = 0x8a8a91;
const TOOLTIP_BORDER: u32 = 0x303035;
const CONTEXT_BAR_TRACK: u32 = 0x2d2d33;
const CONTEXT_BAR_HEIGHT: f32 = 3.0;

const TOOLTIP_WIDTH: f32 = 320.0;
const TOOLTIP_HEIGHT: f32 = 248.0;
const TOOLTIP_GAP_FROM_STRIP: f32 = 8.0;
const TOOLTIP_RADIUS: f32 = 8.0;
const FOCUS_ERROR_WIDTH: f32 = 260.0;
const FOCUS_ERROR_HEIGHT: f32 = 86.0;
const TOOLTIP_LABEL_W: f32 = 70.0;
const CWD_MAX_CHARS: usize = 32;
const HOST_MAX_CHARS: usize = 28;
const TITLE_MAX_CHARS: usize = 38;
const ACTION_MAX_CHARS: usize = 38;
const BRANCH_MAX_CHARS: usize = 28;
const HEADER_ROW_H: f32 = 18.0;
const TITLE_ROW_H: f32 = 17.0;
const CONTEXT_BLOCK_H: f32 = 34.0;
const INFO_ROW_H: f32 = 16.0;
const INFO_ROW_GAP: f32 = 4.0;
const BASE_INFO_ROWS: usize = 7;

#[derive(Debug, Clone)]
pub struct TooltipData {
    pub pid: ProcessId,
    pub tool: Tool,
    pub cwd: Option<PathBuf>,
    pub session_id: Uuid,
    pub state: AgentState,
    pub last_lifecycle_at: Option<DateTime<Utc>>,
    pub subagent_count: usize,
    pub host_app: Option<String>,
    pub model: Option<String>,
    pub git_branch: Option<String>,
    pub session_title: Option<String>,
    pub current_action: Option<String>,
    pub last_action: Option<String>,
    pub context_tokens: Option<u32>,
    pub context_max: Option<u32>,
}

struct TooltipView {
    data: TooltipData,
}

struct FocusErrorView {
    message: String,
}

impl Render for FocusErrorView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .size_full()
            .px_3()
            .py_2()
            .rounded(px(TOOLTIP_RADIUS))
            .border_1()
            .border_color(rgb(0x7f1d1d))
            .bg(rgb(0x211416))
            .text_color(rgb(TOOLTIP_FG))
            .text_size(px(11.0))
            .shadow_lg()
            .child(
                div()
                    .text_color(rgb(0xfca5a5))
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Couldn’t focus terminal"),
            )
            .child(
                div()
                    .text_color(rgb(0xd4d4d8))
                    .text_size(px(10.5))
                    .line_height(px(14.0))
                    .child(self.message.clone()),
            )
    }
}

impl Render for TooltipView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .px_3()
            .py_2()
            .rounded(px(TOOLTIP_RADIUS))
            .border_1()
            .border_color(rgb(TOOLTIP_BORDER))
            .bg(rgb(TOOLTIP_BG))
            .text_color(rgb(TOOLTIP_FG))
            .text_size(px(11.0))
            .shadow_lg()
            .child(info_block(&self.data))
    }
}

/// Build the visual content of an agent's info card (header + divider +
/// data rows). Shared between the hover tooltip and the per-tile context
/// menu so both stay in lock-step.
pub fn info_block(d: &TooltipData) -> gpui::AnyElement {
    let accent = theme::tool_accent(d.tool);

    let lifecycle = d
        .last_lifecycle_at
        .map(|t| format!("{} ago", format_age(Utc::now().signed_duration_since(t))))
        .unwrap_or_else(|| "—".into());

    // Header: tool name + state chip. The title (if any) sits on a
    // second line below — bigger fonts and a different colour so it
    // reads as the dominant visual element of the card.
    let header = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().w(px(8.0)).h(px(8.0)).rounded_full().bg(rgb(accent)))
                .child(
                    div()
                        .flex_grow()
                        .overflow_hidden()
                        .text_color(rgb(TOOLTIP_FG))
                        .text_size(px(12.5))
                        .truncate()
                        .child(tool_label(d.tool)),
                )
                .child(state_chip(d.state)),
        )
        .when_some(d.session_title.as_deref(), |this, title| {
            this.child(
                div()
                    .h(px(TITLE_ROW_H))
                    .overflow_hidden()
                    .text_color(rgb(TOOLTIP_FG))
                    .text_size(px(12.0))
                    .line_height(px(TITLE_ROW_H))
                    .truncate()
                    .child(truncate_end(title, TITLE_MAX_CHARS)),
            )
        });

    // The "divider" doubles as a context-usage gauge: a grey track
    // with the agent's state colour filling left-to-right to mark
    // tokens-used / context-window. A legend below shows the same
    // figure numerically.
    let bar_color = theme::state_color(d.state);
    let context_fraction: f32 = match (d.context_tokens, d.context_max) {
        (Some(t), Some(m)) if m > 0 => (t as f32 / m as f32).clamp(0.0, 1.0),
        _ => 0.0,
    };
    let context_legend = match (d.context_tokens, d.context_max) {
        (Some(t), Some(m)) if m > 0 => format!(
            "{} / {} ({}%)",
            format_tokens(t),
            format_tokens(m),
            (context_fraction * 100.0).round() as u32
        ),
        (Some(t), _) => format_tokens(t),
        _ => "—".into(),
    };

    let activity_value = if d.subagent_count > 0 {
        format!("{lifecycle} · {} subagents", d.subagent_count)
    } else {
        lifecycle
    };

    div()
        .flex()
        .flex_col()
        .text_color(rgb(TOOLTIP_FG))
        .text_size(px(11.0))
        .child(header)
        // Context-usage gauge + legend take the place of the divider.
        // A bit of top breathing room so the title doesn't crowd the
        // bar; the legend then sits below the bar in dim text.
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .mt(px(8.0))
                .mb(px(8.0))
                .child(
                    // Track + fill. The outer div *is* the track (grey
                    // with rounded ends); the inner div fills it with
                    // the state colour up to `context_fraction` using
                    // `relative()` so the fill width follows the
                    // tooltip's actual width without us measuring it.
                    div()
                        .relative()
                        .h(px(CONTEXT_BAR_HEIGHT))
                        .w_full()
                        .rounded(px(CONTEXT_BAR_HEIGHT * 0.5))
                        .bg(rgb(CONTEXT_BAR_TRACK))
                        .child(
                            div()
                                .absolute()
                                .top(px(0.0))
                                .left(px(0.0))
                                .h_full()
                                .w(relative(context_fraction.max(0.0)))
                                .rounded(px(CONTEXT_BAR_HEIGHT * 0.5))
                                .bg(rgb(bar_color)),
                        ),
                )
                .child(
                    div()
                        .overflow_hidden()
                        .text_color(rgb(TOOLTIP_FG_DIM))
                        .text_size(px(10.5))
                        .truncate()
                        .child(context_legend),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .when_some(d.current_action.as_deref(), |this, action| {
                    this.child(row("doing now", truncate_end(action, ACTION_MAX_CHARS)))
                })
                .when_some(d.last_action.as_deref(), |this, action| {
                    this.child(row("last did", truncate_end(action, ACTION_MAX_CHARS)))
                })
                .child(row(
                    "model",
                    d.model
                        .as_deref()
                        .map(short_model)
                        .unwrap_or_else(|| "—".into()),
                ))
                .child(row(
                    "branch",
                    d.git_branch
                        .as_deref()
                        .map(|s| truncate_end(s, BRANCH_MAX_CHARS))
                        .unwrap_or_else(|| "—".into()),
                ))
                .child(row(
                    "cwd",
                    d.cwd
                        .as_deref()
                        .map(|p| truncate_middle(&p.display().to_string(), CWD_MAX_CHARS))
                        .unwrap_or_else(|| "—".into()),
                ))
                .child(row(
                    "host",
                    d.host_app
                        .as_deref()
                        .map(|s| truncate_end(s, HOST_MAX_CHARS))
                        .unwrap_or_else(|| "—".into()),
                ))
                .child(row("PID", d.pid.to_string()))
                .child(row("session", short_session_id(&d.session_id)))
                .child(row("last turn", activity_value)),
        )
        .into_any_element()
}

/// Height model for `info_block`.
///
/// The context menu host window must be sized before GPUI lays out its
/// children, so callers cannot ask the rendered block for its measured
/// height. Keep this in the same module as `info_block` so optional rows
/// and spacing stay coupled to the visual content.
pub fn info_block_height(d: &TooltipData) -> f32 {
    let mut h = HEADER_ROW_H + CONTEXT_BLOCK_H;
    if d.session_title.is_some() {
        h += TITLE_ROW_H;
    }

    let row_count = BASE_INFO_ROWS
        + usize::from(d.current_action.is_some())
        + usize::from(d.last_action.is_some());
    h += row_count as f32 * INFO_ROW_H;
    if row_count > 1 {
        h += (row_count - 1) as f32 * INFO_ROW_GAP;
    }
    h
}

fn format_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f32 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", (n + 500) / 1_000)
    } else {
        n.to_string()
    }
}

/// Strip date/dash noise from Claude's model ids so the row reads as
/// `opus-4-7` rather than `claude-opus-4-7-20250513`.
fn short_model(model: &str) -> String {
    let trimmed = model.strip_prefix("claude-").unwrap_or(model);
    // Drop trailing `-YYYYMMDD` if present.
    let parts: Vec<&str> = trimmed.split('-').collect();
    if let Some(last) = parts.last()
        && last.len() == 8
        && last.chars().all(|c| c.is_ascii_digit())
    {
        return parts[..parts.len() - 1].join("-");
    }
    trimmed.to_string()
}

fn row(label: &'static str, value: String) -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .gap_2()
        .items_baseline()
        .h(px(INFO_ROW_H))
        .overflow_hidden()
        .line_height(px(INFO_ROW_H))
        .child(
            div()
                .w(px(TOOLTIP_LABEL_W))
                .h(px(INFO_ROW_H))
                .overflow_hidden()
                .text_color(rgb(TOOLTIP_FG_DIM))
                .truncate()
                .child(label),
        )
        .child(
            div()
                .flex_grow()
                .h(px(INFO_ROW_H))
                .overflow_hidden()
                .truncate()
                .child(value),
        )
        .into_any_element()
}

fn state_chip(state: AgentState) -> impl IntoElement {
    let color = theme::state_color(state);
    let label = theme::state_label(state);
    div()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .bg(rgb(0x26262c))
        .text_color(rgb(color))
        .text_size(px(10.0))
        .truncate()
        .child(label)
}

/// Live handle for the currently-visible tooltip pop-up window, if any.
/// Stored as an `App` global so the strip's hover handlers can swap or
/// dismiss it without holding direct references between modules.
#[derive(Default)]
pub struct TooltipPopup(Option<WindowHandle<TooltipView>>);
impl gpui::Global for TooltipPopup {}

#[derive(Default)]
pub struct FocusErrorPopup(Option<WindowHandle<FocusErrorView>>);
impl gpui::Global for FocusErrorPopup {}

/// Open or replace the hover popup with `data`. `strip_origin` is the
/// strip window's top-left in screen coordinates and `anchor_y` is the
/// hovered tile's y in strip-window-local coordinates; together they
/// anchor the popup to the left of the strip at the cursor's y.
pub fn open(cx: &mut App, data: TooltipData, strip_origin: Point<Pixels>, anchor_y: Pixels) {
    // Dismiss any prior popup before opening a new one. macOS animates the
    // swap so quickly that consecutive hovers across tiles look fluid.
    close(cx);

    let origin_x = strip_origin.x - px(TOOLTIP_WIDTH) - px(TOOLTIP_GAP_FROM_STRIP);
    let origin_y = strip_origin.y + anchor_y - px(TOOLTIP_HEIGHT * 0.5);

    // Clamp inside the primary display so the popup never opens
    // partially off-screen (small strip+anchor combinations near the
    // top/bottom would otherwise place the popup outside the screen).
    let (origin_x, origin_y) = if let Some(display) = cx.primary_display() {
        let db = display.bounds();
        let min_x = db.origin.x;
        let min_y = db.origin.y;
        let max_x = db.origin.x + db.size.width - px(TOOLTIP_WIDTH);
        let max_y = db.origin.y + db.size.height - px(TOOLTIP_HEIGHT);
        (origin_x.clamp(min_x, max_x), origin_y.clamp(min_y, max_y))
    } else {
        (origin_x, origin_y)
    };

    let bounds = Bounds {
        origin: point(origin_x, origin_y),
        size: size(px(TOOLTIP_WIDTH), px(TOOLTIP_HEIGHT)),
    };

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        window_background: WindowBackgroundAppearance::Transparent,
        focus: false,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        app_id: None,
        window_min_size: None,
        window_decorations: None,
        tabbing_identifier: None,
        ..Default::default()
    };

    tracing::debug!(
        ?strip_origin,
        anchor_y = %anchor_y,
        origin = ?bounds.origin,
        "tooltip: opening popup"
    );
    match cx.open_window(options, |_, app| app.new(|_| TooltipView { data })) {
        Ok(handle) => {
            cx.set_global(TooltipPopup(Some(handle)));
        }
        Err(e) => {
            tracing::warn!(error = %e, "tooltip: open_window failed");
        }
    }
}

pub fn open_focus_error(
    cx: &mut App,
    message: String,
    strip_origin: Point<Pixels>,
    anchor_y: Pixels,
) {
    close_focus_error(cx);

    let origin_x = strip_origin.x - px(FOCUS_ERROR_WIDTH) - px(TOOLTIP_GAP_FROM_STRIP);
    let origin_y = strip_origin.y + anchor_y - px(FOCUS_ERROR_HEIGHT * 0.5);
    open_focus_error_at(cx, message, point(origin_x, origin_y));
}

pub fn open_focus_error_default(cx: &mut App, message: String) {
    let origin = if let Some(display) = cx.primary_display() {
        let db = display.bounds();
        point(
            db.origin.x + db.size.width - px(FOCUS_ERROR_WIDTH + 52.0),
            db.origin.y + db.size.height * 0.5 - px(FOCUS_ERROR_HEIGHT * 0.5),
        )
    } else {
        point(px(20.0), px(20.0))
    };
    open_focus_error_at(cx, message, origin);
}

fn open_focus_error_at(cx: &mut App, message: String, origin: Point<Pixels>) {
    let (origin_x, origin_y) = if let Some(display) = cx.primary_display() {
        let db = display.bounds();
        let min_x = db.origin.x;
        let min_y = db.origin.y;
        let max_x = db.origin.x + db.size.width - px(FOCUS_ERROR_WIDTH);
        let max_y = db.origin.y + db.size.height - px(FOCUS_ERROR_HEIGHT);
        (origin.x.clamp(min_x, max_x), origin.y.clamp(min_y, max_y))
    } else {
        (origin.x, origin.y)
    };

    let bounds = Bounds {
        origin: point(origin_x, origin_y),
        size: size(px(FOCUS_ERROR_WIDTH), px(FOCUS_ERROR_HEIGHT)),
    };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        window_background: WindowBackgroundAppearance::Transparent,
        focus: false,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        app_id: None,
        window_min_size: None,
        window_decorations: None,
        tabbing_identifier: None,
        ..Default::default()
    };

    match cx.open_window(options, |_, app| app.new(|_| FocusErrorView { message })) {
        Ok(handle) => {
            cx.set_global(FocusErrorPopup(Some(handle)));
            let async_app = cx.to_async();
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(4))
                    .await;
                let _ = async_app.update(close_focus_error);
            })
            .detach();
        }
        Err(e) => {
            tracing::warn!(error = %e, "tooltip: focus error window failed");
        }
    }
}

/// Close the active hover popup, if one is open. Safe to call when none.
pub fn close(cx: &mut App) {
    let handle = cx.try_global::<TooltipPopup>().and_then(|p| p.0);
    if let Some(handle) = handle {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
        cx.set_global(TooltipPopup(None));
    }
}

pub fn close_focus_error(cx: &mut App) {
    let handle = cx.try_global::<FocusErrorPopup>().and_then(|p| p.0);
    if let Some(handle) = handle {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
        cx.set_global(FocusErrorPopup(None));
    }
}

fn tool_label(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => "Claude",
        Tool::CodexCli => "Codex CLI",
        Tool::CodexAppServer => "Codex (app)",
    }
}

fn short_session_id(u: &Uuid) -> String {
    let s = u.to_string();
    // Last 8 hex chars after the final hyphen are enough to disambiguate
    // and short enough to read at a glance.
    s.rsplit('-').next().unwrap_or(&s).to_string()
}

fn truncate_middle(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let half = (max - 1) / 2;
    let chars: Vec<char> = s.chars().collect();
    let head: String = chars[..half].iter().collect();
    let tail: String = chars[chars.len() - half..].iter().collect();
    format!("{head}…{tail}")
}

fn format_age(d: chrono::Duration) -> String {
    let total = d.num_seconds().max(0);
    if total < 60 {
        format!("{total}s")
    } else if total < 3600 {
        format!("{}m", total / 60)
    } else if total < 86_400 {
        format!("{}h", total / 3600)
    } else {
        format!("{}d", total / 86_400)
    }
}
