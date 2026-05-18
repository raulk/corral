use crate::transcript::LifecycleEvent;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentState {
    Active,
    /// Assistant is blocked on an `AskUserQuestion` tool call. The
    /// agent process is alive but cannot proceed until the user
    /// answers; this is a stronger "needs you" signal than
    /// `NeedsInput`, which just marks "turn ended, no further input
    /// yet".
    AwaitingUser,
    NeedsInput,
    Idle,
    Closed,
}

pub fn idle_after() -> Duration {
    Duration::minutes(5)
}

/// Pure mapping from observable inputs to a tile state. No I/O.
///
/// - `last_lifecycle` is the most recent `TurnStarted` or `TurnEnded` event seen
///   in the transcript.
/// - `last_write_at` is the wall-clock time of the most recent observed write
///   to the transcript file (mtime or the last parser pass), used to detect
///   "the agent finished a turn five minutes ago and nothing's happened
///   since" → Idle.
/// - `process_alive` short-circuits to `Closed`; everything else assumes a
///   live agent process.
pub fn compute_state(
    last_lifecycle: Option<&LifecycleEvent>,
    last_write_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    process_alive: bool,
) -> AgentState {
    if !process_alive {
        return AgentState::Closed;
    }
    // `AwaitingUser` is sticky — it persists until the next user
    // message overrides it via a `TurnStarted` event. Don't decay
    // to `Idle` over time; the agent literally cannot proceed
    // without an answer and idle-style dimming would understate
    // that.
    if matches!(last_lifecycle, Some(LifecycleEvent::AwaitingUser { .. })) {
        return AgentState::AwaitingUser;
    }
    let mtime_idle = last_write_at
        .map(|t| now.signed_duration_since(t) >= idle_after())
        .unwrap_or(false);
    // Stale mtime overrides any lifecycle classification (other than
    // sticky AwaitingUser, handled above). Without this, a session
    // whose latest parsed event is `TurnStarted` — common for sessions
    // resumed via `claude --continue` from an abandoned mid-tool turn,
    // or any tail line that's an assistant `tool_use` never followed
    // by `end_turn` — renders Active forever despite no recent writes.
    // Trade-off: a genuinely long-running command (>5min, no transcript
    // writes) misclassifies as Idle, which is rarer than the orphaned
    // TurnStarted case and recovers as soon as the next write lands.
    if mtime_idle {
        return AgentState::Idle;
    }

    match last_lifecycle {
        Some(LifecycleEvent::TurnStarted { .. }) => AgentState::Active,
        Some(LifecycleEvent::TurnEnded { .. }) => AgentState::NeedsInput,
        // `ContextUpdated` and `MetadataUpdated` are property updates,
        // never *state-driving* events. The registry routes them into
        // a separate path before this function is called, so seeing
        // them here means there hasn't been a real turn yet — same
        // as `None`.
        Some(LifecycleEvent::AwaitingUser { .. })
        | Some(LifecycleEvent::ContextUpdated { .. })
        | Some(LifecycleEvent::MetadataUpdated { .. })
        | Some(LifecycleEvent::CurrentActionCleared { .. })
        | None => AgentState::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(s, 0).unwrap()
    }

    #[test]
    fn dead_process_is_closed_regardless_of_inputs() {
        assert_eq!(compute_state(None, None, t(0), false), AgentState::Closed,);
        assert_eq!(
            compute_state(
                Some(&LifecycleEvent::TurnStarted { at: t(0) }),
                Some(t(0)),
                t(0),
                false
            ),
            AgentState::Closed,
        );
    }

    #[test]
    fn turn_started_is_active() {
        let s = compute_state(
            Some(&LifecycleEvent::TurnStarted { at: t(100) }),
            Some(t(100)),
            t(120),
            true,
        );
        assert_eq!(s, AgentState::Active);
    }

    #[test]
    fn turn_ended_recently_is_needs_input() {
        let s = compute_state(
            Some(&LifecycleEvent::TurnEnded { at: t(100) }),
            Some(t(100)),
            t(120),
            true,
        );
        assert_eq!(s, AgentState::NeedsInput);
    }

    #[test]
    fn turn_ended_long_ago_is_idle() {
        let s = compute_state(
            Some(&LifecycleEvent::TurnEnded { at: t(0) }),
            Some(t(0)),
            t(0) + idle_after(),
            true,
        );
        assert_eq!(s, AgentState::Idle);
    }

    #[test]
    fn live_without_events_is_active() {
        let s = compute_state(None, None, t(0), true);
        assert_eq!(s, AgentState::Active);
    }

    #[test]
    fn awaiting_user_is_sticky_even_long_after() {
        // Even with mtime well past `idle_after`, an outstanding
        // AskUserQuestion keeps the tile in AwaitingUser rather than
        // decaying to Idle.
        let s = compute_state(
            Some(&LifecycleEvent::AwaitingUser { at: t(0) }),
            Some(t(0)),
            t(0) + idle_after() + chrono::Duration::minutes(30),
            true,
        );
        assert_eq!(s, AgentState::AwaitingUser);
    }

    #[test]
    fn awaiting_user_is_overridden_by_next_user_turn() {
        let s = compute_state(
            Some(&LifecycleEvent::TurnStarted { at: t(200) }),
            Some(t(200)),
            t(210),
            true,
        );
        assert_eq!(s, AgentState::Active);
    }

    #[test]
    fn orphaned_turn_started_with_stale_mtime_is_idle() {
        // A `claude --continue` of a session that was killed mid-tool
        // has its tail line as an assistant `tool_use` → parser's
        // latest_lifecycle is TurnStarted. Without the mtime override
        // the tile would render Active forever; with it, the stale
        // mtime correctly classifies as Idle.
        let s = compute_state(
            Some(&LifecycleEvent::TurnStarted { at: t(0) }),
            Some(t(0)),
            t(0) + idle_after(),
            true,
        );
        assert_eq!(s, AgentState::Idle);
    }

    #[test]
    fn none_with_stale_mtime_is_idle() {
        // Agent observed but no parseable lifecycle event (e.g., the
        // transcript carries only metadata or hook records). If the
        // file's last write is older than `idle_after`, the agent is
        // Idle, not Active.
        let s = compute_state(None, Some(t(0)), t(0) + idle_after(), true);
        assert_eq!(s, AgentState::Idle);
    }
}
