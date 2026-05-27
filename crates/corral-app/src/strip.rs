use crate::context_menu;
use crate::tile::{TILE_SIZE, render_tile};
use crate::tooltip;
use crate::window_geom;
use corral_core::agent::{Agent, Tool};
use corral_core::proc::ProcessId;
use corral_core::registry::RegistryEvent;
use corral_core::status::AgentState;
use gpui::{
    App, AsyncApp, Bounds, Context, DispatchPhase, MouseButton, MouseDownEvent, MouseExitEvent,
    MouseMoveEvent, Pixels, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind, WindowOptions, canvas, div, point, prelude::*, px, rgb, size,
};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const STRIP_WIDTH: f32 = 36.0;
const STRIP_RIGHT_GAP: f32 = 8.0;
const STRIP_PADDING: f32 = 8.0;
const TILE_GAP: f32 = 8.0;
/// Height of the divider rendered between tiles in different
/// `AgentState` buckets. Wider than a regular tile gap so the eye
/// reads it as a section break rather than a slight breathing room.
const GROUP_DIVIDER_HEIGHT: f32 = 14.0;
const GROUP_DIVIDER_LINE_W: f32 = 24.0;
const GROUP_DIVIDER_LINE_H: f32 = 2.0;
const GROUP_DIVIDER_LINE: u32 = 0x52525b;
const STRIP_BG: u32 = 0x18181b;
const STRIP_RADIUS: f32 = 10.0;

const HEIGHT_ANIM_DURATION_MS: u64 = 180;
/// How long the window's bounds must stay still after a drag before
/// the dock-back button reveals itself.
const DRAG_SETTLE_MS: u128 = 220;
/// Slop for "did the user actually drag the window?" — covers
/// floating-point round-trips between display and Cocoa coordinate
/// spaces. Smaller deltas read as noise.
const POSITION_SLOP_PX: f32 = 2.0;
/// Slop for "did the animator finish?" — sub-pixel residuals from
/// easing don't warrant another frame.
const RESIZE_SLOP_PX: f32 = 0.5;

#[derive(Debug, Clone)]
struct AgentRow {
    agent: Agent,
}

/// A sentinel `Instant` well in the past, used to mark "already settled"
/// timers and animations whose elapsed time should evaluate as done.
fn long_past_instant() -> Instant {
    Instant::now()
        .checked_sub(Duration::from_secs(3600))
        .unwrap_or_else(Instant::now)
}

#[derive(Debug, Clone, Copy)]
struct Tween {
    from: f32,
    to: f32,
    started_at: Instant,
}

impl Tween {
    fn settled(h: f32) -> Self {
        Self {
            from: h,
            to: h,
            started_at: long_past_instant(),
        }
    }

    fn current(&self) -> f32 {
        let elapsed_ms = self.started_at.elapsed().as_millis() as f32;
        let t = (elapsed_ms / HEIGHT_ANIM_DURATION_MS as f32).clamp(0.0, 1.0);
        let eased = ease_out_cubic(t);
        self.from + (self.to - self.from) * eased
    }

    fn is_done(&self) -> bool {
        self.started_at.elapsed().as_millis() >= HEIGHT_ANIM_DURATION_MS as u128
    }

    /// Begin animating toward a new target while preserving the
    /// current (possibly mid-ramp) value as the new starting point.
    /// No-op when the target hasn't changed.
    fn retarget(&mut self, to: f32) {
        if (to - self.to).abs() < f32::EPSILON {
            return;
        }
        let from = self.current();
        *self = Self {
            from,
            to,
            started_at: Instant::now(),
        };
    }
}

fn ease_out_cubic(t: f32) -> f32 {
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}

pub struct Strip {
    rows: BTreeMap<ProcessId, AgentRow>,
    /// Cached display order from `sorted_rows()`. Invalidated to `None`
    /// on every `apply()`; recomputed lazily on first use within a
    /// render/retarget pass. Saves the per-frame sort allocation when
    /// both `render` and `retarget_height` run in the same batch.
    sorted_order: Option<Vec<ProcessId>>,
    /// Animated **base** height: `STRIP_PADDING * 2 + tiles + dividers`,
    /// plus the dock-back chip allowance when shown. Hover extensions
    /// are tracked separately in `top_ext` / `bot_ext` so they can
    /// anchor on the opposite edge instead of re-centering the strip.
    height: Tween,
    /// Animates the top hover band's extra height between `0` and
    /// `HANDLE_HEIGHT`. Grows the strip *upward*: the top edge moves
    /// up by `top_ext.current()`; the tile region and bottom edge
    /// don't shift.
    top_ext: Tween,
    /// Mirror of `top_ext` anchored on the bottom edge.
    bot_ext: Tween,
    /// Captured at construction; needed to keep the strip vertically
    /// centred each render frame without a fresh display query.
    display_height: f32,
    origin_x: f32,
    /// Where the strip currently lives: snapped to its default centred
    /// position on the right edge, or free-floating after a user drag.
    /// Encoding placement as an enum makes the invalid combination
    /// `(docked && show_dock_back)` unrepresentable.
    placement: WindowPlacement,
    /// The (x, y) we last asked Cocoa to position the window at, so
    /// we can tell "we just animated to here" apart from "the user
    /// dragged us elsewhere".
    expected_origin: (f32, f32),
    /// Last observed origin from `window.bounds()`. Refreshed each
    /// event-pump tick so motion detection works even while the
    /// height animation is settled and `render` would otherwise not
    /// be called.
    prev_observed_origin: (f32, f32),
    /// True while the cursor is over the top hover band. Drives the
    /// animated reveal of the top drag-handle dots: when on,
    /// `retarget_height` grows the strip by `HANDLE_HEIGHT` and the
    /// next render places the dot grid inside the now-expanded band.
    hover_top: bool,
    /// Mirror of `hover_top` for the bottom band.
    hover_bottom: bool,
}

/// Placement of the strip window. `Docked` is the default centred
/// position on the right edge of the display; `Floating` captures
/// the post-drag state with its anchor, motion timer, and dock-back
/// button visibility bundled together.
#[derive(Debug, Clone, Copy)]
enum WindowPlacement {
    /// Vertically centred on the right edge. Position is derived
    /// from `display_height` and the strip's current animated
    /// height every frame; no anchor is stored.
    Docked,
    /// User dragged the strip elsewhere; it stays where dropped.
    Floating {
        /// Screen-y of the base portion's top edge. The OS window's
        /// actual top sits at `base_y - top_ext.current()`.
        base_y: f32,
        /// Timestamp of the most recent observed origin change. We
        /// wait `DRAG_SETTLE_DURATION` of no motion before showing
        /// the dock-back affordance so the user doesn't see it
        /// shake into view mid-drag.
        last_motion_at: Instant,
        /// Whether the dock-back button is currently rendered.
        /// Distinct from "is floating" so we can hold the button
        /// hidden during the drag itself and reveal it only after
        /// the user releases.
        show_dock_back: bool,
    },
}

impl WindowPlacement {
    fn is_docked(&self) -> bool {
        matches!(self, WindowPlacement::Docked)
    }

    /// `true` when free-floating and the dock-back button is shown.
    /// Always `false` in docked mode — the docked invariant.
    fn show_dock_back(&self) -> bool {
        matches!(
            self,
            WindowPlacement::Floating {
                show_dock_back: true,
                ..
            }
        )
    }
}

impl Strip {
    fn new(display_height: f32, origin_x: f32) -> Self {
        let initial = strip_height_for(1);
        let centred_y = (display_height - initial) * 0.5;
        Self {
            rows: BTreeMap::new(),
            sorted_order: None,
            height: Tween::settled(initial),
            top_ext: Tween::settled(0.0),
            bot_ext: Tween::settled(0.0),
            display_height,
            origin_x,
            placement: WindowPlacement::Docked,
            expected_origin: (origin_x, centred_y),
            prev_observed_origin: (origin_x, centred_y),
            hover_top: false,
            hover_bottom: false,
        }
    }

    fn has_active(&self) -> bool {
        self.rows
            .values()
            .any(|r| matches!(r.agent.state, AgentState::Active))
    }

    fn visible_tile_count(&self) -> usize {
        // Empty state still draws a placeholder tile so the strip stays
        // visible while we wait for the first discovery tick.
        self.rows.len().max(1)
    }

    /// Re-dock the strip to its default centred position. The next
    /// `render` will see `WindowPlacement::Docked` and call
    /// `set_strip_frame` with the centred origin, snapping the window
    /// home.
    fn dock_to_default(&mut self) {
        if self.placement.is_docked() {
            return;
        }
        // Switching to `Docked` discards the floating-state
        // `last_motion_at` and `show_dock_back` entirely, so the
        // post-snap window move (driven by our own setFrame) can't
        // re-arm the settle timer and re-show the button. The next
        // drag will transition back to `Floating` with a fresh timer.
        self.placement = WindowPlacement::Docked;
        self.retarget_height();
    }

    fn retarget_height(&mut self) {
        let rows = self.sorted_rows();
        let mut target = if rows.is_empty() {
            strip_height_for(1)
        } else {
            strip_height_for_rows(&rows)
        };
        // The dock-back affordance lives above the tiles when the
        // strip is free-floating *and the drag has settled*; expand
        // the target height to make room only once it's actually
        // rendered.
        if self.placement.show_dock_back() {
            target += DOCK_BUTTON_SIZE + TILE_GAP;
        }
        if (target - self.height.to).abs() < f32::EPSILON {
            return;
        }
        tracing::debug!(
            tiles = self.visible_tile_count(),
            from = self.height.current(),
            to = target,
            "strip: retargeting base height"
        );
        self.height.retarget(target);
    }

    /// Retarget the top hover-band extension to match `hover_top`.
    /// Called from the band's `on_hover` listener — the band's height
    /// in the column is `STRIP_PADDING + top_ext.current()`, and
    /// `render` shifts the strip's top edge up by the same amount so
    /// the tile region stays put.
    fn retarget_top_ext(&mut self) {
        let target = if self.hover_top { HANDLE_HEIGHT } else { 0.0 };
        self.top_ext.retarget(target);
    }

    fn retarget_bot_ext(&mut self) {
        let target = if self.hover_bottom {
            HANDLE_HEIGHT
        } else {
            0.0
        };
        self.bot_ext.retarget(target);
    }

    fn apply(&mut self, ev: RegistryEvent) {
        // Any mutation may change the display order — invalidate the
        // cached sort so the next reader recomputes.
        self.sorted_order = None;
        match ev {
            RegistryEvent::Added(agent) => {
                self.rows.insert(agent.pid, AgentRow { agent });
            }
            RegistryEvent::StateChanged {
                pid,
                new_state,
                last_lifecycle_at,
            } => {
                if let Some(row) = self.rows.get_mut(&pid) {
                    row.agent.state = new_state;
                    row.agent.last_lifecycle_at = last_lifecycle_at;
                }
            }
            RegistryEvent::SubagentCountChanged { pid, count } => {
                if let Some(row) = self.rows.get_mut(&pid) {
                    row.agent.subagent_pids.resize(count, ProcessId(0));
                }
            }
            RegistryEvent::ContextChanged { pid, tokens, max } => {
                if let Some(row) = self.rows.get_mut(&pid) {
                    row.agent.context_tokens = Some(tokens);
                    if max.is_some() {
                        row.agent.context_max = max;
                    }
                }
            }
            RegistryEvent::MetadataChanged {
                pid,
                model,
                git_branch,
                session_title,
                current_action,
                last_action,
            } => {
                // Each MetadataChanged carries the registry's current
                // snapshot for the agent — assign directly. The registry
                // only flips fields from None to Some, never back, so
                // there's no risk of clearing populated data.
                if let Some(row) = self.rows.get_mut(&pid) {
                    row.agent.model = model;
                    row.agent.git_branch = git_branch;
                    row.agent.session_title = session_title;
                    row.agent.current_action = current_action;
                    row.agent.last_action = last_action;
                }
            }
            RegistryEvent::Removed(pid) => {
                self.rows.remove(&pid);
            }
        }
    }
}

/// Total visual height required to render `n` tiles stacked in the strip
/// when every tile is in the same state group (no section dividers).
/// One padding pair on the column, `n` tiles, and `n - 1` gaps between
/// them.
fn strip_height_for(n: usize) -> f32 {
    let n = n.max(1) as f32;
    STRIP_PADDING * 2.0 + n * TILE_SIZE + (n - 1.0).max(0.0) * TILE_GAP
}

/// Visual height required to render the given sequence of agent
/// rows, accounting for the wider `GROUP_DIVIDER_HEIGHT` slot when
/// adjacent rows belong to different state buckets. Mirrors what
/// `render` produces so the OS-window resize stays in sync with the
/// laid-out content.
fn strip_height_for_rows(rows: &[&AgentRow]) -> f32 {
    if rows.is_empty() {
        return strip_height_for(1);
    }
    let mut between = 0.0;
    let mut prev: Option<AgentState> = None;
    for row in rows {
        if let Some(p) = prev {
            between += if p == row.agent.state {
                TILE_GAP
            } else {
                GROUP_DIVIDER_HEIGHT
            };
        }
        prev = Some(row.agent.state);
    }
    STRIP_PADDING * 2.0 + (rows.len() as f32) * TILE_SIZE + between
}

/// Sort priority for stacking tiles top-to-bottom. Lower is higher
/// (drawn first / topmost). `AwaitingUser` and `NeedsInput` surface
/// above `Active` because they're the states where the agent is blocked
/// on the user; `AwaitingUser` outranks `NeedsInput` because the agent
/// literally can't proceed without an answer to its structured prompt.
fn state_priority(state: AgentState) -> u8 {
    match state {
        AgentState::AwaitingUser => 0,
        AgentState::NeedsInput => 1,
        AgentState::Active => 2,
        AgentState::Idle => 3,
        AgentState::Closed => 4,
    }
}

impl Strip {
    /// Compute (or return cached) display order: by state priority,
    /// then by recency within each state. `sorted_order` is the cached
    /// pid sequence; `apply()` clears it on any mutation.
    fn sorted_pids(&mut self) -> Vec<ProcessId> {
        if let Some(order) = &self.sorted_order {
            return order.clone();
        }
        let mut rows: Vec<&AgentRow> = self.rows.values().collect();
        rows.sort_by(|a, b| {
            state_priority(a.agent.state)
                .cmp(&state_priority(b.agent.state))
                // `last_lifecycle_at` is `Option<DateTime>`; `None`
                // sorts as "oldest" (= largest reverse-ordered), which
                // matches the intent: a tile that hasn't reported any
                // lifecycle yet sits below tiles with recent activity.
                .then(b.agent.last_lifecycle_at.cmp(&a.agent.last_lifecycle_at))
                .then(a.agent.pid.0.cmp(&b.agent.pid.0))
        });
        let order: Vec<ProcessId> = rows.iter().map(|r| r.agent.pid).collect();
        self.sorted_order = Some(order.clone());
        order
    }

    /// Tiles in display order. Yields borrowed rows so callers don't
    /// pay a clone for the sort.
    fn sorted_rows(&mut self) -> Vec<&AgentRow> {
        let order = self.sorted_pids();
        order
            .into_iter()
            .filter_map(|pid| self.rows.get(&pid))
            .collect()
    }
}

/// Vertical breathing room between two tiles in the same state
/// bucket. Matches the legacy column `.gap(TILE_GAP)` exactly so the
/// "no group transition" case looks unchanged from before. Returns
/// `AnyElement` so it can sit alongside `group_divider()` in a single
/// `if/else` branch.
fn tile_spacer() -> gpui::AnyElement {
    div().h(px(TILE_GAP)).into_any_element()
}

/// Visual size of the dock-back button (the visible chip). The hit
/// area is bigger — see `DOCK_HIT_PADDING` — to make the post-drag
/// click reliable.
const DOCK_BUTTON_SIZE: f32 = 18.0;
const DOCK_HIT_PADDING: f32 = 4.0;
const DOCK_BUTTON_BG: u32 = 0x232327;
const DOCK_BUTTON_BG_HOVER: u32 = 0x2d2d33;
const DOCK_BUTTON_FG: u32 = 0xa1a1aa;

/// Small circular button that re-docks the strip to its default
/// screen-centred position. Visible only after a drag has settled.
fn dock_back_button(cx: &mut Context<Strip>) -> gpui::AnyElement {
    let dock = cx.listener(
        |strip: &mut Strip, _ev: &MouseDownEvent, _w: &mut Window, ctx: &mut Context<Strip>| {
            // Stop propagation so the click can't be reinterpreted by
            // any ancestor handler — without this the rapidly-following
            // mouseDown after a drag can occasionally race the system's
            // background-drag plumbing and the click goes nowhere.
            ctx.stop_propagation();
            strip.dock_to_default();
            ctx.notify();
        },
    );
    // Outer wrapper is a larger transparent hit target around the
    // visible chip, so the user doesn't have to land precisely on
    // the 18px circle right after releasing a drag.
    div()
        .id("dock-back-hit")
        .p(px(DOCK_HIT_PADDING))
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, dock)
        .child(
            div()
                .w(px(DOCK_BUTTON_SIZE))
                .h(px(DOCK_BUTTON_SIZE))
                .rounded_full()
                .bg(rgb(DOCK_BUTTON_BG))
                .text_color(rgb(DOCK_BUTTON_FG))
                .text_size(px(11.0))
                .flex()
                .items_center()
                .justify_center()
                .hover(|s| s.bg(rgb(DOCK_BUTTON_BG_HOVER)))
                // U+21E5 ⇥ — "rightwards arrow to bar". Reads as
                // "snap to the right edge" which is what docking
                // does.
                .child("⇥"),
        )
        .into_any_element()
}

/// Drag-handle dot grid: 2 rows of 3 dots, the standard macOS "grab"
/// indicator. Only rendered when the corresponding hover band is
/// expanded; the band itself is what listens for the cursor.
const HANDLE_HEIGHT: f32 = 14.0;
const HANDLE_DOT_SIZE: f32 = 2.0;
const HANDLE_DOT_GAP: f32 = 2.5;
const HANDLE_DOT: u32 = 0xa1a1aa;

/// Top/bottom edge band. Plain, non-interactive div: at rest it's
/// just `STRIP_PADDING` of empty space (matching the pre-handle
/// padding); on hover it grows by `ext_current` and renders the dot
/// grid once there's room. Hover detection happens at the window
/// level (see the `mouse_watcher` canvas in `render`); the band
/// itself must stay non-interactive so clicks fall through to
/// `setMovableByWindowBackground` and the user can drag the strip
/// from this surface.
fn hover_zone(ext_current: f32) -> gpui::AnyElement {
    let mut zone = div()
        .w_full()
        .h(px(STRIP_PADDING + ext_current))
        .flex()
        .flex_col()
        .items_center()
        .justify_center();
    // Suppress the dots until the band has expanded enough to host
    // them comfortably — otherwise they'd flash inside the resting
    // `STRIP_PADDING` slice during the first frame of the ramp.
    if ext_current > HANDLE_DOT_SIZE * 2.0 {
        zone = zone.child(handle_dots());
    }
    zone.into_any_element()
}

fn handle_dots() -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(HANDLE_DOT_GAP))
        .child(handle_dot_row())
        .child(handle_dot_row())
        .into_any_element()
}

fn handle_dot_row() -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .gap(px(HANDLE_DOT_GAP))
        .child(handle_dot())
        .child(handle_dot())
        .child(handle_dot())
        .into_any_element()
}

fn handle_dot() -> gpui::AnyElement {
    div()
        .w(px(HANDLE_DOT_SIZE))
        .h(px(HANDLE_DOT_SIZE))
        .rounded_full()
        .bg(rgb(HANDLE_DOT))
        .into_any_element()
}

/// Section break between two tiles whose `AgentState` differs.
/// A short centred rule that reads as a chapter divider rather than
/// just slightly more air.
fn group_divider() -> gpui::AnyElement {
    div()
        .flex()
        .h(px(GROUP_DIVIDER_HEIGHT))
        .w_full()
        .items_center()
        .justify_center()
        .child(
            div()
                .h(px(GROUP_DIVIDER_LINE_H))
                .w(px(GROUP_DIVIDER_LINE_W))
                .rounded_full()
                .bg(rgb(GROUP_DIVIDER_LINE)),
        )
        .into_any_element()
}

fn tile_menu_items(
    pid: ProcessId,
    tool: Tool,
    tty: Option<std::path::PathBuf>,
    cwd: Option<std::path::PathBuf>,
    session_id: uuid::Uuid,
    transcript: std::path::PathBuf,
) -> Vec<context_menu::MenuItem> {
    use context_menu::{MenuItem, copy_action, reveal_action};
    let cwd_display = cwd
        .as_deref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let focus_cwd = cwd.clone();
    vec![
        MenuItem::action("Focus terminal", move |app| {
            focus_agent_with_feedback(app, pid, tool, tty.clone(), focus_cwd.clone(), None);
        }),
        reveal_action("Reveal transcript in Finder", transcript),
        MenuItem::separator(),
        copy_action("Copy session ID", session_id.to_string()),
        copy_action("Copy working directory", cwd_display),
        copy_action("Copy PID", pid.to_string()),
    ]
}

fn focus_agent_with_feedback(
    app: &mut App,
    pid: ProcessId,
    tool: Tool,
    tty: Option<std::path::PathBuf>,
    cwd: Option<std::path::PathBuf>,
    anchor: Option<(gpui::Point<Pixels>, Pixels)>,
) {
    let request_id = crate::focus::next_request_id();
    app.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { crate::focus::focus_for_request(request_id, pid, tool, tty, cwd) })
            .await;
        let _ = cx.update(move |app| match result {
            Ok(()) => tooltip::close_focus_error(app),
            Err(error) => {
                let message = friendly_focus_error(&error);
                if let Some((strip_origin, anchor_y)) = anchor {
                    tooltip::open_focus_error(app, message, strip_origin, anchor_y);
                } else {
                    tooltip::open_focus_error_default(app, message);
                }
            }
        });
    })
    .detach();
}

fn friendly_focus_error(error: &str) -> String {
    if let Some(rest) = error.split("ambiguous target: ").nth(1) {
        let count = rest.split_whitespace().next().unwrap_or("multiple");
        return format!("{count} terminal tabs match this session. Corral didn’t guess.");
    }
    if error.contains("permission denied") {
        return "macOS denied terminal automation permission.".into();
    }
    if error.contains("not found") {
        return "Couldn’t find the terminal tab for this session.".into();
    }
    "The terminal app did not accept the focus request.".into()
}

impl Render for Strip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Each frame, snap the NSWindow's *frame* (origin + size) to
        // match the animated height while keeping the strip vertically
        // centred on its display. GPUI's `Window::resize` only changes
        // the size and anchors the window's top-left, which would
        // otherwise pull the strip downward as tiles arrive. Reaching
        // into NSWindow's `setFrame:display:animate:` lets us move and
        // resize in one shot.
        //
        // The actual `setFrame:` call has to run *after* render returns,
        // not inside it: macOS dispatches a synchronous resize back into
        // GPUI's frame machinery, which would re-borrow the strip
        // entity that this very `render` already holds mutably and
        // panic with "RefCell already borrowed".
        let h_base = self.height.current();
        // Quantize the hover extensions to integer pixels so that
        // `target_y = anchor - top_ext` and the zone's own height
        // (`STRIP_PADDING + top_ext`) always round in the same
        // direction. GPUI's renderer rounds each div's position to
        // the pixel grid independently; without matching quantization
        // the tile region's screen-y can shift by 1px mid-ramp as the
        // two terms disagree.
        let top_ext = self.top_ext.current().round();
        let bot_ext = self.bot_ext.current().round();
        let h_total = h_base + top_ext + bot_ext;
        let current_bounds = window.bounds();
        let cur_h: f32 = current_bounds.size.height.into();
        let cur_x: f32 = current_bounds.origin.x.into();
        let cur_y: f32 = current_bounds.origin.y.into();

        // While *any* of the three animations is in flight we're
        // chasing the OS window ourselves; defer drag detection and
        // `base_y` refresh until everything settles.
        let all_settled = self.height.is_done() && self.top_ext.is_done() && self.bot_ext.is_done();

        // Free-floating mode: keep `base_y` synced with the OS
        // window's actual position so user-driven drags update our
        // anchor. `cur_y` includes the top extension, so add it back
        // to recover the would-be-base top edge.
        if let WindowPlacement::Floating { base_y, .. } = &mut self.placement
            && all_settled
        {
            *base_y = cur_y + top_ext;
        }

        // Detect drag: if the window's actual origin diverged from
        // what we last set, the user moved it. Flip to free-floating
        // and resize the strip so it makes room for the dock-back
        // button on the next render.
        //
        // While docked, `target_x` is always `self.origin_x`, so any
        // x-drift is unambiguously user motion — gate-free. y, by
        // contrast, legitimately lags during height ramps (target is
        // set before the deferred `setFrame:` applies), so we keep the
        // `all_settled` gate on the y axis. Without this split, hovering
        // a band kicks off a 180ms ext-ramp, drag detection is
        // suppressed for that window, and `set_strip_frame` keeps
        // yanking the window back to `origin_x` while AppKit's
        // background-drag is moving it under the cursor.
        if self.placement.is_docked() {
            let drift_x = (cur_x - self.expected_origin.0).abs();
            let drift_y = (cur_y - self.expected_origin.1).abs();
            if drift_x > POSITION_SLOP_PX || (drift_y > POSITION_SLOP_PX && all_settled) {
                // Seed the free-floating anchor from the dragged
                // position; subsequent hover toggles will pivot
                // around this y.
                self.placement = WindowPlacement::Floating {
                    base_y: cur_y + top_ext,
                    last_motion_at: Instant::now(),
                    show_dock_back: false,
                };
                self.retarget_height();
                tracing::info!(
                    cur_x,
                    cur_y,
                    expected = ?self.expected_origin,
                    "strip: detected drag — flipping to free-floating"
                );
            }
        }

        // `target_y_anchor` is where the *base* portion's top edge
        // belongs; `target_y` then sits `top_ext` pixels above so the
        // band can grow upward into the freed space.
        let (target_y_anchor, target_x) = match self.placement {
            WindowPlacement::Docked => ((self.display_height - h_base) * 0.5, self.origin_x),
            WindowPlacement::Floating { base_y, .. } => (base_y, cur_x),
        };
        let target_y = target_y_anchor - top_ext;

        if (cur_h - h_total).abs() > RESIZE_SLOP_PX
            || (cur_x - target_x).abs() > RESIZE_SLOP_PX
            || (cur_y - target_y).abs() > RESIZE_SLOP_PX
        {
            self.expected_origin = (target_x, target_y);
            let display_height = self.display_height;
            // `cx.defer` drains within the current effect cycle, which is
            // still inside an `App.borrow_mut()`. macOS's `setFrame:`
            // synchronously fires `windowDidResize`, and GPUI's
            // `on_resize` callback tries `try_borrow_mut` on the App;
            // that fails ("RefCell already borrowed") and gets swallowed
            // by `.log_err()`. Spawning on the foreground executor
            // pushes the call to the next event-loop tick, after the
            // current borrow has been released — the same pattern
            // GPUI's own `Window::resize` uses internally.
            cx.foreground_executor()
                .spawn(async move {
                    window_geom::set_strip_frame(
                        target_x,
                        target_y,
                        STRIP_WIDTH,
                        h_total,
                        display_height,
                    );
                })
                .detach();
        }

        // Zero-sized Canvas whose only job is to give us a `paint`
        // callback. From there we register two window-level mouse
        // listeners:
        //
        // 1. `MouseMoveEvent` drives the top/bottom hover-band state.
        //    We can't use `on_hover` on the bands themselves: that
        //    requires `.id()`, which marks the band as interactive
        //    and lets GPUI swallow mouse-down before macOS's
        //    `setMovableByWindowBackground` can start a window drag.
        //    The bands are the natural place to grab the strip from,
        //    so they have to stay non-interactive.
        //
        // 2. `MouseExitEvent` clears the tooltip *and* both band
        //    hovers, since a fast cursor exit might not produce a
        //    final intra-strip `MouseMove`. Each render re-registers
        //    both handlers; GPUI clears them between frames.
        let entity = cx.entity();
        let mouse_watcher = canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let entity_move = entity.clone();
                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, app: &mut App| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let win_size = window.bounds().size;
                    let pos_x: f32 = ev.position.x.into();
                    let pos_y: f32 = ev.position.y.into();
                    let win_w: f32 = win_size.width.into();
                    let win_h: f32 = win_size.height.into();
                    entity_move.update(app, |strip, ctx| {
                        let top_ext = strip.top_ext.current();
                        let bot_ext = strip.bot_ext.current();
                        let in_x = pos_x >= 0.0 && pos_x <= win_w;
                        // Inclusive at the outer edge, exclusive
                        // at the inner edge — keeps the two bands
                        // disjoint and gives `y == STRIP_PADDING`
                        // to the tile region.
                        let want_top = in_x && pos_y >= 0.0 && pos_y < STRIP_PADDING + top_ext;
                        let want_bot =
                            in_x && pos_y > win_h - STRIP_PADDING - bot_ext && pos_y <= win_h;
                        let mut changed = false;
                        if strip.hover_top != want_top {
                            strip.hover_top = want_top;
                            strip.retarget_top_ext();
                            changed = true;
                        }
                        if strip.hover_bottom != want_bot {
                            strip.hover_bottom = want_bot;
                            strip.retarget_bot_ext();
                            changed = true;
                        }
                        if changed {
                            ctx.notify();
                        }
                    });
                });
                let entity_exit = entity.clone();
                window.on_mouse_event(move |_: &MouseExitEvent, phase, _window, app: &mut App| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    tooltip::close(app);
                    entity_exit.update(app, |strip, ctx| {
                        let mut changed = false;
                        if strip.hover_top {
                            strip.hover_top = false;
                            strip.retarget_top_ext();
                            changed = true;
                        }
                        if strip.hover_bottom {
                            strip.hover_bottom = false;
                            strip.retarget_bot_ext();
                            changed = true;
                        }
                        if changed {
                            ctx.notify();
                        }
                    });
                });
            },
        )
        .absolute()
        .w(px(0.0))
        .h(px(0.0));

        let mut col = div()
            .flex()
            .flex_col()
            .size_full()
            .items_center()
            // Horizontal padding is just enough to centre a `TILE_SIZE`
            // tile inside a `STRIP_WIDTH` column. Vertical padding is
            // contributed by the top/bottom `hover_zone` children
            // below: each is `STRIP_PADDING` tall at rest (matching the
            // pre-handle baseline) and grows by `HANDLE_HEIGHT` on
            // hover.
            .px(px((STRIP_WIDTH - TILE_SIZE) * 0.5))
            .rounded(px(STRIP_RADIUS))
            .bg(rgb(STRIP_BG))
            .shadow_lg()
            .on_mouse_down(
                MouseButton::Right,
                |ev: &MouseDownEvent, window: &mut Window, app: &mut App| {
                    let strip_origin = window.bounds().origin;
                    let anchor = context_menu::anchor_left_of_strip(strip_origin, ev.position);
                    context_menu::open(app, anchor, vec![context_menu::quit_action()]);
                },
            )
            .child(mouse_watcher)
            // Top hover band — at rest it's just `STRIP_PADDING` of
            // empty space (same as the original `py(STRIP_PADDING)`).
            // On hover the band grows by `top_ext`, anchored on its
            // bottom edge so the tile region underneath doesn't move.
            .child(hover_zone(top_ext));

        if self.placement.show_dock_back() {
            col = col.child(dock_back_button(cx)).child(tile_spacer());
        }

        let mut prev_state: Option<AgentState> = None;
        for row in self.sorted_rows() {
            if let Some(prev) = prev_state {
                col = col.child(if prev == row.agent.state {
                    tile_spacer()
                } else {
                    group_divider()
                });
            }
            prev_state = Some(row.agent.state);
            let pid = row.agent.pid;
            let tool = row.agent.tool;
            let tty = row.agent.tty.clone();
            let cwd = row.agent.cwd.clone();
            let transcript = row.agent.transcript_path.clone();
            let session_id = row.agent.session_id;
            let tip = crate::tooltip::TooltipData {
                pid: row.agent.pid,
                tool: row.agent.tool,
                cwd: row.agent.cwd.clone(),
                session_id: row.agent.session_id,
                state: row.agent.state,
                last_lifecycle_at: row.agent.last_lifecycle_at,
                subagent_count: row.agent.subagent_pids.len(),
                host_app: row.agent.host_app.clone(),
                model: row.agent.model.clone(),
                git_branch: row.agent.git_branch.clone(),
                session_title: row.agent.session_title.clone(),
                current_action: row.agent.current_action.clone(),
                last_action: row.agent.last_action.clone(),
                context_tokens: row.agent.context_tokens,
                context_max: row.agent.context_max,
            };
            let tip_for_menu = tip.clone();
            col = col.child(render_tile(
                row.agent.pid,
                row.agent.state,
                row.agent.tool,
                row.agent.subagent_pids.len(),
                row.agent.context_tokens,
                row.agent.context_max,
                Some(tip),
                {
                    let tty = tty.clone();
                    let cwd = cwd.clone();
                    move |ev, window, app| {
                        focus_agent_with_feedback(
                            app,
                            pid,
                            tool,
                            tty.clone(),
                            cwd.clone(),
                            Some((window.bounds().origin, ev.position.y)),
                        );
                    }
                },
                {
                    let cwd = cwd.clone();
                    let tty = tty.clone();
                    let transcript = transcript.clone();
                    move |ev: &MouseDownEvent, window: &mut Window, app: &mut App| {
                        // Tooltip and menu shouldn't be visible together —
                        // the menu carries the same info now.
                        tooltip::close(app);
                        let strip_origin = window.bounds().origin;
                        let anchor = context_menu::anchor_left_of_strip(strip_origin, ev.position);
                        let items = tile_menu_items(
                            pid,
                            tool,
                            tty.clone(),
                            cwd.clone(),
                            session_id,
                            transcript.clone(),
                        );
                        context_menu::open_with_info(app, anchor, tip_for_menu.clone(), items);
                    }
                },
            ));
        }
        if self.rows.is_empty() {
            col = col.child(render_tile(
                ProcessId(0),
                AgentState::Idle,
                Tool::Claude,
                0,
                None,
                None,
                None,
                |_ev, _w, _a| {},
                |_ev, _w, _a| {},
            ));
        }
        // Bottom hover band mirrors the top one, anchored on its top
        // edge so it grows downward.
        col.child(hover_zone(bot_ext))
    }
}

pub fn open(cx: &mut App, events: crossbeam_channel::Receiver<RegistryEvent>) {
    let Some(display) = cx.primary_display() else {
        return;
    };
    let display_bounds = display.bounds();

    let initial_h = strip_height_for(1);
    let window_size = size(px(STRIP_WIDTH), px(initial_h));
    let origin_x =
        display_bounds.origin.x + display_bounds.size.width - px(STRIP_WIDTH) - px(STRIP_RIGHT_GAP);
    // Centred for the initial 1-tile size; subsequent height changes
    // re-centre via the `setFrame:` call in `render`.
    let origin_y = display_bounds.origin.y + (display_bounds.size.height - px(initial_h)) * 0.5;
    let display_h: f32 = display_bounds.size.height.into();
    let origin_x_f: f32 = origin_x.into();

    tracing::info!(
        screen_h = display_h,
        initial_h,
        origin_y = %origin_y,
        "strip: initial placement"
    );
    let bounds: Bounds<Pixels> = Bounds {
        origin: point(origin_x, origin_y),
        size: window_size,
    };

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        display_id: Some(display.id()),
        titlebar: None,
        window_background: WindowBackgroundAppearance::Transparent,
        focus: false,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: true,
        app_id: None,
        window_min_size: None,
        window_decorations: None,
        tabbing_identifier: None,
        ..Default::default()
    };

    let handle = cx
        .open_window(options, |_, cx| {
            cx.new(|_| Strip::new(display_h, origin_x_f))
        })
        .unwrap_or_else(|e| panic!("failed to open strip window: {e}"));

    // Enable click-anywhere-on-background drag. We must call after
    // the NSWindow exists; spawning on the foreground executor pushes
    // the call to the next runloop tick where `find_strip_window`
    // will succeed.
    cx.foreground_executor()
        .spawn(async move {
            if !window_geom::set_strip_movable(true) {
                tracing::warn!("strip: failed to enable click-drag — NSWindow not found");
            }
        })
        .detach();

    spawn_event_pump(cx, handle, events);
}

/// Poll the registry's outbound channel on the foreground executor and apply
/// each event to the strip entity. Cadence adapts to load: ~60fps while any
/// animation is in flight, a tile is pulsing, or a drag is in progress;
/// drops to 4Hz when the strip is fully quiescent.
fn spawn_event_pump(
    cx: &mut App,
    handle: WindowHandle<Strip>,
    events: crossbeam_channel::Receiver<RegistryEvent>,
) {
    /// 60Hz cadence reserved for animations and live drag detection
    /// (sub-frame jitter is visible at this rate).
    const ACTIVE_INTERVAL: Duration = Duration::from_millis(16);
    /// 30Hz cadence for the active-tile pulse. The dot pulses at
    /// ~0.6Hz with smooth sinusoidal interpolation — 30 samples per
    /// second is more than enough to read as continuous.
    const PULSE_INTERVAL: Duration = Duration::from_millis(33);
    /// 4Hz cadence when fully quiescent. Still polls the registry
    /// channel and window bounds (for drag detection) often enough that
    /// a user action feels responsive, but cuts idle wakeups ~15x.
    const IDLE_INTERVAL: Duration = Duration::from_millis(250);

    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut interval = ACTIVE_INTERVAL;
        loop {
            cx.background_executor().timer(interval).await;
            let mut batch: Vec<RegistryEvent> = Vec::new();
            let mut producer_gone = false;
            loop {
                match events.try_recv() {
                    Ok(ev) => batch.push(ev),
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        producer_gone = true;
                        break;
                    }
                }
            }
            // Notify whenever a registry event landed, a tile is pulsing, or
            // the height animation is mid-flight — all three read
            // `Instant::now()` in `render()` and need an explicit `notify()`.
            let mut active = false;
            let mut pulse_only = false;
            let update_ok = cx.update(|app| {
                handle.update(app, |strip, window, ctx| {
                    let had_events = !batch.is_empty();
                    for ev in batch {
                        strip.apply(ev);
                    }
                    // State changes can flip the number of group
                    // dividers even when the tile count is steady, so
                    // we can't rely on `count_before != count_after`
                    // here. `retarget_height` short-circuits if the
                    // computed value matches `height.to`, so calling
                    // it unconditionally is cheap.
                    if had_events {
                        strip.retarget_height();
                    }

                    // Watch for user-driven window motion. The drift
                    // detection in `render` may not fire between
                    // frames if nothing else is animating, so we
                    // sample from here at 16ms cadence to keep the
                    // dock-back-reveal timer honest.
                    let bounds = window.bounds();
                    let observed = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
                    let drift_x = (observed.0 - strip.expected_origin.0).abs();
                    let drift_y = (observed.1 - strip.expected_origin.1).abs();
                    // Same axis-split as in `render`: x-drift is
                    // unambiguous user motion (target_x = origin_x
                    // while docked), but y can legitimately diverge
                    // mid-ramp because `expected_origin` is repointed
                    // ahead of the deferred `setFrame:`.
                    let all_settled = strip.height.is_done()
                        && strip.top_ext.is_done()
                        && strip.bot_ext.is_done();
                    let dragged =
                        drift_x > POSITION_SLOP_PX || (drift_y > POSITION_SLOP_PX && all_settled);
                    if dragged && strip.placement.is_docked() {
                        // `base_y` is reseeded by `render` on the
                        // next frame from the actual NSWindow
                        // position; seed conservatively from the
                        // observed origin here so the floating
                        // anchor is valid even before that frame.
                        strip.placement = WindowPlacement::Floating {
                            base_y: observed.1,
                            last_motion_at: Instant::now(),
                            show_dock_back: false,
                        };
                        tracing::info!(?observed, "strip: user dragged — free-floating");
                    }
                    let moved = observed != strip.prev_observed_origin;
                    if moved {
                        strip.prev_observed_origin = observed;
                        if let WindowPlacement::Floating { last_motion_at, .. } =
                            &mut strip.placement
                        {
                            *last_motion_at = Instant::now();
                        }
                    }

                    // Reveal the dock-back button once motion has
                    // been still for `DRAG_SETTLE_MS`; this avoids
                    // the button shaking into view mid-drag.
                    if let WindowPlacement::Floating {
                        last_motion_at,
                        show_dock_back,
                        ..
                    } = &mut strip.placement
                    {
                        let want_show = last_motion_at.elapsed().as_millis() >= DRAG_SETTLE_MS;
                        if want_show != *show_dock_back {
                            *show_dock_back = want_show;
                            strip.retarget_height();
                            ctx.notify();
                        }
                    }

                    let animating = !strip.height.is_done()
                        || !strip.top_ext.is_done()
                        || !strip.bot_ext.is_done();
                    let dock_back_pending =
                        !strip.placement.is_docked() && !strip.placement.show_dock_back();
                    if had_events || strip.has_active() || animating || moved {
                        ctx.notify();
                    }
                    let pulse = strip.has_active();
                    active = had_events || animating || moved || dock_back_pending;
                    pulse_only = !active && pulse;
                })
            });
            // Producer hung up: registry dropped its outbound channel
            // (runtime shutdown). Cooperate by ending the pump so the
            // detached future doesn't dangle into app teardown.
            if producer_gone {
                return;
            }
            // Either the app or this strip window is gone. Same outcome:
            // stop polling so the detached future cannot spin during teardown.
            if !matches!(update_ok, Ok(Ok(()))) {
                return;
            }
            interval = if active {
                ACTIVE_INTERVAL
            } else if pulse_only {
                PULSE_INTERVAL
            } else {
                IDLE_INTERVAL
            };
        }
    })
    .detach();
}
