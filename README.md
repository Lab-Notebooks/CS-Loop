# The CodeScribe loop

A bounded agentic loop driven by Anthropic models, packaged as a single
compiled binary based on the open source research work performed
with CodeScribe (https://github.com/Lab-Notebooks/CodeScribe)

## Install

```sh
cargo install --git https://github.com/Lab-Notebooks/csloop
```

This builds and installs the `csloop` binary to `~/.cargo/bin` — no source checkout is
left behind.

### Install over SSH

Useful if the repository is private or you'd rather authenticate with your SSH key than
an HTTPS credential helper:

```sh
cargo install --git ssh://git@github.com/Lab-Notebooks/csloop.git
```

Cargo's built-in git support requires the full `ssh://` form — the scp-like shorthand
(`git@github.com:Lab-Notebooks/csloop.git`) is not accepted. It also needs an `ssh-agent`
with your key loaded:

```sh
eval "$(ssh-agent -s)"
ssh-add ~/.ssh/id_ed25519   # or whichever key is authorized on GitHub
```

If you rely on `~/.ssh/config` (host aliases, a custom `IdentityFile`, `ProxyCommand`,
etc.), Cargo's native git client ignores it. Tell Cargo to shell out to your system `git`
instead, which picks up your normal SSH config:

```sh
export CARGO_NET_GIT_FETCH_WITH_CLI=true
```

(or set `git-fetch-with-cli = true` under `[net]` in `~/.cargo/config.toml`.)

## Update

`cargo install --git` won't overwrite an existing install by default. Reinstall from the
latest commit on the default branch with `--force`:

```sh
cargo install --force --git https://github.com/Lab-Notebooks/csloop
# or, over SSH:
cargo install --force --git ssh://git@github.com/Lab-Notebooks/csloop.git
```

Check what's currently installed with:

```sh
cargo install --list | grep csloop
```

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
