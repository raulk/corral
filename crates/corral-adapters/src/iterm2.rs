use crate::adapter::{AdapterError, FocusContext, FocusOutcome, TerminalAdapter};
use corral_core::trace::FocusStrategy;
use std::process::Command;

const BUNDLE_ID: &str = "com.googlecode.iterm2";

pub struct Iterm2Adapter;

impl TerminalAdapter for Iterm2Adapter {
    fn name(&self) -> &'static str {
        "iterm2"
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
        // iTerm2 stores `tty` per session; loop through every session looking
        // for a match. When found, raise its window, select its tab, then
        // select the session. The `activate` at the top brings the app
        // forward so a focused session is actually visible.
        let script = format!(
            r#"
            tell application "iTerm2"
                activate
                repeat with w in windows
                    repeat with t in tabs of w
                        repeat with s in sessions of t
                            if tty of s is {tty} then
                                select w
                                tell t to select
                                tell s to select
                                return "ok"
                            end if
                        end repeat
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

/// osascript surfaces "User canceled." (errAEEventNotPermitted, -1743)
/// when the user denies a first-time Automation prompt, and
/// "not allowed to send Apple events" when permission was previously
/// revoked. Both must drop dispatch back to Generic rather than fail.
const OSA_NOT_ALLOWED: &str = "not allowed";
const OSA_USER_CANCELED: &str = "User canceled";

pub(crate) fn run_osascript(script: &str) -> Result<(), AdapterError> {
    // Absolute path: avoid PATH-resolution surprises (a malicious PATH
    // entry could shadow `osascript`).
    let out = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()?;
    if out.status.success() {
        // The per-adapter scripts return "ok" on success and
        // "not-found" / "no-match" / "no-tab" on a miss. A miss from a
        // matched adapter is terminal state, not a reason to focus a
        // random window through GenericAdapter.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let last = stdout.lines().last().unwrap_or("").trim();
        if last == "not-found" || last == "no-match" || last == "no-tab" {
            return Err(AdapterError::NotFound(last.into()));
        }
        if let Some(reason) = last.strip_prefix("unsupported:") {
            return Err(AdapterError::Unavailable(reason.trim().into()));
        }
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if is_permission_error(&stderr) {
        return Err(AdapterError::PermissionDenied);
    }
    Err(AdapterError::Unavailable(format!(
        "osascript exit {:?}: {}",
        out.status.code(),
        stderr.trim()
    )))
}

fn is_permission_error(stderr: &str) -> bool {
    stderr.contains(OSA_NOT_ALLOWED) || stderr.contains(OSA_USER_CANCELED)
}

/// Wrap a Rust string as an AppleScript string literal. Backslashes and
/// double quotes have to be escaped; CR and LF would terminate the
/// literal and so are stripped. Anything else is preserved verbatim —
/// paths legitimately containing spaces or Unicode work as-is.
pub(crate) fn applescript_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\r' | '\n' => {}
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_a_plain_path() {
        assert_eq!(applescript_string("/a/b/c"), "\"/a/b/c\"");
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(applescript_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn strips_line_breaks() {
        assert_eq!(applescript_string("a\nb\rc"), "\"abc\"");
    }
}
