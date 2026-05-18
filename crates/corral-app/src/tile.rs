use crate::theme;
use crate::tooltip::{self, TooltipData};
use corral_core::agent::Tool;
use corral_core::proc::ProcessId;
use corral_core::status::AgentState;
use gpui::{
    AnyElement, App, Bounds, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Path, PathBuilder, Pixels, Point, StatefulInteractiveElement, Styled, Window,
    canvas, div, point, prelude::FluentBuilder, px, rgb,
};
use std::sync::OnceLock;
use std::time::Instant;

const PULSE_PERIOD_SECS: f32 = 1.6;
const PULSE_MIN_OPACITY: f32 = 0.7;
const PULSE_MAX_OPACITY: f32 = 1.0;
const IDLE_OPACITY: f32 = 0.6;
const CLOSED_OPACITY: f32 = 0.3;
const NEEDS_INPUT_OPACITY: f32 = 1.0;
const AWAITING_USER_OPACITY: f32 = 1.0;

fn app_start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn dot_opacity(state: AgentState) -> f32 {
    match state {
        AgentState::Active => {
            // Smooth sinusoidal pulse so the eye reads it as "alive". Half-cycle
            // alignment chosen so the dot starts mid-pulse on launch.
            let t = app_start().elapsed().as_secs_f32();
            let phase = (t / PULSE_PERIOD_SECS * std::f32::consts::TAU).sin() * 0.5 + 0.5;
            PULSE_MIN_OPACITY + phase * (PULSE_MAX_OPACITY - PULSE_MIN_OPACITY)
        }
        AgentState::AwaitingUser => AWAITING_USER_OPACITY,
        AgentState::NeedsInput => NEEDS_INPUT_OPACITY,
        AgentState::Idle => IDLE_OPACITY,
        AgentState::Closed => CLOSED_OPACITY,
    }
}

pub const TILE_SIZE: f32 = 28.0;
const TILE_RADIUS: f32 = 6.0;
const DOT_SIZE: f32 = 10.0;

/// How far the context-usage ring sits *inside* the tile's edge. The
/// canvas is the same size as the tile and painted directly on top of
/// the bg; the path is inset by this many pixels so the stroke reads
/// as an inner border rather than overlapping the tile's silhouette.
const RING_INSET: f32 = 2.0;
const RING_STROKE: f32 = 1.0;
/// Fallback context window when the transcript provided neither a
/// model id (Claude pre-`message.model`) nor an explicit window
/// (Codex pre-`token_count.info`). The parsers normally resolve a
/// real value via `claude_context_window(model)` or
/// `info.model_context_window`; this only kicks in for malformed or
/// truncated transcripts.
const DEFAULT_CONTEXT_MAX: u32 = 200_000;

// blue-400 disk + near-black text for the subagent count badge.
const BADGE_BG: u32 = 0x60a5fa;
const BADGE_FG: u32 = 0x0b1220;

// Tile composition is fundamentally a "lots of orthogonal inputs"
// problem — every additional grouping (a `TileVisuals` struct, etc.)
// just adds an indirection without shrinking the API surface that the
// strip already has to pass through. Allow the lint here so the call
// sites stay flat and readable.
#[allow(clippy::too_many_arguments)]
pub fn render_tile(
    pid: ProcessId,
    state: AgentState,
    tool: Tool,
    subagent_count: usize,
    context_tokens: Option<u32>,
    context_max: Option<u32>,
    tooltip: Option<TooltipData>,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    on_right_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let dot = div()
        .w(px(DOT_SIZE))
        .h(px(DOT_SIZE))
        .rounded_full()
        .bg(rgb(theme::state_color(state)))
        .opacity(dot_opacity(state));

    let (bg, bg_hover) = theme::tile_bg(tool);
    let ring_fraction = context_fraction(context_tokens, context_max);
    // Ring colour mirrors the dot / status colour so all status cues
    // on the tile (dot, ring, tooltip header rule) read as one
    // consistent signal.
    let ring_color = theme::state_color(state);
    let ring_canvas = ring_fraction.map(|fraction| {
        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| paint_context_ring(window, bounds, fraction, ring_color),
        )
        // Overlay the tile exactly; the path itself is inset so the
        // stroke reads as a thin inner border on top of the bg.
        .absolute()
        .top(px(0.0))
        .left(px(0.0))
        .w(px(TILE_SIZE))
        .h(px(TILE_SIZE))
    });

    // Element id is required for the GPUI focus/scroll machinery once we
    // attach interactive handlers. Pid is unique per agent within a run.
    let mut tile = div()
        .id(("corral-tile", pid.0 as usize))
        .relative()
        .w(px(TILE_SIZE))
        .h(px(TILE_SIZE))
        .rounded(px(TILE_RADIUS))
        .bg(rgb(bg))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(bg_hover)))
        .on_mouse_down(MouseButton::Left, on_click)
        .on_mouse_down(MouseButton::Right, move |ev, window, app| {
            // Prevent the strip's right-click (context menu) from also
            // firing when the user right-clicks an individual tile.
            app.stop_propagation();
            on_right_click(ev, window, app);
        })
        .when_some(ring_canvas, |this, ring| this.child(ring))
        .child(dot);

    if let Some(data) = tooltip {
        let pid_for_log = pid;
        tile = tile.on_hover(move |entering, window, app| {
            if *entering {
                let strip_origin = window.bounds().origin;
                let anchor_y = window.mouse_position().y;
                tracing::debug!(
                    pid = %pid_for_log,
                    ?strip_origin,
                    anchor_y = %anchor_y,
                    "tile: hover-in"
                );
                tooltip::open(app, data.clone(), strip_origin, anchor_y);
            } else {
                tracing::debug!(pid = %pid_for_log, "tile: hover-out");
                tooltip::close(app);
            }
        });
    }

    if subagent_count > 0 {
        let label = if subagent_count > 9 {
            "9+".to_string()
        } else {
            subagent_count.to_string()
        };
        let badge = div()
            .absolute()
            .bottom(px(1.0))
            .right(px(2.0))
            .min_w(px(11.0))
            .h(px(10.0))
            .px(px(2.0))
            .rounded(px(5.0))
            .bg(rgb(BADGE_BG))
            .text_color(rgb(BADGE_FG))
            .text_size(px(7.0))
            .flex()
            .items_center()
            .justify_center()
            .child(label);
        tile = tile.child(badge);
    }

    tile.into_any_element()
}

/// Translate the latest observed token counts into a `[0.0, 1.0]`
/// fraction of the model's context window. Returns `None` when the
/// transcript hasn't surfaced any usage yet — callers omit the ring
/// entirely in that case rather than drawing an empty 0% track that
/// would suggest a *known* zero.
fn context_fraction(tokens: Option<u32>, max: Option<u32>) -> Option<f32> {
    let tokens = tokens?;
    let max = max.unwrap_or(DEFAULT_CONTEXT_MAX).max(1);
    Some((tokens as f32 / max as f32).clamp(0.0, 1.0))
}

/// Paint the context-usage ring as an *inner border* on the tile.
/// The canvas is sized identically to the tile and overlays its bg;
/// the path is inset by `RING_INSET` (plus half the stroke width so
/// the visible stroke lives entirely inside the tile silhouette).
/// Drawing a faint full-outline track first gives the eye a "how much
/// room is left" reference; the active portion then sits on top in
/// the brand colour.
fn paint_context_ring(window: &mut Window, bounds: Bounds<Pixels>, fraction: f32, color: u32) {
    let f = fraction.clamp(0.0, 1.0);
    let bounds_w: f32 = bounds.size.width.into();
    let bounds_h: f32 = bounds.size.height.into();
    // Total path-to-tile-edge gap: the requested inset, plus half the
    // stroke so the outer-facing pixel of the stroke lines up with
    // `RING_INSET` rather than spilling past it.
    let inset = RING_INSET + RING_STROKE * 0.5;
    let rect_w = bounds_w - 2.0 * inset;
    let rect_h = bounds_h - 2.0 * inset;
    // Match the tile's own corner curvature: original radius minus the
    // total inward offset, clamped non-negative.
    let radius = (TILE_RADIUS - inset).max(0.0);
    let origin: Point<f32> = point(
        f32::from(bounds.origin.x) + inset,
        f32::from(bounds.origin.y) + inset,
    );

    if f <= 0.0 {
        return;
    }

    if let Some(path) = build_rounded_rect_path(origin, rect_w, rect_h, radius, f) {
        window.paint_path(path, rgb(color));
    }
}

/// Build a stroke path tracing the rounded rectangle's outline,
/// starting at the top centre and going clockwise for `fraction` of
/// the perimeter. Returns `None` when the rectangle is degenerate or
/// the fraction reduces to nothing.
/// Cached segments+perimeter for the tile-sized rounded rect.
/// (w, h, r) are derived from compile-time constants
/// (`TILE_SIZE`, `RING_INSET`, `TILE_RADIUS`) so the table is invariant
/// across every tile, every frame.
fn tile_ring_segments() -> &'static ([Segment; 9], f32) {
    static SEGS: OnceLock<([Segment; 9], f32)> = OnceLock::new();
    SEGS.get_or_init(|| {
        let inset = RING_INSET + RING_STROKE * 0.5;
        let w = TILE_SIZE - 2.0 * inset;
        let h = w;
        let r = (TILE_RADIUS - inset).max(0.0).clamp(0.0, w.min(h) * 0.5);
        let segs = rounded_rect_segments(w, h, r);
        let perimeter: f32 = segs.iter().map(|s| s.length).sum();
        (segs, perimeter)
    })
}

fn build_rounded_rect_path(
    origin: Point<f32>,
    w: f32,
    h: f32,
    r: f32,
    fraction: f32,
) -> Option<Path<Pixels>> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = r.clamp(0.0, w.min(h) * 0.5);
    // Fast path: the common caller is the tile context-ring, whose
    // (w, h, r) match the cached precompute. Reuse the segment table
    // and perimeter rather than rebuilding 9 segments per frame.
    let cached = tile_ring_segments();
    let cached_w = TILE_SIZE - 2.0 * (RING_INSET + RING_STROKE * 0.5);
    let cached_r = (TILE_RADIUS - (RING_INSET + RING_STROKE * 0.5))
        .max(0.0)
        .clamp(0.0, cached_w * 0.5);
    let use_cached = (w - cached_w).abs() < f32::EPSILON
        && (h - cached_w).abs() < f32::EPSILON
        && (r - cached_r).abs() < f32::EPSILON;
    let (segs_owned, perimeter);
    let (segs, perimeter): (&[Segment; 9], f32) = if use_cached {
        (&cached.0, cached.1)
    } else {
        segs_owned = rounded_rect_segments(w, h, r);
        perimeter = segs_owned.iter().map(|s| s.length).sum();
        (&segs_owned, perimeter)
    };
    if perimeter <= 0.0 {
        return None;
    }
    let target = perimeter * fraction.clamp(0.0, 1.0);
    if target <= 0.0 {
        return None;
    }

    let mut builder = PathBuilder::stroke(px(RING_STROKE));
    let start = abs_point(origin, segs[0].start);
    builder.move_to(start);

    let mut acc = 0.0_f32;
    for seg in segs {
        let next = acc + seg.length;
        if next <= target + f32::EPSILON {
            // Full segment.
            let end = abs_point(origin, seg.end);
            match seg.kind {
                SegKind::Line => builder.line_to(end),
                SegKind::Arc { .. } => {
                    builder.arc_to(point(px(r), px(r)), px(0.0), false, true, end)
                }
            }
            acc = next;
            if (target - acc).abs() < f32::EPSILON {
                break;
            }
        } else {
            // Stop mid-segment: emit the partial portion and bail.
            let frac_in = ((target - acc) / seg.length).clamp(0.0, 1.0);
            match seg.kind {
                SegKind::Line => {
                    let end = lerp(seg.start, seg.end, frac_in);
                    builder.line_to(abs_point(origin, end));
                }
                SegKind::Arc {
                    center,
                    start_angle,
                } => {
                    let sweep = (std::f32::consts::PI / 2.0) * frac_in;
                    let end_angle = start_angle + sweep;
                    let end = (
                        center.0 + r * end_angle.cos(),
                        center.1 + r * end_angle.sin(),
                    );
                    builder.arc_to(
                        point(px(r), px(r)),
                        px(0.0),
                        false,
                        true,
                        abs_point(origin, end),
                    );
                }
            }
            break;
        }
    }
    builder.build().ok()
}

/// One unit of the rounded-rect outline.
struct Segment {
    kind: SegKind,
    /// Start point in local coordinates (origin at the top-left of the
    /// rounded rect's bounding box).
    start: (f32, f32),
    end: (f32, f32),
    length: f32,
}

enum SegKind {
    Line,
    /// Quarter arc. `start_angle` is in radians under the canvas's
    /// y-down convention (`-π/2` is 12 o'clock, `0` is 3 o'clock).
    Arc {
        center: (f32, f32),
        start_angle: f32,
    },
}

/// Emit the rounded rectangle's outline as nine segments, starting at
/// the top-centre and going clockwise (so a partially-drawn path
/// always begins at the 12 o'clock mark, matching the conventional
/// progress-ring metaphor).
fn rounded_rect_segments(w: f32, h: f32, r: f32) -> [Segment; 9] {
    use std::f32::consts::PI;
    let arc_len = PI * r * 0.5;
    let half_w = w * 0.5;
    [
        // 1. Top-right half-edge.
        Segment {
            kind: SegKind::Line,
            start: (half_w, 0.0),
            end: (w - r, 0.0),
            length: (half_w - r).max(0.0),
        },
        // 2. Top-right corner arc.
        Segment {
            kind: SegKind::Arc {
                center: (w - r, r),
                start_angle: -PI / 2.0,
            },
            start: (w - r, 0.0),
            end: (w, r),
            length: arc_len,
        },
        // 3. Right edge.
        Segment {
            kind: SegKind::Line,
            start: (w, r),
            end: (w, h - r),
            length: (h - 2.0 * r).max(0.0),
        },
        // 4. Bottom-right corner arc.
        Segment {
            kind: SegKind::Arc {
                center: (w - r, h - r),
                start_angle: 0.0,
            },
            start: (w, h - r),
            end: (w - r, h),
            length: arc_len,
        },
        // 5. Bottom edge.
        Segment {
            kind: SegKind::Line,
            start: (w - r, h),
            end: (r, h),
            length: (w - 2.0 * r).max(0.0),
        },
        // 6. Bottom-left corner arc.
        Segment {
            kind: SegKind::Arc {
                center: (r, h - r),
                start_angle: PI / 2.0,
            },
            start: (r, h),
            end: (0.0, h - r),
            length: arc_len,
        },
        // 7. Left edge.
        Segment {
            kind: SegKind::Line,
            start: (0.0, h - r),
            end: (0.0, r),
            length: (h - 2.0 * r).max(0.0),
        },
        // 8. Top-left corner arc.
        Segment {
            kind: SegKind::Arc {
                center: (r, r),
                start_angle: PI,
            },
            start: (0.0, r),
            end: (r, 0.0),
            length: arc_len,
        },
        // 9. Top-left half-edge — closes back to the start.
        Segment {
            kind: SegKind::Line,
            start: (r, 0.0),
            end: (half_w, 0.0),
            length: (half_w - r).max(0.0),
        },
    ]
}

fn lerp(a: (f32, f32), b: (f32, f32), t: f32) -> (f32, f32) {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

fn abs_point(origin: Point<f32>, local: (f32, f32)) -> Point<Pixels> {
    point(px(origin.x + local.0), px(origin.y + local.1))
}
