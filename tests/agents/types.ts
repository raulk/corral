// Agent binding contract.
//
// Each agent the harness can spawn exposes a probed capability bag plus
// the operations the matrix needs (spawn, drive `/clear`, drive
// `/resume`). The capability bag is filled in at construction time by
// invoking the agent's `--version` and inspecting the binary; nothing
// here calls into the running agent.

import type { TerminalBinding, TabHandle } from "../terminals/types.ts";

export interface AgentCapabilities {
  /// Driveable `/clear` slash command. Needed for the
  /// `binding/clear-rebind` scenario.
  driveableClear: boolean;
  /// Driveable `/resume` slash command.
  driveableResume: boolean;
  /// Writes `~/.claude/sessions/<pid>.json` on startup.
  writesSessionRecord: boolean;
  /// Pre-2.1.140 builds omit `startedAt` from the session record.
  recordHasStartedAt: boolean;
  /// Agent can run without a controlling terminal (Codex app-server).
  runsWithoutTerminal: boolean;
}

export interface SpawnArgs {
  cwd: string;
  sessionId?: string;
  /// Terminal to host the agent (`null` for runsWithoutTerminal agents).
  terminal: TerminalBinding | null;
  /// Extra environment for the spawned process. The agent binding
  /// merges this on top of its own defaults.
  env?: Record<string, string>;
  /// Override the binding's default model. Scenarios that need
  /// reliable tool-calling (e.g. AskUserQuestion) should pass a more
  /// capable model than the harness's haiku default.
  model?: string;
}

export interface AgentProcess {
  pid: number;
  /// Tab handle the agent runs in; `null` when `runsWithoutTerminal`.
  tab: TabHandle | null;
  /// Terminal binding paired with this agent. Carried on the process
  /// so `driveClear` and similar slash-command drivers can route
  /// input through `terminal.inputText` / `terminal.sendKey` without
  /// the scenario plumbing it through.
  terminal: TerminalBinding | null;
  kill(): Promise<void>;
  /// Resolves when the OS reaps the process. The harness waits on
  /// the registry's `process-exited` trace event for the registry
  /// view; this is just the kernel view.
  exited: Promise<number | null>;
}

export interface AgentBinding {
  readonly name: "claude-cli" | "codex-cli" | "codex-app-server";
  readonly binaryPath: string;
  readonly probedVersion: string | null;
  readonly capabilities: AgentCapabilities;
  spawn(args: SpawnArgs): Promise<AgentProcess>;
  driveClear?(proc: AgentProcess): Promise<void>;
  driveResume?(proc: AgentProcess, sessionId: string): Promise<void>;
}
