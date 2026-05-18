use crate::adapter::{AdapterError, FocusContext, FocusOutcome, TerminalAdapter};
use corral_core::trace::FocusStrategy;
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};

/// Last-resort focus: bring the hosting app to the foreground via Cocoa.
/// Does not select a specific window/tab; the OS picks the most-recent one
/// of that app. This is the catch-all for apps without a per-tab focus API.
pub struct GenericAdapter;

impl TerminalAdapter for GenericAdapter {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn bundle_id(&self) -> Option<&'static str> {
        None
    }

    fn matches(&self, _bundle_id: &str) -> bool {
        true
    }

    fn focus(&self, ctx: &FocusContext) -> Result<FocusOutcome, AdapterError> {
        let Some(parent_app) = ctx.parent_app.as_ref() else {
            return Err(AdapterError::Unavailable(
                "no GUI parent available for generic focus".into(),
            ));
        };
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(parent_app.pid.0);
        let Some(app) = app else {
            return Err(AdapterError::Unavailable(format!(
                "no NSRunningApplication for pid {}",
                parent_app.pid.0
            )));
        };
        // `ActivateIgnoringOtherApps` was deprecated in macOS 14; the
        // single-flag form below delivers equivalent behavior.
        let ok = app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
        if !ok {
            return Err(AdapterError::Unavailable(
                "NSRunningApplication.activateWithOptions returned false".into(),
            ));
        }
        Ok(FocusOutcome {
            strategy: Some(FocusStrategy::Generic),
            focused_target_id: None,
        })
    }
}
