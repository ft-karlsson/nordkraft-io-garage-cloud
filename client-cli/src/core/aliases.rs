//! Short names for containers, kept in `~/.nordkraft/aliases.json`.

use std::collections::HashMap;

use super::config::*;

pub(crate) fn load_aliases() -> HashMap<String, String> {
    let path = get_config_dir().join(ALIASES_FILE);
    if path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Ok(aliases) = serde_json::from_str(&contents) {
                return aliases;
            }
        }
    }
    HashMap::new()
}

pub(crate) fn save_aliases(
    aliases: &HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = get_config_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(ALIASES_FILE);
    std::fs::write(path, serde_json::to_string_pretty(aliases)?)?;
    Ok(())
}

pub(crate) fn resolve_alias(name: &str) -> String {
    let aliases = load_aliases();
    aliases
        .get(name)
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

pub(crate) fn set_alias(
    alias: &str,
    container_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut aliases = load_aliases();
    aliases.insert(alias.to_string(), container_name.to_string());
    save_aliases(&aliases)?;
    Ok(())
}

pub(crate) fn remove_alias(alias: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut aliases = load_aliases();
    aliases.remove(alias);
    save_aliases(&aliases)?;
    Ok(())
}
