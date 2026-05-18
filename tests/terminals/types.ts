// Terminal binding contract.
//
// `TabHandle` is intentionally opaque: a Ghostty tab id is a string, an
// iTerm session id is a string, Terminal.app's tab is an ordinal — let
// each binding pick its representation as long as it round-trips
// through `focusedTargetId()` for the focus/tab-focus scenario.

export interface TerminalCapabilities {
  exposesTty: boolean;
  exposesPid: boolean;
  exposesCwd: boolean;
  /// Adapter can read the focused tab id back through scripting.
  /// Required for the `focus/tab-focus` scenario.
  canReportFocusedTargetId: boolean;
}

export interface TabHandle {
  /// Terminal-side identifier of this tab (Ghostty: `id of terminal`,
  /// iTerm: session id, Terminal.app: tab index). Compared against
  /// `focusedTargetId()` to assert focus actually landed here.
  id: string;
  close(): Promise<void>;
}

export interface OpenTabArgs {
  cwd: string;
  command: string;
}

export interface TerminalBinding {
  readonly name: "ghostty" | "iterm2" | "terminal-app";
  readonly bundleId: string;
  readonly capabilities: TerminalCapabilities;
  openTab(args: OpenTabArgs): Promise<TabHandle>;
  focusedTargetId(): Promise<string | null>;
  ensureRunning(): Promise<void>;
  ensureNotRunning(): Promise<void>;
  /// Paste-style text injection into the terminal's pty. Does not
  /// steal keyboard focus; bytes go straight to the program running
  /// inside the tab. Most terminal emulators expose this — Ghostty's
  /// `input text "..." to t` verb is the canonical example.
  inputText(tabId: string, text: string): Promise<void>;
  /// Send a named key to the terminal (Enter, Tab, etc.). Names map
  /// to the underlying terminal's key vocabulary (Ghostty:
  /// `send key "enter" to t`).
  sendKey(tabId: string, key: "enter"): Promise<void>;
  /// Release any harness-owned resources (windows, scratch state).
  /// Called from `main.ts` after all scenarios run. Optional because
  /// some bindings (e.g. iTerm2) attach to user-owned windows and
  /// have nothing to release.
  dispose?(): Promise<void>;
}
