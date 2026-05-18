use crate::proc::is_jsonl;
use crossbeam_channel::Sender;
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use std::path::PathBuf;
use std::time::Duration;

/// FSEvents-backed corral (via `notify`) over `~/.claude/projects/` and
/// `~/.codex/sessions/`. Coalesces rapid writes within `debounce` and emits
/// one `TranscriptChanged(path)` per affected `.jsonl` file.
///
/// Caller owns the returned `Debouncer`; dropping it stops the watch.
pub fn spawn_transcript_watcher<F>(
    roots: &[PathBuf],
    debounce: Duration,
    on_event: F,
) -> notify::Result<Debouncer<notify::RecommendedWatcher>>
where
    F: Fn(PathBuf) + Send + 'static,
{
    let mut debouncer = new_debouncer(debounce, move |result: DebounceEventResult| match result {
        Ok(events) => {
            for ev in events {
                if is_jsonl(&ev.path) {
                    on_event(ev.path);
                }
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "fsevents debouncer reported error");
        }
    })?;

    for root in roots {
        // Create the root if absent so the FSEvents watch attaches
        // *now* rather than being silently skipped forever. Once Claude
        // or Codex starts and writes its first session, the watch
        // picks it up immediately. Skipping permanently (the previous
        // behaviour) meant a user who hadn't yet run those tools
        // before launching corral would never see live transcript
        // updates without restarting the app.
        if !root.exists()
            && let Err(e) = std::fs::create_dir_all(root)
        {
            tracing::warn!(?root, error = %e, "could not create transcript root");
            continue;
        }
        debouncer
            .watcher()
            .watch(root, notify::RecursiveMode::Recursive)?;
    }

    Ok(debouncer)
}
/// Convenience: wires the corral to a `crossbeam_channel::Sender` that the
/// registry consumes.
pub fn spawn_transcript_watcher_into(
    roots: &[PathBuf],
    debounce: Duration,
    tx: Sender<PathBuf>,
) -> notify::Result<Debouncer<notify::RecommendedWatcher>> {
    spawn_transcript_watcher(roots, debounce, move |path| {
        let _ = tx.send(path);
    })
}
