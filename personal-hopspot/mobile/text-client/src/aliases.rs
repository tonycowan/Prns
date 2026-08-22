//! Per-peer display aliases (persisted locally).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::HeardAnnounce;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AliasStore {
    aliases: HashMap<String, String>,
    #[serde(default)]
    next_default: u32,
}

fn store_path() -> PathBuf {
    #[cfg(target_os = "android")]
    {
        return PathBuf::from("/data/data/org.personal.textclient/files/aliases.json");
    }
    #[cfg(not(target_os = "android"))]
    {
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join(".personal-text-client").join("aliases.json")
    }
}

pub fn load() -> HashMap<String, String> {
    let path = store_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    serde_json::from_str::<AliasStore>(&text)
        .map(|store| store.aliases)
        .unwrap_or_default()
}

fn save(map: &HashMap<String, String>, next_default: u32) {
    let path = store_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let store = AliasStore {
        aliases: map.clone(),
        next_default,
    };
    if let Ok(json) = serde_json::to_string_pretty(&store) {
        let _ = std::fs::write(path, json);
    }
}

/// Assign default names for newly seen peers; returns updated map and next counter.
pub fn ensure_defaults(
    heard: &[HeardAnnounce],
    extra_peers: &[String],
    mut aliases: HashMap<String, String>,
    mut next_default: u32,
) -> (HashMap<String, String>, u32) {
    if next_default == 0 {
        next_default = aliases
            .values()
            .filter_map(|name| name.strip_prefix("Alias ")?.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
    }
    let mut seen = HashSet::new();
    for entry in heard {
        if !seen.insert(entry.destination_hex.clone()) {
            continue;
        }
        if aliases.contains_key(&entry.destination_hex) {
            continue;
        }
        aliases.insert(
            entry.destination_hex.clone(),
            format!("Alias {next_default}"),
        );
        next_default += 1;
    }
    for hex in extra_peers {
        if !seen.insert(hex.clone()) {
            continue;
        }
        if aliases.contains_key(hex) {
            continue;
        }
        aliases.insert(hex.clone(), format!("Alias {next_default}"));
        next_default += 1;
    }
    (aliases, next_default)
}

pub fn persist(map: &HashMap<String, String>, next_default: u32) {
    save(map, next_default);
}

pub fn display_name(hex: &str, aliases: &HashMap<String, String>) -> String {
    aliases
        .get(hex)
        .cloned()
        .unwrap_or_else(|| hex.to_string())
}
