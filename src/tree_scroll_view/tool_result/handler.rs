use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{
    ComponentKeyResult, FileDeltaState, PatchHunk, ToolResultPayload, ToolResultUiState,
    format_unified_diff, max_context_in_hunk,
};

pub fn handle_tool_result_key(key: KeyEvent, state: &mut ToolResultUiState) -> ComponentKeyResult {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Esc => return ComponentKeyResult::ExitInteraction,
        KeyCode::Char('c') if ctrl => return ComponentKeyResult::ExitInteraction,
        KeyCode::Char('n') | KeyCode::Char('p') if ctrl => {
            return ComponentKeyResult::Passthrough;
        }
        KeyCode::Char('d') | KeyCode::Char('u') if ctrl => {
            return ComponentKeyResult::Passthrough;
        }
        KeyCode::PageDown | KeyCode::PageUp => return ComponentKeyResult::Passthrough,
        KeyCode::Char('w') if !ctrl => {
            state.wrap = !state.wrap;
            return ComponentKeyResult::Consumed {
                invalidates_height: true,
            };
        }
        _ => {}
    }

    match &mut state.payload {
        ToolResultPayload::FileDelta(fd) => {
            if fd.pending_y {
                fd.pending_y = false;
                if key.code == KeyCode::Char('y') {
                    let content = build_copy_content(fd, state.expanded);
                    return ComponentKeyResult::Copy { content };
                }
                return ComponentKeyResult::Unhandled;
            }

            match key.code {
                KeyCode::Char('Y') => {
                    let content = build_copy_content(fd, state.expanded);
                    return ComponentKeyResult::Copy { content };
                }
                KeyCode::Char('y') => {
                    fd.pending_y = true;
                    return ComponentKeyResult::Consumed {
                        invalidates_height: false,
                    };
                }
                KeyCode::Char(' ') => {
                    state.expanded = !state.expanded;
                    return ComponentKeyResult::Consumed {
                        invalidates_height: true,
                    };
                }
                _ => {}
            }

            handle_file_delta_key(key, fd).unwrap_or(ComponentKeyResult::Unhandled)
        }
        ToolResultPayload::ShellOutput(_) => match key.code {
            KeyCode::Char(' ') => {
                state.expanded = !state.expanded;
                ComponentKeyResult::Consumed {
                    invalidates_height: true,
                }
            }
            _ => ComponentKeyResult::Unhandled,
        },
    }
}

fn build_copy_content(fd: &FileDeltaState, expanded: bool) -> String {
    let hunks: &[PatchHunk] = if expanded {
        &fd.hunks
    } else {
        fd.hunks
            .get(fd.current_hunk..=fd.current_hunk)
            .unwrap_or(&[])
    };
    format_unified_diff(&fd.file_path, hunks)
}

fn handle_file_delta_key(key: KeyEvent, fd: &mut FileDeltaState) -> Option<ComponentKeyResult> {
    let n_hunks = fd.hunks.len();

    match key.code {
        // Navigate between hunks.
        KeyCode::Char('h') | KeyCode::Left => {
            fd.current_hunk = fd.current_hunk.saturating_sub(1);
            Some(ComponentKeyResult::Consumed {
                invalidates_height: true,
            })
        }
        KeyCode::Char('l') | KeyCode::Right => {
            if n_hunks > 0 {
                fd.current_hunk = fd.current_hunk.min(n_hunks - 1);
                // Advance but stay in bounds.
                if fd.current_hunk + 1 < n_hunks {
                    fd.current_hunk += 1;
                }
            }
            Some(ComponentKeyResult::Consumed {
                invalidates_height: true,
            })
        }
        // First / last hunk.
        KeyCode::Char('0') | KeyCode::Char('^') => {
            fd.current_hunk = 0;
            Some(ComponentKeyResult::Consumed {
                invalidates_height: true,
            })
        }
        KeyCode::Char('$') => {
            if n_hunks > 0 {
                fd.current_hunk = n_hunks - 1;
            }
            Some(ComponentKeyResult::Consumed {
                invalidates_height: true,
            })
        }
        // Context lines adjustment.
        KeyCode::Char('-') => {
            let new_ctx = match fd.context_lines {
                None => {
                    // Compute max context from current hunk.
                    let max = fd
                        .hunks
                        .get(fd.current_hunk)
                        .map(max_context_in_hunk)
                        .unwrap_or(0);
                    Some(max.saturating_sub(1))
                }
                Some(n) => Some(n.saturating_sub(1)),
            };
            fd.context_lines = new_ctx;
            Some(ComponentKeyResult::Consumed {
                invalidates_height: true,
            })
        }
        KeyCode::Char('=') | KeyCode::Char('+') => {
            match fd.context_lines {
                None => {
                    // Already showing all; no-op.
                    Some(ComponentKeyResult::Consumed {
                        invalidates_height: false,
                    })
                }
                Some(n) => {
                    let max = fd
                        .hunks
                        .get(fd.current_hunk)
                        .map(max_context_in_hunk)
                        .unwrap_or(0);
                    if n + 1 > max {
                        fd.context_lines = None; // snap back to show-all
                    } else {
                        fd.context_lines = Some(n + 1);
                    }
                    Some(ComponentKeyResult::Consumed {
                        invalidates_height: true,
                    })
                }
            }
        }
        _ => None,
    }
}
