// Helpers that nudge `discover-now` on a polling loop while waiting
// for a trace predicate to match. Useful when Claude takes a little
// while to write its session record after its first prompt — a single
// `discover-now` followed by a 5s wait can miss the window.

import type { ScenarioContext } from "./scenario.ts";
import type { TraceEvent } from "./trace.ts";

export async function waitForDiscoveredBySessionId(
  ctx: ScenarioContext,
  sessionId: string,
  from: number,
  timeoutMs: number,
): Promise<TraceEvent & { kind: "agent-discovered" }> {
  return waitWithDiscoverPoll(
    ctx,
    (e) => e.kind === "agent-discovered" && e.session_id === sessionId,
    from,
    timeoutMs,
    `agent with session-id ${sessionId}`,
  ) as Promise<TraceEvent & { kind: "agent-discovered" }>;
}

export async function waitForDiscoveredByPid(
  ctx: ScenarioContext,
  pid: number,
  from: number,
  timeoutMs: number,
): Promise<TraceEvent & { kind: "agent-discovered" }> {
  return waitWithDiscoverPoll(
    ctx,
    (e) => e.kind === "agent-discovered" && e.pid === pid,
    from,
    timeoutMs,
    `agent for pid ${pid}`,
  ) as Promise<TraceEvent & { kind: "agent-discovered" }>;
}

async function waitWithDiscoverPoll(
  ctx: ScenarioContext,
  predicate: (e: TraceEvent) => boolean,
  from: number,
  timeoutMs: number,
  description: string,
): Promise<TraceEvent> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    await ctx.control.discoverNow();
    try {
      return await ctx.trace.waitFor(predicate, { from, timeoutMs: 1_500 });
    } catch {
      // Loop and try again after another discover-now.
    }
  }
  throw new Error(`discovery: ${description} not seen within ${timeoutMs}ms`);
}
