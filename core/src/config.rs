//! UI config: last folders + tooltip flag.
//! Persisted at `~/.config/bundleforge/config.toml`.
//!
//! The file uses plain `key = value` lines, which is valid TOML for this
//! schema — but BundleForge parses/writes only these three keys with a
//! zero-dependency line parser (same precedent as `remote.rs`), so hand
//! edits outside the schema are ignored, never fatal. Corrupt file =
//! defaults, no crash. `BUNDLEFORGE_CONFIG` overrides the path (tests).
use std::path::{Path, PathBuf};

/// UI preferences persisted across runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub last_project_folder: String,
    pub last_output_folder: String,
    pub show_button_tips: bool,
    /// Persist each folder across runs; false = session-only (starts empty,
    /// saved value cleared when toggled off).
    pub persist_project_folder: bool,
    pub persist_output_folder: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            last_project_folder: String::new(),
            last_output_folder: String::new(),
            show_button_tips: true,
            persist_project_folder: true,
            persist_output_folder: true,
        }
    }
}

/// Config file location (overridable for tests).
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("BUNDLEFORGE_CONFIG") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    Path::new(&home).join(".config/bundleforge/config.toml")
}

fn unquote(s: &str) -> Option<String> {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        let inner = &t[1..t.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut it = inner.chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                match it.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(q @ ('"' | '\\')) => out.push(q),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        Some(out)
    } else {
        None
    }
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Pure parser (unit-tested). Unknown keys and `#` comments ignored.
pub fn parse_config(text: &str) -> Config {
    let mut cfg = Config::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "last_project_folder" => {
                if let Some(s) = unquote(v) {
                    cfg.last_project_folder = s;
                }
            }
            "last_output_folder" => {
                if let Some(s) = unquote(v) {
                    cfg.last_output_folder = s;
                }
            }
            "show_button_tips" => match v.trim() {
                "true" => cfg.show_button_tips = true,
                "false" => cfg.show_button_tips = false,
                _ => {}
            },
            "persist_project_folder" => match v.trim() {
                "true" => cfg.persist_project_folder = true,
                "false" => cfg.persist_project_folder = false,
                _ => {}
            },
            "persist_output_folder" => match v.trim() {
                "true" => cfg.persist_output_folder = true,
                "false" => cfg.persist_output_folder = false,
                _ => {}
            },
            _ => {}
        }
    }
    cfg
}

/// Pure renderer (unit-tested). Output is valid TOML for this schema.
pub fn render_config(cfg: &Config) -> String {
    format!(
        "# BundleForge UI config (written automatically, edits tolerated)\nlast_project_folder = {}\nlast_output_folder = {}\nshow_button_tips = {}\npersist_project_folder = {}\npersist_output_folder = {}\n",
        quote(&cfg.last_project_folder),
        quote(&cfg.last_output_folder),
        cfg.show_button_tips,
        cfg.persist_project_folder,
        cfg.persist_output_folder,
    )
}

/// Load from `path`. Missing file is NOT an error (returns defaults).
/// Corrupt content returns defaults, never crashes.
pub fn load_from(path: &Path) -> Config {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_config(&text),
        Err(_) => Config::default(),
    }
}

/// Save to `path` (creates parent dirs). Errors as strings.
pub fn save_to(path: &Path, cfg: &Config) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("config dir: {e}"))?;
    }
    std::fs::write(path, render_config(cfg)).map_err(|e| format!("writing config: {e}"))
}

/// Load from the standard location; missing file is created with defaults
/// (best effort — a failed write still returns usable defaults).
pub fn load() -> Config {
    let path = config_path();
    if !path.exists() {
        let _ = save_to(&path, &Config::default());
        return Config::default();
    }
    load_from(&path)
}

/// Save to the standard location.
pub fn save(cfg: &Config) -> Result<(), String> {
    save_to(&config_path(), cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_defaults() {
        let cfg = Config::default();
        assert_eq!(parse_config(&render_config(&cfg)), cfg);
    }

    #[test]
    fn roundtrip_values_with_quotes_and_backslashes() {
        let cfg = Config {
            last_project_folder: "/home/u/my \"quoted\" proj\\x".to_string(),
            last_output_folder: "/tmp/out".to_string(),
            show_button_tips: false,
            persist_project_folder: false,
            persist_output_folder: true,
        };
        assert_eq!(parse_config(&render_config(&cfg)), cfg);
    }

    #[test]
    fn corrupt_and_unknown_tolerated() {
        let cfg = parse_config(
            "last_project_folder = /unquoted\nshow_button_tips = maybe\nbogus_key = 1\n# comment\n\nlast_output_folder = \"/ok\"\n",
        );
        assert_eq!(cfg.last_project_folder, "");
        assert!(cfg.show_button_tips); // invalid bool keeps default
        assert_eq!(cfg.last_output_folder, "/ok");
    }

    #[test]
    fn save_load_file_roundtrip() {
        let dir = std::env::temp_dir().join("bf-test-config");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let cfg = Config {
            last_project_folder: "/a".to_string(),
            last_output_folder: "/b".to_string(),
            show_button_tips: false,
            persist_project_folder: true,
            persist_output_folder: true,
        };
        save_to(&path, &cfg).unwrap();
        assert_eq!(load_from(&path), cfg);
        assert_eq!(load_from(&dir.join("missing.toml")), Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
