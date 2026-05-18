use super::{LifecycleEvent, TranscriptParser, read_bounded_line};
use crate::text::truncate_end;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::Path;

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum LineKind {
    User,
    Assistant,
    System,
    /// Marker Claude writes when a new prompt is queued; reliably
    /// indicates a new turn even before the user-line lands.
    QueueOperation,
    /// AI-derived session title — Claude writes this to a `ai-title`
    /// line near the top of a transcript.
    #[serde(rename = "ai-title")]
    AiTitle,
    /// User-set session title; takes precedence over `ai-title`.
    #[serde(rename = "custom-title")]
    CustomTitle,
    /// Any other `type` value Claude emits. Forward-compatible: new
    /// kinds don't cause parse failures.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ClaudeLine {
    #[serde(rename = "type")]
    kind: LineKind,
    timestamp: Option<DateTime<Utc>>,
    message: Option<ClaudeMessage>,
    subtype: Option<String>,
    #[serde(default, rename = "isSidechain")]
    is_sidechain: bool,
    /// Harness-synthesised lines that mimic user/assistant shapes
    /// (wake-up nudges, "Recheck X status" follow-ups). Treating them
    /// as real turns inflates lifecycle events; skip outright.
    #[serde(default, rename = "isMeta")]
    is_meta: bool,
    /// `gitBranch` is attached top-level to most lines Claude writes.
    #[serde(default, rename = "gitBranch")]
    git_branch: Option<String>,
    /// `slug` is a kebab-case AI-generated topic ID, present on
    /// assistant lines and a few metadata rows.
    #[serde(default)]
    slug: Option<String>,
    /// `queue-operation` discriminator. Only `enqueue` is a new
    /// prompt; `dequeue` and `remove` aren't turn-starts.
    #[serde(default)]
    operation: Option<String>,
    /// Top-level tool result metadata. Normal tool results use shapes
    /// like `{ stdout, stderr }`; AskUserQuestion answers use
    /// `{ questions, answers }`.
    #[serde(default, rename = "toolUseResult")]
    tool_use_result: Option<serde_json::Value>,
    /// `ai-title` records carry the AI-generated session title here.
    #[serde(default, rename = "aiTitle")]
    ai_title: Option<String>,
    /// `custom-title` records carry the user-set session title here.
    #[serde(default, rename = "customTitle")]
    custom_title: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    stop_reason: Option<String>,
    /// Model id for this turn (e.g. `claude-opus-4-7`,
    /// `claude-sonnet-4-6`). Used to look up the model's context
    /// window since Claude doesn't surface that value in transcripts
    /// directly.
    #[serde(default)]
    model: Option<String>,
    /// Raw content blocks. We only inspect the `name` field of any
    /// `tool_use` entries (specifically `AskUserQuestion`), so leaving
    /// the rest as opaque JSON keeps the parser robust against the
    /// frequent additions to Claude's content-block schema.
    ///
    /// Claude transcripts sometimes emit `content` as a plain string
    /// (legacy user lines, system messages); `deserialize_content`
    /// coerces both shapes into the block-array form so downstream
    /// callers see a uniform type.
    #[serde(default, deserialize_with = "deserialize_content")]
    content: Vec<serde_json::Value>,
    /// Per-turn token usage Claude attaches to every assistant
    /// message.  `input_tokens` is the *new* (uncached) input;
    /// `cache_read_input_tokens` is the older conversation served from
    /// the prompt cache; `cache_creation_input_tokens` is new content
    /// being added to the cache. Their sum is the actual context
    /// length the model saw on this turn.
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
}

/// Tool name the assistant uses to open a structured prompt the user
/// must answer before the agent can continue.
const ASK_USER_TOOL_NAME: &str = "AskUserQuestion";

/// Accept `content` as either a JSON array of blocks or a plain
/// string. Lines using the legacy string form are wrapped as
/// `[{"type": "text", "text": s}]` so downstream block-shaped checks
/// (`is_asking_user`, `has_user_text`, `latest_tool_action`) see a
/// uniform Vec.
fn deserialize_content<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Vec<serde_json::Value>, D::Error> {
    use serde::Deserialize;
    use serde_json::Value;
    match Value::deserialize(de)? {
        Value::Array(arr) => Ok(arr),
        Value::String(s) => Ok(vec![serde_json::json!({"type": "text", "text": s})]),
        Value::Null => Ok(Vec::new()),
        other => Err(serde::de::Error::custom(format!(
            "content must be array or string, got {}",
            type_name_of(&other)
        ))),
    }
}

fn type_name_of(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

pub struct ClaudeParser;

/// Returns true if the assistant message includes at least one
/// `tool_use` content block whose `name` matches the `AskUserQuestion`
/// tool — the marker for "agent is blocked on a user answer".
fn is_asking_user(msg: Option<&ClaudeMessage>) -> bool {
    let Some(msg) = msg else { return false };
    msg.content.iter().any(|c| {
        c.get("type").and_then(|t| t.as_str()) == Some("tool_use")
            && c.get("name").and_then(|n| n.as_str()) == Some(ASK_USER_TOOL_NAME)
    })
}

/// Distinguish a "user typed a new prompt" turn-start from a "user
/// line carrying only tool_result blocks" continuation. Returns true
/// only when the message has at least one content block that is not a
/// `tool_result` (i.e. an actual text input from the human). When
/// `content` is a plain string instead of an array, treat it as user
/// text too.
fn has_user_text(msg: Option<&ClaudeMessage>) -> bool {
    let Some(msg) = msg else { return true };
    if msg.content.is_empty() {
        // Unknown shape (e.g. content was a plain string serialised
        // elsewhere). Err on the side of "yes, real user turn" so we
        // don't silently lose lifecycle events.
        return true;
    }
    msg.content
        .iter()
        .any(|c| c.get("type").and_then(|t| t.as_str()) != Some("tool_result"))
}

/// Claude records an AskUserQuestion answer as a `tool_result` user
/// line, not as user text. The top-level `toolUseResult` payload is
/// the stable discriminator: prompt answers carry both the original
/// `questions` and selected `answers`, while ordinary tool outputs use
/// tool-specific keys such as `stdout` or `commandName`.
fn is_ask_user_answer(line: &ClaudeLine) -> bool {
    let Some(result) = line.tool_use_result.as_ref() else {
        return false;
    };
    matches!(result.get("questions"), Some(serde_json::Value::Array(_)))
        && matches!(result.get("answers"), Some(serde_json::Value::Object(_)))
}

/// Build a short, human-readable label from the latest `tool_use`
/// block in an assistant message — e.g. `Read foo.rs`, `$ ls -la`,
/// `Editing src/main.rs`. Returns `None` when the message has no
/// `tool_use` (pure-text assistant reply).
fn latest_tool_action(msg: &ClaudeMessage) -> Option<String> {
    let last = msg
        .content
        .iter()
        .rev()
        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("tool_use"))?;
    let name = last.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let input = last.get("input");
    Some(format_action(name, input))
}

/// How to render the input value for a tool action.
#[derive(Clone, Copy)]
enum ArgFormat {
    /// Show just the file basename: `Read foo.rs`.
    Basename,
    /// Show the raw value truncated to N chars: `Glob *.py`.
    Truncate(usize),
}

/// Tool name → (input key, prefix, fallback label, arg format).
/// Prefix is a `format!`-style template applied to the rendered arg.
const TOOL_FORMATTERS: &[(&str, &str, &str, &str, ArgFormat)] = &[
    ("Read", "file_path", "Read {}", "Read", ArgFormat::Basename),
    ("Edit", "file_path", "Edit {}", "Edit", ArgFormat::Basename),
    (
        "Write",
        "file_path",
        "Write {}",
        "Write",
        ArgFormat::Basename,
    ),
    (
        "Bash",
        "command",
        "$ {}",
        "Run command",
        ArgFormat::Truncate(36),
    ),
    (
        "Glob",
        "pattern",
        "Glob {}",
        "Glob",
        ArgFormat::Truncate(32),
    ),
    (
        "Grep",
        "pattern",
        "Grep {}",
        "Grep",
        ArgFormat::Truncate(32),
    ),
    (
        "Task",
        "description",
        "Task: {}",
        "Subagent",
        ArgFormat::Truncate(32),
    ),
    (
        "WebFetch",
        "url",
        "Fetch {}",
        "WebFetch",
        ArgFormat::Truncate(32),
    ),
    (
        "WebSearch",
        "query",
        "Search {}",
        "WebSearch",
        ArgFormat::Truncate(32),
    ),
];

/// Static labels for tools that don't render any input.
const STATIC_LABELS: &[(&str, &str)] = &[
    ("AskUserQuestion", "Asking question"),
    ("TodoWrite", "Update todos"),
];

/// Turn a Claude tool invocation into a short single-line description.
/// Falls back to the tool name itself when no recognisable input
/// field is present — MCP tools (`mcp__plugin_playwright_…`) and any
/// new built-in remain identifiable.
fn format_action(name: &str, input: Option<&serde_json::Value>) -> String {
    if let Some((_, label)) = STATIC_LABELS.iter().find(|(n, _)| *n == name) {
        return (*label).into();
    }
    let Some(&(_, key, prefix, fallback, fmt)) =
        TOOL_FORMATTERS.iter().find(|(n, _, _, _, _)| *n == name)
    else {
        return name.to_string();
    };
    let raw = input
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    match raw {
        None => fallback.into(),
        Some(s) => {
            let rendered = match fmt {
                ArgFormat::Basename => std::path::Path::new(s)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(s)
                    .to_string(),
                ArgFormat::Truncate(max) => truncate_end(s, max),
            };
            prefix.replacen("{}", &rendered, 1)
        }
    }
}

/// Convert a kebab-case slug like `let-s-conceptualize-a-docker-piglet`
/// into a sentence-cased title `Let's conceptualize a docker piglet`.
/// Heuristic: replace hyphens with spaces, capitalise the first
/// character, and stitch single-letter contractions like `let s` →
/// `let's`.
fn humanise_slug(slug: &str) -> String {
    let with_spaces: String = slug
        .chars()
        .map(|c| if c == '-' { ' ' } else { c })
        .collect();
    // Re-join orphan single letters that sit between words: those
    // come from possessives/contractions Claude flattens when slugifying
    // (e.g. `let-s-do-x` ← "let's do x").
    let mut joined = String::with_capacity(with_spaces.len());
    let words: Vec<&str> = with_spaces.split(' ').filter(|w| !w.is_empty()).collect();
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            let prev = words[i - 1];
            if w.len() == 1 && prev.len() > 1 {
                joined.push('\'');
                joined.push_str(w);
                continue;
            }
            joined.push(' ');
        }
        joined.push_str(w);
    }
    let mut out = joined;
    if let Some(first) = out.chars().next() {
        let upper = first.to_uppercase().to_string();
        out.replace_range(..first.len_utf8(), &upper);
    }
    out
}

/// Look up a Claude model's context window in tokens. Claude doesn't
/// emit this value in transcripts, so we keep a small table keyed on
/// the model id Anthropic ships in `message.model`. Unknown / future
/// models fall through to the 200K default, which is the published
/// floor across the 4.x family; sessions exceeding that value will
/// simply render the ring at 100% until the table is updated.
pub fn claude_context_window(model: &str) -> u32 {
    // Opus 4.7 ships with a 1M context window. The full id is
    // `claude-opus-4-7-<date>` so match on the prefix.
    if model.starts_with("claude-opus-4-7") {
        return 1_000_000;
    }
    // Sonnet, Haiku, older Opus revisions, and anything else: 200K.
    200_000
}

fn push_lifecycle(
    events: &mut Vec<LifecycleEvent>,
    current_lifecycle: &mut Option<LifecycleEvent>,
    ev: LifecycleEvent,
) {
    *current_lifecycle = Some(ev.clone());
    events.push(ev);
}

impl TranscriptParser for ClaudeParser {
    fn parse_incremental(
        &self,
        path: &Path,
        byte_offset: u64,
    ) -> std::io::Result<(Vec<LifecycleEvent>, u64)> {
        self.parse_incremental_with_context(path, byte_offset, None)
    }

    fn parse_incremental_with_context(
        &self,
        path: &Path,
        byte_offset: u64,
        last_lifecycle: Option<&LifecycleEvent>,
    ) -> std::io::Result<(Vec<LifecycleEvent>, u64)> {
        let mut file = std::fs::File::open(path)?;
        // Truncation guard: Claude rewrites transcripts during compaction.
        // Use fstat on the already-open fd rather than a separate
        // `fs::metadata(path)` syscall.
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
        // Accumulate metadata across all lines in this pass and emit a
        // single MetadataUpdated at the end. Per-line emission would
        // produce one event per assistant line (often dozens per turn),
        // all carrying duplicate model / git_branch / session_title
        // values. The registry's dedup would discard them, but only
        // after thousands of clones per turn.
        let mut meta_model: Option<String> = None;
        let mut meta_git_branch: Option<String> = None;
        // Title sources tracked separately so priority can be applied
        // at emit time: custom > ai > slug-derived.
        let mut meta_title_custom: Option<String> = None;
        let mut meta_title_ai: Option<String> = None;
        let mut meta_title_slug: Option<String> = None;
        let mut meta_current_action: Option<String> = None;
        let mut meta_last_ts: Option<DateTime<Utc>> = None;
        let mut current_lifecycle = last_lifecycle.cloned();

        loop {
            let n = read_bounded_line(&mut reader, &mut buf)?;
            if n == 0 {
                break;
            }
            // Unterminated tail: file is mid-write, leave for next pass.
            if !buf.ends_with('\n') {
                break;
            }
            consumed += n as u64;

            let Ok(line) = serde_json::from_str::<ClaudeLine>(&buf) else {
                continue;
            };
            if line.is_sidechain || line.is_meta {
                continue;
            }
            // Title records (`ai-title`, `custom-title`) omit `timestamp`,
            // so they have to be captured before the timestamp guard
            // below would drop them. They carry no other state-driving
            // signal — just record the title and move on.
            match line.kind {
                LineKind::CustomTitle => {
                    if let Some(t) = line.custom_title.clone() {
                        meta_title_custom = Some(t);
                        meta_last_ts.get_or_insert_with(Utc::now);
                    }
                    continue;
                }
                LineKind::AiTitle => {
                    if let Some(t) = line.ai_title.clone() {
                        meta_title_ai = Some(t);
                        meta_last_ts.get_or_insert_with(Utc::now);
                    }
                    continue;
                }
                _ => {}
            }
            let Some(ts) = line.timestamp else { continue };

            let stop = line.message.as_ref().and_then(|m| m.stop_reason.as_deref());
            let sub = line.subtype.as_deref();
            match (line.kind, stop, sub) {
                // An AskUserQuestion answer is encoded as a
                // `tool_result`, but it clears the outstanding
                // AwaitingUser state: the agent can continue with the
                // answer in hand.
                (LineKind::User, _, _) if is_ask_user_answer(&line) => {
                    push_lifecycle(
                        &mut events,
                        &mut current_lifecycle,
                        LifecycleEvent::TurnStarted { at: ts },
                    );
                    meta_current_action = None;
                    events.push(LifecycleEvent::CurrentActionCleared { at: ts });
                }
                // A user line *with* user-typed text genuinely starts a
                // new turn. Other user lines whose content is only
                // `tool_result` blocks (e.g. Claude returning the
                // outcome of a Bash call) are continuations of the
                // current turn and must not overwrite lifecycle state.
                (LineKind::User, _, _) if has_user_text(line.message.as_ref()) => {
                    push_lifecycle(
                        &mut events,
                        &mut current_lifecycle,
                        LifecycleEvent::TurnStarted { at: ts },
                    );
                }
                // `queue-operation` with `enqueue` is a new user prompt
                // entering the queue. `dequeue` (picked up to process)
                // and `remove` (user pulled it back out) are not
                // turn-starts and must not flip the tile to Active.
                (LineKind::QueueOperation, _, _)
                    if line.operation.as_deref() == Some("enqueue") =>
                {
                    push_lifecycle(
                        &mut events,
                        &mut current_lifecycle,
                        LifecycleEvent::TurnStarted { at: ts },
                    );
                }
                // Canonical assistant turn end.
                (LineKind::Assistant, Some("end_turn"), _) => {
                    push_lifecycle(
                        &mut events,
                        &mut current_lifecycle,
                        LifecycleEvent::TurnEnded { at: ts },
                    );
                }
                // The assistant ended its turn on a tool call. Two
                // distinct cases:
                //   - `AskUserQuestion`: agent is blocked on the user
                //     → `AwaitingUser` (sticky pink state).
                //   - Any other tool: agent is actively executing →
                //     `TurnStarted` (Active).
                // Without the second branch, a session that came out
                // of `AwaitingUser` by calling some *other* tool
                // would never transition off the pink state, because
                // its prior `AwaitingUser` would remain the latest
                // state-driving event in the parser's output.
                (LineKind::Assistant, Some("tool_use"), _) => {
                    let ev = if is_asking_user(line.message.as_ref()) {
                        LifecycleEvent::AwaitingUser { at: ts }
                    } else {
                        LifecycleEvent::TurnStarted { at: ts }
                    };
                    push_lifecycle(&mut events, &mut current_lifecycle, ev);
                }
                // Supplementary marker, sometimes emitted by stop
                // hooks. Suppress when the agent is currently on an
                // `AwaitingUser` — turn_duration can fire alongside an
                // AskUserQuestion and would otherwise overwrite the
                // sticky waiting state with a stale TurnEnded.
                (LineKind::System, _, Some("turn_duration"))
                    if !matches!(current_lifecycle, Some(LifecycleEvent::AwaitingUser { .. })) =>
                {
                    push_lifecycle(
                        &mut events,
                        &mut current_lifecycle,
                        LifecycleEvent::TurnEnded { at: ts },
                    );
                }
                _ => {}
            }

            // Every assistant turn carries fresh usage numbers; emit a
            // `ContextUpdated` so the registry's running view of token
            // pressure stays accurate even between turn boundaries.
            // Claude doesn't surface the model's context window in
            // transcripts, so `max` is `None` and the UI defaults to
            // the documented 200K limit for Sonnet/Opus.
            if line.kind == LineKind::Assistant
                && let Some(msg) = line.message.as_ref()
                && let Some(usage) = msg.usage.as_ref()
            {
                let tokens = usage.input_tokens
                    + usage.cache_read_input_tokens
                    + usage.cache_creation_input_tokens;
                if tokens > 0 {
                    let max = msg.model.as_deref().map(claude_context_window);
                    events.push(LifecycleEvent::ContextUpdated {
                        at: ts,
                        tokens,
                        max,
                    });
                }
            }

            // Metadata side-channel: model, git branch, AI-derived
            // session title, and the most-recent tool invocation.
            // Accumulate the latest value across the pass; the actual
            // event is emitted once after the loop.
            if let Some(m) = line.message.as_ref().and_then(|m| m.model.clone()) {
                meta_model = Some(m);
                meta_last_ts = Some(ts);
            }
            if let Some(b) = line.git_branch.clone() {
                meta_git_branch = Some(b);
                meta_last_ts = Some(ts);
            }
            if let Some(t) = line.slug.as_deref().map(humanise_slug) {
                meta_title_slug = Some(t);
                meta_last_ts = Some(ts);
            }
            if let Some(action) = line
                .message
                .as_ref()
                .filter(|_| line.kind == LineKind::Assistant)
                .and_then(latest_tool_action)
            {
                meta_current_action = Some(action);
                meta_last_ts = Some(ts);
            }
        }
        if let Some(ts) = meta_last_ts {
            let session_title = meta_title_custom.or(meta_title_ai).or(meta_title_slug);
            events.push(LifecycleEvent::MetadataUpdated {
                at: ts,
                model: meta_model,
                git_branch: meta_git_branch,
                session_title,
                current_action: meta_current_action,
            });
        }
        Ok((events, consumed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_transcript(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for l in lines {
            writeln!(tmp.as_file_mut(), "{l}").unwrap();
        }
        tmp.as_file_mut().flush().unwrap();
        tmp
    }

    #[test]
    fn user_marks_turn_started() {
        let tmp = write_transcript(&[
            r#"{"type":"user","timestamp":"2026-05-15T20:16:07.704Z","message":{"role":"user"}}"#,
        ]);
        let (ev, off) = ClaudeParser
            .parse_incremental(tmp.path(), 0)
            .expect("parsed");
        assert_eq!(ev.len(), 1);
        assert!(matches!(ev[0], LifecycleEvent::TurnStarted { .. }));
        assert!(off > 0);
    }

    #[test]
    fn user_line_with_string_content_parses() {
        // Legacy / occasional Claude shape: `content` is a plain
        // string instead of an array of blocks. Pre-fix this failed
        // to deserialize entirely and dropped the lifecycle event.
        let tmp = write_transcript(&[
            r#"{"type":"user","timestamp":"2026-05-15T20:16:07.704Z","message":{"role":"user","content":"hello"}}"#,
        ]);
        let (ev, off) = ClaudeParser
            .parse_incremental(tmp.path(), 0)
            .expect("parsed");
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, LifecycleEvent::TurnStarted { .. }))
                .count(),
            1
        );
        assert!(off > 0);
    }

    #[test]
    fn assistant_tool_use_does_not_emit_turn_end() {
        let tmp = write_transcript(&[
            r#"{"type":"assistant","timestamp":"2026-05-15T20:16:38Z","message":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"assistant","timestamp":"2026-05-15T20:16:50Z","message":{"stop_reason":"end_turn"}}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        // Non-AskUser tool_use → TurnStarted (Active);
        // end_turn → TurnEnded.
        assert!(
            ev.iter()
                .any(|e| matches!(e, LifecycleEvent::TurnStarted { .. })),
            "expected TurnStarted among {ev:?}"
        );
        assert!(
            ev.iter()
                .any(|e| matches!(e, LifecycleEvent::TurnEnded { .. })),
            "expected TurnEnded among {ev:?}"
        );
    }

    #[test]
    fn sidechain_lines_are_ignored() {
        let tmp = write_transcript(&[
            r#"{"type":"user","timestamp":"2026-05-15T20:16:07.704Z","isSidechain":true}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(ev.is_empty());
    }

    #[test]
    fn malformed_lines_skipped() {
        let tmp = write_transcript(&[
            "not json at all",
            r#"{"type":"user","timestamp":"2026-05-15T20:16:07Z"}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert_eq!(ev.len(), 1);
    }

    #[test]
    fn incremental_resume_from_offset() {
        let tmp = write_transcript(&[
            r#"{"type":"user","timestamp":"2026-05-15T20:16:07Z"}"#,
            r#"{"type":"assistant","timestamp":"2026-05-15T20:16:38Z","message":{"stop_reason":"end_turn"}}"#,
        ]);
        let (ev1, off1) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        let (ev2, off2) = ClaudeParser.parse_incremental(tmp.path(), off1).unwrap();
        assert_eq!(ev1.len() + ev2.len(), 2);
        assert_eq!(off2, off1.max(off2));
    }

    #[test]
    fn turn_duration_system_line_ends_turn() {
        let tmp = write_transcript(&[
            r#"{"type":"system","subtype":"turn_duration","timestamp":"2026-05-15T20:16:51.666Z","durationMs":43894}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert_eq!(ev.len(), 1);
        assert!(matches!(ev[0], LifecycleEvent::TurnEnded { .. }));
    }

    #[test]
    fn last_prompt_is_not_a_turn_start() {
        // `last-prompt` is a state snapshot rewritten every time the
        // user's pending prompt changes; per the transcript census it
        // never carries `timestamp`. The same prompt is already
        // captured by `queue-operation`/`enqueue` and the eventual
        // `user` line, both timestamped, so `last-prompt` itself must
        // not synthesise turn-starts.
        let tmp = write_transcript(&[r#"{"type":"last-prompt","leafUuid":"x","sessionId":"y"}"#]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(ev.is_empty());
    }

    #[test]
    fn queue_enqueue_starts_turn() {
        let tmp = write_transcript(&[
            r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-05-15T20:16:07Z","content":"hi"}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(
            ev.iter()
                .any(|e| matches!(e, LifecycleEvent::TurnStarted { .. }))
        );
    }

    #[test]
    fn queue_dequeue_does_not_start_turn() {
        let tmp = write_transcript(&[
            r#"{"type":"queue-operation","operation":"dequeue","timestamp":"2026-05-15T20:16:07Z"}"#,
            r#"{"type":"queue-operation","operation":"remove","timestamp":"2026-05-15T20:16:08Z"}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(
            !ev.iter()
                .any(|e| matches!(e, LifecycleEvent::TurnStarted { .. })),
            "non-enqueue queue-operations must not produce TurnStarted ({ev:?})"
        );
    }

    #[test]
    fn is_meta_user_line_is_skipped() {
        // Harness-synthesised user lines (wake-ups, recheck nudges)
        // carry `isMeta: true` and must not produce TurnStarted.
        let tmp = write_transcript(&[
            r#"{"type":"user","timestamp":"2026-05-15T20:16:07Z","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"recheck X"}]}}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(
            !ev.iter()
                .any(|e| matches!(e, LifecycleEvent::TurnStarted { .. })),
            "isMeta user lines must not produce TurnStarted ({ev:?})"
        );
    }

    #[test]
    fn custom_title_overrides_ai_title_and_slug() {
        // ai-title and custom-title both omit `timestamp` per the
        // transcript census; the parser captures them before the
        // timestamp guard. Custom wins over AI which wins over the
        // assistant-slug fallback.
        let tmp = write_transcript(&[
            r#"{"type":"ai-title","aiTitle":"AI Title","sessionId":"y"}"#,
            r#"{"type":"custom-title","customTitle":"User Title","sessionId":"y"}"#,
            r#"{"type":"assistant","timestamp":"2026-05-15T20:16:50Z","slug":"slug-fallback","message":{"model":"claude-opus-4-7","stop_reason":"end_turn","content":[]}}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        let title = ev.iter().find_map(|e| match e {
            LifecycleEvent::MetadataUpdated { session_title, .. } => session_title.clone(),
            _ => None,
        });
        assert_eq!(title.as_deref(), Some("User Title"));
    }

    #[test]
    fn ai_title_overrides_slug_when_no_custom_title() {
        let tmp = write_transcript(&[
            r#"{"type":"ai-title","aiTitle":"AI Title","sessionId":"y"}"#,
            r#"{"type":"assistant","timestamp":"2026-05-15T20:16:50Z","slug":"slug-fallback","message":{"model":"claude-opus-4-7","stop_reason":"end_turn","content":[]}}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        let title = ev.iter().find_map(|e| match e {
            LifecycleEvent::MetadataUpdated { session_title, .. } => session_title.clone(),
            _ => None,
        });
        assert_eq!(title.as_deref(), Some("AI Title"));
    }

    #[test]
    fn turn_duration_after_prior_awaiting_user_is_suppressed() {
        let first_ts = "2026-05-15T20:16:50Z";
        let second = r#"{"type":"system","subtype":"turn_duration","timestamp":"2026-05-15T20:16:51.666Z","durationMs":43894}"#;
        let first = r#"{"type":"assistant","timestamp":"2026-05-15T20:16:50Z","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"AskUserQuestion","input":{}}]}}"#;
        let tmp = write_transcript(&[first, second]);
        let offset = first.len() as u64 + 1;
        let prior = LifecycleEvent::AwaitingUser {
            at: first_ts.parse().unwrap(),
        };

        let (ev, _) = ClaudeParser
            .parse_incremental_with_context(tmp.path(), offset, Some(&prior))
            .unwrap();

        assert!(
            !ev.iter()
                .any(|e| matches!(e, LifecycleEvent::TurnEnded { .. })),
            "turn_duration should not end a turn while awaiting user ({ev:?})"
        );
    }

    #[test]
    fn opus_4_7_context_window_is_one_million() {
        assert_eq!(claude_context_window("claude-opus-4-7-20250513"), 1_000_000);
        assert_eq!(claude_context_window("claude-opus-4-7"), 1_000_000);
    }

    #[test]
    fn other_claude_models_default_to_200k() {
        assert_eq!(claude_context_window("claude-sonnet-4-6"), 200_000);
        assert_eq!(claude_context_window("claude-sonnet-4-5"), 200_000);
        assert_eq!(claude_context_window("claude-haiku-4-5"), 200_000);
        assert_eq!(claude_context_window("claude-opus-4-6"), 200_000);
        assert_eq!(claude_context_window("future-model-3-0"), 200_000);
    }

    #[test]
    fn context_update_carries_model_max() {
        let tmp = write_transcript(&[
            r#"{"type":"assistant","timestamp":"2026-05-15T20:16:50Z","message":{"model":"claude-opus-4-7","stop_reason":"end_turn","usage":{"input_tokens":100,"cache_read_input_tokens":50000,"cache_creation_input_tokens":0,"output_tokens":42}}}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        // Two events: TurnEnded (end_turn) + ContextUpdated.
        let ctx = ev.iter().find_map(|e| match e {
            LifecycleEvent::ContextUpdated { tokens, max, .. } => Some((*tokens, *max)),
            _ => None,
        });
        assert_eq!(ctx, Some((50_100, Some(1_000_000))));
    }

    #[test]
    fn ask_user_question_tool_use_emits_awaiting_user() {
        let tmp = write_transcript(&[
            r#"{"type":"assistant","timestamp":"2026-05-15T20:16:50Z","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"AskUserQuestion","input":{}}]}}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(
            ev.iter()
                .any(|e| matches!(e, LifecycleEvent::AwaitingUser { .. })),
            "expected AwaitingUser among {ev:?}"
        );
    }

    #[test]
    fn non_ask_user_tool_use_does_not_emit_awaiting_user() {
        let tmp = write_transcript(&[
            r#"{"type":"assistant","timestamp":"2026-05-15T20:16:50Z","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(
            !ev.iter()
                .any(|e| matches!(e, LifecycleEvent::AwaitingUser { .. })),
            "did not expect AwaitingUser among {ev:?}"
        );
    }

    #[test]
    fn user_tool_result_does_not_emit_turn_started() {
        let tmp = write_transcript(&[
            r#"{"type":"user","timestamp":"2026-05-15T20:17:00Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_x","content":[{"type":"text","text":"option A"}]}]}}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(
            !ev.iter()
                .any(|e| matches!(e, LifecycleEvent::TurnStarted { .. })),
            "tool_result-only user lines must not start a new turn ({ev:?})"
        );
    }

    #[test]
    fn ask_user_question_answer_emits_turn_started() {
        let tmp = write_transcript(&[
            r#"{"type":"assistant","timestamp":"2026-05-15T20:00:00Z","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_x","name":"AskUserQuestion","input":{}}]}}"#,
            r#"{"type":"user","timestamp":"2026-05-15T20:00:30Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_x","content":"User has answered your questions: \"choose\"=\"A\". You can now continue with the user's answers in mind."}]},"toolUseResult":{"questions":[{"question":"choose","options":[{"label":"A","description":"A"}],"multiSelect":false}],"answers":{"choose":"A"}}}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        let last_state = ev.iter().rev().find(|e| {
            !matches!(
                e,
                LifecycleEvent::ContextUpdated { .. }
                    | LifecycleEvent::MetadataUpdated { .. }
                    | LifecycleEvent::CurrentActionCleared { .. }
            )
        });
        assert!(
            matches!(last_state, Some(LifecycleEvent::TurnStarted { .. })),
            "expected AskUserQuestion answer to clear AwaitingUser, got {last_state:?}"
        );
        assert!(
            ev.iter()
                .any(|e| matches!(e, LifecycleEvent::CurrentActionCleared { .. })),
            "expected AskUserQuestion answer to clear current_action ({ev:?})"
        );
    }

    #[test]
    fn normal_tool_result_metadata_does_not_emit_turn_started() {
        let tmp = write_transcript(&[
            r#"{"type":"user","timestamp":"2026-05-15T20:17:00Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_x","content":"stdout"}]},"toolUseResult":{"stdout":"stdout","stderr":"","interrupted":false}}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(
            !ev.iter()
                .any(|e| matches!(e, LifecycleEvent::TurnStarted { .. })),
            "normal tool result metadata must not start a turn ({ev:?})"
        );
    }

    #[test]
    fn non_ask_user_tool_use_emits_turn_started() {
        // Regression: a non-AskUserQuestion tool_use after an
        // AwaitingUser must drive the state back out of pink. The
        // sequence below is what Claude writes when the user answers
        // a question and the agent immediately starts running Bash.
        let tmp = write_transcript(&[
            r#"{"type":"assistant","timestamp":"2026-05-15T20:00:00Z","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"AskUserQuestion","input":{}}]}}"#,
            r#"{"type":"user","timestamp":"2026-05-15T20:00:30Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_x","content":[]}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-05-15T20:00:45Z","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        // The latest state-driving event should be TurnStarted.
        let last_state = ev.iter().rev().find(|e| {
            !matches!(
                e,
                LifecycleEvent::ContextUpdated { .. }
                    | LifecycleEvent::MetadataUpdated { .. }
                    | LifecycleEvent::CurrentActionCleared { .. }
            )
        });
        assert!(
            matches!(last_state, Some(LifecycleEvent::TurnStarted { .. })),
            "expected TurnStarted after non-AskUser tool_use, got {last_state:?}"
        );
    }

    #[test]
    fn user_text_does_emit_turn_started() {
        let tmp = write_transcript(&[
            r#"{"type":"user","timestamp":"2026-05-15T20:17:00Z","message":{"role":"user","content":[{"type":"text","text":"please do X"}]}}"#,
        ]);
        let (ev, _) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(
            ev.iter()
                .any(|e| matches!(e, LifecycleEvent::TurnStarted { .. })),
            "user text line should start a turn ({ev:?})"
        );
    }

    #[test]
    fn unterminated_tail_left_for_next_pass() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.as_file_mut()
            .write_all(br#"{"type":"user","timestamp":"2026-05-15T20:16:07Z"}"#)
            .unwrap();
        // No trailing newline.
        tmp.as_file_mut().flush().unwrap();
        let (ev, off) = ClaudeParser.parse_incremental(tmp.path(), 0).unwrap();
        assert!(ev.is_empty());
        assert_eq!(off, 0, "offset must not advance past an unterminated line");
    }
}
