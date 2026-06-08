use std::any::Any;

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::theme::Palette;
use crate::theme::styles::MessageStyle;
use crate::tree_scroll_view::MessageState;
use crate::tree_scroll_view::search::SearchHighlight;

// ── ComponentState ────────────────────────────────────────────────────────────

/// Minimal marker trait for per-node widget state stored in `MessageState::ui_state`.
/// Provides cloneability, downcasting, and a few read-only queries that can be answered
/// without constructing a full transient `MessageComponent`.
pub trait ComponentState: Any + Send + 'static {
    fn clone_box(&self) -> Box<dyn ComponentState>;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn type_name(&self) -> &'static str;
    /// Merge logic called on Replace/Update ops. Returns the new ui_state to store,
    /// preserving any interaction state from `self` while updating data from `new_message`.
    /// Default: clone new message's ui_state.
    fn on_update(&self, new_message: &MessageState) -> Option<Box<dyn ComponentState>> {
        new_message.ui_state.clone()
    }
    /// Whether this state type supports interactive mode. Defaults to `false`.
    fn supports_interaction(&self) -> bool {
        false
    }
}

impl Clone for Box<dyn ComponentState> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}
// ── MessageComponent ──────────────────────────────────────────────────────────

/// Result returned by [`MessageComponent::handle_key`].
pub enum ComponentKeyResult {
    /// Key consumed; height may need recomputation.
    Consumed { invalidates_height: bool },
    /// Key signals exit from interaction mode.
    ExitInteraction,
    /// Key should be passed to the outer scroll view (e.g. Ctrl-N/P).
    Passthrough,
    /// Key not recognised by this component.
    Unhandled,
    /// Component wants to copy `content` to the clipboard.
    Copy { content: String },
}

/// Context passed to [`MessageComponent::render_content`].
pub struct ContentRenderContext<'a> {
    pub palette: &'a Palette,
    pub style: &'a MessageStyle<'a>,
    pub skip_lines: u16,
    pub interaction: bool,
    pub highlight: Option<&'a SearchHighlight>,
}

/// Unified component trait implemented by transient structs that borrow
/// `&mut MessageState`. Covers interaction, layout, and rendering.
///
/// Use [`get_message_component`] (in `message_widget`) to obtain a boxed
/// component for a given node.
///
/// Merge logic for Replace/Update ops lives on [`ComponentState::on_update`]
/// (an `&self` method), not here, to avoid borrow-checker conflicts.
pub trait MessageComponent {
    fn message_mut(&mut self) -> &mut MessageState;

    /// Handle a key event in interaction mode.
    fn handle_key(&mut self, _key: KeyEvent) -> ComponentKeyResult {
        ComponentKeyResult::Unhandled
    }

    /// Half-open focused line range within the rendered area, if applicable.
    fn focused_line_range(&self, _palette: &Palette) -> Option<(u16, u16)> {
        None
    }

    /// Called when the available viewport width changes. The component should
    /// clear any layout that depends on width (e.g. col_widths) unless the
    /// user has manually overridden it.
    fn on_viewport_width_changed(&mut self) {}

    /// Perform a layout pass given `available_width` and return the computed
    /// node height, or `None` to fall back to the default text-height path.
    fn layout_pass(&mut self, _available_width: u16, _palette: &Palette) -> Option<u16> {
        None
    }

    /// Render the node's content area (already inset past gutter/prefix).
    fn render_content(&self, area: Rect, buf: &mut Buffer, ctx: &ContentRenderContext<'_>);
}
