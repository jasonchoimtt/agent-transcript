use crate::tree_scroll_view::{MessageState, MessageType};

pub fn build_key_shortcuts_nodes() -> Vec<MessageState> {
    let categories: &[(&str, &[(&str, &str)])] = &[
        ("In-chat", &[("Ctrl-O", "Exit chat mode")]),
        (
            "Chat session",
            &[
                ("Ctrl-Y", "Resume / launch session"),
                ("Ctrl-X", "Open session picker"),
                ("Ctrl-K", "Kill session"),
                ("Ctrl-M", "Send Ctrl-O to terminal"),
            ],
        ),
        (
            "Navigation",
            &[
                ("h / l", "Select parent / child"),
                ("j / k", "Select next / previous"),
                ("Ctrl-D / Ctrl-U", "Scroll half page down / up"),
                ("Ctrl-N / Ctrl-P", "Scroll 3 lines down / up"),
                ("g / G", "Jump to first / last item"),
                ("H / M / L", "Select top / middle / bottom of viewport"),
                ("zt / zz / zb", "Scroll selection to top / middle / bottom"),
                ("]] / ][", "Next turn start / end"),
                ("[[ / []", "Prev turn start / end"),
                (") / (", "Next / prev message group start"),
                ("} / {", "Next / prev user or agent message"),
                ("m<char> / dm<char>", "Set / delete mark on current message"),
                ("'<char> / `<char>", "Go to mark"),
                ("Ctrl-T", "Pop jump list (return to previous position)"),
            ],
        ),
        (
            "Drill-down",
            &[
                ("Space", "Cycle display / toggle show-more"),
                ("r", "Open raw data view for selected message"),
                ("Enter", "Toggle expand"),
                ("o / c", "Expand / collapse selected node"),
                (
                    "O / C",
                    "Expand+reveal hidden children / collapse+hide revealed",
                ),
                ("J / K", "Reveal next / prev 5 hidden nodes"),
                (
                    "zJ / zK",
                    "Reveal all hidden and jump past run (fwd / back)",
                ),
                ("zh", "Reveal all hidden nodes; again to hide all revealed"),
                ("Y / yy", "Copy markdown"),
                ("yt", "Copy plain text"),
                ("yr", "Copy raw"),
            ],
        ),
        (
            "Table mode",
            &[
                ("h / j / k / l", "Navigate"),
                ("+ / -", "Resize column wider / narrower"),
                ("0", "Reset table layout"),
                ("Esc", "Exit table mode"),
            ],
        ),
        (
            "Info & Debug",
            &[
                ("Shift-I", "Open session info view"),
                (":", "Open this key shortcuts view"),
                ("!s", "Toggle debug info in status bar"),
                ("!l", "Open reader log view"),
                ("!L", "Enable debug file logging"),
                ("!r", "Restart reader (with confirm)"),
            ],
        ),
    ];

    let mut nodes = Vec::new();
    for (category_label, bindings) in categories {
        let cat_id = category_label.to_lowercase().replace(' ', "_");
        let children: Vec<MessageState> = bindings
            .iter()
            .enumerate()
            .map(|(i, (key, desc))| {
                MessageState::new(format!("shortcuts.{cat_id}.{i}"))
                    .text(format!("{key:<16}{desc}"))
                    .message_type(MessageType::Other)
                    .tag("shortcut")
            })
            .collect();
        nodes.push(
            MessageState::new(format!("shortcuts.{cat_id}"))
                .text(category_label.to_string())
                .message_type(MessageType::Other)
                .tag("category")
                .expanded(true)
                .indent_children(true)
                .children(children),
        );
    }
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_categories_present() {
        let nodes = build_key_shortcuts_nodes();
        let labels: Vec<_> = nodes
            .iter()
            .map(|n| n.text.as_deref().unwrap_or(""))
            .collect();
        assert!(labels.contains(&"In-chat"));
        assert!(labels.contains(&"Chat session"));
        assert!(labels.contains(&"Navigation"));
        assert!(labels.contains(&"Drill-down"));
        assert!(labels.contains(&"Table mode"));
        assert!(labels.contains(&"Info & Debug"));
    }

    #[test]
    fn category_nodes_have_correct_structure() {
        let nodes = build_key_shortcuts_nodes();
        for node in &nodes {
            assert_eq!(node.message_type, MessageType::Other);
            assert_eq!(node.tag.as_deref(), Some("category"));
            assert!(node.expanded);
            assert!(!node.children.is_empty());
            for child in &node.children {
                assert_eq!(child.message_type, MessageType::Other);
                assert_eq!(child.tag.as_deref(), Some("shortcut"));
                let text = child.text.as_deref().unwrap_or("");
                assert!(text.len() > 16, "binding text too short: {text:?}");
            }
        }
    }

    #[test]
    fn table_mode_has_resize_and_reset() {
        let nodes = build_key_shortcuts_nodes();
        let table = nodes
            .iter()
            .find(|n| n.text.as_deref() == Some("Table mode"))
            .expect("Table mode category missing");
        let texts: Vec<_> = table
            .children
            .iter()
            .map(|c| c.text.as_deref().unwrap_or(""))
            .collect();
        assert!(texts.iter().any(|t| t.contains("+ / -")));
        assert!(texts.iter().any(|t| t.contains('0') && t.contains("Reset")));
    }
}
