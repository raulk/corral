// GhosttyTerminal — drives Ghostty via osascript.
//
// Uses Ghostty's `new window` / `new tab` AppleScript verbs with a
// surface-configuration record carrying initial working directory and
// command. The window's first surface id is the `TabHandle.id` — same
// id Ghostty returns from `focused terminal of selected tab of front
// window`, so focus readbacks compare against this string directly.

import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type {
  OpenTabArgs,
  TabHandle,
  TerminalBinding,
  TerminalCapabilities,
} from "./types.ts";

const BUNDLE_ID = "com.mitchellh.ghostty";

/// Path used to remember the harness's Ghostty window across `bun run
/// tests` invocations. The first invocation creates the window and
/// writes its id; subsequent invocations reuse the same window if it
/// still exists. The user can close the window manually to reset.
const WINDOW_ID_FILE = join(tmpdir(), "corral-harness-ghostty-window-id");

export class GhosttyTerminal implements TerminalBinding {
  readonly name = "ghostty" as const;
  readonly bundleId = BUNDLE_ID;
  readonly capabilities: TerminalCapabilities;
  /// Id of the harness-owned Ghostty window. Lazily created on the
  /// first `openTab` call; all subsequent tabs land inside it so the
  /// harness occupies exactly one window of the user's screen.
  private harnessWindowId: string | null = null;

  constructor() {
    this.capabilities = probeCapabilities();
    this.harnessWindowId = loadPersistedWindowId();
  }

  /// Open a tab inside the harness window, creating that window on
  /// first use. `tab` vs `window` doesn't matter for the corral —
  /// both surface as a `terminal` in Ghostty's enumeration with a
  /// stable id — and one window keeps the user's screen clean.
  ///
  /// `new window` / `new tab` activate Ghostty as a side-effect; we
  /// capture the frontmost app's bundle id beforehand and re-activate
  /// it right after so the user's keyboard focus is restored.
  async openTab(args: OpenTabArgs): Promise<TabHandle> {
    const cwd = appleString(args.cwd);
    const command = appleString(args.command);

    // Check whether the harness window still exists. Ghostty closes
    // the window when its last tab closes, leaving a stale id behind;
    // recreate transparently if so.
    if (this.harnessWindowId && !this.windowExists(this.harnessWindowId)) {
      this.harnessWindowId = null;
    }

    const frontBundle = frontmostBundleId();
    try {
      if (!this.harnessWindowId) {
        const out = osascript(`
          tell application "Ghostty"
            set w to new window with configuration {initial working directory:${cwd}, command:${command}, wait after command:true}
            delay 0.4
            return (id of w as string) & "|" & (id of focused terminal of selected tab of w as string)
          end tell
        `);
        const [windowId, terminalId] = parseIdPair(out);
        this.harnessWindowId = windowId;
        persistWindowId(windowId);
        return makeTabHandle(terminalId);
      }

      const winId = appleString(this.harnessWindowId);
      const out = osascript(`
        tell application "Ghostty"
          set w to first window whose id is ${winId}
          set t to new tab in w with configuration {initial working directory:${cwd}, command:${command}, wait after command:true}
          delay 0.4
          return (id of t as string) & "|" & (id of focused terminal of t as string)
        end tell
      `);
      const [_, terminalId] = parseIdPair(out);
      return makeTabHandle(terminalId);
    } finally {
      if (frontBundle && frontBundle !== BUNDLE_ID) {
        restoreFrontmost(frontBundle);
      }
    }
  }

  private windowExists(id: string): boolean {
    const out = osascript(`
      tell application "Ghostty"
        if (count of (every window whose id is ${appleString(id)})) > 0 then
          return "yes"
        else
          return "no"
        end if
      end tell
    `);
    return out === "yes";
  }

  /// No-op: we deliberately leave the harness window open across
  /// `bun run tests` invocations so the next run rebinds to the same
  /// window via `${WINDOW_ID_FILE}`. The user can close it manually
  /// once they're done iterating.
  async dispose(): Promise<void> {}

  async focusedTargetId(): Promise<string | null> {
    const script = `
      tell application "Ghostty"
        if (count of windows) = 0 then return ""
        try
          return id of focused terminal of selected tab of front window
        on error
          return ""
        end try
      end tell
    `;
    const out = osascript(script);
    return out.length > 0 ? out : null;
  }

  async ensureRunning(): Promise<void> {
    osascript('tell application "Ghostty" to activate');
  }

  async ensureNotRunning(): Promise<void> {
    osascript('tell application "Ghostty" to quit');
  }

  async inputText(tabId: string, text: string): Promise<void> {
    osascriptStrict(`
      tell application "Ghostty"
        set t to first terminal whose id is ${appleString(tabId)}
        input text ${appleString(text)} to t
      end tell
    `);
  }

  async sendKey(tabId: string, key: "enter"): Promise<void> {
    osascriptStrict(`
      tell application "Ghostty"
        set t to first terminal whose id is ${appleString(tabId)}
        send key ${appleString(key)} to t
      end tell
    `);
  }
}

/// Return the bundle id of the frontmost macOS application, or `null`
/// when the lookup fails. Used to restore focus after Ghostty steals
/// it during `new window` / `new tab`.
function frontmostBundleId(): string | null {
  const out = osascript(`
    tell application "System Events"
      try
        return bundle identifier of first application process whose frontmost is true
      on error
        return ""
      end try
    end tell
  `);
  return out.length > 0 ? out : null;
}

function restoreFrontmost(bundleId: string): void {
  osascript(`tell application id ${appleString(bundleId)} to activate`);
}

function loadPersistedWindowId(): string | null {
  if (!existsSync(WINDOW_ID_FILE)) return null;
  try {
    const id = readFileSync(WINDOW_ID_FILE, "utf-8").trim();
    return id.length > 0 ? id : null;
  } catch {
    return null;
  }
}

function persistWindowId(id: string): void {
  try {
    writeFileSync(WINDOW_ID_FILE, id, "utf-8");
  } catch {
    // Persistence is a best-effort optimisation; on failure the next
    // run will simply create a new window.
  }
}

/// Parse `<winOrTabId>|<terminalId>` from the AppleScript helper output.
function parseIdPair(out: string): [string, string] {
  if (!out) throw new Error("ghostty: openTab returned no id");
  const [a, b] = out.split("|");
  if (!a || !b) throw new Error(`ghostty: malformed id pair: ${out}`);
  return [a, b];
}

/// Build a `TabHandle` whose `close` is a no-op. Tabs accumulate
/// inside the single harness window during a run; closing them
/// mid-run risks tearing the window down (Ghostty closes a window
/// when its last tab goes away) which then forces the next scenario
/// to open a fresh window. `dispose()` at the end of `main.ts` tears
/// the whole window down — that's where the cleanup actually happens.
function makeTabHandle(terminalId: string): TabHandle {
  return {
    id: terminalId,
    close: async () => {},
  };
}

/// Run an osascript snippet and surface stderr as a thrown error. Used
/// by paths where a silent failure would mask a real bug (input
/// injection, key send). The original `osascript` helper above stays
/// for probe paths where "no answer" is a valid outcome.
function osascriptStrict(script: string): string {
  const out = spawnSync("/usr/bin/osascript", ["-e", script], {
    encoding: "utf-8",
  });
  if (out.status !== 0) {
    throw new Error(`osascript failed: ${out.stderr.trim()}`);
  }
  return out.stdout.trim();
}

/// One-shot probe that mirrors `crates/corral-adapters/src/ghostty.rs`.
/// We compute the bag locally rather than reading the corral's
/// `ghostty-caps-probed` trace event so the harness can decide whether
/// to even spawn the corral.
function probeCapabilities(): TerminalCapabilities {
  const out = osascript(`
    tell application "Ghostty"
      set terms to terminals
      if (count of terms) = 0 then return "empty"
      set caps to ""
      try
        set _t to tty of item 1 of terms
        set caps to caps & "tty,"
      end try
      try
        set _p to pid of item 1 of terms
        set caps to caps & "pid,"
      end try
      try
        set _c to working directory of item 1 of terms
        set caps to caps & "cwd,"
      end try
      try
        set _i to id of item 1 of terms
        set caps to caps & "id,"
      end try
      return caps
    end tell
  `);
  if (out === "empty" || out.length === 0) {
    // Without any open terminals we can't probe; assume the modern
    // shape (tty + pid + cwd + id). The corral's own probe runs
    // lazily and is the source of truth.
    return {
      exposesTty: true,
      exposesPid: true,
      exposesCwd: true,
      canReportFocusedTargetId: true,
    };
  }
  return {
    exposesTty: out.includes("tty"),
    exposesPid: out.includes("pid"),
    exposesCwd: out.includes("cwd"),
    canReportFocusedTargetId: out.includes("id"),
  };
}

function osascript(script: string): string {
  const out = spawnSync("/usr/bin/osascript", ["-e", script], {
    encoding: "utf-8",
  });
  if (out.status !== 0) {
    // Don't throw on Automation-permission errors during probe — let
    // the TCC preflight surface them with a useful hint. Empty string
    // is a safe sentinel: the caller falls back to defaults.
    return "";
  }
  return out.stdout.trim();
}

/// Quote a string for use as an AppleScript literal. Mirrors
/// `applescript_string` on the Rust side: escape backslash + quote,
/// drop CR/LF (they'd terminate the literal).
function appleString(s: string): string {
  let out = '"';
  for (const c of s) {
    if (c === '"' || c === "\\") out += "\\" + c;
    else if (c !== "\r" && c !== "\n") out += c;
  }
  return out + '"';
}
