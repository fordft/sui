---
name: tui-quality
description: Terminal-UI interaction quality — keyboard/paste handling, streaming output, scroll/collapse behavior, resize, terminal restoration. Use for any change touching the TUI surface, even when the user frames it as a logic fix.
cues: [tui, terminal, keypress, keyboard, paste, scroll, render, repaint, modal, picker, transcript, crossterm, ratatui, mouse, resize, cursor]
roles: [lead, worker, auditor]
---

# TUI quality

The terminal is the product surface. Verify through a real PTY, not just
unit-mapped key enums.

## Interaction contract

- Every advertised shortcut works with ONE physical keypress — including
  inside modals and permission prompts.
- Paste reaches the intended field; bracketed-paste doesn't leak into
  global chords (a pasted 'q' must not quit).
- Terminal state restores on exit — alternate screen, mouse capture,
  bracketed paste all undone even on error paths.

## Streaming and scroll

- Long output folds/bounds — the transcript stays navigable; failures
  keep their context visible.
- Scroll position survives fold/expand cycles; anchor doesn't jump.
- New content during user scrollback doesn't yank the view.

## States that must work

- Resize mid-operation (narrow/wide re-layout, no panic, no wrap-clobber).
- Permission modal during running task — stop still works (Ctrl+S).
- Unicode widths: CJK/emoji don't corrupt column math.

## Verify

- One keypress through the real terminal path, not `map(key)->action`.
- Rapid input during a repaint — no lost or double-processed keys.
- Detach/exit paths leave the shell usable.

## Do not

- Do not test only the mapping layer — the bug lives in delivery.
- Do not assume a repaint loop fixes a scheduling starvation.
