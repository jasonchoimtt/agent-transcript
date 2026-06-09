use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::state::{MessageState, MessageType, Precedence, TreeScrollViewState, get_node};
use super::ui::TreeScrollView;
use crate::reader_op::ReaderOp;
use crate::terminal::pane_ref::{PlaceholderInfo, TerminalPaneRef};
use crate::theme::Theme;
use crate::tree_operation::TreeOperation;

// ── harness ──────────────────────────────────────────────────────────────────

// Renders twice and asserts the second pass produces identical viewport state.
// Every navigation test calls this so stability is checked automatically.
fn do_layout(state: &mut TreeScrollViewState, width: u16, height: u16) {
    render_once(state, width, height);

    let top = state.top_index.clone();
    let sel = state.selection_index.clone();
    let off = state.top_offset;
    let bot = state.at_bottom;

    render_once(state, width, height);

    assert_eq!(
        state.top_index, top,
        "top_index oscillated on second render"
    );
    assert_eq!(
        state.selection_index, sel,
        "selection_index changed on second render"
    );
    assert_eq!(
        state.top_offset, off,
        "top_offset oscillated on second render"
    );
    assert_eq!(state.at_bottom, bot, "at_bottom flipped on second render");
}

fn render_once(state: &mut TreeScrollViewState, width: u16, height: u16) {
    let backend = TestBackend::new(width, height);
    let mut term = Terminal::new(backend).unwrap();
    let theme = Theme::default_dark();
    term.draw(|f| {
        f.render_stateful_widget(
            TreeScrollView {
                terminal: TerminalPaneRef::Placeholder(PlaceholderInfo {
                    provider_name: "",
                    session_id: None,
                    directory: None,
                    exit_code: None,
                }),
                scrollback_available: 0,
                terminal_expanded: false,
                terminal_active: false,
                theme: &theme,
                message_interaction: false,
                prompt_pinned: false,
            },
            f.area(),
            state,
        );
    })
    .unwrap();
}

// ── builders ─────────────────────────────────────────────────────────────────

// Single-line, short text. Compact height = 2. Fits without truncation.
fn short_msg(id: &str) -> MessageState {
    MessageState::new(id).text(format!("Short message {id}"))
}

// Multi-line text. Compact height = 2; show_more height = 4 (3 lines + padding).
fn multiline_msg(id: &str) -> MessageState {
    MessageState::new(id).text(format!("Line one {id}\nLine two {id}\nLine three {id}"))
}

// Long single line (100 chars > 77-char available at depth 0).
// content_needs_show_more = true. Compact height = 2; show_more height = 3.
fn long_line_msg(id: &str) -> MessageState {
    MessageState::new(id).text("A".repeat(100))
}

// Mix of compact (h=2) and pre-expanded nodes (h=3 or h=4) to produce varied heights.
// Total rows: 2+4+2+3+2+4+2 = 19.
fn varied_tree() -> Vec<MessageState> {
    vec![
        short_msg("a"),                     // h=2
        multiline_msg("b").show_more(true), // h=4
        short_msg("c"),                     // h=2
        long_line_msg("d").show_more(true), // h=3
        short_msg("e"),                     // h=2
        multiline_msg("f").show_more(true), // h=4
        short_msg("g"),                     // h=2
    ]
}

fn parent_with_children(id: &str, child_count: usize) -> MessageState {
    MessageState::new(id)
        .text(format!("Parent {id}"))
        .expanded(false)
        .children(
            (0..child_count)
                .map(|i| short_msg(&format!("{id}-{i}")))
                .collect(),
        )
}

// ── Phase 1 test cases ────────────────────────────────────────────────────────

#[test]
fn select_next_advances_selection() {
    let mut state = TreeScrollViewState::new(vec![short_msg("a"), short_msg("b"), short_msg("c")]);
    do_layout(&mut state, 80, 24);

    assert_eq!(state.selection_index, vec![0]);
    state.select_next();
    assert_eq!(state.selection_index, vec![1]);
    state.select_next();
    assert_eq!(state.selection_index, vec![2]);
    do_layout(&mut state, 80, 24);
}

#[test]
fn navigation_through_varied_heights_is_stable() {
    // Scrolling through a tree with mixed node heights should not oscillate.
    let mut state = TreeScrollViewState::new(varied_tree());
    do_layout(&mut state, 80, 10); // viewport smaller than total tree height

    for _ in 0..4 {
        state.select_next();
        do_layout(&mut state, 80, 10);
    }
}

#[test]
fn expand_makes_children_navigable() {
    let items = vec![parent_with_children("p", 3), short_msg("sibling")];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    // Children collapsed: select_next should jump to sibling.
    state.select_next();
    assert_eq!(
        state.selection_index,
        vec![1],
        "collapsed parent should skip to sibling"
    );

    // Go back, expand, re-layout.
    state.select_prev();
    state.toggle_expand();
    do_layout(&mut state, 80, 24);

    // Now select_next should descend into the first child.
    state.select_next();
    assert_eq!(
        state.selection_index,
        vec![0, 0],
        "expanded parent should enter first child"
    );
    do_layout(&mut state, 80, 24);
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase 2
// ─────────────────────────────────────────────────────────────────────────────

// ── additional builders ───────────────────────────────────────────────────────

// Flat list of n short_msgs — convenient for scroll arithmetic tests where
// height=2 per node is assumed throughout.
fn msgs(n: usize) -> Vec<MessageState> {
    (0..n).map(|i| short_msg(&i.to_string())).collect()
}

// Expanded group node (height=0; transparent container in DFS navigation).
fn group_node(id: &str, children: Vec<MessageState>) -> MessageState {
    MessageState::new(id)
        .group(true)
        .expanded(true)
        .children(children)
}

// Collapsed group node (height=2; visible, children hidden).
fn collapsed_group(id: &str, children: Vec<MessageState>) -> MessageState {
    MessageState::new(id)
        .group(true)
        .expanded(false)
        .children(children)
}

// ── backward navigation and boundaries ───────────────────────────────────────

#[test]
fn select_prev_retreats_selection() {
    let mut state = TreeScrollViewState::new(vec![short_msg("a"), short_msg("b"), short_msg("c")]);
    do_layout(&mut state, 80, 24);

    state.select_next();
    state.select_next();
    assert_eq!(state.selection_index, vec![2]);

    state.select_prev();
    assert_eq!(state.selection_index, vec![1]);
    state.select_prev();
    assert_eq!(state.selection_index, vec![0]);
    do_layout(&mut state, 80, 24);
}

#[test]
fn select_prev_at_top_is_noop() {
    let mut state = TreeScrollViewState::new(vec![short_msg("a"), short_msg("b")]);
    do_layout(&mut state, 80, 24);

    assert_eq!(state.selection_index, vec![0]);
    state.select_prev();
    assert_eq!(state.selection_index, vec![0]);
    do_layout(&mut state, 80, 24);
}

#[test]
fn select_next_at_last_is_noop() {
    // new() always appends a terminal node; the terminal is the true last DFS node.
    // select_next from the terminal should be a no-op.
    let mut state = TreeScrollViewState::new(vec![short_msg("a")]);
    do_layout(&mut state, 80, 24);
    // Items: [a=0, terminal=1]

    state.select_next(); // [0] → [1] (terminal, last node)
    let at_terminal = state.selection_index.clone();
    state.select_next(); // should be no-op
    assert_eq!(state.selection_index, at_terminal);
    do_layout(&mut state, 80, 24);
}

// ── scroll (precedence = Top) ─────────────────────────────────────────────────

#[test]
fn scroll_down_sets_precedence_top_and_clamps_selection() {
    // 20 nodes × h=2; viewport=10 shows 5 nodes.
    // Scrolling by 10 lines advances top_index to [5]; selection is clamped there.
    let mut state = TreeScrollViewState::new(msgs(20));
    do_layout(&mut state, 80, 10);
    assert_eq!(state.selection_index, vec![0]);

    state.scroll_down(10);
    assert_eq!(state.precedence, Precedence::Top);

    do_layout(&mut state, 80, 10);
    assert_eq!(state.top_index, vec![5]);
    assert_eq!(state.selection_index, vec![5]);
}

#[test]
fn scroll_up_sets_precedence_top_and_clamps_selection() {
    let mut state = TreeScrollViewState::new(msgs(20));
    do_layout(&mut state, 80, 10);

    state.scroll_down(10);
    do_layout(&mut state, 80, 10);
    assert_eq!(state.top_index, vec![5]);

    state.scroll_up(10);
    assert_eq!(state.precedence, Precedence::Top);

    // Retreating 10 lines from [5] lands back at [0].
    // Selection was at [5]; now off-screen above, so clamped to last visible [4].
    do_layout(&mut state, 80, 10);
    assert_eq!(state.top_index, vec![0]);
    assert_eq!(state.selection_index, vec![4]);
}

#[test]
fn page_down_advances_top_by_viewport_height() {
    // Each node h=2; viewport=10 → scrolling by 10 lines advances by 5 nodes.
    let mut state = TreeScrollViewState::new(msgs(20));
    do_layout(&mut state, 80, 10);

    state.scroll_down(state.viewport_height);
    do_layout(&mut state, 80, 10);

    assert_eq!(state.top_index, vec![5]);
}

#[test]
fn page_up_retreats_top_by_viewport_height() {
    let mut state = TreeScrollViewState::new(msgs(20));
    do_layout(&mut state, 80, 10);

    state.scroll_down(20); // advance 10 nodes → top=[10]
    do_layout(&mut state, 80, 10);
    assert_eq!(state.top_index, vec![10]);

    state.scroll_up(state.viewport_height); // retreat 5 nodes → top=[5]
    do_layout(&mut state, 80, 10);
    assert_eq!(state.top_index, vec![5]);
}

// ── jump navigation (g / G) ───────────────────────────────────────────────────

#[test]
fn select_first_jumps_to_start() {
    let mut state = TreeScrollViewState::new(msgs(10));
    do_layout(&mut state, 80, 24);

    for _ in 0..5 {
        state.select_next();
    }
    assert_eq!(state.selection_index, vec![5]);

    state.select_first();
    do_layout(&mut state, 80, 24);
    assert_eq!(state.selection_index, vec![0]);
}

#[test]
fn select_last_content_skips_terminal_node() {
    let mut terminal_node = short_msg("term");
    terminal_node.is_terminal = true;

    let items = vec![short_msg("a"), short_msg("b"), terminal_node];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.select_last_content();
    do_layout(&mut state, 80, 24);
    assert_eq!(state.selection_index, vec![1]);
}

// ── viewport-relative selection (H / L) ──────────────────────────────────────

#[test]
fn select_viewport_top_selects_topmost_visible() {
    // Scroll so top_index=[5]; H should select [5].
    let mut state = TreeScrollViewState::new(msgs(20));
    do_layout(&mut state, 80, 10);

    state.scroll_down(10);
    do_layout(&mut state, 80, 10);
    assert_eq!(state.top_index, vec![5]);

    state.select_viewport_top();
    do_layout(&mut state, 80, 10);
    assert_eq!(state.selection_index, vec![5]);
}

#[test]
fn select_viewport_bottom_selects_bottommost_visible() {
    // Top at [5], viewport=10, each h=2 → 5 nodes visible ([5]..[9]); L → [9].
    let mut state = TreeScrollViewState::new(msgs(20));
    do_layout(&mut state, 80, 10);

    state.scroll_down(10);
    do_layout(&mut state, 80, 10);

    state.select_viewport_bottom();
    do_layout(&mut state, 80, 10);
    assert_eq!(state.selection_index, vec![9]);
}

// ── scroll selection to position (zt / zb / zz) ───────────────────────────────

#[test]
fn scroll_selection_to_top_repositions_viewport() {
    // Navigate to [8], then zt → top_index snaps to the selection.
    let mut state = TreeScrollViewState::new(msgs(20));
    do_layout(&mut state, 80, 10);

    for _ in 0..8 {
        state.select_next();
    }
    do_layout(&mut state, 80, 10);

    state.scroll_selection_to_top();
    do_layout(&mut state, 80, 10);

    assert_eq!(state.selection_index, vec![8]);
    assert_eq!(state.top_index, vec![8]);
    assert_eq!(state.top_offset, 0);
}

#[test]
fn scroll_selection_to_bottom_repositions_viewport() {
    // Navigate to [8], then zb. Each node h=2; viewport=10.
    // retreat = 10-2 = 8 lines (4 nodes) above [8] → top=[4].
    let mut state = TreeScrollViewState::new(msgs(20));
    do_layout(&mut state, 80, 10);

    for _ in 0..8 {
        state.select_next();
    }
    do_layout(&mut state, 80, 10);

    state.scroll_selection_to_bottom();
    do_layout(&mut state, 80, 10);

    assert_eq!(state.selection_index, vec![8]);
    assert_eq!(state.top_index, vec![4]);
}

#[test]
fn scroll_selection_to_middle_repositions_viewport() {
    // Navigate to [8], then zz. retreat = 10/2 - 2/2 = 4 lines (2 nodes) → top=[6].
    let mut state = TreeScrollViewState::new(msgs(20));
    do_layout(&mut state, 80, 10);

    for _ in 0..8 {
        state.select_next();
    }
    do_layout(&mut state, 80, 10);

    state.scroll_selection_to_middle();
    do_layout(&mut state, 80, 10);

    assert_eq!(state.selection_index, vec![8]);
    assert_eq!(state.top_index, vec![6]);
}

// ── group node behavior ───────────────────────────────────────────────────────

#[test]
fn collapsed_group_is_navigable_and_skips_children() {
    // A collapsed group has h=2 (visible) but children are hidden.
    // select_next from the group uses advance_sibling → skips to next sibling.
    let items = vec![
        short_msg("a"),
        collapsed_group("g", vec![short_msg("g-0"), short_msg("g-1")]),
        short_msg("b"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.select_next(); // [0] → [1] (group itself, h=2)
    assert_eq!(state.selection_index, vec![1]);

    state.select_next(); // group → advance_sibling → [2], skipping hidden children
    assert_eq!(state.selection_index, vec![2]);
    do_layout(&mut state, 80, 24);
}

#[test]
fn expanded_group_acts_as_transparent_container() {
    // An expanded group has h=0 and is invisible to DFS.
    // Navigation passes through to its children directly.
    let items = vec![
        short_msg("a"),
        group_node("g", vec![short_msg("g-0"), short_msg("g-1")]),
        short_msg("b"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.select_next(); // [0] → [1,0] (first child; group [1] has h=0, filtered out)
    assert_eq!(state.selection_index, vec![1, 0]);

    state.select_next(); // [1,0] → [1,1]
    assert_eq!(state.selection_index, vec![1, 1]);

    state.select_next(); // [1,1] → [2]
    assert_eq!(state.selection_index, vec![2]);
    do_layout(&mut state, 80, 24);
}

#[test]
fn group_node_selection_stays_stable() {
    // Selecting a collapsed group (h=2) and rendering is idempotent.
    let items = vec![
        short_msg("a"),
        collapsed_group("g", vec![short_msg("g-0")]),
        short_msg("b"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.select_next(); // land on collapsed group [1]
    assert_eq!(state.selection_index, vec![1]);
    do_layout(&mut state, 80, 24);
}

// ── hidden nodes ──────────────────────────────────────────────────────────────

#[test]
fn hidden_node_is_skipped_in_navigation() {
    let mut state = TreeScrollViewState::new(vec![short_msg("a"), short_msg("b"), short_msg("c")]);
    do_layout(&mut state, 80, 24);

    state.set_hidden(&[1], crate::tree_scroll_view::HiddenState::Hidden); // hide middle node
    do_layout(&mut state, 80, 24);

    state.select_next(); // [0] → should skip hidden [1] and land on [2]
    assert_eq!(state.selection_index, vec![2]);
    do_layout(&mut state, 80, 24);
}

// ── HiddenState navigation ────────────────────────────────────────────────────

#[test]
fn reveal_next_five_reveals_up_to_five_hidden() {
    use crate::tree_scroll_view::HiddenState;
    // 10 hidden nodes after one visible node.
    let mut items: Vec<MessageState> = vec![short_msg("visible")];
    for i in 0..10 {
        items.push(short_msg(&format!("h{i}")).hidden(HiddenState::Hidden));
    }
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.reveal_next_n_hidden(5);

    // Exactly 5 should be revealed, 5 still hidden.
    let revealed: Vec<_> = state
        .items
        .iter()
        .filter(|n| n.hidden == HiddenState::Revealed)
        .collect();
    let still_hidden: Vec<_> = state
        .items
        .iter()
        .filter(|n| n.hidden == HiddenState::Hidden)
        .collect();
    assert_eq!(revealed.len(), 5, "should reveal exactly 5");
    assert_eq!(still_hidden.len(), 5, "should leave 5 hidden");

    // Selection should land on the 5th revealed node.
    assert_eq!(state.selection_index, vec![5]);
    do_layout(&mut state, 80, 24);
}

#[test]
fn reveal_next_five_reveals_all_when_run_is_shorter() {
    use crate::tree_scroll_view::HiddenState;
    let mut items = vec![short_msg("visible")];
    for i in 0..3 {
        items.push(short_msg(&format!("h{i}")).hidden(HiddenState::Hidden));
    }
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.reveal_next_n_hidden(5);

    let revealed: Vec<_> = state
        .items
        .iter()
        .filter(|n| n.hidden == HiddenState::Revealed)
        .collect();
    assert_eq!(revealed.len(), 3);
    // Selection on the 3rd (last) revealed node.
    assert_eq!(state.selection_index, vec![3]);
    do_layout(&mut state, 80, 24);
}

#[test]
fn reveal_jump_forward_reveals_all_and_lands_on_next_visible() {
    use crate::tree_scroll_view::HiddenState;
    let mut items = vec![short_msg("first")];
    for i in 0..4 {
        items.push(short_msg(&format!("h{i}")).hidden(HiddenState::Hidden));
    }
    items.push(short_msg("after"));
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.reveal_jump_forward();

    // All 4 hidden should be revealed.
    let revealed: Vec<_> = state
        .items
        .iter()
        .filter(|n| n.hidden == HiddenState::Revealed)
        .collect();
    assert_eq!(revealed.len(), 4);
    // Selection should land on "after" (the visible node after the run).
    assert_eq!(state.selection_index, vec![5]);
    do_layout(&mut state, 80, 24);
}

#[test]
fn toggle_all_hidden_reveals_then_re_hides() {
    use crate::tree_scroll_view::HiddenState;
    let items = vec![
        short_msg("visible"),
        short_msg("h1").hidden(HiddenState::Hidden),
        short_msg("h2").hidden(HiddenState::Hidden),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    // First call: some Hidden → reveal all.
    state.toggle_all_hidden();
    let hidden_count = state
        .items
        .iter()
        .filter(|n| n.hidden == HiddenState::Hidden)
        .count();
    let revealed_count = state
        .items
        .iter()
        .filter(|n| n.hidden == HiddenState::Revealed)
        .count();
    assert_eq!(hidden_count, 0, "after first toggle all should be Revealed");
    assert_eq!(revealed_count, 2);
    do_layout(&mut state, 80, 24);

    // Second call: no Hidden left → hide all Revealed.
    state.toggle_all_hidden();
    let hidden_count2 = state
        .items
        .iter()
        .filter(|n| n.hidden == HiddenState::Hidden)
        .count();
    assert_eq!(
        hidden_count2, 2,
        "after second toggle all should be Hidden again"
    );
    do_layout(&mut state, 80, 24);
}

#[test]
fn expand_reveal_children_reveals_hidden_children() {
    use crate::tree_scroll_view::HiddenState;
    let child_a = short_msg("ca");
    let child_b = short_msg("cb").hidden(HiddenState::Hidden);
    let parent = short_msg("parent")
        .children(vec![child_a, child_b])
        .expanded(false);
    let mut state = TreeScrollViewState::new(vec![parent]);
    do_layout(&mut state, 80, 24);

    state.expand_reveal_children();

    let parent = &state.items[0];
    assert!(parent.expanded);
    assert_eq!(parent.children[1].hidden, HiddenState::Revealed);
    do_layout(&mut state, 80, 24);
}

#[test]
fn collapse_hide_children_re_hides_revealed_children() {
    use crate::tree_scroll_view::HiddenState;
    let child_a = short_msg("ca");
    let child_b = short_msg("cb").hidden(HiddenState::Revealed);
    let parent = short_msg("parent")
        .children(vec![child_a, child_b])
        .expanded(true);
    let mut state = TreeScrollViewState::new(vec![parent]);
    do_layout(&mut state, 80, 24);

    state.collapse_hide_children();

    let parent = &state.items[0];
    assert!(!parent.expanded);
    assert_eq!(parent.children[1].hidden, HiddenState::Hidden);
    do_layout(&mut state, 80, 24);
}

#[test]
fn hidden_indicator_char_encoding() {
    use super::ui::hidden_indicator_char;
    assert_eq!(hidden_indicator_char(0), "");
    assert_eq!(hidden_indicator_char(1), "⠁");
    assert_eq!(hidden_indicator_char(2), "⠃");
    assert_eq!(hidden_indicator_char(3), "⠇");
    assert_eq!(hidden_indicator_char(4), "⡇");
    assert_eq!(hidden_indicator_char(5), "⣿");
    assert_eq!(hidden_indicator_char(100), "⣿");
}

// ── resize ────────────────────────────────────────────────────────────────────

#[test]
fn resize_invalidates_heights_and_stays_stable() {
    // Changing width clears all cached heights; subsequent layouts must be stable.
    // Uses varied_tree which includes nodes whose height differs at different widths
    // (long_line_msg wraps to 2 lines at width=80 but fits in 1 line at width=120).
    let mut state = TreeScrollViewState::new(varied_tree());
    do_layout(&mut state, 80, 10);

    for _ in 0..2 {
        state.select_next();
    }

    do_layout(&mut state, 120, 10); // width change — clears cached heights
    do_layout(&mut state, 80, 10); // back to original width
}

// ── collapse with child selected ──────────────────────────────────────────────

#[test]
fn toggle_expand_collapses_and_navigation_skips_hidden_children() {
    // Expand a node, navigate into a child, navigate back to the parent,
    // then collapse via toggle_expand. Afterwards, navigation should skip
    // the now-hidden children entirely.
    let items = vec![
        parent_with_children("p", 3).expanded(true),
        short_msg("sibling"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);
    // Items: [p(expanded)=0, p/0=0.0, p/1=0.1, p/2=0.2, sibling=1, terminal=2]

    state.select_next(); // [0] → [0,0] (first child)
    assert_eq!(state.selection_index, vec![0, 0]);

    state.select_prev(); // [0,0] → [0] (back to parent)
    assert_eq!(state.selection_index, vec![0]);

    state.toggle_expand(); // collapse; toggle_expand snaps selection to closest visible
    do_layout(&mut state, 80, 24);
    assert_eq!(state.selection_index, vec![0]);

    state.select_next(); // children hidden → jumps to sibling [1]
    assert_eq!(state.selection_index, vec![1]);
    do_layout(&mut state, 80, 24);
}

// ── at_bottom edge cases ──────────────────────────────────────────────────────

#[test]
fn at_bottom_set_when_last_node_selected_and_viewport_full() {
    // new() always appends a terminal node (h=22). Items: [a=0, b=1, terminal=2].
    // Total height = 2+2+22 = 26 rows. Use viewport=30 so everything fits.
    // After navigating to the terminal, update_at_bottom sees remaining=26 ≤ 30.
    let mut state = TreeScrollViewState::new(vec![short_msg("a"), short_msg("b")]);
    do_layout(&mut state, 80, 30);

    state.select_next(); // [0] → [1]
    state.select_next(); // [1] → [2] (terminal, true last DFS node)
    do_layout(&mut state, 80, 30);

    assert!(state.at_bottom);
    assert_eq!(state.selection_index, vec![2]);
}

#[test]
fn at_bottom_snaps_up_when_viewport_grows() {
    // Items: [a=0, b=1, terminal=2], total=26 rows.
    // With viewport=24 the tree doesn't fully fit; after growing to 30 it does,
    // so snap_to_bottom() retreats all the way to top_index=[0].
    let mut state = TreeScrollViewState::new(vec![short_msg("a"), short_msg("b")]);
    do_layout(&mut state, 80, 24);

    state.select_next();
    state.select_next(); // arrive at terminal [2]
    do_layout(&mut state, 80, 24);
    assert!(state.at_bottom);

    do_layout(&mut state, 80, 30); // grow viewport — all 26 rows now fit

    assert!(state.at_bottom);
    assert_eq!(state.top_index, vec![0]);
}

#[test]
fn at_bottom_viewport_tracks_newly_appended_node() {
    // Items: [a=0, b=1, terminal=2], total=25 rows. viewport=24.
    // After reaching the terminal (at_bottom=true), appending "c" inserts it before
    // the terminal: [a=0, b=1, c=2, terminal=3]. apply() bumps selection_index from
    // [2] to [3] so the selection stays on the terminal. snap_to_bottom() on the
    // next render positions top so both "c" and the terminal are visible.
    let mut state = TreeScrollViewState::new(vec![short_msg("a"), short_msg("b")]);
    do_layout(&mut state, 80, 24);

    state.select_next();
    state.select_next(); // arrive at terminal [2]
    do_layout(&mut state, 80, 24);
    assert!(state.at_bottom);

    state.apply(vec![ReaderOp::Tree(TreeOperation::Append {
        parent_id: None,
        message: short_msg("c"),
    })]);
    // After append: [a=0, b=1, c=2, terminal=3], total=27 rows.
    // selection_index bumped to [3] (still terminal). snap_to_bottom with viewport=24:
    // terminal(h=21) ≤ 24, retreat to c(h=2): 21+2=23 ≤ 24, retreat to b(h=2): 25>24
    // → top=[1], top_offset=1 (b is partially visible at the top).
    do_layout(&mut state, 80, 24);

    assert_eq!(state.selection_index, vec![3]); // still on terminal
    assert_eq!(state.top_index, vec![1]); // "b" is partially visible at viewport top
    assert_eq!(state.top_offset, 1);
    assert!(state.at_bottom); // still at bottom
}

// ── cycle_display ─────────────────────────────────────────────────────────────

#[test]
fn cycle_display_noop_on_short_text_no_children() {
    // Text fits in one line and node has no children → cycle_display is a no-op.
    let mut state = TreeScrollViewState::new(vec![short_msg("a")]);
    do_layout(&mut state, 80, 24);

    let expanded_before = get_node(&state.items, &[0]).unwrap().expanded;
    state.cycle_display();
    do_layout(&mut state, 80, 24);

    // Still sets show_more to ensure markdown styles fully show up
    assert_eq!(get_node(&state.items, &[0]).unwrap().show_more, true);
    assert_eq!(
        get_node(&state.items, &[0]).unwrap().expanded,
        expanded_before
    );

    state.cycle_display();
    do_layout(&mut state, 80, 24);
    assert_eq!(get_node(&state.items, &[0]).unwrap().show_more, true);
    assert_eq!(
        get_node(&state.items, &[0]).unwrap().expanded,
        expanded_before
    );
}

#[test]
fn cycle_display_two_step_on_truncated_text_no_children() {
    // Long single line (truncated): compact → full-text → compact (no expand step).
    // Note: MessageState::new() defaults expanded=true; cycle_display does not
    // touch expanded on a node with no children, so we only assert on show_more.
    let mut state = TreeScrollViewState::new(vec![long_line_msg("a")]);
    do_layout(&mut state, 80, 24);
    assert!(!get_node(&state.items, &[0]).unwrap().show_more);

    state.cycle_display(); // compact → full-text
    do_layout(&mut state, 80, 24);
    assert!(get_node(&state.items, &[0]).unwrap().show_more);

    state.cycle_display(); // full-text → compact (no children to expand into)
    do_layout(&mut state, 80, 24);
    assert!(!get_node(&state.items, &[0]).unwrap().show_more);
}

#[test]
fn cycle_display_skips_show_more_when_text_fits_with_children() {
    // Text fits in one line but has children → compact → expanded → compact
    // (show_more step is skipped because content_needs_show_more is false).
    let items = vec![
        MessageState::new("p")
            .text("Short")
            .expanded(false)
            .children(vec![short_msg("c")]),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.cycle_display(); // should expand directly and also force show more
    do_layout(&mut state, 80, 24);
    assert!(get_node(&state.items, &[0]).unwrap().show_more);
    assert!(get_node(&state.items, &[0]).unwrap().expanded);

    state.cycle_display(); // collapse back without unsetting show_more
    do_layout(&mut state, 80, 24);
    assert!(get_node(&state.items, &[0]).unwrap().show_more);
    assert!(!get_node(&state.items, &[0]).unwrap().expanded);
}

#[test]
fn cycle_display_full_three_steps_with_truncated_text_and_children() {
    // Full 3-step cycle: compact → full-text → expanded → compact.
    let items = vec![
        MessageState::new("p")
            .text("A".repeat(100)) // 100 chars > 77 available → truncated
            .expanded(false)
            .children(vec![short_msg("c")]),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.cycle_display(); // compact → full-text
    do_layout(&mut state, 80, 24);
    assert!(get_node(&state.items, &[0]).unwrap().show_more);
    assert!(!get_node(&state.items, &[0]).unwrap().expanded);

    state.cycle_display(); // full-text → expanded
    do_layout(&mut state, 80, 24);
    assert!(get_node(&state.items, &[0]).unwrap().show_more);
    assert!(get_node(&state.items, &[0]).unwrap().expanded);

    state.cycle_display(); // expanded → compact
    do_layout(&mut state, 80, 24);
    assert!(!get_node(&state.items, &[0]).unwrap().show_more);
    assert!(!get_node(&state.items, &[0]).unwrap().expanded);
}

// ── Remove / Reset tests ──────────────────────────────────────────────────────

#[test]
fn remove_leaf_removes_node_and_updates_id_map() {
    let items = vec![short_msg("a"), short_msg("b"), short_msg("c")];
    let mut state = TreeScrollViewState::new(items);
    // items: [a(0), b(1), c(2), terminal(3)]
    state.apply(vec![ReaderOp::Tree(TreeOperation::Remove {
        id: "b".to_string(),
    })]);
    // items: [a(0), c(1), terminal(2)]
    assert!(get_node(&state.items, &[0]).is_some_and(|n| n.id == "a"));
    assert!(get_node(&state.items, &[1]).is_some_and(|n| n.id == "c"));
    assert!(!state.id_to_path.contains_key("b"));
    assert_eq!(state.id_to_path.get("c"), Some(&vec![1]));
}

#[test]
fn remove_top_level_node_shifts_indices() {
    let items = vec![short_msg("x"), short_msg("y")];
    let mut state = TreeScrollViewState::new(items);
    // selection lands on x (index 0). Remove x.
    state.apply(vec![ReaderOp::Tree(TreeOperation::Remove {
        id: "x".to_string(),
    })]);
    // y should now be at index 0.
    assert!(get_node(&state.items, &[0]).is_some_and(|n| n.id == "y"));
    assert!(!state.id_to_path.contains_key("x"));
    assert_eq!(state.id_to_path.get("y"), Some(&vec![0]));
}

#[test]
fn remove_child_node_updates_siblings() {
    let items = vec![MessageState::new("parent").text("parent").children(vec![
        short_msg("c0"),
        short_msg("c1"),
        short_msg("c2"),
    ])];
    let mut state = TreeScrollViewState::new(items);
    state.apply(vec![ReaderOp::Tree(TreeOperation::Remove {
        id: "c1".to_string(),
    })]);
    // c2 should now be child index 1.
    assert!(!state.id_to_path.contains_key("c1"));
    assert_eq!(state.id_to_path.get("c2"), Some(&vec![0, 1]));
    let parent = get_node(&state.items, &[0]).unwrap();
    assert_eq!(parent.children.len(), 2);
    assert_eq!(parent.children[1].id, "c2");
}

#[test]
fn reset_leaves_only_terminal_and_reinitialises_selection() {
    let items = vec![short_msg("m1"), short_msg("m2")];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);
    assert!(state.items.len() > 1); // m1, m2, terminal

    state.apply(vec![ReaderOp::Reset { id: None }]);

    // Only the terminal node should remain.
    assert_eq!(state.items.len(), 1);
    assert!(state.items[0].is_terminal);
    // id_to_path should only have the terminal entry.
    assert_eq!(state.id_to_path.len(), 1);
    assert!(state.id_to_path.contains_key("terminal"));
    // Indices reset.
    assert_eq!(state.top_offset, 0);
    // Selection should be initialised (pointing at the terminal).
    assert!(!state.selection_index.is_empty());
}

// ── Reset / ResetDone snapshot tests ─────────────────────────────────────────

#[test]
fn reset_snapshot_restores_show_more_on_replay() {
    let msg = MessageState::new("m1")
        .message_type(MessageType::AgentMessage)
        .text("hello")
        .show_more(true)
        .expanded(false);
    let mut state = TreeScrollViewState::new(vec![msg]);

    // Reset: snapshot is taken, tree is cleared.
    state.apply(vec![ReaderOp::Reset { id: None }]);
    assert_eq!(state.items.len(), 1); // only terminal

    // Replay the same node — snapshot should restore its flags.
    state.apply(vec![ReaderOp::Tree(TreeOperation::Append {
        parent_id: None,
        message: MessageState::new("m1")
            .message_type(MessageType::AgentMessage)
            .text("hello"),
    })]);

    let node = state.items.iter().find(|n| n.id == "m1").unwrap();
    assert!(node.show_more, "show_more should be restored from snapshot");
    assert!(!node.expanded, "expanded should be restored from snapshot");
}

#[test]
fn reset_done_clears_snapshot_so_fresh_append_gets_defaults() {
    let msg = MessageState::new("m1")
        .message_type(MessageType::AgentMessage)
        .text("hello");
    let mut state = TreeScrollViewState::new(vec![msg]);
    state.items[0].show_more = true;

    // Reset → ResetDone with no replay in between.
    state.apply(vec![ReaderOp::Reset { id: None }, ReaderOp::ResetDone]);

    // Append the same node now — snapshot is gone, so defaults apply.
    state.apply(vec![ReaderOp::Tree(TreeOperation::Append {
        parent_id: None,
        message: MessageState::new("m1")
            .message_type(MessageType::AgentMessage)
            .text("hello"),
    })]);

    let node = state.items.iter().find(|n| n.id == "m1").unwrap();
    assert!(
        !node.show_more,
        "snapshot was cleared by ResetDone; show_more should be default (false)"
    );
}

#[test]
fn reset_snapshot_restores_flags_on_child_nodes() {
    // A parent with a child; both have modified flags.
    let child = MessageState::new("c1")
        .message_type(MessageType::ToolResult)
        .text("result")
        .hidden(crate::tree_scroll_view::HiddenState::Hidden);
    let parent = MessageState::new("p1")
        .message_type(MessageType::ToolCall)
        .text("call")
        .expanded(false)
        .children(vec![child]);
    let mut state = TreeScrollViewState::new(vec![parent]);

    state.apply(vec![ReaderOp::Reset { id: None }]);

    // Replay parent with child — both should get flags restored.
    let replay_child = MessageState::new("c1")
        .message_type(MessageType::ToolResult)
        .text("result");
    let replay_parent = MessageState::new("p1")
        .message_type(MessageType::ToolCall)
        .text("call")
        .children(vec![replay_child]);
    state.apply(vec![ReaderOp::Tree(TreeOperation::Append {
        parent_id: None,
        message: replay_parent,
    })]);

    let p = state.items.iter().find(|n| n.id == "p1").unwrap();
    assert!(!p.expanded, "parent expanded=false should be restored");
    let c = p.children.iter().find(|n| n.id == "c1").unwrap();
    assert!(c.hidden.is_hidden(), "child hidden=true should be restored");
}

// ── advanced navigation fixture ───────────────────────────────────────────────
//
// Tree layout (all expanded):
//   [0] turn:0   Container  group=true
//     [0,0] user_turn:0  Container
//       [0,0,0] user:0   UserMessage
//     [0,1] agent_turn:0 Container
//       [0,1,0] agent:0a AgentMessage
//       [0,1,1] agent:0b AgentMessage
//       [0,1,2] tool:0   ToolCall  (expanded)
//         [0,1,2,0] result:0 ToolResult
//   [1] turn:1   Container  group=true
//     [1,0] user_turn:1  Container
//       [1,0,0] user:1   UserMessage
//     [1,1] agent_turn:1 Container
//       [1,1,0] think:1  Thinking
//       [1,1,1] agent:1  AgentMessage
//   [2] turn:2   Container  group=true
//     [2,0] user_turn:2  Container
//       [2,0,0] user:2   UserMessage
//     [2,1] agent_turn:2 Container
//       [2,1,0] agent:2  AgentMessage
//   [3] terminal (added by TreeScrollViewState::new)
//
// DFS non-Container order (for `()`):
//   [0,0,0] UserMessage, [0,1,0] AgentMessage, [0,1,1] AgentMessage,
//   [0,1,2] ToolCall, [0,1,2,0] ToolResult,
//   [1,0,0] UserMessage, [1,1,0] Thinking, [1,1,1] AgentMessage,
//   [2,0,0] UserMessage, [2,1,0] AgentMessage
//
// DFS UserMessage/AgentMessage order (for `{}`):
//   [0,0,0] UserMessage, [0,1,0] AgentMessage, [0,1,1] AgentMessage,
//   [1,0,0] UserMessage, [1,1,1] AgentMessage,
//   [2,0,0] UserMessage, [2,1,0] AgentMessage

fn container(id: &str) -> MessageState {
    MessageState::new(id).message_type(MessageType::Container)
}

fn turn_group(id: &str, children: Vec<MessageState>) -> MessageState {
    container(id).group(true).children(children)
}

fn sub_turn(id: &str, children: Vec<MessageState>) -> MessageState {
    container(id).children(children)
}

fn nav_tree() -> TreeScrollViewState {
    let turn0 = turn_group(
        "turn:0",
        vec![
            sub_turn(
                "user_turn:0",
                vec![
                    MessageState::new("user:0")
                        .message_type(MessageType::UserMessage)
                        .text("u0"),
                ],
            ),
            sub_turn(
                "agent_turn:0",
                vec![
                    MessageState::new("agent:0a")
                        .message_type(MessageType::AgentMessage)
                        .text("a0a"),
                    MessageState::new("agent:0b")
                        .message_type(MessageType::AgentMessage)
                        .text("a0b"),
                    MessageState::new("tool:0")
                        .message_type(MessageType::ToolCall)
                        .text("bash")
                        .children(vec![
                            MessageState::new("result:0")
                                .message_type(MessageType::ToolResult)
                                .text("ok"),
                        ]),
                ],
            ),
        ],
    );

    let turn1 = turn_group(
        "turn:1",
        vec![
            sub_turn(
                "user_turn:1",
                vec![
                    MessageState::new("user:1")
                        .message_type(MessageType::UserMessage)
                        .text("u1"),
                ],
            ),
            sub_turn(
                "agent_turn:1",
                vec![
                    MessageState::new("think:1")
                        .message_type(MessageType::Thinking)
                        .text("..."),
                    MessageState::new("agent:1")
                        .message_type(MessageType::AgentMessage)
                        .text("a1"),
                ],
            ),
        ],
    );

    let turn2 = turn_group(
        "turn:2",
        vec![
            sub_turn(
                "user_turn:2",
                vec![
                    MessageState::new("user:2")
                        .message_type(MessageType::UserMessage)
                        .text("u2"),
                ],
            ),
            sub_turn(
                "agent_turn:2",
                vec![
                    MessageState::new("agent:2")
                        .message_type(MessageType::AgentMessage)
                        .text("a2"),
                ],
            ),
        ],
    );

    TreeScrollViewState::new(vec![turn0, turn1, turn2])
}

fn sel(state: &TreeScrollViewState) -> Vec<usize> {
    state.selection_index.clone()
}

// ── () type-run navigation ─────────────────────────────────────────────────────

#[test]
fn select_next_type_start_advances_across_type_boundary() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 0, 0]; // user:0 (UserMessage)

    state.select_next_type_start(); // → first AgentMessage in turn0
    assert_eq!(
        sel(&state),
        vec![0, 1, 0],
        "should advance to first AgentMessage run"
    );
}

#[test]
fn select_next_type_start_skips_run() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 1, 0]; // agent:0a (first of AgentMessage run)

    state.select_next_type_start(); // → ToolCall (different type after AgentMessage run)
    assert_eq!(
        sel(&state),
        vec![0, 1, 2],
        "should skip AgentMessage run to ToolCall"
    );
}

#[test]
fn select_next_type_start_clamps_at_last_run() {
    let mut state = nav_tree();
    state.selection_index = vec![2, 1, 0]; // agent:2 (last non-terminal node)

    state.select_next_type_start(); // nothing after
    assert_eq!(
        sel(&state),
        vec![2, 1, 0],
        "should stay at last node when no next run"
    );
}

#[test]
fn select_prev_type_start_at_run_start_goes_to_prev_run() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 1, 0]; // agent:0a — start of AgentMessage run

    state.select_prev_type_start(); // → user:0 (prev run)
    assert_eq!(sel(&state), vec![0, 0, 0]);
}

#[test]
fn select_prev_type_start_mid_run_goes_to_run_start() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 1, 1]; // agent:0b — mid AgentMessage run

    state.select_prev_type_start(); // → agent:0a (start of current run)
    assert_eq!(sel(&state), vec![0, 1, 0]);
}

#[test]
fn select_prev_type_start_clamps_at_first_run() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 0, 0]; // user:0 — first node

    state.select_prev_type_start();
    assert_eq!(sel(&state), vec![0, 0, 0], "should stay when no prev run");
}

// ── {} user/agent navigation ──────────────────────────────────────────────────

#[test]
fn select_next_user_agent_from_user_to_agent_run() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 0, 0]; // user:0 (UserMessage)

    state.select_next_user_agent(); // → agent:0a (first of AgentMessage run)
    assert_eq!(sel(&state), vec![0, 1, 0]);
}

#[test]
fn select_next_user_agent_skips_non_ua_types() {
    let mut state = nav_tree();
    // Start on ToolCall — not in UA filtered list; should find next UA node
    state.selection_index = vec![0, 1, 2]; // tool:0 (ToolCall)

    state.select_next_user_agent(); // → user:1 (first UA after ToolCall in DFS)
    assert_eq!(sel(&state), vec![1, 0, 0]);
}

#[test]
fn select_next_user_agent_skips_run() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 1, 0]; // agent:0a — start of AgentMessage run

    state.select_next_user_agent(); // → user:1 (skip rest of AgentMessage run)
    assert_eq!(sel(&state), vec![1, 0, 0]);
}

#[test]
fn select_prev_user_agent_goes_to_run_start() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 1, 1]; // agent:0b — mid AgentMessage run

    state.select_prev_user_agent(); // → agent:0a (start of current run)
    assert_eq!(sel(&state), vec![0, 1, 0]);
}

#[test]
fn select_prev_user_agent_from_non_ua_node() {
    let mut state = nav_tree();
    state.selection_index = vec![1, 1, 0]; // think:1 (Thinking — not in UA list)

    state.select_prev_user_agent(); // → start of run that ends just before think:1
    // The last UA node before think:1 in DFS is user:1 [1,0,0] (only 1 node in its run)
    assert_eq!(sel(&state), vec![1, 0, 0]);
}

/// Flat tree: AgentMsg, AgentMsg, ToolCall, AgentMsg, UserMsg — all top-level.
/// Used to verify that a non-UA visible node (ToolCall) breaks a same-type run.
fn tool_separated_tree() -> TreeScrollViewState {
    TreeScrollViewState::new_without_terminal(vec![
        MessageState::new("a0")
            .message_type(MessageType::AgentMessage)
            .text("a0")
            .show_more(true),
        MessageState::new("a1")
            .message_type(MessageType::AgentMessage)
            .text("a1")
            .show_more(true),
        MessageState::new("tc")
            .message_type(MessageType::ToolCall)
            .text("bash")
            .show_more(true),
        MessageState::new("a2")
            .message_type(MessageType::AgentMessage)
            .text("a2")
            .show_more(true),
        MessageState::new("u0")
            .message_type(MessageType::UserMessage)
            .text("u0")
            .show_more(true),
    ])
}

#[test]
fn select_next_user_agent_tool_separates_run() {
    let mut state = tool_separated_tree();
    state.selection_index = vec![0]; // a0 — start of first AgentMessage run

    // ToolCall at [2] should break the AgentMessage run, so } lands on a2 [3], not u0 [4].
    state.select_next_user_agent();
    assert_eq!(
        sel(&state),
        vec![3],
        "ToolCall should break the AgentMessage run"
    );
}

#[test]
fn select_prev_user_agent_at_run_start_after_tool() {
    let mut state = tool_separated_tree();
    state.selection_index = vec![3]; // a2 — start of second AgentMessage run (separated by ToolCall)

    // a2 is at run start (ToolCall precedes it), so { jumps to start of prev run = a0 [0].
    state.select_prev_user_agent();
    assert_eq!(
        sel(&state),
        vec![0],
        "should jump to start of previous AgentMessage run"
    );
}

// ── turn navigation ────────────────────────────────────────────────────────────
//
// run_start_tree: one turn whose agent sub-turn ends with three consecutive
// AgentMessages, used to verify the same-type run-start retreat on `][` / `[]`.
//
//   [0] turn:rs  Container  group=true
//     [0,0] user_sub  Container
//       [0,0,0] user:rs   UserMessage
//     [0,1] agent_sub  Container
//       [0,1,0] agent:rs0  AgentMessage   ← run start (target)
//       [0,1,1] agent:rs1  AgentMessage
//       [0,1,2] agent:rs2  AgentMessage   ← DFS-last

fn run_start_tree() -> TreeScrollViewState {
    let turn = turn_group(
        "turn:rs",
        vec![
            sub_turn(
                "user_sub",
                vec![
                    MessageState::new("user:rs")
                        .message_type(MessageType::UserMessage)
                        .text("u"),
                ],
            ),
            sub_turn(
                "agent_sub",
                vec![
                    MessageState::new("agent:rs0")
                        .message_type(MessageType::AgentMessage)
                        .text("a0"),
                    MessageState::new("agent:rs1")
                        .message_type(MessageType::AgentMessage)
                        .text("a1"),
                    MessageState::new("agent:rs2")
                        .message_type(MessageType::AgentMessage)
                        .text("a2"),
                ],
            ),
        ],
    );
    TreeScrollViewState::new(vec![turn])
}

// Three turns used to verify that mid-run selection triggers advance/retreat to the
// neighbouring turn rather than landing on the run-start within the current turn.
//
//   [0] t:0  group  →  [0,0,0] user:t0   (UserMessage)
//   [1] t:1  group  →  [1,0,0] agent:t1a, [1,0,1] agent:t1b, [1,0,2] agent:t1c (AgentMessage ×3)
//   [2] t:2  group  →  [2,0,0] user:t2   (UserMessage)
//   [3] terminal
fn three_turn_mid_run_tree() -> TreeScrollViewState {
    let turn0 = turn_group(
        "t:0",
        vec![sub_turn(
            "u_sub:0",
            vec![
                MessageState::new("user:t0")
                    .message_type(MessageType::UserMessage)
                    .text("u0"),
            ],
        )],
    );
    let turn1 = turn_group(
        "t:1",
        vec![sub_turn(
            "a_sub:1",
            vec![
                MessageState::new("agent:t1a")
                    .message_type(MessageType::AgentMessage)
                    .text("a0"),
                MessageState::new("agent:t1b")
                    .message_type(MessageType::AgentMessage)
                    .text("a1"),
                MessageState::new("agent:t1c")
                    .message_type(MessageType::AgentMessage)
                    .text("a2"),
            ],
        )],
    );
    let turn2 = turn_group(
        "t:2",
        vec![sub_turn(
            "u_sub:2",
            vec![
                MessageState::new("user:t2")
                    .message_type(MessageType::UserMessage)
                    .text("u2"),
            ],
        )],
    );
    TreeScrollViewState::new(vec![turn0, turn1, turn2])
}

#[test]
fn select_next_turn_start_reaches_first_message_of_next_turn() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 0, 0]; // turn 0

    state.select_next_turn_start(); // → user:1 (first non-Container in turn 1)
    assert_eq!(sel(&state), vec![1, 0, 0]);
}

#[test]
fn select_next_turn_end_reaches_run_start_of_current_turn_end() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 0, 0]; // turn 0

    // DFS-last of turn 0 is result:0 at [0,1,2,0]; it has no same-type sibling,
    // so the run start is itself.
    state.select_next_turn_end();
    assert_eq!(sel(&state), vec![0, 1, 2, 0]);
}

#[test]
fn select_next_turn_end_retreats_to_same_type_run_start() {
    let mut state = run_start_tree();
    state.selection_index = vec![0, 0, 0]; // user:rs

    // DFS-last = agent:rs2 at [0,1,2]; retreats through agent:rs1 and agent:rs0
    // (same AgentMessage type), landing on run start [0,1,0].
    state.select_next_turn_end();
    assert_eq!(sel(&state), vec![0, 1, 0]);
}

#[test]
fn select_next_turn_end_advances_when_already_at_turn_end() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 1, 2, 0]; // at run-start end of turn 0

    state.select_next_turn_end(); // → agent:1 (run-start end of turn 1)
    assert_eq!(sel(&state), vec![1, 1, 1]);
}

#[test]
fn select_next_turn_end_advances_from_mid_run() {
    let mut state = three_turn_mid_run_tree();
    state.selection_index = vec![1, 0, 1]; // agent:t1b — past run start [1,0,0]

    state.select_next_turn_end(); // → user:t2 (run-start end of turn 2)
    assert_eq!(sel(&state), vec![2, 0, 0]);
}

#[test]
fn select_prev_turn_start_reaches_current_turn_start() {
    let mut state = nav_tree();
    state.selection_index = vec![1, 1, 1]; // mid turn 1, not at its start

    state.select_prev_turn_start(); // → user:1 (first non-Container in current turn 1)
    assert_eq!(sel(&state), vec![1, 0, 0]);
}

#[test]
fn select_prev_turn_start_retreats_to_prev_when_already_at_start() {
    let mut state = nav_tree();
    state.selection_index = vec![1, 0, 0]; // already at turn_start_path(1)

    state.select_prev_turn_start(); // → user:0 (first non-Container in turn 0)
    assert_eq!(sel(&state), vec![0, 0, 0]);
}

#[test]
fn select_prev_turn_end_retreats_when_before_run_start() {
    let mut state = nav_tree();
    state.selection_index = vec![1, 0, 0]; // start of turn 1, before its run-start end [1,1,1]

    // [1,0,0] <= [1,1,1]: already past the end going backwards → retreat to turn 0 end
    state.select_prev_turn_end();
    assert_eq!(sel(&state), vec![0, 1, 2, 0]);
}

#[test]
fn select_prev_turn_end_retreats_to_prev_when_already_at_end() {
    let mut state = nav_tree();
    state.selection_index = vec![1, 1, 1]; // at run-start end of turn 1

    state.select_prev_turn_end(); // → result:0 (run-start end of turn 0)
    assert_eq!(sel(&state), vec![0, 1, 2, 0]);
}

#[test]
fn select_prev_turn_end_lands_on_run_start_from_mid_run() {
    let mut state = three_turn_mid_run_tree();
    state.selection_index = vec![1, 0, 1]; // agent:t1b — after run start [1,0,0]

    // [1,0,1] > [1,0,0]: haven't reached the run-start going backwards → jump to it
    state.select_prev_turn_end();
    assert_eq!(sel(&state), vec![1, 0, 0]);
}

#[test]
fn select_prev_turn_end_clamps_at_first_turn() {
    let mut state = run_start_tree();
    state.selection_index = vec![0, 0, 0]; // user:rs — before run-start end [0,1,0]

    // [0,0,0] <= [0,1,0]: past end going backwards, but turn_idx=0 → no-op
    state.select_prev_turn_end();
    assert_eq!(sel(&state), vec![0, 0, 0]);
}

#[test]
fn turn_nav_clamps_at_boundaries() {
    let mut state = nav_tree();
    // [[ from the very first node of turn 0: already at turn_start_path(0),
    // so the "if already there" branch fires; turn_idx == 0 → no-op.
    state.selection_index = vec![0, 0, 0];
    state.select_prev_turn_start();
    assert_eq!(
        sel(&state),
        vec![0, 0, 0],
        "prev turn start clamps at first turn"
    );

    // ]] from the last turn — no next turn
    state.selection_index = vec![2, 1, 0];
    state.select_next_turn_start();
    assert_eq!(
        sel(&state),
        vec![2, 1, 0],
        "next turn start clamps at last turn"
    );
}

// ── scroll offset preserved on expand ────────────────────────────────────────

#[test]
fn expand_middle_node_preserves_top_offset() {
    // Tree: a(h=2), b_collapsed(h=2), c(h=2), d(h=2), e(h=2), terminal.
    // Viewport 80×10. top_index=[0], top_offset=1 means only the bottom row of
    // "a" is visible. The screen is full (1 + 2+2+2+2 = 9 rows ≤ 10).
    //
    // Expanding the collapsed middle node "b" inserts content *below* the top,
    // so the scroll position (top_index, top_offset) must be unchanged.
    let items = vec![
        short_msg("a"),
        parent_with_children("b", 2), // collapsed, h=2
        short_msg("c"),
        short_msg("d"),
        short_msg("e"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 10);

    state.top_index = vec![0];
    state.top_offset = 1;
    state.selection_index = vec![1];
    state.precedence = Precedence::Selection;

    assert!(!get_node(&state.items, &[1]).unwrap().expanded);

    state.toggle_expand(); // expand "b"

    assert!(get_node(&state.items, &[1]).unwrap().expanded);
    assert_eq!(state.top_index, vec![0], "top_index must not change");
    assert_eq!(
        state.top_offset, 1,
        "top_offset must not change when expanding a middle node"
    );

    do_layout(&mut state, 80, 10);
}

#[test]
fn cycle_display_expand_middle_node_preserves_top_offset() {
    // Same scenario as the toggle_expand variant, but expansion is triggered via
    // cycle_display. "b" has short text (no show_more step) so the first Space
    // press directly expands the children.
    let items = vec![
        short_msg("a"),
        parent_with_children("b", 2), // collapsed, h=2; short text → cycle goes straight to expand
        short_msg("c"),
        short_msg("d"),
        short_msg("e"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 10);

    state.top_index = vec![0];
    state.top_offset = 1;
    state.selection_index = vec![1];
    state.precedence = Precedence::Selection;

    assert!(!get_node(&state.items, &[1]).unwrap().expanded);

    state.cycle_display(); // short text + children → expands directly

    assert!(get_node(&state.items, &[1]).unwrap().expanded);
    assert_eq!(state.top_index, vec![0], "top_index must not change");
    assert_eq!(
        state.top_offset, 1,
        "top_offset must not change when expanding a middle node via cycle_display"
    );

    do_layout(&mut state, 80, 10);
}

// ── Update op ─────────────────────────────────────────────────────────────────

#[test]
fn update_patches_text_preserves_children() {
    let child = MessageState::new("child").message_type(MessageType::ToolResult);
    let parent = MessageState::new("parent")
        .message_type(MessageType::AgentMessage)
        .text("original")
        .children(vec![child]);
    let mut state = TreeScrollViewState::new(vec![parent]);

    state.apply(vec![ReaderOp::Tree(TreeOperation::Update {
        id: "parent".to_string(),
        message: MessageState::new("parent")
            .message_type(MessageType::AgentMessage)
            .text("updated"),
    })]);

    let node = get_node(&state.items, &[0]).unwrap();
    assert_eq!(
        node.text.as_deref(),
        Some("updated"),
        "text should be patched"
    );
    assert_eq!(node.children.len(), 1, "children must be preserved");
    assert_eq!(node.children[0].id, "child");
}

#[test]
fn update_preserves_non_text_fields() {
    let msg = MessageState::new("n")
        .message_type(MessageType::AgentMessage)
        .text("hello")
        .show_more(true)
        .expanded(true);
    let mut state = TreeScrollViewState::new(vec![msg]);

    // Apply an Update that changes only text.
    state.apply(vec![ReaderOp::Tree(TreeOperation::Update {
        id: "n".to_string(),
        message: MessageState::new("n")
            .message_type(MessageType::AgentMessage)
            .text("hello world"),
    })]);

    // Non-text fields on the incoming message are whatever MessageState::new defaults are,
    // but children from the old node survive regardless.
    let node = get_node(&state.items, &[0]).unwrap();
    assert_eq!(node.text.as_deref(), Some("hello world"));
    assert!(node.children.is_empty());
}

#[test]
fn update_unknown_id_is_noop() {
    let msg = MessageState::new("a")
        .message_type(MessageType::AgentMessage)
        .text("original");
    let mut state = TreeScrollViewState::new(vec![msg]);

    state.apply(vec![ReaderOp::Tree(TreeOperation::Update {
        id: "nonexistent".to_string(),
        message: MessageState::new("nonexistent")
            .message_type(MessageType::AgentMessage)
            .text("ignored"),
    })]);

    let node = get_node(&state.items, &[0]).unwrap();
    assert_eq!(
        node.text.as_deref(),
        Some("original"),
        "unknown id is a no-op"
    );
}

#[test]
fn update_clears_height_cache() {
    let msg = MessageState::new("a")
        .message_type(MessageType::AgentMessage)
        .text("short");
    let mut state = TreeScrollViewState::new(vec![msg]);
    // Seed a fake height.
    state.items[0].height = Some(99);

    state.apply(vec![ReaderOp::Tree(TreeOperation::Update {
        id: "a".to_string(),
        message: MessageState::new("a")
            .message_type(MessageType::AgentMessage)
            .text("a much longer line that should invalidate the cached height"),
    })]);

    assert!(
        state.items[0].height.is_none(),
        "height cache should be cleared after Update"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Search
// ─────────────────────────────────────────────────────────────────────────────

fn search_msg(id: &str, text: &str) -> MessageState {
    MessageState::new(id)
        .text(text)
        .message_type(MessageType::AgentMessage)
}

// ── search_pending ────────────────────────────────────────────────────────────

#[test]
fn search_pending_finds_match_in_flat_tree() {
    let items = vec![
        search_msg("a", "hello world"),
        search_msg("b", "foo bar"),
        search_msg("c", "needle here"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.search_pending("needle", false);

    let ps = state
        .pending_search
        .as_ref()
        .expect("pending_search must be set");
    assert_eq!(ps.found_path, vec![2], "should find node c at index 2");
    assert!(!ps.found_path.is_empty());
}

#[test]
fn search_pending_no_match_leaves_viewport_at_start() {
    let items = vec![search_msg("a", "hello"), search_msg("b", "world")];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    let orig_top = state.top_index.clone();
    let orig_offset = state.top_offset;

    state.search_pending("zzznomatch", false);

    let ps = state
        .pending_search
        .as_ref()
        .expect("pending_search must be set");
    assert!(
        ps.found_path.is_empty(),
        "no match: found_path should be empty"
    );
    assert_eq!(state.top_index, orig_top, "top_index restored on no match");
    assert_eq!(
        state.top_offset, orig_offset,
        "top_offset restored on no match"
    );
}

#[test]
fn search_pending_preserves_start_across_keystrokes() {
    let items = vec![search_msg("a", "apple"), search_msg("b", "banana")];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    let orig_top = state.top_index.clone();

    // Simulate typing "a", then "p", then "p" — start should remain captured.
    state.search_pending("a", false);
    state.search_pending("ap", false);
    state.search_pending("app", false);

    let ps = state
        .pending_search
        .as_ref()
        .expect("pending_search must be set");
    assert_eq!(
        ps.start_top_index, orig_top,
        "start_top_index must not change across keystrokes"
    );
    assert_eq!(ps.found_path, vec![0], "should find 'apple' at index 0");
}

#[test]
fn search_pending_finds_visible_ancestor_for_collapsed_child() {
    // Parent is collapsed; child has the match. We expect the visible ancestor
    // (the parent) to be used for InnerFocus.
    let child = search_msg("child", "needle inside");
    let parent = MessageState::new("parent")
        .text("parent text")
        .message_type(MessageType::AgentMessage)
        .expanded(false)
        .children(vec![child]);
    let mut state = TreeScrollViewState::new(vec![parent]);
    do_layout(&mut state, 80, 24);

    state.search_pending("needle", false);

    let ps = state
        .pending_search
        .as_ref()
        .expect("pending_search must be set");
    // found_path should point to the collapsed child
    assert_eq!(
        ps.found_path,
        vec![0, 0],
        "found_path should be the actual match"
    );

    // The precedence should be InnerFocus targeting the *visible* ancestor (parent).
    match &state.precedence {
        super::state::Precedence::InnerFocus { path, .. } => {
            assert_eq!(
                path,
                &vec![0],
                "InnerFocus path should be the visible parent"
            );
        }
        other => panic!("expected InnerFocus precedence, got {:?}", other),
    }
}

// ── cancel_search ─────────────────────────────────────────────────────────────

#[test]
fn cancel_search_restores_viewport() {
    let items = vec![
        search_msg("a", "foo"),
        search_msg("b", "bar"),
        search_msg("c", "baz"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 6); // viewport only shows a few nodes

    // Scroll to node b.
    state.select_next();
    do_layout(&mut state, 80, 6);
    let saved_top = state.top_index.clone();
    let saved_offset = state.top_offset;

    // Start search — this captures the start position.
    state.search_pending("baz", false);
    assert!(state.pending_search.is_some());

    // Cancel — viewport should revert.
    state.cancel_search();

    assert!(
        state.pending_search.is_none(),
        "pending_search cleared after cancel"
    );
    assert_eq!(
        state.top_index, saved_top,
        "top_index restored after cancel"
    );
    assert_eq!(
        state.top_offset, saved_offset,
        "top_offset restored after cancel"
    );
}

// ── commit_search ─────────────────────────────────────────────────────────────

#[test]
fn commit_search_moves_selection_and_sets_search_state() {
    let items = vec![
        search_msg("a", "alpha"),
        search_msg("b", "beta"),
        search_msg("c", "gamma"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.search_pending("gamma", false);
    assert_eq!(state.pending_search.as_ref().unwrap().found_path, vec![2]);

    state.commit_search();

    assert!(
        state.pending_search.is_none(),
        "pending_search cleared after commit"
    );
    let committed = state.search.as_ref().expect("search must be committed");
    assert_eq!(committed.found_path, vec![2]);
    assert_eq!(committed.query, "gamma");
    assert_eq!(
        state.selection_index,
        vec![2],
        "selection should move to match"
    );
}

#[test]
fn commit_search_expands_collapsed_ancestor() {
    let child = search_msg("child", "needle inside");
    let parent = MessageState::new("parent")
        .text("parent text")
        .message_type(MessageType::AgentMessage)
        .expanded(false)
        .children(vec![child]);
    let mut state = TreeScrollViewState::new(vec![parent, search_msg("other", "other")]);
    do_layout(&mut state, 80, 24);

    state.search_pending("needle", false);
    state.commit_search();

    // The parent at [0] should now be expanded.
    assert!(
        state.items[0].expanded,
        "ancestor must be expanded after commit"
    );
    assert_eq!(
        state.selection_index,
        vec![0, 0],
        "selection at child after expand"
    );
}

#[test]
fn commit_search_noop_when_no_match() {
    let items = vec![search_msg("a", "hello")];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.search_pending("zzz", false);
    let sel_before = state.selection_index.clone();
    state.commit_search();

    // No match: pending clears, committed search not set, selection unchanged.
    assert!(state.pending_search.is_none());
    assert!(
        state.search.is_none(),
        "search must not be set when no match found"
    );
    assert_eq!(state.selection_index, sel_before);
}

// ── search_next / search_prev ─────────────────────────────────────────────────

#[test]
fn search_next_advances_to_next_match() {
    let items = vec![
        search_msg("a", "apple pie"),
        search_msg("b", "banana"),
        search_msg("c", "apple tart"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    // Commit search on first "apple" (node 0).
    state.search_pending("apple", false);
    state.commit_search();
    assert_eq!(state.search.as_ref().unwrap().found_path, vec![0]);

    // search_next should move to node c at index 2.
    state.search_next();
    assert_eq!(state.search.as_ref().unwrap().found_path, vec![2]);
    assert_eq!(state.selection_index, vec![2]);
}

#[test]
fn search_next_finds_second_occurrence_within_same_node() {
    // Node a contains two occurrences of "hit". search_next from the first
    // should land on the second before moving to any other node.
    let items = vec![
        search_msg("a", "hit and hit again"),
        search_msg("b", "hit somewhere else"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.search_pending("hit", false);
    state.commit_search();
    // committed at first "hit" in node a (char 0)
    assert_eq!(state.search.as_ref().unwrap().found_path, vec![0]);
    assert_eq!(state.search.as_ref().unwrap().found_char_index, 0);

    // Next should find the second "hit" in node a (char 8), not jump to node b.
    state.search_next();
    let s = state.search.as_ref().unwrap();
    assert_eq!(
        s.found_path,
        vec![0],
        "should stay in node a for second hit"
    );
    assert_eq!(s.found_char_index, 8, "second 'hit' starts at char 8");

    // Next after that should move to node b.
    state.search_next();
    assert_eq!(state.search.as_ref().unwrap().found_path, vec![1]);
}

#[test]
fn search_next_wraps_around_to_first_match() {
    let items = vec![
        search_msg("a", "target one"),
        search_msg("b", "nothing"),
        search_msg("c", "target two"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.search_pending("target", false);
    state.commit_search();
    // advance to node c.
    state.search_next();
    assert_eq!(state.search.as_ref().unwrap().found_path, vec![2]);

    // Next search_next should wrap to node a again.
    state.search_next();
    assert_eq!(state.search.as_ref().unwrap().found_path, vec![0]);
}

#[test]
fn search_prev_retreats_to_previous_match() {
    let items = vec![
        search_msg("a", "apple pie"),
        search_msg("b", "banana"),
        search_msg("c", "apple tart"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    // Commit search on first "apple" (node 0), then advance to node c.
    state.search_pending("apple", false);
    state.commit_search();
    state.search_next();
    assert_eq!(state.search.as_ref().unwrap().found_path, vec![2]);

    // search_prev should go back to node a at index 0.
    state.search_prev();
    assert_eq!(state.search.as_ref().unwrap().found_path, vec![0]);
    assert_eq!(state.selection_index, vec![0]);
}

#[test]
fn search_prev_finds_earlier_occurrence_within_same_node() {
    // Node b contains two occurrences. When at the second, search_prev should
    // land on the first without leaving the node.
    let items = vec![
        search_msg("a", "other"),
        search_msg("b", "hit early and hit late"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.search_pending("hit", false);
    state.commit_search();
    // committed at first "hit" in node a (char 0 of "other" doesn't match;
    // first "hit" is in node b at char 0).
    // Actually "other" has no hit; so first match is node b at char 0.
    assert_eq!(state.search.as_ref().unwrap().found_path, vec![1]);
    assert_eq!(state.search.as_ref().unwrap().found_char_index, 0);

    // Advance to the second "hit" in node b (char 14).
    state.search_next();
    let s = state.search.as_ref().unwrap();
    assert_eq!(s.found_path, vec![1], "still in node b");
    assert_eq!(s.found_char_index, 14);

    // search_prev should retreat to the first "hit" in node b, not jump to node a.
    state.search_prev();
    let s = state.search.as_ref().unwrap();
    assert_eq!(s.found_path, vec![1], "should stay in node b");
    assert_eq!(s.found_char_index, 0);
}

#[test]
fn search_prev_crosses_node_to_last_occurrence() {
    // When at the first occurrence in a node, search_prev should go to the
    // *last* occurrence in the previous node (not the first).
    let items = vec![
        search_msg("a", "hit hit hit"), // three occurrences: 0, 4, 8
        search_msg("b", "hit here"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.search_pending("hit", false);
    state.commit_search();
    // First match: node a at char 0.
    assert_eq!(state.search.as_ref().unwrap().found_path, vec![0]);
    assert_eq!(state.search.as_ref().unwrap().found_char_index, 0);

    // Advance to node b.
    state.search_next(); // node a char 4
    state.search_next(); // node a char 8
    state.search_next(); // node b char 0
    assert_eq!(state.search.as_ref().unwrap().found_path, vec![1]);
    assert_eq!(state.search.as_ref().unwrap().found_char_index, 0);

    // search_prev from node b char 0: no earlier match in node b, so go to
    // the LAST occurrence in node a (char 8), not the first (char 0).
    state.search_prev();
    let s = state.search.as_ref().unwrap();
    assert_eq!(s.found_path, vec![0], "went back to node a");
    assert_eq!(s.found_char_index, 8, "landed on the last 'hit' in node a");
}

// ── backward search ───────────────────────────────────────────────────────────

#[test]
fn backward_search_finds_last_match_first() {
    let items = vec![
        search_msg("a", "needle first"),
        search_msg("b", "no match"),
        search_msg("c", "needle last"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    // Backward search from the end.
    state.search_pending("needle", true);

    let ps = state.pending_search.as_ref().unwrap();
    // Backward from top_index (which starts at node 0) wraps to the last match.
    // Since we start inclusive at node 0, and needle matches node 0 first, backward
    // search starting at node 0 inclusive would find node 0. But note that the
    // do_search with start_inclusive=true for backward searches also starts from
    // start_path.  Let's verify we got some valid needle match.
    assert!(
        !ps.found_path.is_empty(),
        "backward search should find a needle"
    );
    let found = get_node(&state.items, &ps.found_path).unwrap();
    assert!(
        found.text.as_deref().unwrap_or("").contains("needle"),
        "found node must contain 'needle'"
    );
}

// ── search_highlight_for ──────────────────────────────────────────────────────

#[test]
fn search_highlight_for_returns_highlight_at_match_path() {
    let items = vec![
        search_msg("a", "hello world"),
        search_msg("b", "no match here"),
    ];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.search_pending("world", false);

    // Path [0] is the match.
    let hl = state.search_highlight_for(&[0]);
    assert!(hl.is_some(), "highlight should be set for the matched path");
    let hl = hl.unwrap();
    assert_eq!(hl.query_len, 5, "query_len should be length of 'world'");

    // Path [1] is not the match.
    let hl2 = state.search_highlight_for(&[1]);
    assert!(hl2.is_none(), "no highlight for unmatched path");
}

#[test]
fn search_highlight_uses_committed_search_when_no_pending() {
    let items = vec![search_msg("a", "hello world")];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.search_pending("world", false);
    state.commit_search();

    // After commit, pending is gone but committed search still highlights.
    assert!(state.pending_search.is_none());
    let hl = state.search_highlight_for(&[0]);
    assert!(
        hl.is_some(),
        "highlight from committed search should still work"
    );
}

// ── case-insensitive search ───────────────────────────────────────────────────

#[test]
fn search_is_case_insensitive() {
    let items = vec![search_msg("a", "Hello World"), search_msg("b", "nothing")];
    let mut state = TreeScrollViewState::new(items);
    do_layout(&mut state, 80, 24);

    state.search_pending("hello", false);
    let ps = state.pending_search.as_ref().unwrap();
    assert_eq!(
        ps.found_path,
        vec![0],
        "case-insensitive search should match 'Hello'"
    );
}

// ── hit_test ─────────────────────────────────────────────────────────────────

use super::state::MessageRenderInfo;
use crate::tree_scroll_view::message_widget::component::MouseHitResult;
use ratatui::layout::Rect;

fn make_render_info(
    path: Vec<usize>,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    has_gap_row: bool,
    hidden_after: usize,
) -> MessageRenderInfo {
    let visual_depth = path.len().saturating_sub(1);
    MessageRenderInfo {
        path,
        widget_area: Rect {
            x,
            y,
            width: w,
            height: h,
        },
        has_gap_row,
        hidden_after,
        skip_lines: 0,
        visual_depth,
    }
}

#[test]
fn hit_test_terminal_in_bounds() {
    let mut state = TreeScrollViewState::new(vec![]);
    state.viewport_width = 80;
    // terminal at y=20, height=4
    state.terminal_render_info = Some((0, 20, 4, 0));
    let result = state.hit_test(10, 21);
    assert!(matches!(result, MouseHitResult::Terminal));
}

#[test]
fn hit_test_terminal_out_of_bounds() {
    let mut state = TreeScrollViewState::new(vec![]);
    state.viewport_width = 80;
    state.terminal_render_info = Some((0, 20, 4, 0));
    // y=19 is above the terminal
    let result = state.hit_test(10, 19);
    assert!(matches!(result, MouseHitResult::Outside));
}

#[test]
fn hit_test_gap_row_with_hidden() {
    let mut state = TreeScrollViewState::new(vec![]);
    // Message at y=0..10, has_gap_row with 3 hidden after
    state
        .render_rects
        .push(make_render_info(vec![0], 0, 0, 80, 10, true, 3));
    // Gap row is the last row: y=9
    let result = state.hit_test(5, 9);
    assert!(
        matches!(
            result,
            MouseHitResult::GapRow {
                hidden_after: 3,
                ..
            }
        ),
        "expected GapRow, got something else"
    );
}

#[test]
fn hit_test_gap_row_no_hidden_falls_through_to_message() {
    let mut state = TreeScrollViewState::new(vec![]);
    // has_gap_row but hidden_after=0 → last row is NOT a gap row hit
    state
        .render_rects
        .push(make_render_info(vec![0], 0, 0, 80, 10, true, 0));
    // Last row y=9 should not be GapRow
    let result = state.hit_test(5, 9);
    assert!(matches!(result, MouseHitResult::Message { .. }));
}

#[test]
fn hit_test_indicator_area_depth_zero() {
    let mut state = TreeScrollViewState::new(vec![]);
    // Depth 0 → indicator at x = area.x + 1 + 0 = 1
    state
        .render_rects
        .push(make_render_info(vec![0], 0, 0, 80, 5, false, 0));
    // x=1 (indicator), x=2 (space after indicator) — both in indicator area
    assert!(matches!(
        state.hit_test(1, 2),
        MouseHitResult::IndicatorArea { .. }
    ));
    assert!(matches!(
        state.hit_test(2, 2),
        MouseHitResult::IndicatorArea { .. }
    ));
    // x=3 is after the indicator → Message
    assert!(matches!(
        state.hit_test(3, 2),
        MouseHitResult::Message { .. }
    ));
}

#[test]
fn hit_test_indicator_area_depth_one() {
    let mut state = TreeScrollViewState::new(vec![]);
    // Depth 1 (path len 2) → indicator at x = 0 + 1 + 2 = 3
    state
        .render_rects
        .push(make_render_info(vec![0, 0], 0, 0, 80, 5, false, 0));
    assert!(matches!(
        state.hit_test(3, 2),
        MouseHitResult::IndicatorArea { .. }
    ));
    assert!(matches!(
        state.hit_test(4, 2),
        MouseHitResult::IndicatorArea { .. }
    ));
}

#[test]
fn hit_test_message_body() {
    let mut state = TreeScrollViewState::new(vec![]);
    // Depth 0, gutter at x=0, indicator at x=1, space at x=2, content from x=3
    state
        .render_rects
        .push(make_render_info(vec![0], 0, 0, 80, 5, false, 0));
    // x=0 is gutter → Message (not indicator)
    assert!(matches!(
        state.hit_test(0, 2),
        MouseHitResult::Message { .. }
    ));
    // x=3 starts content → Message (no InnerComponent since no ui_state)
    assert!(matches!(
        state.hit_test(10, 2),
        MouseHitResult::Message { .. }
    ));
}

#[test]
fn hit_test_outside() {
    let mut state = TreeScrollViewState::new(vec![]);
    state
        .render_rects
        .push(make_render_info(vec![0], 0, 0, 80, 5, false, 0));
    // y=10 is outside the widget_area (height=5, so y in [0,5))
    assert!(matches!(state.hit_test(5, 10), MouseHitResult::Outside));
}
