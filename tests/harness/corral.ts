// Singleton corral process manager.
//
// Spawns the corral binary with a per-run trace file and control
// socket, waits for the socket to appear, and exposes the matching
// `ControlClient` + `TraceReader` to the harness. Shutdown either calls
// the `shutdown` control op (preferred, lets the corral flush trace)
// or SIGKILLs the process if the control channel is wedged.

import { spawn, type Subprocess } from "bun";
import { existsSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { ControlClient } from "./control.ts";
import { TraceReader } from "./trace.ts";

export interface CorralOptions {
  binaryPath: string;
  /// Optional override for the directory holding trace.jsonl + the
  /// control socket. Defaults to a fresh `mkdtemp` under `$TMPDIR`.
  runDir?: string;
}

export class Corral {
  private subprocess: Subprocess;
  readonly control: ControlClient;
  readonly trace: TraceReader;
  readonly tracePath: string;
  readonly socketPath: string;
  readonly runDir: string;

  private constructor(
    subprocess: Subprocess,
    control: ControlClient,
    trace: TraceReader,
    tracePath: string,
    socketPath: string,
    runDir: string,
  ) {
    this.subprocess = subprocess;
    this.control = control;
    this.trace = trace;
    this.tracePath = tracePath;
    this.socketPath = socketPath;
    this.runDir = runDir;
  }

  static async spawn(opts: CorralOptions): Promise<Corral> {
    const runDir = opts.runDir ?? mkdtempSync(join(tmpdir(), "corral-run-"));
    const tracePath = join(runDir, "trace.jsonl");
    const socketPath = join(runDir, "control.sock");

    const subprocess = spawn({
      cmd: [resolve(opts.binaryPath), "--trace-file", tracePath],
      env: {
        ...process.env,
        CORRAL_CONTROL_SOCKET: socketPath,
      },
      stdout: "inherit",
      stderr: "inherit",
    });

    await waitForSocket(socketPath, subprocess, 5_000);
    const control = await ControlClient.connect(socketPath);
    const trace = new TraceReader(tracePath);
    return new Corral(subprocess, control, trace, tracePath, socketPath, runDir);
  }

  async shutdown(): Promise<void> {
    // Best-effort: prefer the control op so the corral exits cleanly.
    try {
      await this.control.shutdown();
    } catch {
      // ignore — the process may already be gone.
    }
    try {
      this.control.close();
    } catch {
      // ignore
    }
    // Wait up to 2s for the process to exit; SIGKILL if it doesn't.
    const exited = this.subprocess.exited;
    const timeout = sleep(2_000).then(() => "timeout" as const);
    const result = await Promise.race([exited, timeout]);
    if (result === "timeout") {
      this.subprocess.kill("SIGKILL");
      await this.subprocess.exited;
    }
  }
}

async function waitForSocket(
  path: string,
  proc: Subprocess,
  timeoutMs: number,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(path)) return;
    if (proc.exitCode !== null) {
      throw new Error(`corral exited before binding control socket`);
    }
    await sleep(25);
  }
  throw new Error(`corral: control socket ${path} did not appear within ${timeoutMs}ms`);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
