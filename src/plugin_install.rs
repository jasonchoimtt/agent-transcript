use std::path::{Path, PathBuf};

use crate::providers::ProviderKind;

// Embedded plugin files, compiled into the binary.
const CLAUDE_MANIFEST: &str = include_str!("plugins/claude/.claude-plugin/plugin.json");
const CLAUDE_HOOKS: &str = include_str!("plugins/claude/hooks/hooks.json");

struct PluginSpec {
    /// Name of the manifest subdirectory (e.g. `.claude-plugin`).
    manifest_subdir: &'static str,
    manifest_json: &'static str,
    hooks_json: &'static str,
}

struct PluginInfo {
    spec: PluginSpec,
    /// Directory name under `~/.config/agent-transcript/`.  Shown by the CLI
    /// when listing loaded plugins, so it includes "agent-transcript" as a
    /// prefix to make the origin obvious.
    dir_name: &'static str,
}

fn plugin_info(provider: &ProviderKind) -> Option<PluginInfo> {
    match provider {
        ProviderKind::Claude => Some(PluginInfo {
            spec: PluginSpec {
                manifest_subdir: ".claude-plugin",
                manifest_json: CLAUDE_MANIFEST,
                hooks_json: CLAUDE_HOOKS,
            },
            dir_name: "agent-transcript-claude",
        }),
        ProviderKind::Cursor => None,
    }
}

/// Resolve `$XDG_CONFIG_HOME` or fall back to `$HOME/.config`.
fn system_config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config")
}

/// Extract the `"version"` field from a `plugin.json` string.
fn manifest_version(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v["version"].as_str().map(|s| s.to_string())
}

/// Write plugin files into `plugin_dir` if the deployed version differs from
/// the embedded one, then return.
///
/// When a write is needed the whole directory is removed first so no stale
/// files from a prior plugin version linger.
fn extract_to(spec: &PluginSpec, plugin_dir: &Path) -> color_eyre::Result<()> {
    let manifest_path = plugin_dir.join(spec.manifest_subdir).join("plugin.json");

    let deployed = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|s| manifest_version(&s));
    let embedded = manifest_version(spec.manifest_json);
    if embedded.is_some() && embedded == deployed {
        return Ok(());
    }

    // Remove the whole directory first to avoid stale files from prior versions.
    if plugin_dir.exists() {
        std::fs::remove_dir_all(plugin_dir)?;
    }

    std::fs::create_dir_all(plugin_dir.join(spec.manifest_subdir))?;
    std::fs::create_dir_all(plugin_dir.join("hooks"))?;
    std::fs::write(manifest_path, spec.manifest_json)?;
    std::fs::write(plugin_dir.join("hooks").join("hooks.json"), spec.hooks_json)?;

    Ok(())
}

/// Ensure the plugin for `provider` is written to
/// `~/.config/agent-transcript/<provider>-plugin/` and return that path.
///
/// The deployed version is compared against the embedded version by the
/// `"version"` field in `plugin.json`.  Files are only written when the
/// version differs or the directory is absent, so repeated launches are cheap.
///
/// Returns an error for providers that have no plugin (e.g. `Cursor`).
pub fn extract_plugin(provider: &ProviderKind) -> color_eyre::Result<PathBuf> {
    let info = plugin_info(provider)
        .ok_or_else(|| color_eyre::eyre::eyre!("no plugin defined for provider {:?}", provider))?;
    let plugin_dir = system_config_dir()
        .join("agent-transcript")
        .join(info.dir_name);
    extract_to(&info.spec, &plugin_dir)?;
    Ok(plugin_dir)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agt-plugin-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn claude_plugin_creates_expected_files() {
        let base = tmp_dir();
        let plugin_dir = base.join("agent-transcript-claude");
        let info = plugin_info(&ProviderKind::Claude).unwrap();
        extract_to(&info.spec, &plugin_dir).expect("extract must succeed");

        assert!(plugin_dir.join(".claude-plugin/plugin.json").exists());
        assert!(plugin_dir.join("hooks/hooks.json").exists());

        let manifest: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(plugin_dir.join(".claude-plugin/plugin.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["version"], "1.0.0");
    }

    #[test]
    fn idempotent_when_version_matches() {
        let base = tmp_dir();
        let plugin_dir = base.join("agent-transcript-claude");
        let info = plugin_info(&ProviderKind::Claude).unwrap();

        extract_to(&info.spec, &plugin_dir).unwrap();

        // Corrupt the hooks file, then re-extract — version matches so no overwrite.
        let hooks_path = plugin_dir.join("hooks/hooks.json");
        fs::write(&hooks_path, "{}").unwrap();
        extract_to(&info.spec, &plugin_dir).unwrap();
        assert_eq!(fs::read_to_string(&hooks_path).unwrap(), "{}");
    }

    #[test]
    fn re_extracts_when_version_differs() {
        let base = tmp_dir();
        let plugin_dir = base.join("agent-transcript-claude");
        let info = plugin_info(&ProviderKind::Claude).unwrap();

        extract_to(&info.spec, &plugin_dir).unwrap();

        // Write a stale version to the manifest to trigger a re-extraction.
        let manifest_path = plugin_dir.join(".claude-plugin/plugin.json");
        fs::write(&manifest_path, r#"{"version":"0.9.0"}"#).unwrap();
        let hooks_path = plugin_dir.join("hooks/hooks.json");
        fs::write(&hooks_path, "{}").unwrap();

        extract_to(&info.spec, &plugin_dir).unwrap();
        assert_ne!(fs::read_to_string(&hooks_path).unwrap(), "{}");
    }

    #[test]
    fn cursor_has_no_plugin() {
        assert!(extract_plugin(&ProviderKind::Cursor).is_err());
    }
}
