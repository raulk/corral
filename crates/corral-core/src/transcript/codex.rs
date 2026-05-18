use super::{LifecycleEvent, TranscriptParser};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::Path;

#[derive(Deserialize)]
struct CodexLine {
    timestamp: DateTime<Utc>,
    #[serde(rename = "type")]
    kind: String,
    payload: Option<CodexPayload>,
}

#[derive(Deserialize)]
struct CodexPayload {
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Present on `token_count` events. May be `null` when Codex hasn't
    /// yet produced an estimate for the current turn (sentinel rows),
    /// hence `Option`.
    #[serde(default)]
    info: Option<CodexTokenInfo>,
}

#[derive(Deserialize)]
struct CodexTokenInfo {
    /// Aggregate token usage across the session. Useful for accounting,
    /// but not for a context-window gauge because it can grow far past
    /// `model_context_window`.
    total_token_usage: Option<CodexTokenUsage>,
    /// Token usage for the latest model request. This is the value that
    /// corresponds to current context-window pressure.
    last_token_usage: Option<CodexTokenUsage>,
    /// Model's context window in tokens — Codex reports this explicitly
    /// on every `token_count` event (e.g. 272000 for gpt-5.4 today).
    #[serde(default)]
    model_context_window: Option<u32>,
}

#[derive(Deserialize)]
struct CodexTokenUsage {
    /// Non-cached + cached input tokens. Cached tokens are already
    /// counted within `input_tokens` for Codex (the
    /// `cached_input_tokens` field is a subset), so we don't add them
    /// again the way we do for Claude.
    #[serde(default)]
    input_tokens: u32,
}

pub struct CodexParser;

impl TranscriptParser for CodexParser {
    fn parse_incremental(
        &self,
        path: &Path,
        byte_offset: u64,
    ) -> std::io::Result<(Vec<LifecycleEvent>, u64)> {
        let mut file = std::fs::File::open(path)?;
        // Truncation guard: re-seed from byte 0 if the offset is past
        // the current end-of-file (rare for Codex, but kept symmetric
        // with the Claude parser).
        let file_size = file.metadata()?.len();
        let start = if byte_offset > file_size {
            0
        } else {
            byte_offset
        };
        file.seek(SeekFrom::Start(start))?;
        let mut reader = BufReader::new(file);
        let mut events = Vec::new();
        let mut consumed = start;
        let mut buf = String::new();

        loop {
            let n = super::read_bounded_line(&mut reader, &mut buf)?;
            if n == 0 {
                break;
            }
            if !buf.ends_with('\n') {
                break;
            }
            consumed += n as u64;

            let Ok(line) = serde_json::from_str::<CodexLine>(&buf) else {
                continue;
            };
            if line.kind != "event_msg" {
                continue;
            }
            let payload = line.payload;
            let pkind = payload.as_ref().and_then(|p| p.kind.as_deref());
            match pkind {
                Some("task_started") => {
                    events.push(LifecycleEvent::TurnStarted { at: line.timestamp })
                }
                Some("task_complete") => {
                    events.push(LifecycleEvent::TurnEnded { at: line.timestamp })
                }
                Some("token_count") => {
                    if let Some(info) = payload.and_then(|p| p.info) {
                        let tokens = info
                            .last_token_usage
                            .or(info.total_token_usage)
                            .map(|u| u.input_tokens)
                            .unwrap_or(0);
                        if tokens > 0 {
                            events.push(LifecycleEvent::ContextUpdated {
                                at: line.timestamp,
                                tokens,
                                max: info.model_context_window,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        Ok((events, consumed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_lines(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for l in lines {
            writeln!(tmp.as_file_mut(), "{l}").unwrap();
        }
        tmp.as_file_mut().flush().unwrap();
        tmp
    }

    #[test]
    fn task_started_then_complete() {
        let tmp = write_lines(&[
            r#"{"timestamp":"2026-05-12T19:08:51.253Z","type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"timestamp":"2026-05-12T19:12:09.628Z","type":"event_msg","payload":{"type":"task_complete"}}"#,
        ]);
        let (ev, _) = CodexParser.parse_incremental(tmp.path(), 0).unwrap();
        assert_eq!(ev.len(), 2);
        assert!(matches!(ev[0], LifecycleEvent::TurnStarted { .. }));
        assert!(matches!(ev[1], LifecycleEvent::TurnEnded { .. }));
    }

    #[test]
    fn session_meta_is_ignored() {
        let tmp = write_lines(&[
            r#"{"timestamp":"2026-05-12T19:08:51.252Z","type":"session_meta","payload":{"id":"abc"}}"#,
        ]);
        let (ev, _) = CodexParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(ev.is_empty());
    }

    #[test]
    fn token_count_with_info_emits_context_update() {
        let tmp = write_lines(&[
            r#"{"timestamp":"2026-05-12T19:09:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":3018,"cached_input_tokens":2048,"output_tokens":91,"reasoning_output_tokens":64,"total_tokens":3109},"model_context_window":272000}}}"#,
        ]);
        let (ev, _) = CodexParser.parse_incremental(tmp.path(), 0).unwrap();
        assert_eq!(ev.len(), 1);
        let LifecycleEvent::ContextUpdated { tokens, max, .. } = ev[0] else {
            panic!("expected ContextUpdated, got {:?}", ev[0]);
        };
        assert_eq!(tokens, 3018);
        assert_eq!(max, Some(272000));
    }

    #[test]
    fn token_count_uses_last_usage_for_context_window_pressure() {
        let tmp = write_lines(&[
            r#"{"timestamp":"2026-05-18T10:55:24.911Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":4308176,"cached_input_tokens":4012032,"output_tokens":11381,"reasoning_output_tokens":3482,"total_tokens":4319557},"last_token_usage":{"input_tokens":178639,"cached_input_tokens":177536,"output_tokens":561,"reasoning_output_tokens":346,"total_tokens":179200},"model_context_window":258400}}}"#,
        ]);
        let (ev, _) = CodexParser.parse_incremental(tmp.path(), 0).unwrap();
        assert_eq!(ev.len(), 1);
        let LifecycleEvent::ContextUpdated { tokens, max, .. } = ev[0] else {
            panic!("expected ContextUpdated, got {:?}", ev[0]);
        };
        assert_eq!(tokens, 178_639);
        assert_eq!(max, Some(258_400));
    }

    #[test]
    fn token_count_with_null_info_is_ignored() {
        let tmp = write_lines(&[
            r#"{"timestamp":"2026-05-12T19:09:00Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#,
        ]);
        let (ev, _) = CodexParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(ev.is_empty());
    }

    #[test]
    fn other_event_msg_payloads_ignored() {
        let tmp = write_lines(&[
            r#"{"timestamp":"2026-05-12T19:09:01Z","type":"event_msg","payload":{"type":"agent_message"}}"#,
            r#"{"timestamp":"2026-05-12T19:09:02Z","type":"event_msg","payload":{"type":"task_complete"}}"#,
        ]);
        let (ev, _) = CodexParser.parse_incremental(tmp.path(), 0).unwrap();
        assert_eq!(ev.len(), 1);
        assert!(matches!(ev[0], LifecycleEvent::TurnEnded { .. }));
    }

    #[test]
    fn malformed_and_unterminated_handled() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // Good complete line, malformed line, then partial (no newline).
        writeln!(
            tmp.as_file_mut(),
            r#"{{"timestamp":"2026-05-12T19:08:51.253Z","type":"event_msg","payload":{{"type":"task_started"}}}}"#,
        ).unwrap();
        writeln!(tmp.as_file_mut(), "not json").unwrap();
        write!(tmp.as_file_mut(), "partial").unwrap();
        tmp.as_file_mut().flush().unwrap();
        let (ev, off) = CodexParser.parse_incremental(tmp.path(), 0).unwrap();
        assert_eq!(ev.len(), 1);
        // The partial line at the tail must NOT be consumed.
        let len = std::fs::metadata(tmp.path()).unwrap().len();
        assert!(off < len, "offset must stop short of the partial tail");
    }
}
