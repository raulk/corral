//! Right-click context menu — generic over its item list.
//!
//! Both the strip shell ("Quit Corral") and individual tiles
//! ("Reveal transcript", "Copy session id", …) open a menu via this
//! module. The menu lives in its own borderless pop-up window
//! positioned to the left of the strip so it can extend past the 36px
//! strip width. macOS's window-deactivation event handles dismissal —
//! click anywhere outside the menu and it closes, matching native
//! NSMenu behaviour.

use crate::tooltip::TooltipData;
use gpui::{
    App, Bounds, ClipboardItem, Context, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Point, Render, SharedString, Styled, Window, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions, div, point, prelude::*, px, rgb, size,
};
use std::process::Command;
use std::rc::Rc;

const MENU_WIDTH_COMPACT: f32 = 220.0;
const MENU_WIDTH_WITH_INFO: f32 = 300.0;
const MENU_ITEM_HEIGHT: f32 = 26.0;
const MENU_SEPARATOR_HEIGHT: f32 = 7.0;
const MENU_PADDING: f32 = 4.0;
const MENU_GAP_FROM_STRIP: f32 = 6.0;
const MENU_RADIUS: f32 = 6.0;
/// Vertical padding around the shared tooltip info block inside the
/// context menu's own frame.
const MENU_INFO_PAD_Y: f32 = 6.0;
/// Inner vertical padding around the divider between the info block
/// and the items list.
const MENU_INFO_GAP: f32 = 6.0;

const MENU_BG: u32 = 0x202024;
const MENU_BG_HOVER: u32 = 0x2d2d33;
const MENU_FG: u32 = 0xe7e7ea;
const MENU_FG_DANGER: u32 = 0xf87171;
const MENU_BORDER: u32 = 0x36363c;
const MENU_SEPARATOR: u32 = 0x2a2a30;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuRole {
    /// Regular action label.
    Normal,
    /// Destructive or otherwise attention-grabbing action.
    Danger,
    /// Inert horizontal rule between groups.
    Separator,
}

#[derive(Clone)]
pub struct MenuItem {
    pub label: SharedString,
    pub role: MenuRole,
    /// Action to invoke on click. Wrapped in `Rc` so menu items can be
    /// cheaply cloned into the render closure. Pass an empty closure
    /// for `Separator` items.
    pub action: Rc<dyn Fn(&mut App)>,
}

impl MenuItem {
    pub fn action(label: impl Into<SharedString>, action: impl Fn(&mut App) + 'static) -> Self {
        Self {
            label: label.into(),
            role: MenuRole::Normal,
            action: Rc::new(action),
        }
    }

    pub fn danger(label: impl Into<SharedString>, action: impl Fn(&mut App) + 'static) -> Self {
        Self {
            label: label.into(),
            role: MenuRole::Danger,
            action: Rc::new(action),
        }
    }

    pub fn separator() -> Self {
        Self {
            label: SharedString::default(),
            role: MenuRole::Separator,
            action: Rc::new(|_| {}),
        }
    }
}

struct ContextMenu {
    items: Vec<MenuItem>,
    /// Optional agent-info card rendered above the items, separated by a
    /// divider. Used by the per-tile menu so the menu carries the same
    /// information the hover tooltip would show.
    info: Option<TooltipData>,
}

impl Render for ContextMenu {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut col = div()
            .flex()
            .flex_col()
            .size_full()
            .p(px(MENU_PADDING))
            .rounded(px(MENU_RADIUS))
            .bg(rgb(MENU_BG))
            .border_1()
            .border_color(rgb(MENU_BORDER))
            .text_size(px(12.0))
            .text_color(rgb(MENU_FG))
            .shadow_lg();

        if let Some(data) = &self.info {
            col = col
                .child(
                    div()
                        .px(px(6.0))
                        .pt(px(4.0))
                        .pb(px(2.0))
                        .child(crate::tooltip::info_block(data)),
                )
                .child(
                    div()
                        .h(px(1.0))
                        .my(px(MENU_INFO_GAP * 0.5))
                        .mx(px(4.0))
                        .bg(rgb(MENU_SEPARATOR)),
                );
        }

        for (idx, item) in self.items.iter().enumerate() {
            col = col.child(render_item(idx, item.clone()));
        }
        col
    }
}

fn render_item(idx: usize, item: MenuItem) -> gpui::AnyElement {
    if item.role == MenuRole::Separator {
        return div()
            .h(px(1.0))
            .my(px(3.0))
            .mx(px(4.0))
            .bg(rgb(MENU_SEPARATOR))
            .into_any_element();
    }
    let (fg, hover_bg) = match item.role {
        MenuRole::Danger => (MENU_FG_DANGER, MENU_BG_HOVER),
        _ => (MENU_FG, MENU_BG_HOVER),
    };
    let action = item.action.clone();
    div()
        .id(("menu-item", idx))
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(MENU_ITEM_HEIGHT))
        .px(px(8.0))
        .rounded(px(MENU_RADIUS - 2.0))
        .text_color(rgb(fg))
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(hover_bg)))
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window: &mut Window, app: &mut App| {
                action(app);
                window.remove_window();
            },
        )
        .child(item.label.clone())
        .into_any_element()
}

/// Anchor describes where on screen to position the menu.
#[derive(Debug, Clone, Copy)]
pub struct Anchor {
    /// The x at which the menu's RIGHT edge should sit. Typically the
    /// strip's left edge minus a small gap so the menu opens leftward
    /// past the strip.
    pub right_x: Pixels,
    /// The y at which the menu's vertical centre should sit (clamped
    /// inside the display by the caller if needed).
    pub center_y: Pixels,
}

pub fn open(cx: &mut App, anchor: Anchor, items: Vec<MenuItem>) {
    open_inner(cx, anchor, None, items);
}

/// Open the menu with an agent-info block above the items. The caller is
/// expected to close any active hover tooltip first; the two pieces of
/// UI shouldn't be visible at the same time.
pub fn open_with_info(cx: &mut App, anchor: Anchor, info: TooltipData, items: Vec<MenuItem>) {
    open_inner(cx, anchor, Some(info), items);
}

fn open_inner(cx: &mut App, anchor: Anchor, info: Option<TooltipData>, items: Vec<MenuItem>) {
    if items.is_empty() && info.is_none() {
        return;
    }

    let has_info = info.is_some();
    let menu_width = if has_info {
        MENU_WIDTH_WITH_INFO
    } else {
        MENU_WIDTH_COMPACT
    };

    // Sum the heights of each row + the optional info card. Separators
    // are short; data items get the full item height.
    let mut content_h: f32 = MENU_PADDING * 2.0;
    if let Some(data) = info.as_ref() {
        content_h +=
            crate::tooltip::info_block_height(data) + MENU_INFO_PAD_Y + 1.0 + MENU_INFO_GAP;
    }
    for item in &items {
        content_h += match item.role {
            MenuRole::Separator => MENU_SEPARATOR_HEIGHT,
            _ => MENU_ITEM_HEIGHT,
        };
    }

    let origin_x = anchor.right_x - px(menu_width);
    let origin_y = anchor.center_y - px(content_h * 0.5);

    // Clamp inside the primary display so the menu never opens
    // partially off-screen for tiles near the top/bottom of the strip.
    let (origin_x, origin_y) = if let Some(display) = cx.primary_display() {
        let db = display.bounds();
        let min_x = db.origin.x;
        let min_y = db.origin.y;
        let max_x = db.origin.x + db.size.width - px(menu_width);
        let max_y = db.origin.y + db.size.height - px(content_h);
        (origin_x.clamp(min_x, max_x), origin_y.clamp(min_y, max_y))
    } else {
        (origin_x, origin_y)
    };

    let bounds = Bounds {
        origin: point(origin_x, origin_y),
        size: size(px(menu_width), px(content_h)),
    };

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        window_background: WindowBackgroundAppearance::Transparent,
        focus: true,
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
        ?bounds,
        item_count = items.len(),
        with_info = has_info,
        "context_menu: opening"
    );

    if let Err(e) = cx.open_window(options, |window, app| {
        app.new(|ctx: &mut Context<ContextMenu>| {
            ctx.observe_window_activation(window, |_, win, _| {
                if !win.is_window_active() {
                    win.remove_window();
                }
            })
            .detach();
            ContextMenu { items, info }
        })
    }) {
        tracing::warn!(error = %e, "context_menu: open_window failed");
    }
}

/// Anchor a menu to the LEFT of the strip at the given click position.
/// `strip_origin` is the strip window's top-left in screen coordinates;
/// `click_in_strip_window` is the right-click position in strip-window
/// local coordinates.
pub fn anchor_left_of_strip(
    strip_origin: Point<Pixels>,
    click_in_strip_window: Point<Pixels>,
) -> Anchor {
    Anchor {
        right_x: strip_origin.x - px(MENU_GAP_FROM_STRIP),
        center_y: strip_origin.y + click_in_strip_window.y,
    }
}

// -- Pre-baked action helpers ----------------------------------------------

/// Quit Corral. Used by the shell-level (right-click on strip
/// background) menu.
pub fn quit_action() -> MenuItem {
    MenuItem::danger("Quit Corral", |app| app.quit())
}

/// Copy a value to the system clipboard.
pub fn copy_action(label: impl Into<SharedString>, value: String) -> MenuItem {
    MenuItem::action(label, move |app| {
        app.write_to_clipboard(ClipboardItem::new_string(value.clone()));
    })
}

/// "Reveal in Finder" via `open -R`. Runs the command on a detached
/// thread so the menu dismissal isn't blocked by the spawn.
pub fn reveal_action(label: impl Into<SharedString>, path: std::path::PathBuf) -> MenuItem {
    MenuItem::action(label, move |_app| {
        let path = path.clone();
        std::thread::spawn(move || {
            if let Err(e) = Command::new("/usr/bin/open").arg("-R").arg(&path).status() {
                tracing::warn!(error = %e, path = %path.display(), "open -R failed");
            }
        });
    })
}
