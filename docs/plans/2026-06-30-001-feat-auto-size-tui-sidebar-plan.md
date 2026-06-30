---
title: Auto-size TUI Sidebar - Plan
date: 2026-06-30
type: feat
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
execution: code
product_contract_source: ce-plan-bootstrap
---

# Auto-size TUI Sidebar - Plan

## Goal Capsule

| Field | Value |
| --- | --- |
| Objective | Minimize the TUI left sidebar width to the smallest useful width for the rendered session list so the session preview/detail area gets maximum space. |
| Authority | User request in LFG invocation: "Minimize the width of the left side bar to the minimum amount required to show the text". |
| Execution profile | Single local TUI layout change with focused render/layout tests. |
| Stop conditions | Stop if the sidebar can no longer show the visible session/group labels or if manual resize behavior is lost. |
| Tail ownership | LFG owns implementation, review, commit, push, PR, and CI watch. |

---

## Product Contract

The TUI should allocate less default width to the left session list and give that recovered width to the selected session view.
The session list remains readable; the detail/session panel is the area that should expand.

### Requirements

- R1. The default sidebar width is content-based, not a fixed percentage, and uses the minimum width needed for currently visible session/group rows plus the list border.
- R2. The computed sidebar width is bounded by a small minimum and the existing maximum-percent cap so tiny or very long names do not break the layout.
- R3. Manual divider resizing continues to work and should override auto sizing until the user drags it again or the app is restarted.
- R4. Preview/session hit testing uses the same computed layout as rendering so mouse wheel forwarding stays aligned with the visible session panel.
- R5. Group headers keep status visibility compact by showing only non-zero status counts with Unicode markers.

### Scope Boundaries

- Keep the existing two-pane layout, divider, mouse resize behavior, and session rows.
- Do not show zero-count status categories in group headers.
- Do not add a persisted sidebar-width setting.
- Do not change the browser dashboard or CLI output.

---

## Planning Contract

### Key Technical Decisions

- KTD1. Add an `auto_sidebar_width` helper that measures the same row text shape rendered by `session_list`, then feed that width into `body_layout`.
This keeps sizing local to the TUI and avoids introducing persistence or app-wide configuration.
- KTD2. Represent manual resizing as an optional sidebar width in columns rather than a default percentage.
This makes the default auto-sized path precise while preserving drag behavior.
- KTD3. Clamp auto and manual widths to a minimum column count and the existing `MAX_SIDEBAR_PERCENT` cap.
The sidebar should shrink for normal labels but still avoid pathological layouts.

### Implementation Notes

- The content width calculation should include group headers and visible session rows, including branch marker, status marker, session name, optional PR label, agent, and optional activity label.
- Group header text should omit zero-count status categories so default width is not dominated by inactive statuses.
- The rendered list block needs border columns plus the `> ` highlight spacing; clamp after adding that frame width.
- `terminal_preview_area`, `embedded_terminal_area`, mouse hit testing, and render should all call the same layout computation path.

---

## Implementation Units

### U1. Add content-based sidebar sizing

- **Goal:** Replace fixed default percentage sizing with auto width derived from visible sidebar content.
- **Requirements:** R1, R2.
- **Files:** `src/tui.rs`.
- **Approach:** Add a helper that computes the longest visible list row width from `DashboardView` and returns the sidebar width including borders, clamped by a small minimum and the existing max-percent limit for the current body width.
- **Patterns follow:** Existing `session_list`, `group_status_counts`, and `body_layout` helpers in `src/tui.rs`.
- **Test scenarios:** Verify `body_layout` or its replacement gives a smaller sidebar than the old 24-column default for short session names; verify a longer session/group label increases the sidebar enough to show the text; verify width never exceeds the max-percent cap.
- **Verification:** Focused TUI layout tests pass.

### U2. Preserve manual resize and hit-test parity

- **Goal:** Keep divider dragging functional while making preview/session hit testing use the same computed layout as rendering.
- **Requirements:** R3, R4.
- **Files:** `src/tui.rs`.
- **Approach:** Store manual sidebar width in columns when the divider is dragged; render and hit-test helpers should derive their `BodyLayout` from either manual width or auto width using the current `DashboardView`.
- **Patterns follow:** Existing `set_sidebar_percent_from_mouse`, `mouse_on_session_preview`, `mouse_on_embedded_session`, `terminal_preview_area`, and `embedded_terminal_area`.
- **Test scenarios:** Verify drag sizing still moves the divider; verify `terminal_preview_area` and `embedded_terminal_area` place their inner areas immediately to the right of the computed sidebar and divider.
- **Verification:** Focused TUI mouse/layout tests and full crate tests pass.

---

## Verification Contract

| Check | Command | Expected |
| --- | --- | --- |
| Format | `rtk cargo fmt --check` | No formatting diff. |
| Focused tests | `rtk cargo test body_layout` | Layout sizing tests pass. |
| Full tests | `rtk cargo test` | Full crate test suite passes. |

---

## Definition of Done

- Sidebar defaults to a content-sized width in normal/search/session modes.
- Session preview/detail area gains the remaining horizontal space.
- Manual resizing still works.
- The plan file, code changes, and tests are committed and pushed through the LFG pipeline.
