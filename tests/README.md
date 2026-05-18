# corral integration harness

End-to-end test harness that drives a live `corral` process against real
Claude/Codex CLIs and real terminal emulators. macOS-only. Bun + TS.

## Layout

- `harness/` — orchestration: corral manager, control client, JSONL
  trace parser, TCC preflight, scenario + matrix runner, quiescence
  barrier, cleanup.
- `agents/` — bindings for the CLIs we drive (currently: Claude CLI).
- `terminals/` — bindings for the terminal emulators we focus
  (currently: Ghostty).
- `scenarios/` — individual scenarios. Empty in step 4; populated in
  step 5.
- `main.ts` — CLI entry point.

## Running

From this directory:

```sh
bun run tests                  # run all scenarios
bun run tests --grep <pattern> # only matching scenario names
bun run tests --isolate        # one fresh corral per scenario
bun run tests --shuffle        # randomise scenario order
```

The harness auto-builds the corral binary on first invocation
(`cargo build -p corral-app`). The build artefact lives at
`target/debug/corral` under the repo root.

## Bun version

Pinned to Bun ≥ 1.3 (see `engines.bun`). Install via
`curl -fsSL https://bun.sh/install | bash`.

## Prerequisites

- macOS with Automation permissions granted to your terminal of choice
  (the harness probes by running a no-op AppleScript and surfaces a
  `tccutil reset AppleEvents` hint when denied).
- The CLIs the matrix covers must be on `PATH` (just `claude` today).
- The Anthropic API is reachable — every spawned Claude makes one
  `--model haiku` turn so the corral has a session record to bind
  against. Each turn is small but billable.

## Quirks worth knowing about

- The harness creates Ghostty windows using `make new window with
  configuration` and writes the kick prompt via the `initial input`
  surface-config property (pty-level injection, no `System Events`
  keystrokes). The only scenario that *does* need `System Events` is
  `binding/clear-rebind`, which has to drive `/clear` mid-session;
  those keystrokes target the Ghostty process via `tell process`, but
  if you actively click in another Ghostty window while the scenario
  is running, the keystrokes can still land where keyboard focus is.
  Easiest workaround: don't interact with Ghostty during a
  `clear-rebind` run.
- `focus/tab-focus` is intentionally `applies`-skipped when the
  terminal binding doesn't expose `tty` or `pid` (Ghostty 1.3.1 falls
  here). The corral's `focus_by_cwd` strategy cannot disambiguate
  two terminals at the same cwd, so the scenario's terminal-side
  readback would be a false-green. The scenario lands intact for
  future Ghostty versions and other terminals.

## Verification state

| Scenario | Verified end-to-end on this machine? |
|----------|----------------------------------------|
| `discovery/spawn-detect` | yes — passes against Claude 2.1.140 + Ghostty 1.3.1 |
| `binding/clear-rebind`   | yes — passes when run in isolation; sequential runs are race-prone (see the *Quirks* note about keyboard focus) |
| `focus/tab-focus`        | not run — `applies`-skipped on Ghostty 1.3.1 |
