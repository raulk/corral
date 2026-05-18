// Control-socket client.
//
// Speaks the NDJSON request/response protocol from
// `crates/corral-app/src/control.rs`. One in-flight request per
// connection — we serialise via a promise chain.

import { createConnection, type Socket } from "node:net";

import type { AgentKind, BindingSource } from "./trace.ts";

export interface AgentSnapshot {
  pid: number;
  agent: AgentKind;
  session_id: string;
  transcript: string;
  cwd: string | null;
  tty: string | null;
  binding_source: BindingSource;
  model: string | null;
  git_branch: string | null;
  session_title: string | null;
  current_action: string | null;
  last_action: string | null;
  context_tokens: number | null;
  context_max: number | null;
}

interface Response {
  ok: boolean;
  error?: string;
  agents?: AgentSnapshot[];
}

export class ControlClient {
  private socket: Socket;
  private buffer = "";
  private pending: Array<{
    resolve: (v: Response) => void;
    reject: (e: Error) => void;
  }> = [];
  /// Serialises in-flight requests so one socket can only have a
  /// single open request at a time, matching the server.
  private chain: Promise<unknown> = Promise.resolve();

  private constructor(socket: Socket) {
    this.socket = socket;
    socket.on("data", (chunk: Buffer) => this.onData(chunk));
    socket.on("close", () => {
      for (const p of this.pending) {
        p.reject(new Error("control socket closed"));
      }
      this.pending = [];
    });
  }

  static connect(socketPath: string): Promise<ControlClient> {
    return new Promise((resolve, reject) => {
      const sock = createConnection({ path: socketPath }, () => {
        resolve(new ControlClient(sock));
      });
      sock.once("error", reject);
    });
  }

  async focus(pid: number, requestId: number): Promise<void> {
    const resp = await this.send({ op: "focus", pid, request_id: requestId });
    requireOk(resp);
  }

  async discoverNow(): Promise<void> {
    const resp = await this.send({ op: "discover-now" });
    requireOk(resp);
  }

  async snapshot(): Promise<AgentSnapshot[]> {
    const resp = await this.send({ op: "snapshot" });
    requireOk(resp);
    return resp.agents ?? [];
  }

  /// Asks the corral to exit. The socket closes immediately after;
  /// callers should expect their connection to drop.
  async shutdown(): Promise<void> {
    try {
      await this.send({ op: "shutdown" });
    } catch (e) {
      // The server replies "ok" then exits — Bun sees the close as
      // ECONNRESET on some kernels. Treat any error after the request
      // as success since the goal (process exit) is achieved.
      if (!String(e).includes("closed")) throw e;
    }
  }

  close(): void {
    this.socket.end();
  }

  private send(req: object): Promise<Response> {
    const enqueued = this.chain.then(() => this.sendInternal(req));
    // Don't let one failure poison the chain for later callers.
    this.chain = enqueued.catch(() => {});
    return enqueued;
  }

  private sendInternal(req: object): Promise<Response> {
    return new Promise((resolve, reject) => {
      this.pending.push({ resolve, reject });
      this.socket.write(JSON.stringify(req) + "\n", (err) => {
        if (err) {
          this.pending.pop();
          reject(err);
        }
      });
    });
  }

  private onData(chunk: Buffer): void {
    this.buffer += chunk.toString("utf-8");
    while (true) {
      const nl = this.buffer.indexOf("\n");
      if (nl === -1) break;
      const raw = this.buffer.slice(0, nl);
      this.buffer = this.buffer.slice(nl + 1);
      const waiter = this.pending.shift();
      if (!waiter) {
        // Server sent more than we asked for — protocol error.
        throw new Error(`control: unexpected response ${raw}`);
      }
      try {
        const parsed = JSON.parse(raw) as Response;
        waiter.resolve(parsed);
      } catch (e) {
        waiter.reject(new Error(`control: bad response ${raw}: ${e}`));
      }
    }
  }
}

function requireOk(resp: Response): void {
  if (!resp.ok) {
    throw new Error(`control: ${resp.error ?? "unknown error"}`);
  }
}
