// JSONL trace tail with cursor + typed event parsing.
//
// The corral writes one TraceEvent per line via FileSink (see
// corral_core::trace). This reader polls the file's size and reads new
// bytes since the last cursor; lines that end with \n are decoded as
// JSON. A partial trailing line is held back until its newline arrives
// on a later poll so we never split a record mid-token.

import { promises as fs } from "node:fs";

export type AgentKind = "claude" | "codex-cli" | "codex-app-server";
export type BindingSource =
  | "session-record"
  | "argv-env"
  | "mtime-fallback"
  | "open-fd";
export type FocusStrategy = "tty" | "pid" | "cwd" | "generic";
export type FocusResult = "ok" | "not-found" | "unavailable" | "permission-denied";
export type ExitSource = "kqueue" | "discovery-reconcile";
export type LifecycleKind = "turn-started" | "turn-ended" | "awaiting-user";

export type TraceEvent =
  | { kind: "discovery-pass-started" }
  | { kind: "discovery-pass-completed"; agent_count: number }
  | {
      kind: "agent-discovered";
      pid: number;
      process_start_ms: number | null;
      agent: AgentKind;
      transcript: string;
      session_id: string;
      cwd: string | null;
      binding_source: BindingSource;
    }
  | {
      kind: "agent-rebound";
      pid: number;
      old_transcript: string;
      new_transcript: string;
      new_session_id: string;
    }
  | { kind: "subagent-rollup-changed"; pid: number; count: number }
  | { kind: "process-exited"; pid: number; via: ExitSource }
  | {
      kind: "transcript-parsed";
      pid: number;
      lifecycle: LifecycleKind | null;
      metadata_changed: boolean;
    }
  | {
      kind: "ghostty-caps-probed";
      has_tty: boolean;
      has_pid: boolean;
      has_cwd: boolean;
    }
  | { kind: "focus-requested"; pid: number; request_id: number }
  | {
      kind: "focus-dispatched";
      request_id: number;
      adapter: string | null;
      strategy: FocusStrategy | null;
      result: FocusResult;
      focused_target_id: string | null;
    };

export interface ParsedLine {
  schema: string;
  ts: string; // RFC3339 UTC
  event: TraceEvent;
}

export interface WaitOptions {
  from?: number; // byte offset to start scanning at (defaults to 0)
  timeoutMs?: number;
}

const POLL_MS = 25;

/// Yields ParsedLine values as the corral appends them. Tracks the
/// byte cursor so consumers can replay from a known starting point.
export class TraceReader {
  private cursorOffset = 0;
  private buffered = "";
  /// History of every line parsed so far. Cheap (a few hundred lines
  /// per scenario) and lets `waitFor({ from: ... })` look back over
  /// the trace without re-reading the file.
  private history: Array<{ offset: number; line: ParsedLine }> = [];

  constructor(private readonly path: string) {}

  cursor(): number {
    return this.cursorOffset;
  }

  /// Read all currently-available lines and append them to history.
  /// Returns the lines parsed during this call (may be empty).
  async pump(): Promise<ParsedLine[]> {
    let stat;
    try {
      stat = await fs.stat(this.path);
    } catch {
      return [];
    }
    if (stat.size <= this.cursorOffset) {
      return [];
    }
    const fd = await fs.open(this.path, "r");
    try {
      const len = stat.size - this.cursorOffset;
      const buf = Buffer.alloc(len);
      await fd.read(buf, 0, len, this.cursorOffset);
      this.cursorOffset = stat.size;
      this.buffered += buf.toString("utf-8");
    } finally {
      await fd.close();
    }

    const parsed: ParsedLine[] = [];
    while (true) {
      const nl = this.buffered.indexOf("\n");
      if (nl === -1) break;
      const raw = this.buffered.slice(0, nl);
      this.buffered = this.buffered.slice(nl + 1);
      if (raw.trim().length === 0) continue;
      try {
        const obj = JSON.parse(raw);
        const line: ParsedLine = {
          schema: obj.schema,
          ts: obj.ts,
          // Internal-tag flattening: the rest of `obj` is the event.
          event: obj as TraceEvent,
        };
        parsed.push(line);
        this.history.push({ offset: this.cursorOffset, line });
      } catch (e) {
        // A malformed line is a corral bug; surface it loudly rather
        // than silently dropping data.
        throw new Error(
          `trace: failed to parse line ${JSON.stringify(raw)}: ${e}`,
        );
      }
    }
    return parsed;
  }

  /// Wait until a matching event lands. Looks back through history
  /// from `opts.from` (default 0) before polling for new events.
  async waitFor(
    predicate: (e: TraceEvent) => boolean,
    opts: WaitOptions = {},
  ): Promise<TraceEvent> {
    const from = opts.from ?? 0;
    const timeoutMs = opts.timeoutMs ?? 5_000;
    const deadline = Date.now() + timeoutMs;

    const findInHistory = () => {
      for (const entry of this.history) {
        if (entry.offset >= from && predicate(entry.line.event)) {
          return entry.line.event;
        }
      }
      return undefined;
    };

    const initial = findInHistory();
    if (initial) return initial;

    while (Date.now() < deadline) {
      await this.pump();
      const hit = findInHistory();
      if (hit) return hit;
      await sleep(POLL_MS);
    }
    throw new Error("trace: waitFor timed out");
  }

  /// Assert an ordered sequence of events. Each predicate must match a
  /// later event than the previous one's offset. Returns the matched
  /// events in order. Useful for scenario assertions like:
  ///   expectOrder([isStartFor(pid), isParseFor(pid), isCompletedFor(pid)])
  async expectOrder(
    predicates: Array<(e: TraceEvent) => boolean>,
    opts: WaitOptions = {},
  ): Promise<TraceEvent[]> {
    const out: TraceEvent[] = [];
    let from = opts.from ?? 0;
    for (const p of predicates) {
      const event = await this.waitFor(p, { from, timeoutMs: opts.timeoutMs });
      out.push(event);
      // Advance `from` past the matched event so the next predicate
      // starts looking after it. We find the matched history entry
      // by identity and use its offset + 1 as the next cursor.
      for (const entry of this.history) {
        if (entry.line.event === event) {
          from = entry.offset + 1;
          break;
        }
      }
    }
    return out;
  }

  /// Assert that no event matching `predicate` lands within
  /// `withinMs`. Cheaper sibling of `waitFor`.
  async expectNoEvent(
    predicate: (e: TraceEvent) => boolean,
    withinMs: number,
  ): Promise<void> {
    const deadline = Date.now() + withinMs;
    while (Date.now() < deadline) {
      await this.pump();
      for (const entry of this.history) {
        if (predicate(entry.line.event)) {
          throw new Error(
            `trace: expectNoEvent saw a match: ${JSON.stringify(entry.line.event)}`,
          );
        }
      }
      await sleep(POLL_MS);
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
