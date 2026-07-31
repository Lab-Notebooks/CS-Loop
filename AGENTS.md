# Processing Pipeline

This describes the system as a data/control-flow pipeline, independent of the
implementing language. It answers "what happens to a task as it moves through the
system," not "which function calls which." A legend mapping each stage to its source
location follows at the end for engineers who need to jump into the implementation.

```text
════════════════════════════════════════════════════════════════════════════════
 STAGE 0 — INTAKE
════════════════════════════════════════════════════════════════════════════════
  Inputs:  CLI arguments, a task-definition file (chat template), env credentials
  Output:  a validated run configuration

  ┌──────────────────────────────────────────────────────────────────────────┐
  │ 1. Parse CLI arguments (task file path, model id, loop/iteration budgets,│
  │    working-directory override, logging + verbosity flags).               │
  │ 2. Resolve the working directory (the filesystem boundary every         │
  │    downstream file operation is sandboxed to).                          │
  │ 3. Reject the run outright if the task file resolves outside that       │
  │    boundary — no execution happens until this containment check passes.│
  │ 4. Load and structurally validate the task-definition file: role        │
  │    ordering, non-empty turns, an optional allowlist of extra shell      │
  │    commands the author agent may use.                                   │
  │ 5. Generate a unique run identifier and create the on-disk run/loop     │
  │    directory tree that all artifacts for this run will live under.      │
  │ 6. Write an initial run record and loop-state record (so a run's        │
  │    progress is inspectable/resumable even if the process dies mid-way). │
  └──────────────────────────────────────────────────────────────────────────┘
                                      │
                                      v
════════════════════════════════════════════════════════════════════════════════
 STAGE 1 — BOUNDED ITERATION CONTROLLER
════════════════════════════════════════════════════════════════════════════════
  A fixed-size loop (never unbounded) alternating two roles per iteration:
  an AUTHOR that changes the workspace, and a REVIEWER that judges the change.

  for loop_index in 1..=max_loops:
      ┌────────────────────────┐        ┌───────────────────────────────┐
      │   run AUTHOR phase     │──────▶│   run REVIEWER phase            │
      └────────────────────────┘        └───────────────────────────────┘
                 │                                    │
                 │ author reports itself done         │ reviewer reports
                 │ (no unresolved work)                │ no pending items and
                 v                                    │ no blocker
           exit loop early                             v
                                                  exit loop early
      Neither role can force extra iterations beyond max_loops; if consensus
      ("done") is never reached the controller simply stops after the last
      scheduled loop and reports partial completion — it never spins forever.
                                      │
                                      v
════════════════════════════════════════════════════════════════════════════════
 STAGE 2 — AUTHOR PHASE  (a "make progress" pass)
════════════════════════════════════════════════════════════════════════════════
  Inputs:  task description, prior loop summaries, outstanding pending items
  Output:  a workspace mutation + a self-reported status + a plain-text summary

  ┌──────────────────────────────────────────────────────────────────────────┐
  │ 1. Record phase = "author" in the run state.                            │
  │ 2. Assemble a full read/write toolset scoped to the working directory:  │
  │    read file, glob/find files, edit (exact text replacement), write     │
  │    (create/overwrite), and a sandboxed shell (see Stage 4).             │
  │ 3. Compose the author's task prompt from: the task file, which loop     │
  │    this is out of the total budget, a digest of prior loops' actions,   │
  │    and any items the reviewer flagged as still outstanding.             │
  │ 4. Run one bounded AGENT SESSION (Stage 3) with that prompt + toolset.  │
  │ 5. Summarize what actually happened (files written/edited/read,        │
  │    commands run, errors) directly from the session's tool-call record — │
  │    this is a harness-computed summary, not something the model wrote.  │
  │ 6. Parse the model's final answer for a "next steps" list and a         │
  │    completion status line (e.g. done vs. not-done).                    │
  │ 7. Persist phase metadata and overall run progress to disk.            │
  └──────────────────────────────────────────────────────────────────────────┘
                                      │
                                      v (unless self-reported "done")
════════════════════════════════════════════════════════════════════════════════
 STAGE 2′ — REVIEWER PHASE  (a "judge the change" pass)
════════════════════════════════════════════════════════════════════════════════
  Inputs:  the author's summary + final answer for this loop
  Output:  accept (nothing pending) or a list of pending items / a blocker

  ┌──────────────────────────────────────────────────────────────────────────┐
  │ 1. Record phase = "review" in the run state.                            │
  │ 2. Assemble a DELIBERATELY SMALLER toolset than the author's: read,     │
  │    glob, write, and a shell restricted to read-only inspection          │
  │    commands — no edit tool, and the shell allowlist is narrower than    │
  │    the author's (inspection commands only; process-environment dumping │
  │    commands are excluded on purpose so credentials can never end up in  │
  │    a persisted tool-output log).                                       │
  │ 3. Compose the reviewer's task prompt from the author's summary and a   │
  │    fixed structured-output contract (a small machine-readable verdict  │
  │    file the reviewer must write: pending items + optional blocker).    │
  │ 4. Run one bounded AGENT SESSION (Stage 3) with that prompt + toolset.  │
  │ 5. Read back the structured verdict file the reviewer wrote.           │
  │ 6. If it's empty/unwritten, treat this loop as inconclusive and         │
  │    continue to the next author/reviewer cycle rather than failing.     │
  │ 7. If it has no pending items and no blocker, signal the controller to │
  │    stop early; otherwise feed the pending items back into the next     │
  │    author phase's prompt (Stage 2, step 3).                            │
  └──────────────────────────────────────────────────────────────────────────┘

════════════════════════════════════════════════════════════════════════════════
 STAGE 3 — AGENT SESSION  (shared engine behind both phases)
════════════════════════════════════════════════════════════════════════════════
  Inputs:  a role prompt, a toolset, an iteration budget
  Output:  a final text answer, OR a "stopped without finishing" status

  A single bounded reasoning loop, common to both the author and reviewer roles:

  for iteration in 1..=max_iterations_per_phase:
      1. Inject a fresh "workspace context" block into the conversation
         (iteration count, recent tool results/errors) — kept OUT of the
         fixed system instructions so the system instructions stay byte-
         identical across iterations (this matters for provider-side
         prompt-cache reuse; changing content there breaks the cache).
      2. Send the conversation + tool schemas to the MODEL GATEWAY (Stage 5)
         and get back: response text, and/or zero or more tool-call requests.
      3. Branch on what came back:
           a. One or more tool calls requested
              → run each through the TOOL SANDBOX (Stage 4), in request
                order, up to a per-iteration call cap; extra calls beyond
                the cap are rejected with an explanatory note instead of
                silently dropped.
              → append every call's (possibly truncated) result back into
                the conversation and continue to the next iteration.
              → if several iterations in a row produce only errors, inject
                a nudge telling the model to stop retrying and report what's
                blocking it, instead of looping on the same failure forever.
           b. No tool calls, but non-empty text
              → treat as the final answer; end the session successfully.
           c. Neither (empty reply)
              → nudge once asking for either a tool call or a final answer;
                if it's still empty on the very next turn, end the session
                as "stopped without finishing" rather than spinning.
      4. A hard call-budget ceiling (across the whole session, not just per
         iteration) forces the session to stop even mid-conversation if the
         model just keeps calling tools without ever finishing.
  On exit: emit a structured record of every call made, every call rejected
  and why, and token/cost accounting for the session.

════════════════════════════════════════════════════════════════════════════════
 STAGE 4 — TOOL SANDBOX  (runs once per tool call, either role)
════════════════════════════════════════════════════════════════════════════════
  Inputs:  a tool name + arguments (model-supplied, therefore untrusted)
  Output:  a text result (success output OR an "Error: ..." string) —
           this stage NEVER lets a failure crash the session; it always
           degrades to an error string the model can read and react to.

  ┌──────────────────────────────────────────────────────────────────────────┐
  │ 1. Reject unknown tool names and disabled tools immediately.            │
  │ 2. Validate arguments against the tool's declared schema (required     │
  │    fields, types, no unexpected extra fields).                         │
  │ 3. Repeated-call guard: the same tool+arguments pair is only allowed a  │
  │    bounded number of retries (higher for read-style paging) before     │
  │    being refused, so the model can't loop on an unproductive call.     │
  │ 4. Path-containment check (for any file- or path-shaped argument, not   │
  │    just the ones on a "file tool"): resolve symlinks and `..`          │
  │    segments and require the final path to stay inside the working      │
  │    directory. This applies uniformly whether the argument came from a  │
  │    dedicated read/write tool or from an argument to the shell tool —   │
  │    a shell command's working directory alone does not bound what its   │
  │    arguments can point at, so arguments get the same check.            │
  │ 5. Command-allowlist check (shell tool only): only a fixed set of      │
  │    inspection-style program names may run at all, and shell            │
  │    metacharacters that could chain additional commands are rejected    │
  │    outright — but allowlisting a program's NAME is not sufficient by   │
  │    itself, because some otherwise-safe programs have built-in flags    │
  │    that spawn arbitrary other programs. Those specific flags are       │
  │    rejected explicitly, and one allowed program is always executed     │
  │    with an extra safety flag forced on regardless of what was asked    │
  │    for, so that its script-execution escape hatch is disabled          │
  │    unconditionally rather than relying on pattern-matching every way   │
  │    it could be invoked.                                                │
  │ 6. Execute, with a wall-clock timeout and output captured separately   │
  │    on both output streams; a crash inside the tool implementation      │
  │    itself is caught and converted into an error result rather than     │
  │    taking down the whole session.                                      │
  │ 7. Classify the result as success/error, truncate oversized output     │
  │    (with a note telling the model how to page through the rest), and   │
  │    record it for the phase-level summary and for replay into the       │
  │    conversation.                                                        │
  └──────────────────────────────────────────────────────────────────────────┘

════════════════════════════════════════════════════════════════════════════════
 STAGE 5 — MODEL GATEWAY
════════════════════════════════════════════════════════════════════════════════
  A provider-agnostic interface — "send a conversation + tool schemas, get back
  text and/or tool-call requests" — sitting in front of one concrete backend
  adapter. The adapter owns everything provider-specific:
    • translating the neutral message/tool-schema shape into that provider's
      wire format
    • choosing streaming vs. non-streaming transport and reassembling a
      streamed response into the same neutral shape either way
    • retry/backoff on transient failures
    • normalizing usage/token accounting back into a neutral shape
  Swapping providers means writing a new adapter behind this interface, not
  touching Stages 1-4.

════════════════════════════════════════════════════════════════════════════════
 CROSS-CUTTING: PERSISTENCE & OBSERVABILITY
════════════════════════════════════════════════════════════════════════════════
  Runs alongside every stage above rather than after them:
    • Every tool call/rejection and model turn can be appended to an
      on-disk event log, written all-or-nothing per write (no partial/
      corrupt records if the process dies mid-write).
    • Per-phase metadata (tokens used, duration, stop reason, tool tallies)
      is written after every author/reviewer phase, independent of whether
      the overall run later succeeds or fails.
    • An optional console reporter mirrors iteration/tool activity live
      for a human watching the run.

════════════════════════════════════════════════════════════════════════════════
 STAGE 6 — RETURN PATH
════════════════════════════════════════════════════════════════════════════════
  Success → a one-line summary (loops completed / budget, run id, artifact
            directory) printed to the caller.
  Failure → a formatted error printed to the caller and a non-zero exit —
            no partial/ambiguous success state.
```

## Source-location legend

| Pipeline stage | Source |
|---|---|
| Stage 0 — Intake | `src/main.rs`, `src/cli.rs`, `src/chat_template.rs`, `src/paths.rs` |
| Stage 1 — Iteration controller | `src/loop_runner.rs` (`PromptLoopRunner::run`) |
| Stage 2 / 2′ — Author / Reviewer phases | `src/loop_runner.rs` (`run_author_phase`, `run_review_phase`, `build_author_tools`/`build_review_tools`) |
| Stage 3 — Agent session | `src/agent.rs` (`Agent::run`, `handle_tool_calls`) |
| Stage 4 — Tool sandbox | `src/tools.rs` (`AgentTool` implementations, `validate_command`, `resolve_within_root`, `harden_command`) |
| Stage 5 — Model gateway | `src/model/mod.rs` (interface), `src/model/anthropic.rs` (Anthropic adapter) |
| Persistence & observability | `src/logging.rs`, `src/telemetry.rs`, `src/observer.rs` |
| Stage 6 — Return path | `src/main.rs`, `src/loop_runner.rs::run` |
