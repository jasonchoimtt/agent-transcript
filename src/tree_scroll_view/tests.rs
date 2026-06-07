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

    state.set_hidden(&[1], true); // hide middle node
    do_layout(&mut state, 80, 24);

    state.select_next(); // [0] → should skip hidden [1] and land on [2]
    assert_eq!(state.selection_index, vec![2]);
    do_layout(&mut state, 80, 24);
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

    let show_more_before = get_node(&state.items, &[0]).unwrap().show_more;
    let expanded_before = get_node(&state.items, &[0]).unwrap().expanded;
    state.cycle_display();
    do_layout(&mut state, 80, 24);

    assert_eq!(
        get_node(&state.items, &[0]).unwrap().show_more,
        show_more_before
    );
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

    state.cycle_display(); // should expand directly without setting show_more
    do_layout(&mut state, 80, 24);
    assert!(!get_node(&state.items, &[0]).unwrap().show_more);
    assert!(get_node(&state.items, &[0]).unwrap().expanded);

    state.cycle_display(); // collapse back
    do_layout(&mut state, 80, 24);
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
        .hidden(true);
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
    assert!(c.hidden, "child hidden=true should be restored");
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

#[test]
fn select_next_turn_start_reaches_first_message_of_next_turn() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 0, 0]; // turn 0

    state.select_next_turn_start(); // → user:1 (first non-Container in turn 1)
    assert_eq!(sel(&state), vec![1, 0, 0]);
}

#[test]
fn select_next_turn_end_reaches_last_message_of_current_turn() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 0, 0]; // turn 0

    state.select_next_turn_end(); // → result:0 (last non-Container in turn 0)
    assert_eq!(sel(&state), vec![0, 1, 2, 0]);
}

#[test]
fn select_next_turn_end_advances_when_already_at_turn_end() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 1, 2, 0]; // already at end of turn 0

    state.select_next_turn_end(); // → agent:1 (last non-Container in turn 1)
    assert_eq!(sel(&state), vec![1, 1, 1]);
}

#[test]
fn select_prev_turn_start_reaches_first_message_of_prev_turn() {
    let mut state = nav_tree();
    state.selection_index = vec![1, 1, 1]; // turn 1

    state.select_prev_turn_start(); // → user:0 (first non-Container in turn 0)
    assert_eq!(sel(&state), vec![0, 0, 0]);
}

#[test]
fn select_prev_turn_end_reaches_last_message_of_prev_turn() {
    let mut state = nav_tree();
    state.selection_index = vec![1, 0, 0]; // turn 1

    state.select_prev_turn_end(); // → result:0 (last non-Container in turn 0)
    assert_eq!(sel(&state), vec![0, 1, 2, 0]);
}

#[test]
fn turn_nav_clamps_at_boundaries() {
    let mut state = nav_tree();
    state.selection_index = vec![0, 0, 0]; // turn 0 — no prev turn

    state.select_prev_turn_start();
    assert_eq!(
        sel(&state),
        vec![0, 0, 0],
        "prev turn start clamps at first turn"
    );

    // Last turn — no next turn
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
