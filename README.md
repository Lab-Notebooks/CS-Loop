# csloop

A bounded author/review coding-agent loop, driven by Anthropic models, as a single
compiled binary.

## Install

```sh
cargo install --git https://github.com/Lab-Notebooks/csloop
```

This builds and installs the `csloop` binary to `~/.cargo/bin` — no source checkout is
left behind.

## Usage

```sh
export ANTHROPIC_API_KEY=sk-ant-...

csloop <task_file> -m <model> [OPTIONS]
```

| Flag | Description |
|---|---|
| `<task_file>` | TOML chat-template file describing the task (see `[[chat.user]]`/`[[chat.assistant]]` format). |
| `-m, --model` | Anthropic model id (e.g. `claude-sonnet-4-6`). Also settable via `CSLOOP_MODEL`. |
| `--agent-loops` | Max bounded author/review loops (default `5`). |
| `--agent-iterations` | Max tool-call iterations per agent session (default `30`). |
| `--workdir` | Working directory bound for the agent (default: current directory). |
| `-v, --verbose` | Print per-iteration reasoning and tool-call diagnostics to stdout. |
| `--log` | Write diagnostic events to `.csloop/logs/toolusage.toml`. |
| `--log-path` | Write diagnostic events to a custom path (implies `--log`). |
| `--reason` | Enable extended thinking. |

Artifacts (run/state/telemetry) are written under `.csloop/loop/` in the working
directory.

## License

MIT — see [LICENSE](LICENSE).
