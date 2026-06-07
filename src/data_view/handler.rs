use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use crate::{data_view::DataViewState, tree_scroll_view::TreeAction};

pub enum DataViewAction {
    Close,
    ToggleDisplay,
    Tree(TreeAction),
}

pub fn handle_key_event_dv(
    dv: &mut DataViewState,
    key: KeyEvent,
    area_height: u16,
) -> DataViewAction {
    if key.kind != KeyEventKind::Press {
        return DataViewAction::Tree(TreeAction::None);
    }
    if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
        return DataViewAction::Close;
    }
    // Space: toggle expand for containers, toggle show_more for leaf nodes.
    if key.code == KeyCode::Char(' ') {
        return DataViewAction::ToggleDisplay;
    }
    DataViewAction::Tree(dv.tree.key_parser.process(key, area_height))
}
