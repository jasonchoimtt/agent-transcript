use std::path::PathBuf;

const HOOK_COMMAND: &str = "agt hook";

fn settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".claude").join("settings.json")
}

/// Returns true if any group in `hooks.SessionStart` already contains our command.
fn hook_present(v: &serde_json::Value) -> bool {
    v["hooks"]["SessionStart"]
        .as_array()
        .map(|groups| {
            groups.iter().any(|group| {
                group["hooks"]
                    .as_array()
                    .map(|hooks| {
                        hooks
                            .iter()
                            .any(|h| h["command"].as_str() == Some(HOOK_COMMAND))
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Returns true if the `SessionStart` hook is already present in `~/.claude/settings.json`.
pub fn is_installed() -> bool {
    let Ok(data) = std::fs::read_to_string(settings_path()) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
        return false;
    };
    hook_present(&v)
}

/// Merge the `SessionStart` hook into `~/.claude/settings.json`, creating the file
/// if it does not yet exist.
pub fn install() -> color_eyre::Result<()> {
    let path = settings_path();

    let mut v: serde_json::Value = if path.exists() {
        let data = std::fs::read_to_string(&path)?;
        serde_json::from_str(&data)?
    } else {
        serde_json::json!({})
    };

    if v["hooks"].is_null() {
        v["hooks"] = serde_json::json!({});
    }
    if v["hooks"]["SessionStart"].is_null() {
        v["hooks"]["SessionStart"] = serde_json::json!([]);
    }

    if !hook_present(&v) {
        let arr = v["hooks"]["SessionStart"]
            .as_array_mut()
            .ok_or_else(|| color_eyre::eyre::eyre!("SessionStart is not an array"))?;
        arr.push(serde_json::json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": HOOK_COMMAND }]
        }));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&path, serde_json::to_string_pretty(&v)?)?;
    println!("Installed Claude session hook in {}", path.display());
    Ok(())
}
