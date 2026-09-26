//! Cache in OWN data dir. Never inside the user's project.
use std::path::PathBuf;

/// ~/.config/bundleforge/cache/ (XDG_CONFIG_HOME if set).
pub fn cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        format!("{home}/.config")
    });
    PathBuf::from(base).join("bundleforge").join("cache")
}

/// FNV-1a 64-bit: stable hash with no dependencies.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Cache file for a project (by hash of its absolute path).
pub fn path_for(project_path: &str) -> PathBuf {
    cache_dir().join(format!("{:016x}.json", fnv1a(project_path)))
}

pub fn read(project_path: &str) -> Option<String> {
    std::fs::read_to_string(path_for(project_path)).ok()
}

pub fn write(project_path: &str, content: &str) -> bool {
    let p = path_for(project_path);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(p, content).is_ok()
}
