# Agent Helm

Agent Helm is a local controller for terminal-backed agent sessions. The first
milestone is deliberately narrow: shell sessions can be created, listed,
attached, sent input, captured, restarted, and stopped through one controller.

## Current Commands

```bash
cargo run -- init
cargo run -- add . --agent shell --cmd 'cat' --name demo
cargo run -- list
cargo run -- output <session-id> --limit 100
cargo run -- send <session-id> 'hello'
cargo run -- attach <session-id>
cargo run -- stop <session-id>
cargo run -- restart <session-id>
cargo run -- remove <session-id>
```

Running `cargo run` without a subcommand opens the TUI. Press `n` to create a shell session from inside the dashboard.

The default profile stores SQLite state at
`~/.local/share/agent-helm/default/state.db`.

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
```

Tmux smoke tests are gated and do not run unless explicitly enabled.

For an end-to-end local check that uses a temporary HOME and cleans up its own tmux/API sessions:

```bash
scripts/harness.sh
```
