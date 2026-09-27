# TUI audit — 2026-09-27

Three independent reviews covered interaction behavior, visual usability,
and output integrity/accessibility. Findings were checked against source,
existing terminal captures, and small executable Ratatui probes before edits.

## Confirmed findings and fixes

| Area | Reproduction | Correction |
| --- | --- | --- |
| Draft history | Recall a task, paste an edit, press Down: the draft disappears. | Every composer mutation exits history traversal. |
| Navigation | Move selection upward beyond the viewport: focus disappears. | Reveal the selected activity and preserve scroll anchors. |
| Focus targets | Hide reasoning or combine repeated tools: navigation still visits hidden items. Folded answers cannot be clicked. | Match focus targets to visible transcript owners. |
| Scrolling | Scroll far beyond the beginning, then reverse: the screen appears stuck. | Bound stored transcript and detail offsets. |
| Mode changes | While running, the palette disables switching but Ctrl+O still switches. | Enforce the same availability rules at shared dispatch. |
| Unicode editing | Insert a ZWJ between emoji: the caret lands inside the joined grapheme. | Normalize the caret after edits that join graphemes. |
| Secondary views | Add 40 tasks and press Page Down: the last task remains inaccessible. | Independent bounded scroll positions and overflow indicators. |
| Changes | A 20-line audit loses its last eight lines without any notice. | Scroll captured audit/diff content and explicitly mark the preview cap. |
| Form fields | Edit a long URL or workspace path: the suffix and caret are invisible. | Cursor-relative field windows; secrets remain masked. |
| Cross-review | Page Down skips two rows; Home/End does not move within provider fields. | Page by the inner viewport height and support field Home/End. |
| Usage layout | At 80 columns the fixed table clips output tokens. | Wrapped entries retain every measurement at narrow widths. |
| Missing telemetry | Missing cache-write counts render as zero; partial totals appear complete. | Track coverage separately for each field; display unknown and partial totals explicitly. |
| Model attribution | Change a model: previous usage is relabeled as the new model. | Aggregate by both agent and model. |
| Tool status | A file tool with no process exit reports `exit 0`. | Render `ok` unless an exit code was actually reported. |
| Reasoning wrap | Expanded reasoning loses two characters per wrapped line. | Reserve the actual indentation width before wrapping. |
| Preview limits | Long reasoning and tool output silently stop at the display cap. | Show an explicit limit notice; retain captured details. |
| Native theme | Cyan essential text has poor contrast in the light-terminal capture; selection depends on color. | Use terminal-default foreground for essential text and attribute-based selection. |
| Activity shortcuts | Ctrl+B/O/R and F1 are swallowed while the transcript has focus. | Dispatch view shortcuts before focus-specific navigation, while preserving modal input handling. |
| Reasoning selection | Hiding reasoning changes the index of the selected visible activity. | Preserve the selected owner when still visible and reveal the resulting selection. |
| Completion status | A blocked mode switch leaves “stop the current task first” after the run ends. | Clear the obsolete warning on completion while retaining unrelated errors. |

Regression tests exercise these reproductions, including terminal cell content
and cursor positions rather than only checking whether rendering panics.
The test suite uses mock providers; it does not certify live provider behavior
or every terminal emulator/color configuration.

## Closure check

The follow-up pass rechecked every original finding. Independent reviewers
found no outstanding gaps in the rendering, telemetry, panels, or forms.
The three remaining interaction cases above were reproduced with failing
regressions before the fixes. A real PTY test exercises Ctrl+B, Ctrl+R,
Ctrl+O, and F1 from transcript focus and verifies preference persistence
and terminal restoration.

## Verification

- `cargo fmt --check`: passed.
- `cargo test`: 204 passed, 2 live-provider smoke tests ignored.
- `cargo clippy --all-targets -- -D warnings`: passed.
- Real PTY walkthrough: permission/write flow, theme persistence, multiline
  paste, resize, mouse palette, retained draft, and terminal restoration passed.
- Refreshed terminal captures; visually checked native text on a light
  background. A subprocess test verifies selection attributes under `NO_COLOR`.
- Release transcript projection benchmark: about 1.8 ms per frame for the
  existing 685–720-row fixture at widths 80 and 120. This measures projection,
  not end-to-end terminal latency (`cargo test --release --test perf
  transcript_rows_cost -- --nocapture`).
