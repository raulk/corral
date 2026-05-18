// Scenario contract.
//
// A scenario is a single named experiment the matrix can fan out across
// agents × terminals. `varies` declares which axes the matrix should
// vary; `applies` filters out cells that don't make sense for this
// scenario; `requiresFreshCorral` forces a per-scenario corral boot
// when the scenario can't tolerate shared state (e.g.
// `focus/capability-cold-start` needs Ghostty NOT running at corral
// boot).

import type { AgentBinding } from "../agents/types.ts";
import type { TerminalBinding } from "../terminals/types.ts";

import type { ControlClient } from "./control.ts";
import type { TraceReader } from "./trace.ts";

export type VariesAxis = "none" | "agent" | "terminal" | "agent+terminal";

export interface ScenarioCell {
  agent: AgentBinding | null;
  terminal: TerminalBinding | null;
}

export interface ScenarioContext extends ScenarioCell {
  control: ControlClient;
  trace: TraceReader;
  cleanup: CleanupHandle;
}

/// Per-scenario cleanup ledger. Scenarios register agents and tabs
/// they spawn so the harness can tear them down in reverse order plus
/// quiesce the registry afterwards.
export interface CleanupHandle {
  registerProcess(pid: number, kill: () => Promise<void>): void;
  registerTab(close: () => Promise<void>): void;
}

export interface Scenario {
  name: string;
  varies?: VariesAxis;
  applies?: (cell: ScenarioCell) => boolean;
  requiresFreshCorral?: boolean;
  run(ctx: ScenarioContext): Promise<void>;
}
