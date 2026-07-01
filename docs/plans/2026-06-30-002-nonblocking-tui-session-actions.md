---
artifact_readiness: implementation-ready
execution: code
---

# Nonblocking TUI Session Actions

## Goal Capsule

Session create, fork, metadata delete, and cleanup delete actions in the TUI should return control immediately while the controller work runs in a background thread. The dashboard should show TUI-local progress on the affected row and reconcile the real session list when the background action completes.

## Verification Contract

- Submit create/fork/delete without blocking the event loop.
- Keep pending create/fork placeholders and pending delete rows visible across refreshes.
- Preserve existing synchronous behavior for all non-create/fork/delete TUI actions.
- Leave API, CLI, store schema, and persisted statuses unchanged.
- Run `cargo fmt --check` and focused/full Rust tests.

## Implementation Units

### U1. Pending Operation Model

Add internal TUI-only pending operation structs, synthetic session rows, pending status overlays, and helpers for blocking unsafe actions on rows with in-progress work.

### U2. Background Action Execution

Use per-action background threads for `TuiAction::Create`, `TuiAction::Fork`, and `TuiAction::Remove`. Drain results in the main event loop before drawing and reconcile success/failure with local state.

### U3. Main Handler Bound

Make the TUI action handler cloneable/sendable so `main.rs` can pass a cloneable controller closure to background threads without changing CLI behavior.

### U4. Tests

Cover immediate pending state, successful completion, delete progress, failure retention, retry/dismiss behavior, and refresh/status overlay regression.
