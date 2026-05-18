# Local patch on top of gpui-0.2.2

This directory is `gpui` v0.2.2 as published to crates.io, with one
behavioural change applied: a nil-guard on `[NSWindow screen]` inside
`start_display_link`.

We vendor rather than depend on upstream zed at HEAD because:

- The same bug still exists in `zed-industries/zed@HEAD`
  (`crates/gpui_macos/src/window.rs::start_display_link` and
  `::display_id_for_screen`), so migrating doesn't fix it.
- The public gpui API has churned significantly since 0.2.2 (split
  into `gpui` / `gpui_platform` / `gpui_macos`, new
  `gpui_platform::application()` entry point, taffy 0.10, etc.).
  Migrating watcher-app would be a multi-hour port for no crash-fix
  benefit.

## The bug

`start_display_link` is invoked from `window_did_change_screen`, the
observer GPUI registers for `NSWindowDidChangeScreenNotification`.
The implementation reads `[NSWindow screen]` and passes the result
straight to `display_id_for_screen`, which calls
`NSScreen::deviceDescription`. The cocoa-rs trait method panics with
a Rust-level null-pointer dereference when `screen` is `nil`.

This is essentially unreachable for standard `WindowKind::Normal` and
`WindowKind::Floating` windows because AppKit auto-clamps regular
`NSWindow`s to a display, so `[NSWindow screen]` is effectively
always non-nil. (This is why Zed itself never trips the bug.)

The strip uses `WindowKind::PopUp`, which gpui allocates as an
`NSPanel` with `NSNonactivatingPanelMask`. Panels are *not*
auto-clamped: their frame can sit in screen coordinates that no
active display covers, in which case `[NSPanel screen]` returns
`nil`. Combined with `setMovableByWindowBackground: YES` and our own
`setFrame:` calls during the height animation, the strip can briefly
land in that state and the next screen-change notification kills
the app.

Crash signature:

```
panic_null_pointer_dereference
  <*mut Object as cocoa::appkit::NSScreen>::deviceDescription
  gpui::platform::mac::window::display_id_for_screen
  gpui::platform::mac::window::MacWindowState::start_display_link
  gpui::platform::mac::window::window_did_change_screen
  __CFNOTIFICATIONCENTER_IS_CALLING_OUT_TO_AN_OBSERVER__
  -[NSWindow _updateSettingsSendingScreenChangeNotificationToScreen:]
  -[NSWindow _setFrameCommon:display:fromServer:]
  -[NSWindow _windowMoved:]
```

## The patch

Skip starting the display link when the window currently has no
screen. AppKit will post another `NSWindowDidChangeScreenNotification`
once it re-acquires one, which routes back into the same code path.

```diff
--- a/src/platform/mac/window.rs
+++ b/src/platform/mac/window.rs
@@ -484,7 +484,21 @@ impl MacWindowState {
                 return;
             }
         }
-        let display_id = unsafe { display_id_for_screen(self.native_window.screen()) };
+        // `[NSWindow screen]` can return `nil` when the window is
+        // between displays, off-screen, or its display is briefly
+        // unavailable (display sleep, hot-unplug, etc.). Passing nil
+        // to `display_id_for_screen` would deref it through the
+        // cocoa-rs `NSScreen::deviceDescription` trait and panic with
+        // a null-pointer dereference. Skip starting the display link
+        // in that case; AppKit will post another
+        // `NSWindowDidChangeScreenNotification` once the window
+        // re-acquires a screen, which routes us back here.
+        let screen = unsafe { self.native_window.screen() };
+        if screen.is_null() {
+            return;
+        }
+        let display_id = unsafe { display_id_for_screen(screen) };
         if let Some(mut display_link) =
             DisplayLink::new(display_id, self.native_view.as_ptr() as *mut c_void, step).log_err()
         {
```

## Maintenance

If we ever bump gpui (vendored or otherwise), re-apply this guard
unless the upstream version has already added one. As of
zed-industries/zed@3bd9d13b63 (2026-05-15) the upstream code is
character-for-character identical to 0.2.2 in this region.
