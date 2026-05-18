// Quiescence barrier.
//
// Asks the corral to run a discovery pass, then waits for the matching
// `discovery-pass-completed` trace event from the cursor we captured
// before the ask. Used both as the end-of-scenario cleanup contract and
// as a checkpoint inside scenarios that need the registry to settle.

import type { ControlClient } from "./control.ts";
import type { TraceReader } from "./trace.ts";

export interface QuiesceOptions {
  timeoutMs?: number;
}

export async function quiesce(
  control: ControlClient,
  trace: TraceReader,
  opts: QuiesceOptions = {},
): Promise<void> {
  const from = trace.cursor();
  await control.discoverNow();
  await trace.waitFor(
    (e) => e.kind === "discovery-pass-completed",
    { from, timeoutMs: opts.timeoutMs ?? 3_000 },
  );
}
