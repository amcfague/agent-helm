# Agent Helm

Agent Helm is a local controller for terminal-backed agent sessions and their
project, group, worktree, attachment, watcher, conductor, and cost state.

## Current Commands

```bash
cargo run -- init
cargo run -- add . --agent shell --cmd 'cat' --name demo
cargo run -- list
cargo run -- session list --status waiting
cargo run -- session output <session-id> --limit 100
cargo run -- session search observability --json  # state, output, and Claude/Codex JSONL transcripts
cargo run -- session send <session-id> 'hello'
cargo run -- session attach <session-id>
cargo run -- session stop <session-id>
cargo run -- session start <session-id>
cargo run -- session remove <session-id>
cargo run -- session fork <session-id> --name paused-copy --no-start
cargo run -- group create api --parent work --default-working-directory /path/to/repo
cargo run -- group update work/api --collapsed true --default-working-directory /path/to/repo
cargo run -- group delete work/api
cargo run -- costs --group work/api --model claude-sonnet
```

Running `cargo run` without a subcommand opens the TUI. Press `n` to create a session, and `c`/`e` to collapse or expand groups in the session tree.

The default profile stores SQLite state at
`~/.local/share/agent-helm/default/state.db`.

Tool profiles can override launch commands in `~/.config/agent-helm/config.toml`, `~/.config/agent-helm/settings.toml`, or the profile config:

```toml
[session_defaults]
agent = "claude"
group = "work"

[tools.claude]
executable = "/opt/homebrew/bin/claude"
flags = ["--dangerously-skip-permissions"]
worktree = "always" # always, manual, or never

[tools.shell]
worktree = "never"

[tools.local-agent]
executable = "/usr/local/bin/local-agent"
flags = ["--mode", "review"]
worktree = "manual"

[headroom]
proxy_savings_path = "~/.headroom/proxy_savings.json"
```

The TUI profile tool settings popup writes launch changes to `~/.config/agent-helm/settings.toml`.

The binary also includes project, group, worktree, MCP, skill, watcher,
conductor, and cost commands. Run `cargo run -- --help` for the current full
surface.

## Optional API

```bash
cargo run --features serve -- serve --listen 127.0.0.1:8420
```

`--read-only` blocks mutation routes before they parse request bodies.

## Checks

```bash
cargo fmt --check
cargo clippy --all-targets --all-features
cargo test --all-features
scripts/harness.sh
git diff --check
```

Tmux smoke tests are gated and do not run unless explicitly enabled.

For an end-to-end local check that uses a temporary HOME and cleans up its own tmux/API sessions:

```bash
scripts/harness.sh
```
