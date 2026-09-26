//! Stack/tool/OS detection (read-only, never modifies anything).
use std::path::Path;

/// Does the command exist in PATH?
pub fn tool_exists(cmd: &str) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).is_file();
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join(cmd);
            if p.is_file() {
                return true;
            }
            // Windows: try with .exe
            if cfg!(windows) && p.with_extension("exe").is_file() {
                return true;
            }
        }
    }
    // GUI-launched apps often lack ~/.local/bin in PATH, but that's
    // exactly where we install user-local tools (e.g. linuxdeploy).
    if let Ok(home) = std::env::var("HOME") {
        let p = Path::new(&home).join(".local").join("bin").join(cmd);
        if p.is_file() {
            return true;
        }
    }
    // Same story for /snap/bin (snap-store binaries like snapcraft):
    // on PATH after relogin, invisible to GUI apps before that.
    if Path::new("/snap/bin").join(cmd).is_file() {
        return true;
    }
    // Canonical snap binary dir (usually what /snap symlinks to).
    if Path::new("/var/lib/snapd/snap/bin").join(cmd).is_file() {
        return true;
    }
    false
}

pub fn os_name() -> &'static str {
    std::env::consts::OS // "linux" | "windows" | "macos" | ...
}

/// Distro ID from /etc/os-release (linux only), e.g. "arch", "ubuntu",
/// "fedora", "debian". Pure parser below is unit-tested.
pub fn os_id() -> Option<String> {
    let data = std::fs::read_to_string("/etc/os-release").ok()?;
    parse_os_id(&data)
}

fn parse_os_id(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("ID=") {
            let v = v.trim().trim_matches('"').trim().to_lowercase();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Detects stack from marker files. Returns (stack, name, version).
pub fn detect_stack(dir: &Path) -> (String, String, String) {
    let has = |f: &str| dir.join(f).exists();
    // Order: most specific first.
    if has("Cargo.toml") {
        return (
            "rust".into(),
            guess_name(dir, "Cargo.toml"),
            guess_version(dir, "Cargo.toml"),
        );
    }
    if has("package.json") {
        return (
            "node".into(),
            guess_name(dir, "package.json"),
            guess_version(dir, "package.json"),
        );
    }
    if has("pyproject.toml") || has("setup.py") || has("setup.cfg") {
        return ("python".into(), dir_name(dir), "0.0.0".into());
    }
    if has("CMakeLists.txt") {
        return ("cmake".into(), dir_name(dir), "0.0.0".into());
    }
    if has("go.mod") {
        return ("go".into(), dir_name(dir), "0.0.0".into());
    }
    if dir.join("pom.xml").exists() || dir.join("build.gradle").exists() {
        return ("java".into(), dir_name(dir), "0.0.0".into());
    }
    ("unknown".into(), dir_name(dir), "0.0.0".into())
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

// Best-effort: looks for "name = ..." / "version = ..." (toml) o
// "name": "..." / "version": "..." (json). Si no hay, "unknown"/"0.0.0".
fn guess_name(dir: &Path, file: &str) -> String {
    guess_field(dir, file, "name").unwrap_or_else(|| dir_name(dir))
}

fn guess_version(dir: &Path, file: &str) -> String {
    guess_field(dir, file, "version").unwrap_or_else(|| "0.0.0".to_string())
}

fn guess_field(dir: &Path, file: &str, field: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(file)).ok()?;
    for line in text.lines() {
        let t = line.trim();
        // toml: name = "foo"   |   json: "name": "foo",
        for sep in ["=", ":"] {
            if let Some((k, v)) = t.split_once(sep) {
                let key = k.trim().trim_matches('"').trim();
                if key == field {
                    let val = v.trim().trim_matches([',', '"', '\'', ' ']);
                    let val = val
                        .trim_end_matches(',')
                        .trim_matches('"')
                        .trim_matches('\'');
                    if !val.is_empty() && val.len() < 120 {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_os_id_cases() {
        assert_eq!(
            parse_os_id("NAME=\"Arch Linux\"\nID=arch\nPRETTY_NAME=\"Arch Linux\"\n"),
            Some("arch".to_string())
        );
        assert_eq!(
            parse_os_id("ID=\"ubuntu\"\nVERSION_ID=\"24.04\"\n"),
            Some("ubuntu".to_string())
        );
        assert_eq!(parse_os_id("NAME=no-id-here\n"), None);
        assert_eq!(parse_os_id(""), None);
    }
}
