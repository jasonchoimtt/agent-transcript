use std::path::PathBuf;

const HOOK_COMMAND: &str = "agt hook";

fn hooks_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".cursor").join("hooks.json")
}

/// Returns true if the `sessionStart` hook is already present in `~/.cursor/hooks.json`.
pub fn is_installed() -> bool {
    let Ok(data) = std::fs::read_to_string(hooks_path()) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
        return false;
    };
    v["hooks"]["sessionStart"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .any(|h| h["command"].as_str() == Some(HOOK_COMMAND))
        })
        .unwrap_or(false)
}

/// Merge the `sessionStart` hook into `~/.cursor/hooks.json`, creating the file
/// if it does not yet exist.
pub fn install() -> color_eyre::Result<()> {
    let path = hooks_path();

    let mut v: serde_json::Value = if path.exists() {
        let data = std::fs::read_to_string(&path)?;
        serde_json::from_str(&data)?
    } else {
        serde_json::json!({ "version": 1, "hooks": {} })
    };

    if v["hooks"].is_null() {
        v["hooks"] = serde_json::json!({});
    }
    if v["hooks"]["sessionStart"].is_null() {
        v["hooks"]["sessionStart"] = serde_json::json!([]);
    }

    let arr = v["hooks"]["sessionStart"]
        .as_array_mut()
        .ok_or_else(|| color_eyre::eyre::eyre!("sessionStart is not an array"))?;

    if !arr
        .iter()
        .any(|h| h["command"].as_str() == Some(HOOK_COMMAND))
    {
        arr.push(serde_json::json!({ "command": HOOK_COMMAND }));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&path, serde_json::to_string_pretty(&v)?)?;
    println!("Installed cursor session hook in {}", path.display());
    Ok(())
}
