use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::state::{PickerState, Tab};
use crate::providers::{ProviderKind, TranscriptEntry};

pub enum PickerAction {
    None,
    Quit,
    Select(TranscriptEntry),
    OpenAndResume(TranscriptEntry),
    ToggleShowAll,
    NewSession(ProviderKind),
}

impl PickerState {
    pub fn handle_key(&mut self, key: KeyEvent) -> PickerAction {
        if key.kind != KeyEventKind::Press {
            return PickerAction::None;
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_down();
                PickerAction::None
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_down();
                PickerAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_up();
                PickerAction::None
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_up();
                PickerAction::None
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                self.next_tab();
                PickerAction::None
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.prev_tab();
                PickerAction::None
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                PickerAction::ToggleShowAll
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match self.selected_entry() {
                    Some(entry) => PickerAction::OpenAndResume(entry.clone()),
                    None => {
                        self.set_flash("Select an entry first");
                        PickerAction::None
                    }
                }
            }
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.new_session_action()
            }
            KeyCode::Enter => {
                if self.is_new_chat_selected() {
                    self.new_session_action()
                } else {
                    match self.selected_entry() {
                        Some(entry) => PickerAction::Select(entry.clone()),
                        None => PickerAction::Quit,
                    }
                }
            }
            KeyCode::Char('g') => {
                self.move_to_top();
                PickerAction::None
            }
            KeyCode::Char('G') => {
                self.move_to_bottom();
                PickerAction::None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_half_page_up();
                PickerAction::None
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_half_page_down();
                PickerAction::None
            }
            KeyCode::Esc | KeyCode::Char('q') => PickerAction::Quit,
            _ => PickerAction::None,
        }
    }

    fn new_session_action(&mut self) -> PickerAction {
        match self.tab {
            Tab::All => {
                self.set_flash("Select a provider tab first");
                PickerAction::None
            }
            Tab::Cursor => PickerAction::NewSession(ProviderKind::Cursor),
            Tab::Claude => PickerAction::NewSession(ProviderKind::Claude),
        }
    }
}
