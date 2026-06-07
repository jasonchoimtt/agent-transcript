use mlua::prelude::*;

use crate::config::LuaTransformConfig;
use crate::transforms::Transform;
use crate::tree_operation::TreeOperation;
use crate::tree_scroll_view::state::{MessageState, MessageType};

/// Runs a user-supplied Lua `process(ops)` function against each batch of ops.
///
/// The Lua VM is created once and reused across all batches. On any Lua error
/// the original batch is forwarded unchanged and a warning is logged.
pub struct LuaTransform {
    lua: Lua,
}

impl LuaTransform {
    pub fn new(config: &LuaTransformConfig) -> mlua::Result<Self> {
        let script = match (&config.script, &config.script_path) {
            (Some(s), _) => s.clone(),
            (_, Some(path)) => std::fs::read_to_string(path)
                .map_err(|e| mlua::Error::runtime(format!("failed to read script: {e}")))?,
            (None, None) => {
                return Err(mlua::Error::runtime(
                    "lua transform requires script or script_path",
                ));
            }
        };
        let lua = Lua::new();
        lua.load(&script).exec()?;
        Ok(Self { lua })
    }

    fn try_process(&self, ops: Vec<TreeOperation>) -> mlua::Result<Vec<TreeOperation>> {
        let process_fn: LuaFunction = self.lua.globals().get("process")?;
        let ops_table = ops_to_lua(&self.lua, &ops)?;
        let result: LuaTable = process_fn.call(ops_table)?;
        lua_to_ops(result)
    }
}

impl Transform for LuaTransform {
    fn process(&mut self, ops: Vec<TreeOperation>) -> Vec<TreeOperation> {
        let original = ops.clone();
        match self.try_process(ops) {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!("lua_transform error: {e}");
                original
            }
        }
    }
}

// ── Rust → Lua ────────────────────────────────────────────────────────────────

fn ops_to_lua(lua: &Lua, ops: &[TreeOperation]) -> mlua::Result<LuaTable> {
    let table = lua.create_table()?;
    for (i, op) in ops.iter().enumerate() {
        table.set(i + 1, op_to_lua(lua, op)?)?;
    }
    Ok(table)
}

fn op_to_lua(lua: &Lua, op: &TreeOperation) -> mlua::Result<LuaTable> {
    let t = lua.create_table()?;
    match op {
        TreeOperation::Append { parent_id, message } => {
            t.set("type", "Append")?;
            if let Some(pid) = parent_id {
                t.set("parent_id", pid.as_str())?;
            }
            t.set("message", message_to_lua(lua, message)?)?;
        }
        TreeOperation::Replace { id, message } => {
            t.set("type", "Replace")?;
            t.set("id", id.as_str())?;
            t.set("message", message_to_lua(lua, message)?)?;
        }
        TreeOperation::Remove { id } => {
            t.set("type", "Remove")?;
            t.set("id", id.as_str())?;
        }
        TreeOperation::Update { id, message } => {
            t.set("type", "Update")?;
            t.set("id", id.as_str())?;
            t.set("message", message_to_lua(lua, message)?)?;
        }
    }
    Ok(t)
}

fn message_to_lua(lua: &Lua, msg: &MessageState) -> mlua::Result<LuaTable> {
    let t = lua.create_table()?;
    t.set("id", msg.id.as_str())?;
    t.set("message_type", msg.message_type.variant_name())?;
    if let Some(text) = &msg.text {
        t.set("text", text.as_str())?;
    }
    if let Some(brief) = &msg.brief {
        t.set("brief", brief.as_str())?;
    }
    t.set("expanded", msg.expanded)?;
    t.set("show_more", msg.show_more)?;
    t.set("hidden", msg.hidden)?;
    t.set("group", msg.group)?;
    let children = lua.create_table()?;
    for (i, child) in msg.children.iter().enumerate() {
        children.set(i + 1, message_to_lua(lua, child)?)?;
    }
    t.set("children", children)?;
    Ok(t)
}

// ── Lua → Rust ────────────────────────────────────────────────────────────────

fn lua_to_ops(table: LuaTable) -> mlua::Result<Vec<TreeOperation>> {
    let mut ops = Vec::new();
    for value in table.sequence_values::<LuaTable>() {
        ops.push(lua_to_op(value?)?);
    }
    Ok(ops)
}

fn lua_to_op(t: LuaTable) -> mlua::Result<TreeOperation> {
    let op_type: String = t.get("type")?;
    match op_type.as_str() {
        "Append" => {
            let parent_id: Option<String> = t.get("parent_id")?;
            let message = lua_to_message(t.get("message")?)?;
            Ok(TreeOperation::Append { parent_id, message })
        }
        "Replace" => {
            let id: String = t.get("id")?;
            let message = lua_to_message(t.get("message")?)?;
            Ok(TreeOperation::Replace { id, message })
        }
        "Remove" => {
            let id: String = t.get("id")?;
            Ok(TreeOperation::Remove { id })
        }
        "Update" => {
            let id: String = t.get("id")?;
            let message = lua_to_message(t.get("message")?)?;
            Ok(TreeOperation::Update { id, message })
        }
        other => Err(mlua::Error::runtime(format!("unknown op type: {other}"))),
    }
}

fn lua_to_message(t: LuaTable) -> mlua::Result<MessageState> {
    let id: String = t.get("id")?;
    let type_str: String = t.get("message_type")?;
    let message_type = parse_message_type(&type_str);
    let text: Option<String> = t.get("text")?;
    let brief: Option<String> = t.get("brief")?;
    let expanded: bool = t.get("expanded")?;
    let show_more: bool = t.get("show_more")?;
    let hidden: bool = t.get("hidden")?;
    let group: bool = t.get("group")?;

    let children_table: LuaTable = t.get("children")?;
    let mut children = Vec::new();
    for value in children_table.sequence_values::<LuaTable>() {
        children.push(lua_to_message(value?)?);
    }

    let mut msg = MessageState::new(id)
        .message_type(message_type)
        .expanded(expanded)
        .show_more(show_more)
        .hidden(hidden)
        .group(group);
    if let Some(s) = text {
        msg = msg.text(s);
    }
    if let Some(s) = brief {
        msg = msg.brief(s);
    }
    msg.children = children;
    Ok(msg)
}

fn parse_message_type(s: &str) -> MessageType {
    match s {
        "UserMessage" => MessageType::UserMessage,
        "AgentMessage" => MessageType::AgentMessage,
        "ToolCall" => MessageType::ToolCall,
        "Thinking" => MessageType::Thinking,
        "Container" => MessageType::Container,
        "TaskSummary" => MessageType::TaskSummary,
        "System" => MessageType::System,
        _ => MessageType::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(script: &str) -> LuaTransformConfig {
        LuaTransformConfig {
            script: Some(script.to_string()),
            script_path: None,
        }
    }

    fn append_op(id: &str) -> TreeOperation {
        TreeOperation::Append {
            parent_id: None,
            message: MessageState::new(id).message_type(MessageType::AgentMessage),
        }
    }

    #[test]
    fn passthrough_script() {
        let mut t =
            LuaTransform::new(&make_config("function process(ops) return ops end")).unwrap();
        let out = t.process(vec![append_op("a"), append_op("b")]);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], TreeOperation::Append { message, .. } if message.id == "a"));
        assert!(matches!(&out[1], TreeOperation::Append { message, .. } if message.id == "b"));
    }

    #[test]
    fn drop_script() {
        let mut t = LuaTransform::new(&make_config("function process(ops) return {} end")).unwrap();
        let out = t.process(vec![append_op("a"), append_op("b")]);
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn transform_script_clears_show_more() {
        let script = r#"
            function process(ops)
                for _, op in ipairs(ops) do
                    if op.message then
                        op.message.show_more = false
                    end
                end
                return ops
            end
        "#;
        let mut msg = MessageState::new("x").message_type(MessageType::AgentMessage);
        msg.show_more = true;
        let op = TreeOperation::Append {
            parent_id: None,
            message: msg,
        };
        let mut t = LuaTransform::new(&make_config(script)).unwrap();
        let out = t.process(vec![op]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            TreeOperation::Append { message, .. } => assert!(!message.show_more),
            _ => panic!("expected Append"),
        }
    }

    #[test]
    fn error_recovery() {
        let mut t =
            LuaTransform::new(&make_config("function process(ops) error('oops') end")).unwrap();
        let out = t.process(vec![append_op("a")]);
        // Original batch returned unchanged on Lua error.
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], TreeOperation::Append { message, .. } if message.id == "a"));
    }

    #[test]
    fn update_round_trips_through_lua() {
        let mut t =
            LuaTransform::new(&make_config("function process(ops) return ops end")).unwrap();
        let msg = MessageState::new("u")
            .message_type(MessageType::AgentMessage)
            .text("streaming text");
        let op = TreeOperation::Update {
            id: "u".to_string(),
            message: msg,
        };
        let out = t.process(vec![op]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            TreeOperation::Update { id, message } => {
                assert_eq!(id, "u");
                assert_eq!(message.text.as_deref(), Some("streaming text"));
            }
            _ => panic!("expected Update after round-trip"),
        }
    }
}
