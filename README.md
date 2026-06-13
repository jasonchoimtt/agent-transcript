# agt - Agent and transcript

agt is a transcript TUI and agent CLI combined into one. 

## About agt

**Features**:

- View agent transcripts in a TUI view with vim-style navigation, incremental search, marks, and jump list
- Start or resume your session within agt, with intelligent terminal cropping to provide a seamless experience
- Use your favourite harness -- supports Claude Code and Cursor Agent -- always compatible with latest releases
- Drill down into nested tool calls and sub-agent sessions
- Find your way around the transcript using turn-based key navigation or mouse navigtaion
- Customize tool call formatting and colour palette to your preferences
- Enjoy a smooth and speedy experience regardless of session length

**Philosophy**:

First-party harnesses provide the best performanceand enable users to stay on the cutting edge of agentic coding. However, they are limited in terms of customizability, especially when it comes to the user interface. There are customizable options, but they often come with trade offs in performance, API cost and usability.

agt aims to change that, by providing a lightweight and well-designed TUI wrapper around the agent CLI. With agt, you can keep using your favourite first-party harness, while customizing how your transcript looks.

**Project status**:

- Supports Claude Code and Cursor Agent
- Provides good TUI-based agentic coding and drill-down experience
- Good defaults but limited customizability

## Installation

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/jasonchoimtt/agent-transcript/releases/latest/download/agent-transcript-installer.sh | sh
```

### Build from source

```bash
cargo install agent-transcript
```

If you don't have Cargo installed yet, you can use the [rustup installer](https://rustup.rs/).

### Agent CLI setup

agt uses hooks to detect session changes, so the following setup is required to use agt to start sessions. You can audit the hook source code in [`src/cmd_hook.rs`](./src/cmd_hook.rs).

- Cursor Agent: Run `agt install-hooks cursor`. This adds `agt hook` to your `~/.cursor/hooks.json`.
- Claude: No setup required -- hooks are configured using an auto-loaded plugin using the `--plugin-dir` flag.
 
## Usage

### Working with sessions

To open the transcript picker:

```bash
# Run from your project workspace:
agt
```

Select a transcript by pressing Enter. When in a transcript:

1. Press Ctrl-Y to resume session.
2. When the chat is in focus, press Ctrl-O to return to normal mode. When in normal mode, press Esc to return to chat.
3. Press Shift-P to toggle a pinned prompt box overlay at the bottom of the transcript view. When scrolled above the terminal, a floating overlay also appears when you press Ctrl-O.
4. Press Ctrl-X to open the transcript picker again.
5. To exit the app, exit the agent CLI first (e.g. by pressing Ctrl-D twice), then press q.

You can also open agt to a session directly:

```bash
# Open a session by session ID
agt claude:<uuid>
agt cursor:<uuid>

# Open session and resume
agt --resume claude:<uuid>

# New session
agt claude
agt cursor
```

### Exploring a transcript

**Navigation**:

1. Press `h / j / k / l` or arrow keys to navigate between messages.
2. Press `[[` / `]]` to jump to the previous / next turn start; `[]` / `][` for turn end.
3. Press `) / (` to jump to the next / prev message group; `} / {` for next / prev user or agent message.
4. Press `g` / `G` to jump to the first / last item. Press `H` / `M` / `L` to select the top / middle / bottom of the viewport.
5. Press `zt` / `zz` / `zb` to scroll the selection to the top / middle / bottom of the viewport.
6. Press `Ctrl-N` / `Ctrl-P` to scroll by line, and `Ctrl-D` / `Ctrl-U` to scroll by half page.

**Search**:

7. Press `/` to search forward, `?` to search backward. Type a query and press Enter to commit. Press `n` / `N` to navigate to the next / previous match.

**Drill-down**:

8. Press `Space` to drill down: it cycles between collapsed, show more and expand children.
9. Press `o` / `c` to expand / collapse the selected node. Press `O` / `C` to expand+reveal or collapse+hide hidden children.
10. On a table, press `Enter` to navigate between cells. Resize column with `+ / -`; press `0` to reset layout.
11. Press `r` to view raw message in a JSON viewer.
12. Press `J` / `K` to reveal the next / prev 5 hidden nodes; `zJ` / `zK` to reveal an entire hidden run; `zh` to reveal all hidden nodes (again to re-hide).

**Marks and jump list**:

13. Press `m<char>` to set a mark on the current message, `dm<char>` to delete it, and `'<char>` or `` `<char> `` to jump to it.
14. Press `Ctrl-T` to pop the jump list and return to the previous position.

**Mouse**:

15. Left-click a message to select it; click the indicator area (`+`/`-`) to toggle expand; click a gap row to reveal hidden nodes; click the terminal pane to enter terminal mode.

**Reference**:

16. Press `:` to view all key shortcuts.

### Clipboard support

Auto-detects pbpaste, xsel and xclip. Falls back to OSC 52 (terminal escape sequences).

1. Press `Y / yy` to copy markdown.
2. Press `yt` to copy plain text.
3. Press `yr` to copy raw.

## Configuration

```
~/.config/agent-transcript/     ($XDG_CONFIG_HOME/agent-transcript/)
  config.toml                   # Main configuration file
  palettes/*.toml               # Custom palettes
  styles/*.toml                 # Custom styles
```

- Default config: [src/default.toml](src/default.toml)
- Default palette: [src/theme/dark.toml](src/theme/dark.toml), [src/theme/light.toml](src/theme/light.toml)
- Default styles: [src/theme/styles.toml](src/theme/styles.toml)

The default config is always loaded, so you only need to specify your overrides.

### Agent CLI configuration

In `config.toml`:

```toml
[agents.claude]
binary = "claude"  # Configure command to use
extra_args = []  # Extra args to pass to the agent CLI
```

### Transform: UI initializer

The UI initializer sets the default UI state based on message type and tag:

```toml
[transforms.ui_initializer.types.UserMessage]
expanded = true # Whether children are shown
show_more = true # Whether to show more than one line of content
tags = {
    attachment = { expanded = false, show_more = false }
}
```

Message types:

| Message type | Supported tags          |
|--------------|-------------------------|
| UserMessage  | `attachment`, `summary` |
| AgentMessage |                         |
| ToolCall     | `success`, `error`      |
| ToolResult   |                         |
| Thinking     | `redacted`              |
| Container    |                         |
| TaskSummary  |                         |
| System       |                         |

### Transform: Tool formatter

The tool formatter allows customizing the format of the `Tool(param)` line of a tool call.

Example:

```toml
[[transforms.tool_formatter.rules]]
providers = ["cursor"]
tools = ["Shell"]
template = "{{command}}"
```

The optional `expanded` flag overrides whether the matched tool call is shown expanded (`show_more = true`) or collapsed, regardless of success or failure:

```toml
[[transforms.tool_formatter.rules]]
providers = ["claude"]
tools = ["Write", "Edit"]
template = "{{file_path|path}}"
expanded = true   # always show content, even on success
```

- `expanded = true` — set `expanded = true` on the tool call node even when it succeeds
- `expanded = false` — set `expanded = false` even when it fails
- omitted — no override (default: `expanded = false`, overridden to `true` on failure by UiInitializer)

To disable default rules:

```toml
[transforms.tool_formatter]
disable_defaults = true
```

NOTE: Make sure you use `[[transforms.tool_formatter.rules]]` rather than `[[transforms.tool_formatter.default_rules]]`.

### Transform: Tool grouper

The tool grouper allows grouping of consecutive matching tool calls into a collapsible container.

Example:

```toml
[[transforms.tool_grouper.groups]]
name = "File reads"
tools = ["Read", "Glob", "Grep", "LS"]
min_count = 2
expanded = false
shorten_as_glob = true
```

The optional `expanded` flag controls whether the sealed container shows its children expanded:

- `expanded = true` — children always visible (e.g. useful for file-write or shell groups)
- `expanded = false` — children always hidden
- omitted — error-aware: expand if any child tool failed, otherwise collapse

To disable default groups:

```toml
[transforms.tool_grouper]
disable_defaults = true
```

### Transform: Lua transform

Experimental.

### Palette and styles

In `config.toml`:

```toml
[theme]
mode = "auto"  # auto | light | dark
dark = "dark"  # Name of the dark palette to use: {config_dir}/palettes/{name}.toml
light = "light"  # Name of the light palette to use: {config_dir}/palettes/{name}.toml
styles = "styles"  # Name of the styles to use: {config_dir}/styles/{name}.toml
```

Palettes define colour tokens, which are used by the styles file to determine how to render messages. Refer to the default palette and styles files for more information.

## Contributing

Feel free to open issues or discussions. Pull requests are currently not accepted.
