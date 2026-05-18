// TCC preflight.
//
// We can't read TCC.db on modern macOS without Full Disk Access, so
// the plan's "read the SQLite store" approach doesn't work in practice.
// Instead we probe each requirement live: run a one-shot AppleScript
// against the target app and watch for an Automation permission error
// in stderr. If the probe succeeds, the (client, target) pair has the
// grant.
//
// Empty requirement list short-circuits — scenarios that don't touch a
// terminal don't need TCC at all.

import { spawnSync } from "node:child_process";

export interface PreflightRequirement {
  /// Bundle id of the target app, e.g. "com.mitchellh.ghostty".
  target: string;
  /// Human-readable name of the target app to put in the script
  /// (Ghostty's bundle id and process name differ from "Ghostty" the
  /// AppleScript name).
  appName: string;
}

const HINT =
  "run `tccutil reset AppleEvents` and re-run; macOS will prompt for each grant";

export async function preflight(reqs: PreflightRequirement[]): Promise<void> {
  if (reqs.length === 0) return;
  const missing: PreflightRequirement[] = [];
  for (const req of reqs) {
    if (!probe(req)) missing.push(req);
  }
  if (missing.length > 0) {
    const lines = missing.map((r) => `  -> ${r.appName} (${r.target})`);
    throw new Error(
      `preflight: missing Automation grants:\n${lines.join("\n")}\n${HINT}`,
    );
  }
}

/// Runs a trivial AppleScript against the target app. Success means
/// Automation is granted; "not allowed" / "User canceled" in stderr
/// means denied; anything else (app not running, etc.) is treated as
/// "can't tell — assume ok and let the scenario surface a clearer
/// error".
function probe(req: PreflightRequirement): boolean {
  const script = `tell application "${req.appName}" to get name`;
  const out = spawnSync("/usr/bin/osascript", ["-e", script], {
    encoding: "utf-8",
  });
  if (out.status === 0) return true;
  const stderr = out.stderr ?? "";
  if (stderr.includes("not allowed") || stderr.includes("User canceled")) {
    return false;
  }
  // Other failure modes (app not running, dictionary mismatch, etc.):
  // be optimistic. The scenario will fail with a clearer error if the
  // app is truly broken.
  return true;
}
