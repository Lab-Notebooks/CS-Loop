# Control Flow Graph

```text
CLI / process startup
┌─────────────────────────────────────────────────────────────────────────────┐
│ src/main.rs                                                                │
│  main()                                                                    │
│   ├─ Cli::parse()                                                          │
│   │   └─ src/cli.rs                                                        │
│   ├─ cli::resolve_logging(...)                                             │
│   ├─ RunnerConfig { ... }                                                  │
│   └─ PromptLoopRunner::new(cfg).and_then(|mut runner| runner.run())        │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      v

Runner construction
┌─────────────────────────────────────────────────────────────────────────────┐
│ src/loop_runner.rs                                                         │
│  PromptLoopRunner::new(cfg)                                                │
│   ├─ resolve workdir                                                       │
│   ├─ ensure_within_workdir(task_file, workdir)                             │
│   ├─ new_run_id()                                                          │
│   │   └─ src/logging.rs                                                    │
│   ├─ get_loop_paths(workdir)                                               │
│   │   └─ src/paths.rs                                                      │
│   ├─ build_system_prompt()                                                 │
│   ├─ load_task_context()                                                   │
│   │   └─ chat_template::load_chat_template(task_path)                      │
│   │       └─ src/chat_template.rs                                          │
│   ├─ initialize_paths()                                                    │
│   │   ├─ create .csloop/loop/...                                           │
│   │   └─ telemetry::ensure_loop_metadata_dir(run_dir)                      │
│   │       └─ src/telemetry.rs                                              │
│   ├─ initialize_run_record()                                               │
│   │   └─ atomic_write_toml(run.toml)                                       │
│   │       └─ src/logging.rs                                                │
│   └─ initialize_loop_state()                                               │
│       └─ write_state(state.toml)                                           │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      v

Top-level bounded loop
┌─────────────────────────────────────────────────────────────────────────────┐
│ src/loop_runner.rs                                                         │
│  PromptLoopRunner::run()                                                   │
│   └─ for loop_idx in 1..=agent_loops                                       │
│       ├─ reload_task_context_if_needed()                                   │
│       ├─ run_author_phase(loop_idx)                                        │
│       ├─ if STATUS: COMPLETE -> clear pending_items -> break               │
│       └─ run_review_phase(loop_idx, loop_summary, author_answer)           │
│           └─ if review says no pending items and no blocker -> break       │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                         ┌────────────┴────────────┐
                         v                         v

Author phase                                                        Review phase
┌───────────────────────────────────────┐               ┌───────────────────────────────────────┐
│ run_author_phase(loop_idx)            │               │ run_review_phase(loop_idx, ...)       │
│  ├─ update_state("author")           │               │  ├─ update_state("review")           │
│  ├─ clear author log file             │               │  ├─ build_review_agent()              │
│  ├─ build_author_logging()            │               │  ├─ build_review_task(...)            │
│  │   ├─ ToolLogToml(author.toml)      │               │  ├─ Agent::run(review_task, ...)      │
│  │   └─ optional MultiToolLogSink     │               │  ├─ write_phase_metadata("review")   │
│  ├─ build_author_agent()              │               │  ├─ persist_run_progress(loop_idx)    │
│  │   ├─ AnthropicModel::new(...)      │               │  ├─ read_toml(review_output.toml)     │
│  │   │   └─ src/model/anthropic.rs    │               │  └─ apply_review_data(...)            │
│  │   ├─ make_tools(...)               │               │      ├─ pending_items = parsed items   │
│  │   │   └─ src/tools.rs              │               │      └─ complete if no blocker/items  │
│  │   ├─ build_observer()              │               └───────────────────────────────────────┘
│  │   │   └─ src/observer.rs           │
│  │   └─ Agent::new(...)               │
│  ├─ read_initial_task_content()       │
│  ├─ build_author_task(...)            │
│  ├─ Agent::run(author_task, ...)      │
│  ├─ loop_summary_from_result(...)     │
│  ├─ extract_pending_items(...)        │
│  ├─ write_phase_metadata("author")   │
│  └─ persist_run_progress(loop_idx)    │
└───────────────────────────────────────┘
                                      │
                                      v

Single agent session
┌─────────────────────────────────────────────────────────────────────────────┐
│ src/agent.rs                                                               │
│  Agent::run(task, system, chat_history)                                    │
│   ├─ build initial message list                                            │
│   │   ├─ system prompt + REACT_NUDGE                                       │
│   │   ├─ prior chat_history from task template                             │
│   │   └─ user task prompt                                                  │
│   ├─ emit run_start                                                        │
│   ├─ observer.on_run_start(...)                                            │
│   └─ for iteration in 0..max_iterations                                    │
│       ├─ workspace_context_block(...)                                      │
│       ├─ upsert_workspace_context(messages, block)                         │
│       ├─ enabled tools -> to_openai_tool() schemas                         │
│       ├─ observer.model_call_start(iteration)                              │
│       ├─ model.chat_with_tools(messages, tool_schemas)                     │
│       │   └─ dyn Model from src/model/mod.rs                               │
│       ├─ TokenUsage::from_raw(response.usage)                              │
│       ├─ observer.on_model_response(...)                                   │
│       ├─ emit model_response                                               │
│       ├─ if response.tool_calls not empty                                  │
│       │   └─ handle_tool_calls(...)                                        │
│       │       ├─ execute_one_tool_call(...) for each allowed call          │
│       │       ├─ append_skipped_tool_calls(...)                            │
│       │       ├─ model.format_tool_result_messages(...)                    │
│       │       ├─ messages.extend(tool result messages)                     │
│       │       ├─ update_consecutive_error_state(...)                       │
│       │       └─ append_stuck_nudge_if_needed(...)                         │
│       ├─ else if response.text not empty                                   │
│       │   └─ final_text = response.text; stop_reason = FinalText; break    │
│       ├─ else if first empty reply                                         │
│       │   └─ append retry/nudge user message                               │
│       └─ else continue / eventually stop at max iterations                 │
│   ├─ build RunResult                                                       │
│   ├─ observer.on_run_end(&result)                                          │
│   ├─ emit run_end                                                          │
│   └─ return RunResult                                                      │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      v

Tool execution path inside Agent
┌─────────────────────────────────────────────────────────────────────────────┐
│ handle_tool_calls(...)                                                     │
│  └─ execute_one_tool_call(state, call, model_text, run_id, iteration)     │
│      ├─ repeated-call guard via call_counts                                │
│      ├─ observer.on_tool_start(...)                                        │
│      ├─ if raw_arguments_error -> reject as BadJson                        │
│      ├─ else execute_tool(name, args, ...)                                 │
│      │   ├─ find tool by name                                              │
│      │   ├─ validate enabled + args schema                                 │
│      │   ├─ emit tool_start event                                           │
│      │   ├─ tool.run(args)                                                 │
│      │   │   └─ src/tools.rs                                               │
│      │   │       ├─ ReadTool                                               │
│      │   │       ├─ GlobTool                                               │
│      │   │       ├─ BashTool                                               │
│      │   │       ├─ EditTool                                               │
│      │   │       └─ WriteTool                                              │
│      │   ├─ classify Error:... output                                      │
│      │   └─ emit tool_end event                                            │
│      ├─ state.tool_results.push(...) for real executions                   │
│      ├─ record_tool_result(...) -> recent history/errors                   │
│      ├─ clear call_counts after successful edit/write                      │
│      └─ observer.on_tool_end(...)                                          │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      v

Model boundary
┌─────────────────────────────────────────────────────────────────────────────┐
│ src/model/mod.rs                                                           │
│  trait Model                                                               │
│   ├─ chat_with_tools(messages, tools) -> ChatResponse                      │
│   └─ format_tool_result_messages(calls, outputs, reasoning_blocks)         │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      v

Anthropic backend
┌─────────────────────────────────────────────────────────────────────────────┐
│ src/model/anthropic.rs                                                     │
│  AnthropicModel::chat_with_tools(...)                                      │
│   ├─ request_body(messages, Some(tools))                                   │
│   │   ├─ merge system messages                                             │
│   │   ├─ optionally add prompt cache_control                               │
│   │   ├─ translate OpenAI-style tool schema -> Anthropic tool schema       │
│   │   └─ optionally enable thinking                                        │
│   ├─ with_retry(...)                                                       │
│   ├─ if streaming                                                          │
│   │   └─ send_streaming(body)                                              │
│   │       ├─ POST /v1/messages with stream=true                            │
│   │       ├─ parse SSE lines                                               │
│   │       ├─ StreamAccumulator::handle_event(...)                          │
│   │       └─ StreamAccumulator::finish() -> ChatResponse                   │
│   └─ else                                                                  │
│       └─ send_once(body) -> finalize_non_streaming(...)                    │
│                                                                            │
│  AnthropicModel::format_tool_result_messages(...)                          │
│   ├─ assistant message with reasoning blocks + tool_use blocks             │
│   └─ user message with tool_result blocks                                  │
└─────────────────────────────────────────────────────────────────────────────┘
                                      │
                                      v

Logging / telemetry / console side channels
┌─────────────────────────────────────────────────────────────────────────────┐
│ src/logging.rs                                                             │
│  ├─ new_run_id()                                                           │
│  ├─ atomic_write_text / atomic_write_toml                                  │
│  ├─ read_toml(...)                                                         │
│  ├─ ToolLogToml / MultiToolLogSink                                         │
│  └─ append_toml_event(...)                                                 │
│                                                                             │
│ src/observer.rs                                                            │
│  ├─ NoopObserver                                                           │
│  └─ ConsoleObserver                                                        │
│      ├─ model spinner                                                      │
│      ├─ reasoning print                                                    │
│      └─ tool start/end diagnostics                                         │
│                                                                             │
│ src/telemetry.rs                                                           │
│  ├─ ensure_loop_metadata_dir(...)                                          │
│  ├─ write_loop_phase_metadata(...)                                         │
│  └─ write_loop_manifest(...)                                               │
└─────────────────────────────────────────────────────────────────────────────┘

Return path
┌─────────────────────────────────────────────────────────────────────────────┐
│ PromptLoopRunner::run() -> summary string                                  │
│  "completed X/Y loop(s) — run: <run_id> — artifacts: <path>"              │
│       │                                                                     │
│       v                                                                     │
│ src/main.rs prints summary on success                                       │
│ src/main.rs prints formatted error and exits 1 on failure                   │
└─────────────────────────────────────────────────────────────────────────────┘
```