//! Tester: quick (~2s, solo verifica) y normal (build real + auto-delete).
//! M2: solo quick. normal devuelve MEH honesto hasta implementarse.
use crate::detectors;
use crate::TestResult;

fn result(format: &str, state: &str, reason: &str, test_type: &str) -> TestResult {
    TestResult {
        format: format.to_string(),
        state: state.to_string(),
        reason: reason.to_string(),
        test_type: test_type.to_string(),
        success: None,
        error_log: None,
        command_output: None,
        artifact_path: None,
    }
}

/// Minimum tool proving the format can be built here.
fn tool_for(format: &str) -> Option<&'static str> {
    match format {
        "deb" => Some("dpkg-deb"),
        "rpm" => Some("rpmbuild"),
        "pacman" => Some("makepkg"),
        "xbps" => Some("xbps-create"),
        "appimage" => None, // linuxdeploy/appimagetool: se verifica aparte
        "exe" => Some("makensis"),
        "msi" => Some("wixl"),
        "dmg" => Some("hdiutil"),
        "pkg" => Some("pkgbuild"),
        "zip" => Some("zip"),
        _ => None,
    }
}

/// Single-format check (for step-by-step execution with real progress).
pub fn check_format(format: &str) -> TestResult {
    let need_os = match format {
        "deb" | "rpm" | "pacman" | "xbps" | "appimage" | "flatpak" | "snap" => "linux",
        "exe" | "msi" => "windows",
        "dmg" | "pkg" => "macos",
        // zip is portable: the tool exists on every host OS, so the gate
        // only checks the tool (generic path below), never the OS.
        "zip" => detectors::os_name(),
        _ => {
            return result(
                format,
                "MEH",
                &format!("unknown format '{format}'"),
                "quick",
            )
        }
    };
    // exe runs natively on Linux too (makensis is cross-platform).
    // msi builds on Linux via wixl (msitools subset); native WiX stays a
    // Windows-host concern for later.
    if format != "exe" && format != "msi" && need_os != detectors::os_name() {
        return result(
            format,
            "BLOCKED",
            &format!("requires {need_os}, this host is {}", detectors::os_name()),
            "quick",
        );
    }
    if format == "appimage" {
        if detectors::tool_exists("linuxdeploy") || detectors::tool_exists("appimagetool") {
            return result(format, "SAFE", "linuxdeploy/appimagetool found", "quick");
        }
        return result(
            format,
            "BLOCKED",
            "missing linuxdeploy or appimagetool (installs with permission)",
            "quick",
        );
    }
    if format == "flatpak" {
        if !detectors::tool_exists("flatpak-builder") {
            return result(
                format,
                "BLOCKED",
                "missing flatpak-builder (installs with permission)",
                "quick",
            );
        }
        // Runtime + SDK present? (GBs, one time — never fetched silently.)
        let mut missing = Vec::new();
        if !crate::install::flatpak_runtime_present("org.freedesktop.Platform") {
            missing.push("org.freedesktop.Platform runtime");
        }
        if !crate::install::flatpak_runtime_present("org.freedesktop.Sdk") {
            missing.push("org.freedesktop.Sdk");
        }
        if !missing.is_empty() {
            return result(
                format,
                "MEH",
                &format!(
                    "missing {} (press Download: provisions everything, ~2 GB one time)",
                    missing.join(" + ")
                ),
                "quick",
            );
        }
        return result(format, "SAFE", "flatpak-builder + SDK found", "quick");
    }
    if format == "snap" {
        if !detectors::tool_exists("snapcraft") {
            return result(
                format,
                "BLOCKED",
                "missing snapcraft (installs with permission)",
                "quick",
            );
        }
        // The socket unit is the persistent one (service socket-activates).
        if !crate::install::snapd_active() {
            return result(
                format,
                "MEH",
                "snapd not running (press Download, or: sudo systemctl enable --now snapd.socket)",
                "quick",
            );
        }
        // Native destructive builds only run on Ubuntu hosts. Elsewhere
        // use Isolated (ubuntu container, automatic) or Remote — no LXD.
        if detectors::os_id().as_deref() != Some("ubuntu") {
            return result(
                format,
                "MEH",
                "native snap needs Ubuntu; use Isolated (container) or Remote",
                "quick",
            );
        }
        return result(format, "SAFE", "snapcraft + snapd found", "quick");
    }
    match tool_for(format) {
        Some(tool) if detectors::tool_exists(tool) => {
            result(format, "SAFE", &format!("{tool} found"), "quick")
        }
        Some(tool) => result(
            format,
            "BLOCKED",
            &format!("missing {tool} (installs with permission)"),
            "quick",
        ),
        None => result(format, "MEH", "no check defined", "quick"),
    }
}

/// Quick test: checks OS + tools. Builds nothing. ~2 seconds.
pub fn quick_test(_path: &str, formats: &[String]) -> Vec<TestResult> {
    formats.iter().map(|f| check_format(f)).collect()
}

/// Normal test: real build + verification + cleanup. Blocking;
// the UI runs it on a background thread with cancellation.
pub fn normal_test(path: &str, formats: &[String]) -> Vec<TestResult> {
    use crate::builders::{build_format, BuildCtl};
    use crate::detectors::detect_stack;
    let (_stack, name, version) = detect_stack(std::path::Path::new(path));
    let ctl = BuildCtl::fresh();
    formats
        .iter()
        .map(|f| build_format(path, &name, &version, f, &ctl))
        .collect()
}
