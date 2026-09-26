//! Phase 3 (slice 1): remote builders over SSH + OS-aware Help matrix.
//!
//! No daemon: a remote is any machine with `bundleforge-core` and an SSH
//! server. Auth is SSH keys only (BatchMode) — BundleForge never touches
//! remote passwords. Config persists in
//! `$HOME/.config/bundleforge/remotes` (line-based, zero new deps).
//!
//! This slice: config + connection test + Help matrix content.
//! Offload (remote build/package over the same SSH) comes next.

use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct Remote {
    pub name: String,
    pub host: String,
    pub user: String,
    /// Remote OS: "linux" | "windows" (drives matrix suggestions).
    pub os: String,
    /// Remote bundleforge-core binary (PATH lookup or absolute path).
    pub core_path: String,
}

impl Remote {
    pub fn new(name: &str, host: &str, user: &str, os: &str) -> Self {
        Self {
            name: name.trim().to_string(),
            host: host.trim().to_string(),
            user: user.trim().to_string(),
            os: os.trim().to_lowercase(),
            core_path: "bundleforge-core".to_string(),
        }
    }

    pub fn valid(&self) -> bool {
        !self.name.is_empty()
            && !self.host.is_empty()
            && !self.user.is_empty()
            && (self.os == "linux" || self.os == "windows")
            && !self.core_path.is_empty()
    }

    pub fn target(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '|' => o.push_str("\\|"),
            '\n' => o.push_str("\\n"),
            _ => o.push(c),
        }
    }
    o
}

/// Split on unescaped `|`, resolving `\`-escapes. None on trailing `\`.
fn split_escaped(line: &str) -> Option<Vec<String>> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                'n' => fields.last_mut().unwrap().push('\n'),
                other => fields.last_mut().unwrap().push(other),
            }
        } else if c == '|' {
            fields.push(String::new());
        } else {
            fields.last_mut().unwrap().push(c);
        }
    }
    Some(fields)
}

fn to_line(r: &Remote) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        esc(&r.name),
        esc(&r.host),
        esc(&r.user),
        esc(&r.os),
        esc(&r.core_path)
    )
}

fn from_line(line: &str) -> Option<Remote> {
    let f = split_escaped(line)?;
    if f.len() != 5 {
        return None;
    }
    let r = Remote {
        name: f[0].clone(),
        host: f[1].clone(),
        user: f[2].clone(),
        os: f[3].clone(),
        core_path: f[4].clone(),
    };
    if r.valid() {
        Some(r)
    } else {
        None
    }
}

pub fn config_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config").join("bundleforge").join("remotes"))
}

/// Missing file = no remotes yet (not an error). Bad lines are skipped.
pub fn load_remotes() -> Vec<Remote> {
    let Some(p) = config_path() else {
        return Vec::new();
    };
    let Ok(data) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    data.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(from_line)
        .collect()
}

pub fn save_remotes(remotes: &[Remote]) -> Result<(), String> {
    let p = config_path().ok_or_else(|| "no HOME".to_string())?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("config dir: {e}"))?;
    }
    let mut out = String::from("# BundleForge remotes: name|host|user|os|core_path\n");
    for r in remotes {
        out.push_str(&to_line(r));
        out.push('\n');
    }
    std::fs::write(&p, out).map_err(|e| format!("saving remotes: {e}"))
}

/// Pure argv for the probe (unit-tested). Keys only: BatchMode never
/// prompts for a password; accept-new records first-seen host keys.
pub fn ssh_probe_argv(r: &Remote) -> (String, Vec<String>) {
    let remote_cmd = format!("echo BF_OK; command -v {} || echo BF_NO_CORE", r.core_path);
    (
        "ssh".to_string(),
        vec![
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ConnectTimeout=8".to_string(),
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
            r.target(),
            remote_cmd,
        ],
    )
}

/// Parse probe output into (ok_seen, no_core_seen).
/// Lines starting with `$ ` are our own command echo (run_logged records
/// every invocation) and must be skipped: they literally contain the
/// sentinel words, which once made every probe report "no core".
fn parse_probe_body(log: &str) -> (bool, bool) {
    let mut ok = false;
    let mut no_core = false;
    for line in log.lines().map(str::trim) {
        if line.starts_with("$ ") {
            continue;
        }
        if line == "BF_OK" {
            ok = true;
        } else if line == "BF_NO_CORE" {
            no_core = true;
        }
    }
    (ok, no_core)
}

/// Honest 3-state probe: ready / reachable-but-no-core / unreachable.
pub fn test_remote(r: &Remote, ctl: &Arc<crate::builders::BuildCtl>) -> Result<String, String> {
    if !crate::detectors::tool_exists("ssh") {
        return Err("no ssh client on this host".to_string());
    }
    let tmp = std::env::temp_dir().join(format!(
        "bundleforge-remote-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("temp dir: {e}"))?;
    let (prog, args) = ssh_probe_argv(r);
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (ok, cancelled) = crate::builders::run_logged(
        &prog,
        &arg_refs,
        &tmp,
        &tmp,
        30,
        ctl,
        &[],
        &crate::container::Runner::Host,
    );
    let stdout = std::fs::read_to_string(tmp.join("stdout.log")).unwrap_or_default();
    let stderr = std::fs::read_to_string(tmp.join("stderr.log")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&tmp);
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    let (saw_ok, saw_no_core) = parse_probe_body(&stdout);
    if !saw_ok || !ok {
        let tail = stderr.lines().last().unwrap_or("ssh failed").trim();
        return Err(format!("unreachable ({}). Check host, user and your SSH key.", tail));
    }
    if saw_no_core {
        return Err(format!(
            "reachable, but '{}' not found there. Install bundleforge-core on {} first.",
            r.core_path, r.host
        ));
    }
    Ok(format!("{} ready (core present)", r.name))
}

/// Static OS-aware recommendation per format (Help view, Option 3).
/// Full guidance on Linux hosts; other hosts get an honest pointer
/// until their native UI lands.
pub fn recommendation(format: &str, host_os: &str) -> &'static str {
    if host_os != "linux" {
        return "Remote builders carry this format — configure one under Remotes.";
    }
    match format {
        "pacman" => {
            "Native: makepkg on Arch. Container: automatic (arch image). Remote: any Linux builder."
        }
        "rpm" => {
            "Native: rpmbuild (or install it — Option 1). Container: automatic (fedora image). Remote: any Linux builder."
        }
        "deb" => {
            "Native: dpkg-deb (or install it — Option 1). Container: automatic (debian image). Remote: any Linux builder."
        }
        "exe" => {
            "Native: makensis (Option 1; AUR on Arch). Container: automatic (debian + nsis). Remote: Linux builder, or Windows builder for native."
        }
        "appimage" => {
            "Native: linuxdeploy download (Option 1). Container: unavailable (needs FUSE). Remote: any Linux builder."
        }
        "msi" => {
            "Native: wixl via msitools (Option 1; WiX subset) on Linux, WiX on Windows. Container: automatic (debian + msitools). Remote: Linux builder, or Windows for native WiX."
        }
        "xbps" => {
            "Native: xbps-create (Option 1 builds it from source). Container: automatic (void image). Remote: Linux builder with xbps tools or podman."
        }
        "flatpak" => {
            "Native: flatpak-builder + Platform runtime (Option 1; SDK is GBs, one time). Container: unavailable (nested sandbox). Remote: Linux builder with SDK."
        }
        "snap" => {
            "Native: Ubuntu only (snapcraft + running snapd, Download provisions). Container: automatic (ubuntu image). Remote: Linux builder with snapd."
        }
        "zip" => {
            "Native: zip tool (Option 1, tiny). Container: automatic (debian + zip). Remote: any Linux builder."
        }
        _ => {
            "No builder on any host yet — check back in later milestones."
        }
    }
}

/// First configured remote able to build `format`, or None.
pub fn matching_remote<'a>(format: &str, remotes: &'a [Remote]) -> Option<&'a Remote> {
    match format {
        "exe" | "msi" => remotes
            .iter()
            .find(|r| r.os == "linux")
            .or_else(|| remotes.iter().find(|r| r.os == "windows")),
        _ => remotes.iter().find(|r| r.os == "linux"),
    }
}

// ---------------------------------------------------------------------------
// Slice 2: SSH offload. Send (tar pipe, no local temp tarball) → remote
// `bundleforge-core package` → fetch artifacts (tar pipe) → remote cleanup
// ALWAYS (success, failure or cancel). One chronological log via run_logged.
// ---------------------------------------------------------------------------

const SEND_TIMEOUT_SECS: u64 = 600;
const REMOTE_BUILD_TIMEOUT_SECS: u64 = 1800;
const FETCH_TIMEOUT_SECS: u64 = 600;
const REMOTE_CLEANUP_TIMEOUT_SECS: u64 = 60;

/// Fixed SSH options for every remote call (keys only, fast fail).
fn ssh_opts() -> &'static [&'static str] {
    &[
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]
}

/// Run `ssh <opts> <target> <remote_cmd>` through run_logged.
fn ssh_run(
    r: &Remote,
    remote_cmd: &str,
    tmp: &std::path::Path,
    ctl: &Arc<crate::builders::BuildCtl>,
    timeout_secs: u64,
) -> (bool, bool) {
    let mut args: Vec<String> = ssh_opts().iter().map(|s| s.to_string()).collect();
    args.push(r.target());
    args.push(remote_cmd.to_string());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    crate::builders::run_logged(
        "ssh",
        &arg_refs,
        tmp,
        tmp,
        timeout_secs,
        ctl,
        &[],
        &crate::container::Runner::Host,
    )
}

fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Local shell for pipes: bash + pipefail when available (tar-side errors
/// must fail the step, plain `a | b` only reports b), else plain sh.
fn pipe_shell() -> (&'static str, bool) {
    if crate::detectors::tool_exists("bash") {
        ("bash", true)
    } else {
        ("sh", false)
    }
}

/// `tar cz (minus junk) | ssh … 'mkdir -p <rdir> && tar xz -C <rdir>'`.
/// Pure function: unit-tested.
fn send_script(project: &std::path::Path, r: &Remote, remote_dir: &str) -> (String, Vec<String>) {
    use crate::container::sh_quote;
    let mut tar_parts = vec!["tar".to_string()];
    for d in crate::builders::EXCLUDE_DIRS {
        tar_parts.push(format!("--exclude={d}"));
    }
    tar_parts.push("-cz".to_string());
    tar_parts.push("-C".to_string());
    tar_parts.push(sh_quote(&project.to_string_lossy()));
    tar_parts.push(".".to_string());
    let inner = format!(
        "mkdir -p {d} && tar xz -C {d}",
        d = sh_quote(remote_dir)
    );
    let mut ssh_parts: Vec<String> = vec!["ssh".to_string()];
    ssh_parts.extend(ssh_opts().iter().map(|s| s.to_string()));
    ssh_parts.push(sh_quote(&r.target()));
    ssh_parts.push(sq(&inner));
    let (shell, pipefail) = pipe_shell();
    let script = format!(
        "{}{} | {}",
        if pipefail { "set -o pipefail; " } else { "" },
        tar_parts.join(" "),
        ssh_parts.join(" ")
    );
    (shell.to_string(), vec!["-c".to_string(), script])
}

/// `ssh … 'tar cz -C <pkgdir> .' | tar xz -C <fetchdir>`. Pure: unit-tested.
fn fetch_script(r: &Remote, pkgdir: &str, fetchdir: &std::path::Path) -> (String, Vec<String>) {
    use crate::container::sh_quote;
    let inner = format!("tar cz -C {} .", sh_quote(pkgdir));
    let (shell, pipefail) = pipe_shell();
    let script = format!(
        "{}ssh {} {} {} | tar xz -C {}",
        if pipefail { "set -o pipefail; " } else { "" },
        ssh_opts().join(" "),
        sh_quote(&r.target()),
        sq(&inner),
        sh_quote(&fetchdir.to_string_lossy())
    );
    (shell.to_string(), vec!["-c".to_string(), script])
}

fn run_shell(
    shell: &str,
    args: &[String],
    tmp: &std::path::Path,
    ctl: &Arc<crate::builders::BuildCtl>,
    timeout_secs: u64,
) -> (bool, bool) {
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    crate::builders::run_logged(
        shell,
        &arg_refs,
        tmp,
        tmp,
        timeout_secs,
        ctl,
        &[],
        &crate::container::Runner::Host,
    )
}

/// Remote package: send → remote `package` → fetch → remote cleanup always.
/// Returns a TestResult shaped like the local packager (SAFE + kept
/// artifact, or honest MEH). Local temp is always deleted too.
pub fn package_remote_format(
    project: &str,
    _name: &str,
    _version: &str,
    format: &str,
    remote: &Remote,
    out_base: &std::path::Path,
    ctl: &Arc<crate::builders::BuildCtl>,
) -> crate::TestResult {
    use crate::TestResult;
    let fail = |reason: String, tmp: &std::path::Path| TestResult {
        format: format.to_string(),
        state: "MEH".to_string(),
        reason,
        test_type: "package".to_string(),
        success: Some(false),
        error_log: Some(crate::builders::tail_lines(&tmp.join("stderr.log"), 60)),
        command_output: None,
        artifact_path: None,
    };
    if !crate::detectors::tool_exists("ssh") {
        return TestResult {
            format: format.to_string(),
            state: "MEH".to_string(),
            reason: "no ssh client on this host".to_string(),
            test_type: "package".to_string(),
            success: Some(false),
            error_log: None,
            command_output: None,
            artifact_path: None,
        };
    }
    if !crate::detectors::tool_exists("tar") {
        return TestResult {
            format: format.to_string(),
            state: "MEH".to_string(),
            reason: "no tar on this host".to_string(),
            test_type: "package".to_string(),
            success: Some(false),
            error_log: None,
            command_output: None,
            artifact_path: None,
        };
    }
    let tmp = crate::builders::temp_dir();
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        return TestResult {
            format: format.to_string(),
            state: "MEH".to_string(),
            reason: format!("no temp dir: {e}"),
            test_type: "package".to_string(),
            success: Some(false),
            error_log: None,
            command_output: None,
            artifact_path: None,
        };
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let remote_dir = format!("/tmp/bundleforge-{}-{nanos}", std::process::id());
    let remote_out = format!("{remote_dir}/bf-out");
    // Remote cleanup ALWAYS (best effort): success, failure or cancel.
    let cleanup_remote = || {
        let cmd = format!("rm -rf {}", crate::container::sh_quote(&remote_dir));
        ssh_run(remote, &cmd, &tmp, ctl, REMOTE_CLEANUP_TIMEOUT_SECS);
    };
    // 1) Send (tar pipe, junk excluded like local staging).
    {
        let (shell, args) =
            send_script(std::path::Path::new(project), remote, &remote_dir);
        let (ok, cancelled) = run_shell(&shell, &args, &tmp, ctl, SEND_TIMEOUT_SECS);
        if cancelled {
            cleanup_remote();
            let r = fail("cancelled by user".to_string(), &tmp);
            let _ = std::fs::remove_dir_all(&tmp);
            return r;
        }
        if !ok {
            cleanup_remote();
            let r = fail("remote send failed (see details)".to_string(), &tmp);
            let _ = std::fs::remove_dir_all(&tmp);
            return r;
        }
    }
    // 2) Build on the remote with its own core (gate + builder + verify).
    {
        use crate::container::sh_quote;
        let cmd = format!(
            "cd {} && {} package . --formats {} --out ./bf-out",
            sh_quote(&remote_dir),
            sh_quote(&remote.core_path),
            sh_quote(format),
        );
        let (ok, cancelled) = ssh_run(remote, &cmd, &tmp, ctl, REMOTE_BUILD_TIMEOUT_SECS);
        if cancelled {
            cleanup_remote();
            let r = fail("cancelled by user".to_string(), &tmp);
            let _ = std::fs::remove_dir_all(&tmp);
            return r;
        }
        if !ok {
            cleanup_remote();
            let r = fail("remote build failed (see details)".to_string(), &tmp);
            let _ = std::fs::remove_dir_all(&tmp);
            return r;
        }
    }
    // 3) Fetch artifacts (tar pipe, symmetric with send).
    let fetchdir = tmp.join("fetch");
    if let Err(e) = std::fs::create_dir_all(&fetchdir) {
        cleanup_remote();
        let r = fail(format!("fetch dir: {e}"), &tmp);
        let _ = std::fs::remove_dir_all(&tmp);
        return r;
    }
    {
        let pkgdir = format!("{remote_out}/packages");
        let (shell, args) = fetch_script(remote, &pkgdir, &fetchdir);
        let (ok, cancelled) = run_shell(&shell, &args, &tmp, ctl, FETCH_TIMEOUT_SECS);
        if cancelled {
            cleanup_remote();
            let r = fail("cancelled by user".to_string(), &tmp);
            let _ = std::fs::remove_dir_all(&tmp);
            return r;
        }
        if !ok {
            cleanup_remote();
            let r = fail("remote fetch failed (see details)".to_string(), &tmp);
            let _ = std::fs::remove_dir_all(&tmp);
            return r;
        }
    }
    // 4) Keep artifacts locally (auto-rename on collision, like local).
    let mut kept: Vec<std::path::PathBuf> = Vec::new();
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&fetchdir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_file() {
                files.push(p);
            }
        }
    }
    files.sort();
    if files.is_empty() {
        cleanup_remote();
        let r = fail("remote build failed (see details)".to_string(), &tmp);
        let _ = std::fs::remove_dir_all(&tmp);
        return r;
    }
    let dest_dir = out_base.join("packages");
    let res = match std::fs::create_dir_all(&dest_dir) {
        Ok(()) => {
            for f in &files {
                match crate::builders::copy_unique(f, &dest_dir) {
                    Ok(d) => kept.push(d),
                    Err(e) => {
                        let r = fail(format!("keeping artifact: {e}"), &tmp);
                        cleanup_remote();
                        let _ = std::fs::remove_dir_all(&tmp);
                        return r;
                    }
                }
            }
            let first = kept.first().cloned();
            let size = first
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .map(|m| m.len())
                .unwrap_or(0);
            TestResult {
                format: format.to_string(),
                state: "SAFE".to_string(),
                reason: format!(
                    "saved · {} (remote {})",
                    crate::builders::human_size(size),
                    remote.name
                ),
                test_type: "package".to_string(),
                success: Some(true),
                error_log: None,
                command_output: None,
                artifact_path: first.map(|p| p.to_string_lossy().into_owned()),
            }
        }
        Err(e) => fail(
            format!("output dir {}: {e}", dest_dir.to_string_lossy()),
            &tmp,
        ),
    };
    cleanup_remote();
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_round_trip_with_escapes() {
        let r = Remote {
            name: "my|pc".to_string(),
            host: "192.168.1.5".to_string(),
            user: "al\\ex".to_string(),
            os: "linux".to_string(),
            core_path: "bundleforge-core".to_string(),
        };
        let back = from_line(&to_line(&r)).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn bad_lines_rejected() {
        assert!(from_line("a|b|c").is_none()); // too few
        assert!(from_line("n|h|u|plan9|core").is_none()); // bad os
        assert!(from_line("|h|u|linux|core").is_none()); // empty name
        assert!(from_line("trailing\\").is_none()); // dangling escape
    }

    #[test]
    fn probe_body_ignores_command_echo() {
        // Regression: run_logged prepends a `$ ssh …` marker line that
        // literally contains the sentinel words. Parsing must skip it.
        let marker = "$ ssh -o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=accept-new dev@h echo BF_OK; command -v bundleforge-core || echo BF_NO_CORE\n";
        // Marker alone (ssh produced nothing): neither state seen.
        assert_eq!(parse_probe_body(marker), (false, false));
        // Ready host: marker + real outputs.
        let ready = format!("{marker}BF_OK\n/usr/local/bin/bundleforge-core\n");
        assert_eq!(parse_probe_body(&ready), (true, false));
        // Missing core: marker + BF_OK + BF_NO_CORE as real lines.
        let missing = format!("{marker}BF_OK\nBF_NO_CORE\n");
        assert_eq!(parse_probe_body(&missing), (true, true));
    }

    #[test]
    fn probe_is_keys_only() {
        let r = Remote::new("lab", "192.168.1.5", "dev", "linux");
        let (prog, args) = ssh_probe_argv(&r);
        assert_eq!(prog, "ssh");
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(!args.iter().any(|a| a.contains("password")));
        assert!(args.contains(&"dev@192.168.1.5".to_string()));
    }

    #[test]
    fn matrix_covers_all_formats() {
        for f in ["pacman", "rpm", "deb", "exe", "appimage", "msi", "xbps", "flatpak", "snap", "???"] {
            let s = recommendation(f, "linux");
            assert!(!s.is_empty(), "{f}");
        }
        assert!(recommendation("msi", "linux").contains("wixl"));
        assert!(recommendation("xbps", "linux").contains("xbps-create"));
        assert!(recommendation("???", "linux").contains("No builder"));
        assert!(!recommendation("deb", "windows").is_empty());
    }

    #[test]
    fn matching_picks_right_os() {
        let lin = Remote::new("lab", "h1", "u", "linux");
        let win = Remote::new("desk", "h2", "u", "windows");
        let rs = vec![lin.clone(), win.clone()];
        assert_eq!(matching_remote("rpm", &rs).unwrap().name, "lab");
        assert_eq!(matching_remote("msi", &rs).unwrap().name, "lab");
        assert_eq!(matching_remote("exe", &rs).unwrap().name, "lab");
        assert_eq!(matching_remote("xbps", &rs).unwrap().name, "lab");
        assert!(matching_remote("rpm", &[]).is_none());
        // windows-only pool still serves exe/msi, never rpm
        let w = vec![win];
        assert_eq!(matching_remote("exe", &w).unwrap().name, "desk");
        assert_eq!(matching_remote("msi", &w).unwrap().name, "desk");
        assert!(matching_remote("rpm", &w).is_none());
    }

    #[test]
    fn send_script_pipes_without_temp_tarball() {
        let r = Remote::new("lab", "192.168.1.5", "dev", "linux");
        let (shell, args) =
            send_script(std::path::Path::new("/home/dev/proj"), &r, "/tmp/bf-1");
        assert!(shell == "bash" || shell == "sh");
        assert_eq!(args[0], "-c");
        let script = &args[1];
        // junk excluded like local staging
        for d in ["target", "node_modules", ".git", "dist"] {
            assert!(script.contains(&format!("--exclude={d}")), "{d}");
        }
        // tar pipe into ssh that unpacks on the remote — no .tar.gz file
        assert!(script.contains('|'));
        assert!(script.contains("tar xz -C"));
        assert!(script.contains("mkdir -p"));
        assert!(!script.contains(".tar.gz"));
        assert!(script.contains("dev@192.168.1.5"));
        assert!(script.contains("BatchMode=yes"));
    }

    #[test]
    fn fetch_script_is_symmetric_tar_pipe() {
        let r = Remote::new("lab", "192.168.1.5", "dev", "linux");
        let (shell, args) = fetch_script(
            &r,
            "/tmp/bf-1/bf-out/packages",
            std::path::Path::new("/tmp/fetch"),
        );
        assert!(shell == "bash" || shell == "sh");
        let script = &args[1];
        assert!(script.contains("tar cz -C"));
        assert!(script.contains("tar xz -C"));
        assert!(script.contains("dev@192.168.1.5"));
        // spaced dirs stay quoted on both ends
        let (_, args2) = fetch_script(
            &r,
            "/tmp/b f/bf-out/packages",
            std::path::Path::new("/tmp/fe tch"),
        );
        assert!(args2[1].contains("'/tmp/b f/bf-out/packages'"));
        assert!(args2[1].contains("'/tmp/fe tch'"));
    }
}
