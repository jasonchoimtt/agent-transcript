# agent-transcript

A lightweight TUI wrapper around agent CLIs (Claude Code, Cursor Agent, Codex) that provides rich observability of agent sessions without modifying the underlying CLI. The UI is a navigable scroll view of the agent transcript with an embedded live terminal at the bottom running the CLI directly.

See [`docs/goals.md`](docs/goals.md) for the high-level goals and design.

## Tech stack

- **Rust** (edition 2024) with Tokio async runtime
- **ratatui** for TUI rendering
- **crossterm** for terminal I/O and keyboard events
- **portable-pty** + **vt100** for embedded PTY terminal pane
- **chrono** for timestamps
- **color-eyre** for error handling

## Key files

Under `src/`:

```
# Application
main.rs: CLI argument parsing, entrypoint
app/mod.rs: App startup logic, main event loop, screen transitions
app/draw.rs: Top-level UI layout render
app/keys.rs: Top-level key dispatch

# Terminal
terminal/panel.rs: Terminal panel; manages absent/uninitialized/live/exited state transitions
terminal/state.rs: PTY lifecycle, terminal I/O, OSC event handling

# Tree scroll view
tree_scroll_view/{state,handler,ui}.rs: Tree scroll view widget
tree_scroll_view/message_widget.rs: Single message node widget
tree_scroll_view/table/{handler,render}.rs: In-message table widget

# Agent providers
providers/mod.rs: `Provider` trait + `TranscriptEntry` type
providers/{cursor,claude}/: Provider implementations

# Data views
data_view/session_info.rs: Session metadata view (Shift-I)
data_view/key_shortcuts.rs: Key shortcuts reference view (?)

# Transcript picker
index/mod.rs: SQLite-backed transcript indexer
picker/{state,handler,ui}.rs: Transcript picker

# Transforms
transforms/mod.rs: `TreeTransform` trait
transforms/
  {ui_initializer,tool_grouper,tool_formatter,
   markdown_splitter,table_converter,lua_transform}.rs: Specific transform implementations

# Config and theming
config.rs: Config parsing and defaults
theme/palette.rs: Palette types
theme/styles.rs: Style types
theme/mod.rs: Default theme
```

## First-time setup

After cloning, run once to register the git hooks:
```bash
husky
```

Requires `husky` and `lint-staged` installed globally (`npm i -g husky lint-staged`).

## Development commands

```bash
cargo build          # compile
cargo run            # run the TUI
cargo clippy         # lint
cargo test           # run tests
```

Release build (size-optimized, LTO enabled):
```bash
cargo build --release
```

## Processing pipeline

Raw transcript data flows through these stages:

1. **Provider** (`src/providers/`) — discovers transcript files/DBs via `scan_paths() -> Vec<(PathBuf, SystemTime)>`, reads metadata via `read_entry()`, and opens an async reader via `open_reader()` that streams `TreeOperation`s.

2. **Provider-specific parsing**:
   - Parses from either JSONL or SQLite depending on provider
   - Maintains an internal `ParseState` (seen IDs, current turn, pending tool calls — provider-specific); converts JSON message objects into `TreeOperation`s

3. **Transform pipeline** (`src/transforms/`) — a `tokio::spawn`ed task drains raw ops in batches and folds them through an ordered list of `Transform` stages before forwarding as `Event::TreeOp` to the app:
   - `UiInitializer` — sets `expanded`, `show_more`, `hidden` on each node based on `MessageType` and tag
   - `ToolGrouper` — groups consecutive matching tool-call nodes into a collapsed `Container` parent
   - `ToolFormatter` — rewrites tool call/result text via glob-matched format templates (provider + workspace aware)
   - `MarkdownSplitter` *(opt-in)* — splits AgentMessage text at CommonMark block boundaries into paragraph children
   - `TableConverter` *(opt-in)* — converts single-GFM-table nodes into `MessageType::Table` with `TableUiState`
   - `LuaTransform` *(opt-in, feature-gated)* — runs a user-supplied Lua `process(ops)` function against each batch
   - `Reset` ops are handled specially: preceding ops are flushed, `reset()` is called on every transform, then `Reset` is forwarded

4. **TreeScrollViewState** (`src/tree_scroll_view/state.rs`) — represents the transcript as a tree of messages derived from operations applied to a `Vec<MessageState>`; maintains viewport state (`top_index`, `top_offset`, `selection_index`, `at_bottom`) and an `id_to_path` lookup map

5. **Render** (`src/tree_scroll_view/ui.rs` + `message_widget.rs`) — DFS traversal via `TreeCursor` renders visible `MessageState` nodes as ratatui widgets

**Key types**:
- `MessageState` — `{ id, text, brief, tag, group, is_terminal, message_type, data, props, timestamp, indent_children, children, hidden, expanded, height, ui_state }`
- `TranscriptEntry` — `{ path, id, title, mtime, last_user_message, message_count, workspace_path, provider }`
- `TreeOperation` — `Append { parent_id, message } | Replace { id, message } | Remove { id } | Reset`
- `ParseState` — provider-specific; transient parse context (seen IDs/blobs, current turn ID, pending tool calls, emitted containers)

## Testing conventions

- Test functions go directly in the file, inside `mod tests { }`. For complex tests such as integration tests, they live alongside the module as `tests.rs` and are declared with `#[cfg(test)] mod tests;` in `mod.rs`.
- `cargo test` runs everything; `#[ignore]` marks tests that require external resources (Cursor DB, real filesystem).

**Unit tests**:
- Test pure parsing and state logic in isolation.
- Verify state fields directly (e.g. `parser.screen().size()`, `pending_resize_cols`).

**Render / navigation tests** (`src/tree_scroll_view/tests.rs`):
- Use `ratatui::backend::TestBackend` + `Terminal::new(backend)` to drive `term.draw(...)` headlessly.
- Every navigation test calls `do_layout(state, width, height)` which renders twice and asserts the viewport state is identical on the second pass — catching oscillation bugs automatically.
- Build test fixtures with small builder functions (`short_msg`, `varied_tree`, etc.) rather than constructing large inline trees.

**Integration tests with real PTYs** (`src/terminal/tests.rs`):
- Spawn a child shell with `trap '...' WINCH` or similar to observe OS-level signals via PTY output.
- Read events from the `mpsc::UnboundedReceiver<Event>` passed to `new_with_cmd` — no mock needed.
- Allow 100 ms of startup time before draining initial output; allow 100 ms after a signal before asserting.

## Architecture

Global state lives in `App` (`src/app.rs`), which owns the event loop and renders the full layout. UI is split into modules (e.g. `tree_scroll_view/`, `terminal/`), each providing:

- `state.rs` — owns the module's data and exposes mutation methods
- `ui.rs` — implements `ratatui::widgets::Widget` (or `StatefulWidget`) for rendering; no business logic
- `handler.rs` — maps key events to actions (returns an action enum, does not mutate directly)

`App` dispatches key events to the focused module's handler, interprets the returned action, and mutates state accordingly. New panes follow the same pattern: add a `state/ui/handler` triplet and wire it into `App`.
