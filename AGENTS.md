# Processing Pipeline

How a task flows through the system, top to bottom: **Loop → Agent Session →
Model Gateway → Model**, with a **Tool Sandbox** gating every tool call.
Source locations are in the legend at the end.

```text
┌──────────────────────────────── INTAKE (once) ─────────────────────────────────┐
│ CLI args + task file → validate → resolve sandbox root → create run/loop dir   │
└──────────────────────────────────────────┬─────────────────────────────────────┘
                                            ▼
╔══════════════════════════════════ LOOP  (1..max_loops) ════════════════════════╗
║                                                                                  ║
║   ┌───────────────┐   summary +    ┌────────────────┐                          ║
║   │    AUTHOR     │───────────────▶│    REVIEWER     │                         ║
║   │    phase      │   pending      │    phase        │                         ║
║   └───────┬───────┘                └────────┬────────┘                         ║
║           │                                  │                                  ║
║      done?├── yes ──▶ EXIT LOOP        clean?├── yes ──▶ EXIT LOOP             ║
║           │no                                 │no (pending items / blocker)     ║
║           ▼                                   ▼                                 ║
║      next loop  ◀─────── pending items feed next AUTHOR prompt ─────────────────║
║                                                                                  ║
║   neither role can force extra loops — after max_loops the controller stops     ║
║   and reports partial completion, it never spins forever                       ║
╚═══════════════════════════════════════════╤════════════════════════════════════╝
                          author & reviewer are the SAME engine, different toolsets
                                             ▼
┌─────────────────────────── AGENT SESSION  (1..max_iterations) ─────────────────┐
│                                                                                  │
│    ┌─────────────────────────── ITERATION ──────────────────────────────────┐  │
│    │  conversation + tool schemas                                           │  │
│    │             │                                                          │  │
│    │             ▼                                                          │  │
│    │      ┌───────────────┐   request    ┌───────────┐                     │  │
│    │      │ MODEL GATEWAY │─────────────▶│   MODEL   │                     │  │
│    │      │ (provider     │◀─────────────│  (LLM)    │                     │  │
│    │      │  adapter)     │   response    └───────────┘                     │  │
│    │      └───────┬───────┘                                                 │  │
│    │              │                                                         │  │
│    │     tool call(s)? ── no, text only ──▶ FINAL ANSWER → end session      │  │
│    │              │ yes                     empty reply ──▶ nudge once,    │  │
│    │              ▼                         then end "not finished"        │  │
│    │      ┌───────────────┐                                                │  │
│    │      │ TOOL SANDBOX  │  (see below — runs once per call)              │  │
│    │      └───────┬───────┘                                                │  │
│    │              │ result appended to conversation                        │  │
│    │              └────────────────────────▶ next ITERATION                │  │
│    └─────────────────────────────────────────────────────────────────────-─┘  │
│                                                                                  │
│   repeated-error streak → nudge to stop retrying & report the blocker          │
│   hard call-budget ceiling → stops the session even mid-conversation           │
└──────────────────────────────────────────────────────────────────────────────┘

┌────────────────────── TOOL SANDBOX  (model input = untrusted) ─────────────────┐
│ reject unknown/disabled tool → validate args vs schema → repeated-call guard    │
│ → path containment (files AND shell-arg paths, symlink/`..`-safe) → shell      │
│ allowlist (no chaining metacharacters; dangerous flags rejected; one program   │
│ always forced into its safe mode) → execute w/ timeout → truncate + classify   │
│ success/error → NEVER crashes the session, always degrades to an error string  │
└──────────────────────────────────────────────────────────────────────────────┘
                                             │
                                             ▼
┌───────────────────────────────── RETURN PATH ──────────────────────────────────┐
│  success → one-line summary (loops used, run id, artifact dir)                │
│  failure → formatted error + non-zero exit — no partial/ambiguous state        │
└──────────────────────────────────────────────────────────────────────────────┘
```

## AUTHOR vs. REVIEWER toolsets

```
 AUTHOR                          REVIEWER
 ───────────────────────         ───────────────────────
 read · glob · edit · write      read · glob · write  (no edit)
 shell: broad inspection         shell: read-only inspection only,
        allowlist                        env-dumping commands excluded
 goal: make progress             goal: pending items[] + optional blocker,
                                        written as a structured verdict file
```

If the reviewer's verdict file is empty/unwritten, the loop treats it as
**inconclusive** and continues rather than failing.

## Cross-cutting: persistence & observability

Runs beside every stage, not after it — run/loop state, per-phase metadata,
and an append-only event log are written incrementally so a run stays
inspectable/resumable even if the process dies mid-way. An optional console
reporter mirrors this live.

## One invariant worth knowing

The per-iteration "workspace context" (iteration count, recent tool
results) is injected as its own block, never mixed into the fixed system
instructions — those must stay byte-identical across iterations for
provider-side prompt-cache reuse to work.

## Source-location legend

| Pipeline stage | Source |
|---|---|
| Intake | `src/main.rs`, `src/cli.rs`, `src/chat_template.rs`, `src/paths.rs` |
| Loop | `src/loop_runner.rs` (`PromptLoopRunner::run`) |
| Author / Reviewer phases | `src/loop_runner.rs` (`run_author_phase`, `run_review_phase`, `build_author_tools`/`build_review_tools`) |
| Agent session | `src/agent.rs` (`Agent::run`, `handle_tool_calls`) |
| Tool sandbox | `src/tools.rs` (`AgentTool` implementations, `validate_command`, `resolve_within_root`, `harden_command`) |
| Model gateway | `src/model/mod.rs` (interface), `src/model/anthropic.rs` (Anthropic adapter) |
| Persistence & observability | `src/logging.rs`, `src/telemetry.rs`, `src/observer.rs` |
| Return path | `src/main.rs`, `src/loop_runner.rs::run` |
