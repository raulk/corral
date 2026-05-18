use crate::adapter::{AdapterError, FocusContext, FocusOutcome, TerminalAdapter};
use crate::iterm2::{applescript_string, run_osascript};
use corral_core::trace::FocusStrategy;

const BUNDLE_ID: &str = "com.apple.Terminal";

pub struct TerminalAppAdapter;

impl TerminalAdapter for TerminalAppAdapter {
    fn name(&self) -> &'static str {
        "terminal.app"
    }

    fn bundle_id(&self) -> Option<&'static str> {
        Some(BUNDLE_ID)
    }

    fn matches(&self, bundle_id: &str) -> bool {
        bundle_id == BUNDLE_ID
    }

    fn focus(&self, ctx: &FocusContext) -> Result<FocusOutcome, AdapterError> {
        let Some(tty) = ctx.cli_tty.as_ref() else {
            return Err(AdapterError::Unavailable("agent has no tty".into()));
        };
        let tty_lit = applescript_string(&tty.to_string_lossy());
        // Terminal.app exposes `tty` on tabs (no separate session concept).
        let script = format!(
            r#"
            tell application "Terminal"
                activate
                repeat with w in windows
                    repeat with t in tabs of w
                        if tty of t is {tty} then
                            set selected tab of w to t
                            set index of w to 1
                            return "ok"
                        end if
                    end repeat
                end repeat
                return "not-found"
            end tell
            "#,
            tty = tty_lit
        );
        run_osascript(&script)?;
        Ok(FocusOutcome {
            strategy: Some(FocusStrategy::Tty),
            focused_target_id: None,
        })
    }
}
