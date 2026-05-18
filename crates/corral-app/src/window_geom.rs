//! macOS-only helper for moving + resizing the strip's NSWindow atomically.
//!
//! GPUI 0.2.2 only exposes `Window::resize`, which calls
//! `-[NSWindow setContentSize:]`. That keeps the window's *top-left*
//! fixed across resizes, so the strip would extend downward as tiles
//! arrive and drift into the lower half of the screen. The user needs
//! the strip to stay centred vertically, which requires moving the
//! window in lock-step with resizing.
//!
//! Cocoa exposes `-[NSWindow setFrame:display:animate:]` for exactly
//! this — it changes origin and size in one shot. GPUI doesn't surface
//! the underlying NSWindow handle, so we reach for it by enumerating
//! `NSApp.windows` and matching on the strip's known width.

use cocoa::appkit::{NSApp, NSWindow};
use cocoa::base::{BOOL, NO, YES, id, nil};
use cocoa::foundation::{NSArray, NSPoint, NSRect, NSSize};
use objc::{msg_send, sel, sel_impl};

/// Flip the strip NSWindow's `movableByWindowBackground` flag. With
/// it on, dragging anywhere on the strip's opaque background moves
/// the window (children that own the click — tiles, the dock-back
/// button — still receive their normal mouse events). Call once after
/// `open_window` has run.
pub fn set_strip_movable(movable: bool) -> bool {
    unsafe {
        if let Some(window) = find_strip_window() {
            let _: () = msg_send![window, setMovableByWindowBackground: movable as BOOL];
            // `is_movable` controls system-level moves (frame manip
            // via setFrame, plus user drag on the title bar). We want
            // both true for our headless / panel window.
            let _: () = msg_send![window, setMovable: movable as BOOL];
            true
        } else {
            false
        }
    }
}

unsafe fn find_strip_window() -> Option<id> {
    let app = unsafe { NSApp() };
    if app == nil {
        return None;
    }
    let windows: id = unsafe { msg_send![app, windows] };
    if windows == nil {
        return None;
    }
    let count = unsafe { NSArray::count(windows) } as usize;
    for i in 0..count {
        let window: id = unsafe { msg_send![windows, objectAtIndex: i as u64] };
        if window == nil {
            continue;
        }
        let frame: NSRect = unsafe { window.frame() };
        if (frame.size.width - 36.0).abs() < 0.5 {
            return Some(window);
        }
    }
    None
}

/// Move and resize the strip's NSWindow atomically. `origin_y_display` is
/// the desired top of the window in *display-local* coordinates (y down,
/// 0 = top of the screen the strip currently lives on); the function
/// converts to Cocoa's bottom-up global frame coordinates internally,
/// using the screen's actual position so multi-display setups work
/// correctly. `display_height` is kept for API stability but only used
/// as a fallback when the NSWindow's `screen` is briefly nil
/// (transient state during display sleep / monitor unplug).
///
/// Returns `true` if the strip window was found and updated, `false`
/// if no matching window exists yet (typically only on the very first
/// render before macOS has finished registering it).
pub fn set_strip_frame(
    origin_x_display: f32,
    origin_y_display: f32,
    width: f32,
    height: f32,
    display_height: f32,
) -> bool {
    unsafe {
        let Some(window) = find_strip_window() else {
            return false;
        };
        // `[NSWindow screen]` can return nil between displays; fall
        // back to display_height with a (0, 0) origin in that case.
        // GPUI's `start_display_link` patch documents the same nil
        // window→screen possibility.
        let screen: id = msg_send![window, screen];
        let (screen_origin_y, screen_height) = if screen == nil {
            (0.0_f64, display_height as f64)
        } else {
            let frame: NSRect = msg_send![screen, frame];
            (frame.origin.y, frame.size.height)
        };
        // Cocoa global coords: y = 0 at bottom of primary screen, up
        // positive. screen.origin.y is the bottom of THIS screen in
        // global coords. Top of window in global = screen_origin_y +
        // screen_height - origin_y_display (display-local y is
        // top-down). Window frame origin is the bottom-left.
        let top_y_global = screen_origin_y + screen_height - origin_y_display as f64;
        let cocoa_y = top_y_global - height as f64;
        let target = NSRect::new(
            NSPoint::new(origin_x_display as f64, cocoa_y),
            NSSize::new(width as f64, height as f64),
        );
        let _: () = msg_send![window, setFrame: target display: YES animate: NO];
        true
    }
}
