---
status: done
tags: []
created-at: '2026-05-15T13:28:44.146+08:00'
related-chats:
  - claude:07c42b93-6fac-4923-a134-fdc4f9db7c7f
  - claude:ca2bb436-c79c-4554-b00c-6c4d563b9647
related-workspaces:
  - eb45ee7b
supersedes: terminal-scrollback
started-at: '2026-05-15T14:02:00.798+08:00'
commits:
  - >-
    feat: replace picker layout with full-screen scroll view and collapsible
    terminal
closed-at: '2026-05-16T00:44:36.250+08:00'
---

# Scroll View with Collapsible Terminal Scrollback

## Idea

Replace the split-pane layout (picker + terminal) with a unified full-screen scroll view containing selectable content blocks (lorem ipsum paragraphs) followed by the terminal at the bottom, whose scrollback buffer is collapsible and supports stick-to-bottom auto-scrolling.

## Proposed Approach

The transcript picker is removed. The entire screen becomes a vertically scrollable view. Content blocks (paragraphs, then the terminal block) are laid out top-to-bottom at computed heights. A `scroll_offset: u16` (rows from top of content) controls which rows are visible.

The terminal block sits at the bottom of the content, always. It has two visual states:
- **Collapsed** (default): shows only the 20 live terminal rows, with a `[▲ Expand]` button above
- **Expanded**: shows all available scrollback rows above the live terminal, with a `[▼ Collapse]` button

The terminal block has two interaction states:
- **Selected**: highlighted border, arrow navigation works, `Space` toggles expand, `Enter` activates
- **Active**: all keystrokes (including `Esc`) go directly to PTY; `Ctrl+O` is the only way to return to selected state

Stick-to-bottom: if the scroll view is at the bottom (`offset == max_scroll`) when PTY output arrives, `offset` is updated to `new_max_scroll` so the terminal stays visible.

`Ctrl+O` globally activates the terminal and snaps the scroll to the bottom.

## Key Design Considerations

- **Custom scroll view**: ratatui has no built-in scroll-container widget. We compute block heights each frame, determine visible range, and render only the blocks that intersect the viewport. Partial block clipping is handled per-block.

- **Fixed PTY row count**: The PTY is always sized at 20 rows (the collapsed height). The `TerminalWidget` renders scrollback rows above the live screen when expanded — no PTY resize needed to expand. This avoids the current bug where `resize()` creates a fresh `vt100::Parser` and loses all history.

- **Resize only on column change**: `TerminalState::resize()` currently replaces the parser wholesale (losing scrollback). In the new design we call resize only when `cols` changes, and we accept that a column-width resize clears history (acceptable for prototype). Rows are never resized.

- **vt100 scrollback API**: Increase `scrollback_len` from `0` → `10000` to support heavy scrollback apps like Claude Code. The vt100 0.16 crate exposes `screen.scrollback_len()` and scrollback row access. The exact API for iterating scrollback rows needs to be confirmed during implementation (likely `screen.rows_iter()` with a negative row offset or a dedicated method).

- **Block height computation**: Each frame, compute heights as:
  - Paragraph block: number of wrapped lines (text width / viewport width, rounded up) + 1 blank line between blocks
  - Terminal block (collapsed): 1 (button) + 20 (live screen) = 21 rows
  - Terminal block (expanded): 1 (button) + `scrollback_len` + 20 (live screen) rows

- **Stick-to-bottom check**: Before processing `TerminalOutput`, record `was_at_bottom = (offset == max_scroll)`. After updating content heights, if `was_at_bottom`, set `offset = new_max_scroll`.

- **Selection scroll-into-view**: After navigating to a new block, if the block top is above the viewport or block bottom is below the viewport, adjust offset to bring the block into view (prefer showing the top of the block).

- **`Space` to expand**: When the terminal block is selected (but not active), `Space` toggles expansion. `Enter` activates the terminal (makes it active).

## Downstream Impact

- `src/picker/` module becomes unused in this prototype but is kept intact for future use.
- `src/providers/` module is no longer referenced by the UI but is kept intact for future use.
- `src/app.rs` is essentially rewritten.
- `src/terminal/state.rs` changes: scrollback_len bump + skip row-resize.
- `src/terminal/ui.rs` changes: accepts `expanded` flag and `scrollback_rows` count.

## Skills to Use

- No special skills needed; standard Rust/ratatui development.

## Implementation Steps

1. **`src/terminal/state.rs`**
   - Change `vt100::Parser::new(rows, cols, 0)` → `vt100::Parser::new(rows, cols, 10000)`
   - In `resize()`: only re-create the parser (and resize PTY) if `cols` changed; ignore `rows` changes
   - Add `pub fn scrollback_len(&self) -> u16` returning `parser.screen().scrollback_len() as u16`

2. **`src/terminal/ui.rs`**
   - Add fields to `TerminalWidget`: `expanded: bool`, `selected: bool`, `active: bool`
   - Render the expand/collapse button as the first line of the terminal block
   - When expanded: render scrollback rows (top-to-bottom) followed by live screen rows
   - When selected (not active): draw a highlighted border or indicator
   - When active: draw an "ACTIVE" label in the block title

3. **`src/scroll_view/mod.rs`** (new module)
   - `ContentBlock` enum: `Paragraph { text: String }` | `Terminal`
   - `ScrollViewState`:
     ```rust
     pub struct ScrollViewState {
         pub offset: u16,
         pub selected_block: usize,
         pub terminal_expanded: bool,
         pub terminal_active: bool,
     }
     ```
   - Methods: `scroll_up(n)`, `scroll_down(n)`, `navigate_up()`, `navigate_down(blocks)`, `snap_to_bottom(total, viewport)`, `is_at_bottom(total, viewport) -> bool`

4. **`src/scroll_view/render.rs`** (new)
   - `render_scroll_view(state, blocks, terminal, area, buf)` free function
   - Compute block heights given current terminal state and expansion flag
   - Iterate blocks; skip fully-out-of-view blocks; render visible portion of each block
   - Paragraph rendering: line-wrapping, highlight selected block with a left border or background
   - Delegate terminal rendering to `TerminalWidget`

5. **`src/app.rs`** (rewrite)
   - `App { events, terminal, scroll_view: ScrollViewState, blocks: Vec<ContentBlock> }`
   - Remove `screen: Screen`, `focus: Focus`, `selected_path`
   - `run()` loop: full-screen layout → call `render_scroll_view`
   - Key dispatch:
     - `Ctrl+O` → activate terminal, select terminal block, snap to bottom
     - `Up` / `k` → navigate up (if terminal not active)
     - `Down` / `j` → navigate down (if terminal not active)
     - `PageUp` / `PageDown` → scroll offset ±viewport_height
     - `Space` (terminal selected) → toggle expand
     - `Enter` (terminal selected) → activate terminal
     - All other keys (terminal active, including `Esc`) → `key_event_to_bytes` → PTY; `Esc` must not be intercepted so TUI apps running inside remain usable
   - `TerminalOutput` event → `was_at_bottom` check → process → maybe snap offset

6. **`src/main.rs`**: update `App::new()` to initialize content blocks (10–15 lorem ipsum paragraphs) and remove picker/provider initialization

7. **Delete**: `src/picker/` directory

## File Listing

```diff
  src/
+   scroll_view/
+     mod.rs            + ContentBlock enum, ScrollViewState struct and methods
+     render.rs         + render_scroll_view() — block height computation and draw logic
!   terminal/
!     state.rs          ! bump scrollback_len to 1000; skip row-resize in resize()
!     ui.rs             ! TerminalWidget: expanded/selected/active flags; scrollback rows
!   app.rs              ! rewrite: new App struct, full-screen scroll view, new key bindings
!   main.rs             ! init lorem ipsum blocks; remove picker/provider wiring from App init
    picker/             (unchanged — kept for future use)
```

## Validation Plan

Manual validation (UI work):

- **Scroll behaviour**: launch the app; verify paragraphs scroll with arrow keys and Page Up/Down; verify scroll position persists between frames.
- **Block selection**: arrow keys should move the highlight between paragraphs and the terminal block; selected block should scroll into view automatically.
- **Terminal collapse/expand**: with terminal block selected, press `e`; verify scrollback rows appear above the live screen; press `e` again to collapse.
- **Terminal activation**: with terminal block selected, press `Enter`; type in the terminal including `Esc` (e.g. open vim and verify Esc works inside it); press `Ctrl+O` to return to scroll navigation.
- **Ctrl+O**: press from any position; verify terminal activates and scroll snaps to bottom.
- **Stick to bottom**: scroll to bottom; run a command that produces many lines (`seq 1 100`); verify new lines push old ones up and scroll stays at bottom. Then scroll up a bit; run the command again; verify scroll does NOT auto-advance.
- **Scrollback expand + stick to bottom**: expand terminal, scroll to bottom; run a command; verify scroll stays at bottom as scrollback grows.

## Out of Scope

- Real transcript content (lorem ipsum is the prototype stand-in)
- Transcript picker restoration
- Mouse support for scrolling
- Scrollbar widget
- Paragraph editing or interaction beyond selection highlight
