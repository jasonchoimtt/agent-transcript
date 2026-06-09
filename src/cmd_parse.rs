use std::io::IsTerminal as _;

use crossterm::style::{Color, Stylize as _};

use crate::config::Config;
use crate::providers::{LoadConfig, ProviderKind, TranscriptReader};
use crate::reader_op::ReaderOp;
use crate::transforms::{apply_batch, build_transforms};
use crate::tree_scroll_view::TreeScrollViewState;
use crate::tree_scroll_view::state::{MessageState, MessageType};

/// `agt parse [--waterfall] [--debug] <provider>:<session-id>` — load a transcript, apply the
/// transform pipeline, and pretty-print the resulting tree to stdout, then exit.
pub async fn run(session_arg: &str, config: &Config, waterfall: bool) -> color_eyre::Result<()> {
    let colon_pos = session_arg
        .find(':')
        .ok_or_else(|| color_eyre::eyre::eyre!("usage: agt parse <provider>:<session-id>"))?;
    let provider_str = &session_arg[..colon_pos];
    let session_id = &session_arg[colon_pos + 1..];

    let provider: ProviderKind = provider_str
        .parse()
        .map_err(|()| color_eyre::eyre::eyre!("unknown provider: {}", provider_str))?;

    let use_color = std::io::stdout().is_terminal();

    let provider_obj = provider.as_provider();
    let path = provider_obj
        .find_transcript_path(session_id, None)
        .ok_or_else(|| color_eyre::eyre::eyre!("session not found: {}", session_id))?;
    let mut reader = provider_obj
        .open_reader(
            &path,
            LoadConfig {
                initial_loaded: 0,
                waterfall,
                snapshot: true,
            },
        )
        .await?;
    drain_transform_print(&mut reader, config, &provider, use_color).await;

    Ok(())
}

async fn drain_transform_print(
    reader: &mut Box<dyn TranscriptReader>,
    config: &Config,
    provider: &ProviderKind,
    use_color: bool,
) {
    let mut raw_ops = Vec::new();
    while let Some(item) = reader.updates().recv().await {
        match item {
            Ok(op) => raw_ops.push(op),
            Err(e) => {
                eprintln!("reader error: {e:#}");
                break;
            }
        }
    }
    eprintln!("{} raw ops loaded", raw_ops.len());

    let mut transforms = build_transforms(
        &config.transforms,
        provider,
        None,
        &config.widgets.tool_result.file_delta,
    );
    let ops = apply_batch(raw_ops, &mut transforms);
    eprintln!("{} ops after transforms", ops.len());

    let mut tree = TreeScrollViewState::new(vec![]);
    for op in &ops {
        if let ReaderOp::Reset { id } = op {
            eprintln!("RESET id={}", id.as_deref().unwrap_or("<none>"));
        }
    }
    tree.apply(ops);

    let roots: Vec<&MessageState> = tree.items.iter().filter(|n| !n.is_terminal).collect();
    eprintln!("{} top-level nodes\n", roots.len());

    for node in roots {
        print_node(node, 0, use_color);
        println!();
    }
}

fn type_color(mt: &MessageType) -> Color {
    match mt {
        MessageType::UserMessage => Color::Cyan,
        MessageType::AgentMessage => Color::Green,
        MessageType::ToolCall => Color::Yellow,
        MessageType::ToolResult => Color::Yellow,
        MessageType::Thinking => Color::Magenta,
        MessageType::Container => Color::Blue,
        MessageType::TaskSummary => Color::Green,
        MessageType::System => Color::DarkGrey,
        MessageType::Json => Color::White,
        MessageType::Table => Color::Cyan,
        MessageType::Other => Color::White,
    }
}

fn print_node(node: &MessageState, depth: usize, color: bool) {
    let indent = "  ".repeat(depth);
    let type_name = node.message_type.variant_name();

    let mut flags: Vec<&str> = Vec::new();
    if node.expanded {
        flags.push("expanded");
    }
    if node.show_more {
        flags.push("show_more");
    }
    if node.group {
        flags.push("group");
    }
    if node.hidden.is_hidden() {
        flags.push("hidden");
    }

    let ui_state_name = node.ui_state.as_ref().map(|s| {
        let full = s.type_name();
        full.rsplit("::").next().unwrap_or(full)
    });

    // Header line: type  [flags]  id=…  ui=…  "brief"  (N children)
    if color {
        let flags_part = if flags.is_empty() {
            String::new()
        } else {
            format!("  {}", format!("[{}]", flags.join(", ")).dark_grey())
        };
        let id_part = format!("  {}", format!("id={}", node.id).dark_grey());
        let ui_part = ui_state_name
            .map(|n| format!("  {}", format!("ui={n}").cyan()))
            .unwrap_or_default();
        let brief_part = node
            .brief
            .as_deref()
            .map(|b| format!("  {}", format!("\"{}\"", truncate_str(b, 80)).yellow()))
            .unwrap_or_default();
        let children_part = if node.children.is_empty() {
            String::new()
        } else {
            format!(
                "  {}",
                format!("({} children)", node.children.len()).dark_grey()
            )
        };
        println!(
            "{}{}{}{}{}{}{}",
            indent,
            type_name.with(type_color(&node.message_type)).bold(),
            flags_part,
            id_part,
            ui_part,
            brief_part,
            children_part,
        );
    } else {
        let flags_str = if flags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", flags.join(", "))
        };
        let ui_str = ui_state_name
            .map(|n| format!("  ui={n}"))
            .unwrap_or_default();
        let brief_str = node
            .brief
            .as_deref()
            .map(|b| format!("  \"{}\"", truncate_str(b, 80)))
            .unwrap_or_default();
        let children_str = if node.children.is_empty() {
            String::new()
        } else {
            format!("  ({} children)", node.children.len())
        };
        println!(
            "{}{}{}  id={}{}{}{}",
            indent, type_name, flags_str, node.id, ui_str, brief_str, children_str,
        );
    }

    // Text content (same in both color modes; overflow indicator is dimmed when colored).
    if let Some(text) = &node.text {
        const LIMIT: usize = 6;
        let lines: Vec<&str> = text.lines().collect();
        for line in lines.iter().take(LIMIT) {
            let s = truncate_str(line, 120);
            if !s.is_empty() {
                println!("{}  {}", indent, s);
            }
        }
        if lines.len() > LIMIT {
            let overflow = format!("… ({} more lines)", lines.len() - LIMIT);
            if color {
                println!("{}  {}", indent, overflow.dark_grey());
            } else {
                println!("{}  {}", indent, overflow);
            }
        }
    }

    for child in &node.children {
        print_node(child, depth + 1, color);
    }
}

/// Truncate `s` to at most `max_chars` Unicode scalar values, returning a sub-slice.
fn truncate_str(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
