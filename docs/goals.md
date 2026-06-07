# agent-transcript: Goals & Design

## Problem

Agent CLIs like Claude Code, Cursor Agent, and Codex produce rich structured transcripts, but expose no native way to browse, navigate, or review them. Users are left with raw JSONL files, no drill-down, no context, and no way to observe what the agent actually did.

## Goals

**1. Observability**
- Surface the full agent transcript as a navigable, structured document
- Support jless-style drill-down into nested tool calls and sub-agent sessions
- Vim-style navigation (`j`/`k`, `gg`/`G`, `/` search) so power users feel at home
- Render content richly: markdown, syntax-highlighted code blocks, tool call metadata — matching the quality of Claude Code's own output

**2. Lightweight, non-intrusive integration**
- The TUI wraps the agent CLI process directly; the CLI remains unmodified
- Users keep their existing harness, settings, plugins, and muscle memory
- Compatible with the latest release of any supported CLI — no coupling to internal CLI APIs
- Zero configuration required to get started

**3. Customizability**
- Theming: colours, icons, and layout configurable via a config file
- Drill-down views: pluggable per-block renderers (e.g. show tool call input as a table vs. raw JSON)
- External annotators: shell commands or scripts that receive a transcript entry and emit annotations, rendered inline (e.g. cost estimates, lint results, test outcomes)

## Architecture

### Core layout

The screen is a full-height scroll view with two logical regions:

- **Transcript pane** — structured, navigable rendering of the agent's conversation. Each turn is a `ContentBlock`: user message, assistant text, tool call, tool result, sub-agent session, etc.
- **Terminal pane** — live embedded PTY running the agent CLI, pinned at the bottom of the scroll view. The scrollback buffer is cropped so that only the agent's prompt box and surrounding chrome are visible; older scrollback is hidden, making the seam invisible.

Switching focus between panes is a single keybind (currently `Ctrl+O`). When the terminal pane is active, all keystrokes pass through to the PTY.

### Scrollback cropping

The terminal pane height is computed from the vt100 screen state. Lines above the agent's input box are in "transcript territory" and belong to the structured pane; we render only enough rows to show the live prompt. This is the mechanism that makes the wrapper feel seamless rather than layered.

### Provider adapters

Each supported CLI has a provider adapter responsible for locating transcript files on disk, parsing the wire format, extracting rich metadata (title, timestamps, token counts, tool call summaries), and watching for real-time updates while the CLI is running. Planned providers include Claude Code, Cursor Agent, and OpenAI Codex.

### Annotator interface

External annotators are shell commands that receive a transcript entry and emit annotations rendered inline — enabling cost estimates, lint results, test outcomes, and other enrichments without coupling them into the core app.

## Non-goals

- Replacing the agent CLI or reimplementing its UX
- Managing agent configuration, API keys, or model selection
- Providing an alternative way to start or stop agent sessions (the embedded CLI handles this)
- Supporting non-CLI agent interfaces (API-only, IDE extensions)
