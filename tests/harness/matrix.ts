// Scenario × cell matrix expansion.
//
// Each scenario declares which axes vary; the matrix multiplies the
// scenario by the registered agents/terminals along those axes and
// then prunes via the scenario's `applies` predicate. The result is a
// flat list of (scenario, cell) pairs that the runner walks
// sequentially.

import type { AgentBinding } from "../agents/types.ts";
import type { TerminalBinding } from "../terminals/types.ts";

import type { Scenario, ScenarioCell } from "./scenario.ts";

export interface MatrixOptions {
  agents: AgentBinding[];
  terminals: TerminalBinding[];
  /// Filter on scenario names. Each cell whose scenario name doesn't
  /// `includes(pattern)` is dropped.
  grep?: string;
  /// Deterministic shuffle of the resulting plan. Same seed → same
  /// order so a flake reproduces.
  shuffleSeed?: number;
}

export interface PlannedRun {
  scenario: Scenario;
  cell: ScenarioCell;
}

export function buildPlan(
  scenarios: Scenario[],
  opts: MatrixOptions,
): PlannedRun[] {
  const filtered = opts.grep
    ? scenarios.filter((s) => s.name.includes(opts.grep!))
    : scenarios;

  const defaultAgent = opts.agents[0] ?? null;
  const defaultTerminal = opts.terminals[0] ?? null;
  const plan: PlannedRun[] = [];
  for (const scenario of filtered) {
    const axis = scenario.varies ?? "none";
    const agents =
      axis === "agent" || axis === "agent+terminal" ? opts.agents : [defaultAgent];
    const terminals =
      axis === "terminal" || axis === "agent+terminal" ? opts.terminals : [defaultTerminal];
    for (const agent of agents) {
      for (const terminal of terminals) {
        const cell: ScenarioCell = { agent, terminal };
        if (scenario.applies && !scenario.applies(cell)) continue;
        plan.push({ scenario, cell });
      }
    }
  }

  if (opts.shuffleSeed !== undefined) {
    shuffle(plan, opts.shuffleSeed);
  }
  return plan;
}

/// xorshift32 — deterministic, dependency-free, good enough for
/// scenario-order randomisation. Same seed reproduces the same order.
function shuffle<T>(items: T[], seed: number): void {
  let state = seed | 0 || 1;
  const next = () => {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    return (state >>> 0) / 0x1_0000_0000;
  };
  for (let i = items.length - 1; i > 0; i--) {
    const j = Math.floor(next() * (i + 1));
    [items[i], items[j]] = [items[j], items[i]];
  }
}
