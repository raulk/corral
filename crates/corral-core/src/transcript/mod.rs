use chrono::{DateTime, Utc};
use std::io::BufRead;
use std::path::Path;

/// Hard cap on a single transcript line. A malformed transcript (or
/// an attacker writing into ~/.claude/projects) could otherwise force
/// `BufRead::read_line` to allocate unbounded memory. 4 MiB
/// comfortably accommodates the largest assistant turns observed in
/// practice while keeping a single bad line bounded.
pub const MAX_TRANSCRIPT_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Read a JSONL line into `buf`. Returns the number of bytes consumed
/// from `reader` (including the trailing `\n`), 0 on EOF. Lines longer
/// than `MAX_TRANSCRIPT_LINE_BYTES` are skipped: `buf` is cleared and
/// the reader advances past the offending newline so subsequent lines
/// parse normally.
pub(crate) fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    buf: &mut String,
) -> std::io::Result<usize> {
    buf.clear();
    // Read by hand so we can both cap the retained buffer and still
    // advance the underlying reader past an over-limit line.
    let mut consumed: usize = 0;
    let mut retained: usize = 0;
    let mut over = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break; // EOF
        }
        let nl_pos = available.iter().position(|&b| b == b'\n');
        let take = nl_pos.map(|i| i + 1).unwrap_or(available.len());
        let remaining = MAX_TRANSCRIPT_LINE_BYTES.saturating_sub(retained);
        let keep = take.min(remaining);
        // Convert just the bytes we want, lossily: invalid sequences
        // become U+FFFD instead of aborting the entire parse pass. A
        // single bad byte in a single line previously poisoned the
        // whole transcript forever (offset never advances past the
        // failing line, every retry hits the same byte), causing
        // affected agents to stay stuck on their default Active state.
        // The downstream JSON parser will reject any line where the
        // replacement breaks structure; valid lines around the
        // corruption parse normally.
        if keep > 0 {
            buf.push_str(&String::from_utf8_lossy(&available[..keep]));
        }
        reader.consume(take);
        consumed = consumed.saturating_add(take);
        retained = retained.saturating_add(keep);
        if keep < take {
            over = true;
        }
        if nl_pos.is_some() {
            if over {
                tracing::warn!(
                    bytes = consumed,
                    limit = MAX_TRANSCRIPT_LINE_BYTES,
                    "transcript line exceeds limit; skipping"
                );
                buf.clear();
            }
            break;
        }
        if over {
            // Drain the rest of the over-limit line and count those
            // bytes in the returned offset. Reopening the file on the
            // next parser pass must seek to the following line, not
            // into the middle of the skipped one.
            loop {
                let avail = reader.fill_buf()?;
                if avail.is_empty() {
                    break;
                }
                if let Some(i) = avail.iter().position(|&b| b == b'\n') {
                    reader.consume(i + 1);
                    consumed = consumed.saturating_add(i + 1);
                    break;
                }
                let l = avail.len();
                reader.consume(l);
                consumed = consumed.saturating_add(l);
            }
            tracing::warn!(
                bytes = consumed,
                limit = MAX_TRANSCRIPT_LINE_BYTES,
                "transcript line exceeds limit; skipping"
            );
            buf.clear();
            break;
        }
    }
    Ok(consumed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    TurnStarted {
        at: DateTime<Utc>,
    },
    TurnEnded {
        at: DateTime<Utc>,
    },
    /// The assistant blocked on an `AskUserQuestion` tool call. The
    /// agent process is still running but cannot proceed until the user
    /// answers the structured prompt. Distinct from `TurnEnded` because
    /// the turn hasn't ended — it's mid-tool-use — and the agent is
    /// definitely waiting on a human, not just idling.
    AwaitingUser {
        at: DateTime<Utc>,
    },
    /// The latest observed prompt-token usage. `max` is the model's
    /// reported context window when the transcript carries it (Codex
    /// includes it on every `token_count` event); for Claude the
    /// parser resolves it via the model id and emits a `Some(_)` here
    /// too. Not strictly a lifecycle event but riding the same channel
    /// keeps the parser's surface area to one function call.
    ContextUpdated {
        at: DateTime<Utc>,
        tokens: u32,
        max: Option<u32>,
    },
    /// Loose metadata observed on transcript lines: model id, git
    /// branch, conversation slug, and the most-recently-invoked tool.
    /// Every field is optional and only `Some(_)` values overwrite
    /// the agent's stored metadata — fields the parser didn't see in
    /// this batch carry their previous value through.
    MetadataUpdated {
        at: DateTime<Utc>,
        model: Option<String>,
        git_branch: Option<String>,
        session_title: Option<String>,
        current_action: Option<String>,
    },
    /// The current tool-action label is no longer live. Used for
    /// structured user-answer tool results: once the answer arrives,
    /// `AskUserQuestion` is no longer the current action even though
    /// there may be no following assistant tool call yet.
    CurrentActionCleared {
        at: DateTime<Utc>,
    },
}

/// Routing taxonomy for `LifecycleEvent`. The registry fans events
/// out by category: `Lifecycle` events feed `compute_state`, `Context`
/// and `Metadata` events mutate the agent record directly. Centralised
/// here so a new variant gets a compile-time prompt to declare which
/// path it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventCategory {
    Lifecycle,
    Context,
    Metadata,
}

impl LifecycleEvent {
    pub fn at(&self) -> DateTime<Utc> {
        match self {
            LifecycleEvent::TurnStarted { at }
            | LifecycleEvent::TurnEnded { at }
            | LifecycleEvent::AwaitingUser { at }
            | LifecycleEvent::ContextUpdated { at, .. }
            | LifecycleEvent::MetadataUpdated { at, .. }
            | LifecycleEvent::CurrentActionCleared { at } => *at,
        }
    }

    pub fn category(&self) -> EventCategory {
        match self {
            LifecycleEvent::TurnStarted { .. }
            | LifecycleEvent::TurnEnded { .. }
            | LifecycleEvent::AwaitingUser { .. } => EventCategory::Lifecycle,
            LifecycleEvent::ContextUpdated { .. } => EventCategory::Context,
            LifecycleEvent::MetadataUpdated { .. }
            | LifecycleEvent::CurrentActionCleared { .. } => EventCategory::Metadata,
        }
    }
}

pub trait TranscriptParser: Send + Sync {
    /// Parse new bytes starting at `byte_offset`. Returns `(events, new_offset)`.
    /// Malformed lines are skipped silently; an unterminated tail line is
    /// treated as partial and left for the next call (the offset stops short
    /// of it).
    fn parse_incremental(
        &self,
        path: &Path,
        byte_offset: u64,
    ) -> std::io::Result<(Vec<LifecycleEvent>, u64)>;

    fn parse_incremental_with_context(
        &self,
        path: &Path,
        byte_offset: u64,
        _last_lifecycle: Option<&LifecycleEvent>,
    ) -> std::io::Result<(Vec<LifecycleEvent>, u64)> {
        self.parse_incremental(path, byte_offset)
    }
}

pub mod claude;
pub mod codex;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn over_limit_line_advances_to_next_line() {
        let mut input = vec![b'a'; MAX_TRANSCRIPT_LINE_BYTES + 16];
        input.push(b'\n');
        input.extend_from_slice(b"{\"ok\":true}\n");
        let mut reader = BufReader::new(Cursor::new(input));
        let mut buf = String::new();

        let skipped = read_bounded_line(&mut reader, &mut buf).unwrap();
        assert_eq!(skipped, MAX_TRANSCRIPT_LINE_BYTES + 17);
        assert!(buf.is_empty());

        let next = read_bounded_line(&mut reader, &mut buf).unwrap();
        assert_eq!(next, b"{\"ok\":true}\n".len());
        assert_eq!(buf, "{\"ok\":true}\n");
    }

    #[test]
    fn invalid_utf8_byte_does_not_abort_parse() {
        // A bad byte mid-line previously bubbled an InvalidData error
        // that aborted the whole parse pass, leaving the consumer
        // unable to advance past the bad line. Lossy decoding now
        // replaces the bad byte with U+FFFD and the parser sees the
        // rest of the file.
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(b"{\"a\":\"");
        input.push(0xff); // invalid UTF-8 sentinel
        input.extend_from_slice(b"\"}\n");
        input.extend_from_slice(b"{\"ok\":true}\n");
        let mut reader = BufReader::new(Cursor::new(input));
        let mut buf = String::new();

        let first = read_bounded_line(&mut reader, &mut buf).unwrap();
        assert!(first > 0, "must consume the corrupted line, not error out");
        assert!(buf.ends_with('\n'));

        buf.clear();
        let second = read_bounded_line(&mut reader, &mut buf).unwrap();
        assert_eq!(buf, "{\"ok\":true}\n");
        assert_eq!(second, b"{\"ok\":true}\n".len());
    }
}
