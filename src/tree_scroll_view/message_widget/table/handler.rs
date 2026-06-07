use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub(super) enum TableAction {
    MoveSelection { row_delta: i32, col_delta: i32 },
    ResizeCol { col: usize, delta: i16 },
    ResetLayout,
}

/// Map a key event to a `TableAction` while in message interaction mode.
/// Returns `None` for unrecognized keys (they are silently swallowed).
pub(super) fn handle_table_key(key: KeyEvent, selected_col: usize) -> Option<TableAction> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // Navigation
        KeyCode::Left | KeyCode::Char('h') if !ctrl => Some(TableAction::MoveSelection {
            row_delta: 0,
            col_delta: -1,
        }),
        KeyCode::Right | KeyCode::Char('l') if !ctrl => Some(TableAction::MoveSelection {
            row_delta: 0,
            col_delta: 1,
        }),
        KeyCode::Up | KeyCode::Char('k') if !ctrl => Some(TableAction::MoveSelection {
            row_delta: -1,
            col_delta: 0,
        }),
        KeyCode::Down | KeyCode::Char('j') if !ctrl => Some(TableAction::MoveSelection {
            row_delta: 1,
            col_delta: 0,
        }),
        // Column resize
        KeyCode::Char('+') | KeyCode::Char('=') => Some(TableAction::ResizeCol {
            col: selected_col,
            delta: 2,
        }),
        KeyCode::Char('-') => Some(TableAction::ResizeCol {
            col: selected_col,
            delta: -2,
        }),
        KeyCode::Char('0') => Some(TableAction::ResetLayout),
        _ => None,
    }
}
