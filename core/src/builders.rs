//! Real per-format builders.
//! pacman implemented (partial M2). Rest: honest MEH until their milestone.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Shared UI<->build-thread control. All Send+Sync.
/// Password is held in memory only until consumed, never logged.
pub struct BuildCtl {
    pub cancel: AtomicBool,
    pub password_needed: AtomicBool,
    pub password: Mutex<Option<String>>,
}

impl BuildCtl {
    pub fn fresh() -> Arc<Self> {
        Arc::new(Self {
            cancel: AtomicBool::new(false),
            password_needed: AtomicBool::new(false),
            password: Mutex::new(None),
        })
    }
    pub fn cancelled(ctl: &Arc<Self>) -> bool {
        ctl.cancel.load(Ordering::SeqCst)
    }
}

const BUILD_TIMEOUT_SECS: u64 = 600;
const LOG_TAIL_LINES: usize = 60;
/// Max wait for the user to type a password before failing.
const PASSWORD_WAIT_SECS: u64 = 180;

/// Does the byte tail look like a password prompt? (EN + ES + sudo style)
fn looks_like_password_prompt(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("password") || t.contains("contrase")
}

/// Dirs never copied into the payload (project build artifacts).
pub(crate) const EXCLUDE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "dist",
    "build",
    "out",
    ".svn",
    "__pycache__",
    ".venv",
    "venv",
    ".hg",
];

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub(crate) fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("bundleforge-{}-{}", std::process::id(), nanos()))
}

fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            if EXCLUDE_DIRS.contains(&name.as_str()) {
                continue;
            }
            // No recursar dentro de nuestro propio temp.
            copy_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

pub(crate) fn tail_lines(path: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn human_bytes(b: u64) -> String {
    if b < 1024 {
        format!("{b} B")
    } else if b < 1024 * 1024 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{:.1} MB", b as f64 / (1024.0 * 1024.0))
    }
}

fn sanitize_pkgname(s: &str) -> String {
    let mut out: String = s
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out = out.trim_matches(['-', '.', '_']).to_string();
    if out.is_empty() {
        "app".to_string()
    } else {
        out
    }
}

fn sanitize_pkgver(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '+' || c == '-' || c == ':'
            {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "0.1.0".to_string()
    } else {
        out
    }
}

/// Runs a command with file logs, timeout and cancellation.
/// Returns (success, was_cancelled).
pub(crate) fn run_logged(
    cmd: &str,
    args: &[&str],
    cwd: &Path,
    logdir: &Path,
    timeout_secs: u64,
    ctl: &Arc<BuildCtl>,
    extra_env: &[(&str, &str)],
    runner: &crate::container::Runner,
) -> (bool, bool) {
    use std::io::Write as _;
    // Container runs get a longer budget (pull + tool installs).
    let timeout_secs = match runner {
        crate::container::Runner::Container { .. } => timeout_secs.max(1800),
        _ => timeout_secs,
    };
    let out_log = logdir.join("stdout.log");
    let err_log = logdir.join("stderr.log");
    // Append mode: pull/setup/build share one chronological log per run.
    let open = |p: &Path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
    };
    let Ok(out_f) = open(&out_log) else {
        return (false, false);
    };
    let Ok(err_f) = open(&err_log) else {
        return (false, false);
    };
    // Container runs get a longer budget (pull + tool installs).
    let timeout_secs = match runner {
        crate::container::Runner::Container { .. } => timeout_secs.max(1800),
        _ => timeout_secs,
    };
    let mut command = match runner {
        crate::container::Runner::Host => {
            let mut c = Command::new(cmd);
            c.args(args).current_dir(cwd);
            for (k, v) in extra_env {
                c.env(k, v);
            }
            // Marker keeps multi-step logs readable.
            let _ = writeln!(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&out_log)
                    .unwrap(),
                "$ {} {}",
                cmd,
                args.join(" ")
            );
            c
        }
        crate::container::Runner::Container { runtime, image, setup } => {
            // One ephemeral run: setup + build chained in a single `sh -c`
            // script (separate runs would discard installed tools with the
            // container). Trailing chown keeps rootful/docker artifacts owned
            // by the host user (rootless: harmless no-op).
            let uid = crate::container::current_uid_gid();
            let argv = crate::container::container_argv(
                runtime,
                image,
                cwd,
                setup,
                cmd,
                args,
                extra_env,
                uid.as_ref().map(|(u, g)| (u.as_str(), g.as_str())),
            );
            let _ = writeln!(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&out_log)
                    .unwrap(),
                "$ {}",
                argv.join(" ")
            );
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]).current_dir(cwd);
            c
        }
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::from(out_f))
        .stderr(Stdio::from(err_f));
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(_) => return (false, false),
    };
    let mut stdin_handle = child.stdin.take();
    let mut scan_offsets: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    let mut pwd_wait_start: Option<Instant> = None;
    let start = Instant::now();
    loop {
        if BuildCtl::cancelled(ctl) {
            ctl.password_needed.store(false, Ordering::SeqCst);
            let _ = child.kill();
            let _ = child.wait();
            return (false, true);
        }
        // Watch stdout AND stderr for sudo-style password prompts
        // (yay relays child errors on either stream).
        for log in [&out_log, &err_log] {
            let key = log.to_string_lossy().into_owned();
            let scanned = scan_offsets.entry(key).or_insert(0u64);
            if let Ok(data) = std::fs::read(log) {
                if (data.len() as u64) > *scanned {
                    let fresh =
                        String::from_utf8_lossy(&data[*scanned as usize..]).to_string();
                    *scanned = data.len() as u64;
                    let answered = ctl.password.lock().map(|g| g.is_some()).unwrap_or(false);
                    if !answered && looks_like_password_prompt(&fresh) {
                        ctl.password_needed.store(true, Ordering::SeqCst);
                        if pwd_wait_start.is_none() {
                            pwd_wait_start = Some(Instant::now());
                        }
                    }
                }
            }
        }
        // Feed the password once the UI provides it (never logged).
        if ctl.password_needed.load(Ordering::SeqCst) {
            let taken = ctl.password.lock().ok().and_then(|mut g| g.take());
            if let Some(pw) = taken {
                ctl.password_needed.store(false, Ordering::SeqCst);
                pwd_wait_start = None;
                if let Some(stdin) = stdin_handle.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(stdin, "{pw}");
                    let _ = stdin.flush();
                }
            } else if let Some(t0) = pwd_wait_start {
                if t0.elapsed() > Duration::from_secs(PASSWORD_WAIT_SECS) {
                    append_log(
                        &err_log,
                        "timed out waiting for password (non-interactive? run `sudo -v` first)",
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return (false, false);
                }
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                ctl.password_needed.store(false, Ordering::SeqCst);
                return (status.success(), false);
            }
            Ok(None) => {}
            Err(_) => return (false, false),
        }
        if start.elapsed() > Duration::from_secs(timeout_secs) {
            let _ = child.kill();
            let _ = child.wait();
            return (false, false);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn append_log(path: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Pulls the image if missing. All output goes to the run's shared logs.
/// No-op on Host runner. (Setup steps run inside the single build
/// invocation, not here — separate `run --rm` calls would discard them.)
fn prepare_container(
    runner: &crate::container::Runner,
    tmp: &Path,
    ctl: &Arc<BuildCtl>,
) -> Result<(), String> {
    use crate::container as C;
    let (runtime, image) = match runner {
        C::Runner::Container { runtime, image, .. } => (runtime, image),
        _ => return Ok(()),
    };
    append_log(
        &tmp.join("stdout.log"),
        &format!("container: {runtime} {image}"),
    );
    if !C::image_present(runtime, image) {
        append_log(&tmp.join("stdout.log"), &format!("pulling {image}…"));
        let (ok, cancelled) = run_logged(
            runtime,
            &["pull", image],
            tmp,
            tmp,
            1800,
            ctl,
            &[],
            &C::Runner::Host,
        );
        if cancelled {
            return Err("cancelled by user".to_string());
        }
        if !ok {
            return Err(format!("image pull failed ({image})"));
        }
    }
    Ok(())
}

/// Real .pacman builder: generated PKGBUILD + makepkg + verification.
/// Everything lives under `tmp` (always deleted on exit, success or failure).
fn build_pacman(
    project: &str,
    name: &str,
    version: &str,
    tmp: &Path,
    ctl: &Arc<BuildCtl>,
    runner: &crate::container::Runner,
) -> Result<(PathBuf, u64), String> {
    let pkgname = sanitize_pkgname(name);
    let pkgver = sanitize_pkgver(version);
    let srcdir = tmp.join("src");
    copy_recursive(Path::new(project), &srcdir).map_err(|e| format!("copying project: {e}"))?;
    let pkgbuild = format!(
        "pkgname='{pkgname}'\npkgver='{pkgver}'\npkgrel=1\npkgdesc='BundleForge test build'\narch=('any')\npackage() {{\n  install -dm755 \"$pkgdir/usr/share/{pkgname}\"\n  cp -a \"$srcdir/.\" \"$pkgdir/usr/share/{pkgname}/\"\n}}\n"
    );
    std::fs::write(tmp.join("PKGBUILD"), pkgbuild).map_err(|e| format!("PKGBUILD: {e}"))?;
    // Container runs as root (single `run` keeps setup state) but makepkg
    // refuses root, so only the build step drops privileges (see
    // container::pacman_container_script). Host path stays a plain makepkg.
    let (ok, cancelled) = if matches!(runner, crate::container::Runner::Container { .. }) {
        let inner = crate::container::pacman_container_script(tmp);
        run_logged(
            "sh",
            &["-c", &inner],
            tmp,
            tmp,
            BUILD_TIMEOUT_SECS,
            ctl,
            &[("PACKAGER", "BundleForge Test <test@test>")],
            runner,
        )
    } else {
        run_logged(
            "makepkg",
            &["--noconfirm"],
            tmp,
            tmp,
            BUILD_TIMEOUT_SECS,
            ctl,
            &[("PACKAGER", "BundleForge Test <test@test>")],
            runner,
        )
    };
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        let out = tail_lines(&tmp.join("stdout.log"), 10);
        return Err(format!("makepkg failed.\nstderr:\n{tail}\nstdout:\n{out}"));
    }
    let mut found = None;
    for entry in std::fs::read_dir(tmp).map_err(|e| format!("reading tmp: {e}"))? {
        let entry = entry.map_err(|e| format!("reading tmp: {e}"))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz") {
            found = Some(entry.path());
            break;
        }
    }
    match found {
        Some(p) => {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            if size == 0 {
                return Err("empty artifact".to_string());
            }
            Ok((p, size))
        }
        None => Err("makepkg OK but no .pkg.tar.* artifact (see stdout.log)".to_string()),
    }
}

/// Formats with a real builder behind them. Single source of truth
/// (build_format and package_format both gate on this).
pub fn has_builder(format: &str) -> bool {
    matches!(format, "pacman" | "rpm" | "deb" | "appimage" | "exe" | "xbps" | "msi" | "flatpak" | "snap" | "zip")
}

/// Snap name rules: lowercase, digits and interior dashes only.
fn snap_name(name: &str) -> String {
    let mut out: String = sanitize_pkgname(name)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        return "app".to_string();
    }
    if out.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        format!("snap-{out}")
    } else {
        out
    }
}

/// snapcraft.yaml for test packages: devmode + devel grade (local
/// `--dangerous` installs, no Store), dump part over our payload copy.
/// dump lays the source CONTENTS at prime root, so command is the
/// payload-relative entry path unchanged.
fn snapcraft_yaml(pkg: &str, version: &str, entry: &str) -> String {
    format!(
        "name: {pkg}\nbase: core24\nversion: '{version}'\nsummary: BundleForge test build\ndescription: BundleForge test build\ngrade: devel\nconfinement: devmode\napps:\n  {pkg}:\n    command: {entry}\nparts:\n  payload:\n    plugin: dump\n    source: ./payload\n"
    )
}

/// meta/snap.yaml for the container path (`snap pack`, official packer
/// from the snapd deb — fully local, no daemon). Mirrors snapcraft_yaml
/// fields: dump lays payload CONTENTS at prime root, so command is the
/// payload-relative entry path unchanged.
fn snap_yaml(pkg: &str, version: &str, entry: &str) -> String {
    format!(
        "name: {pkg}\nbase: core24\nversion: '{version}'\nsummary: BundleForge test build\ndescription: BundleForge test build\ngrade: devel\nconfinement: devmode\napps:\n  {pkg}:\n    command: {entry}\n"
    )
}
/// Newest `*.snap` in a dir (rebuilds accumulate). Shared by both snap
/// paths: native snapcraft drops it in the build dir, and `snap pack`
/// too (its optional 2nd arg is a TARGET DIR, not a filename — passing
/// one just nests the real snap inside a same-named directory).
fn newest_snap(dir: &Path) -> Option<PathBuf> {
    let mut cands: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for e in std::fs::read_dir(dir).ok()? {
        let p = e.ok()?.path();
        if p.extension().map(|x| x == "snap").unwrap_or(false) {
            let mt = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            cands.push((mt, p));
        }
    }
    cands.sort_by_key(|(t, _)| *t);
    cands.pop().map(|(_, p)| p)
}

/// One-line `ls -la` of a dir for error logs (which artifact did the
/// tool actually drop, and where).
fn list_dir(dir: &Path) -> String {
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            let mut names: Vec<String> = Vec::new();
            for e in rd.flatten() {
                let p = e.path();
                let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                let kind = if p.is_dir() { "/" } else { "" };
                names.push(format!(
                    "{}{} ({sz}B)",
                    p.file_name().unwrap_or_default().to_string_lossy(),
                    kind
                ));
            }
            names.sort();
            names.join("\n")
        }
        Err(e) => format!("<unreadable: {e}>"),
    }
}

/// snapcraft binary: store paths are invisible to GUI PATH until relogin,
/// so resolve absolutely when present there.
fn snapcraft_bin() -> String {
    for p in ["/snap/bin/snapcraft", "/var/lib/snapd/snap/bin/snapcraft"] {
        if std::path::Path::new(p).is_file() {
            return p.to_string();
        }
    }
    "snapcraft".to_string()
}

/// Real .snap builder. Host (native Ubuntu): generated snapcraft.yaml +
/// `snapcraft --destructive-mode`. Container (Isolated, automatic ubuntu
/// image): no snapcraft deb exists (transitional → snap store, needs a
/// daemon), so pack with the official `snap pack` from the snapd deb
/// over a generated meta/snap.yaml. Remote: offload to Ubuntu builder.
fn build_snap(
    project: &str,
    name: &str,
    version: &str,
    tmp: &Path,
    ctl: &Arc<BuildCtl>,
    runner: &crate::container::Runner,
) -> Result<(PathBuf, u64), String> {
    const SNAP_BUILD_TIMEOUT_SECS: u64 = 1800;
    let pkgname = snap_name(name);
    let pkgver = sanitize_pkgver(version);
    let build = tmp.join("build");
    let payload = build.join("payload");
    copy_recursive(Path::new(project), &payload)
        .map_err(|e| format!("copying project: {e}"))?;
    let entry = flatpak_entry(&payload, &payload)
        .ok_or_else(|| "no executable entry point found".to_string())?;
    // Container path: prime dir + meta/snap.yaml + `snap pack`
    // (same layout snapcraft's dump part would produce).
    if matches!(runner, crate::container::Runner::Container { .. }) {
        let meta = payload.join("meta");
        std::fs::create_dir_all(&meta).map_err(|e| format!("meta dir: {e}"))?;
        std::fs::write(
            meta.join("snap.yaml"),
            snap_yaml(&pkgname, &pkgver, &entry),
        )
        .map_err(|e| format!("snap.yaml: {e}"))?;
        // `snap pack payload` with no target dir: drops
        // <name>_<ver>_<arch>.snap in cwd (build). NOTE: never pass a
        // filename as 2nd arg — snap reads it as a target DIRECTORY and
        // nests the real snap inside (then the copy step fails: "source
        // path is neither a regular file").
        let (ok, cancelled) = run_logged(
            "snap",
            &["pack", "payload"],
            &build,
            tmp,
            SNAP_BUILD_TIMEOUT_SECS,
            ctl,
            &[("DEBIAN_FRONTEND", "noninteractive")],
            runner,
        );
        if cancelled {
            return Err("cancelled by user".to_string());
        }
        if !ok {
            let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
            let out = tail_lines(&tmp.join("stdout.log"), 10);
            return Err(format!("snap pack failed.\nstderr:\n{tail}\nstdout:\n{out}"));
        }
        let out = newest_snap(&build).ok_or_else(|| {
            format!(
                "snap pack OK but no .snap artifact.\nbuild dir:\n{}",
                list_dir(&build)
            )
        })?;
        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        if size == 0 {
            return Err("empty .snap artifact".to_string());
        }
        return Ok((out, size));
    }
    let snapdir = build.join("snap");
    std::fs::create_dir_all(&snapdir).map_err(|e| format!("snap dir: {e}"))?;
    std::fs::write(
        snapdir.join("snapcraft.yaml"),
        snapcraft_yaml(&pkgname, &sanitize_pkgver(version), &entry),
    )
    .map_err(|e| format!("snapcraft.yaml: {e}"))?;
    // Native Ubuntu host only (the gate guarantees it): destructive build.
    let args: &[&str] = &["--destructive-mode"];
    let (ok, cancelled) = run_logged(
        &snapcraft_bin(),
        args,
        &build,
        tmp,
        SNAP_BUILD_TIMEOUT_SECS,
        ctl,
        &[],
        runner,
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        return Err(format!("snapcraft failed.\nstderr:\n{tail}"));
    }
    // Newest .snap wins (rebuilds accumulate).
    let out = newest_snap(&build)
        .ok_or_else(|| "snapcraft OK but no .snap artifact".to_string())?;
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Err("empty .snap artifact".to_string());
    }
    Ok((out, size))
}

/// Builder .deb real: árbol stage + DEBIAN/control + dpkg-deb --build.
fn build_deb(
    project: &str,
    name: &str,
    version: &str,
    tmp: &Path,
    ctl: &Arc<BuildCtl>,
    runner: &crate::container::Runner,
) -> Result<(PathBuf, u64), String> {
    let pkgname = sanitize_pkgname(name);
    let pkgver = sanitize_pkgver(version);
    let stage = tmp.join("stage");
    let payload = stage.join("usr").join("share").join(&pkgname);
    copy_recursive(Path::new(project), &payload)
        .map_err(|e| format!("copying project: {e}"))?;
    let debdir = stage.join("DEBIAN");
    std::fs::create_dir_all(&debdir).map_err(|e| format!("DEBIAN dir: {e}"))?;
    let control = format!(
        "Package: {pkgname}\nVersion: {pkgver}\nArchitecture: all\nMaintainer: BundleForge Test <test@test>\nDescription: BundleForge test build\n"
    );
    std::fs::write(debdir.join("control"), control).map_err(|e| format!("control: {e}"))?;
    let out = tmp.join(format!("{pkgname}_{pkgver}_all.deb"));
    let (ok, cancelled) = run_logged(
        "dpkg-deb",
        &[
            "--build",
            stage.to_str().unwrap_or("."),
            out.to_str().unwrap_or("out.deb"),
        ],
        tmp,
        tmp,
        BUILD_TIMEOUT_SECS,
        ctl,
        &[],
        runner,
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        return Err(format!("dpkg-deb failed.\nstderr:\n{tail}"));
    }
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    if size == 0 || !out.exists() {
        return Err("dpkg-deb OK but no .deb artifact".to_string());
    }
    Ok((out, size))
}

/// Full WiX source for the test MSI. MediaTemplate embeds the cab so the
/// .msi is a single file (external cabs break standalone installs).
fn msi_doc(pkgname: &str, wixver: &str, upgrade: &str, inner: &str, refs: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Wix xmlns=\"http://schemas.microsoft.com/wix/2006/wi\">\n<Product Id=\"*\" Name=\"{pkgname}\" Language=\"1033\" Version=\"{wixver}\" Manufacturer=\"BundleForge\" UpgradeCode=\"{upgrade}\">\n<Package InstallerVersion=\"200\" Compressed=\"yes\" InstallScope=\"perMachine\" />\n<MediaTemplate EmbedCab=\"yes\" />\n<Directory Id=\"TARGETDIR\" Name=\"SourceDir\">\n<Directory Id=\"ProgramFilesFolder\">\n<Directory Id=\"INSTALLDIR\" Name=\"{pkgname}\">\n{inner}</Directory>\n</Directory>\n</Directory>\n<Feature Id=\"Main\" Title=\"Main\" Level=\"1\">\n{refs}</Feature>\n</Product>\n</Wix>\n",
        pkgname = xml_escape(pkgname),
    )
}

/// Real .msi builder: staged tree → generated WiX XML → wixl.
/// 32-bit package on purpose (no Platform/Win64 attrs): installs anywhere,
/// bitness is irrelevant for data payloads.
fn build_msi(
    project: &str,
    name: &str,
    version: &str,
    tmp: &Path,
    ctl: &Arc<BuildCtl>,
    runner: &crate::container::Runner,
) -> Result<(PathBuf, u64), String> {
    let pkgname = sanitize_pkgname(name);
    let wixver = wix_version(version);
    let stage = tmp.join("stage");
    let payload = stage.join("usr").join("share").join(&pkgname);
    copy_recursive(Path::new(project), &payload)
        .map_err(|e| format!("copying project: {e}"))?;
    let seed = nanos() ^ (std::process::id() as u128);
    let mut inner = String::new();
    let mut files: Vec<(String, String, String, PathBuf)> = Vec::new();
    let mut idc = 0u32;
    wxs_walk(&payload, &mut inner, &mut files, &mut idc, seed)
        .map_err(|e| format!("scanning stage: {e}"))?;
    if files.is_empty() {
        return Err("nothing to package (empty project?)".to_string());
    }
    let refs: String = files
        .iter()
        .map(|(_, cid, _, _)| format!("<ComponentRef Id=\"{cid}\" />"))
        .collect();
    let upgrade = guidish(seed ^ 0x9e3779b97f4a7c15);
    let doc = msi_doc(&pkgname, &wixver, &upgrade, &inner, &refs);
    std::fs::write(tmp.join("setup.wxs"), doc).map_err(|e| format!("wxs: {e}"))?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => "x86",
    };
    let out = tmp.join(format!("{pkgname}-{}.msi", sanitize_pkgver(version)));
    let (ok, cancelled) = run_logged(
        "wixl",
        &["-a", arch, "setup.wxs", "-o", out.to_str().unwrap_or("out.msi")],
        tmp,
        tmp,
        BUILD_TIMEOUT_SECS,
        ctl,
        &[],
        runner,
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        return Err(format!("wixl failed.\nstderr:\n{tail}"));
    }
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    if size == 0 || !out.exists() {
        return Err("wixl OK but no .msi artifact".to_string());
    }
    Ok((out, size))
}

pub fn human_size(b: u64) -> String {
    human_bytes(b)
}

/// Reverse-DNS app id for flatpak (dots required, segments start with a
/// letter): io.bundleforge.<sanitized>.
fn flatpak_app_id(name: &str) -> String {
    let mut s: String = sanitize_pkgname(name)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if s.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(true) {
        s = format!("app{s}");
    }
    format!("io.bundleforge.{s}")
}

/// First executable regular file under `dir` (sorted walk), relative to
/// `root`. Flatpak `command` needs a real entry point; data-only trees
/// fail honestly here instead of producing a dead bundle.
fn flatpak_entry(dir: &Path, root: &Path) -> Option<String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let ft = e.file_type().ok()?;
        if ft.is_dir() {
            if let Some(found) = flatpak_entry(&p, root) {
                return Some(found);
            }
        } else if ft.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if std::fs::metadata(&p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false) {
                    return p.strip_prefix(root).ok().map(|r| r.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    None
}

/// Bundle filename follows the Flathub convention: the app-id, no version.
/// (Version segments would break ref rules — segments can't start with a
/// digit — and copy_unique already handles collisions locally.)
fn flatpak_bundle_name(app_id: &str) -> String {
    format!("{app_id}.flatpak")
}

/// Minimal flatpak manifest (verified semantics: dir sources are copied
/// into the source dir, `simple` build-commands run there).
fn flatpak_manifest(app_id: &str, command: &str, pkg: &str, stage: &Path) -> String {
    use crate::jstr;
    format!(
        "{{\n  \"app-id\": {},\n  \"runtime\": \"org.freedesktop.Platform\",\n  \"runtime-version\": \"24.08\",\n  \"sdk\": \"org.freedesktop.Sdk\",\n  \"command\": {},\n  \"finish-args\": [\"--share=ipc\", \"--socket=fallback-x11\", \"--device=dri\"],\n  \"modules\": [\n    {{\n      \"name\": {},\n      \"buildsystem\": \"simple\",\n      \"build-commands\": [\"cp -a . /app/\"],\n      \"sources\": [{{\"type\": \"dir\", \"path\": {}}}]\n    }}\n  ]\n}}\n",
        jstr(app_id),
        jstr(&format!("/app/{command}")),
        jstr(pkg),
        jstr(&stage.to_string_lossy()),
    )
}

/// Real .flatpak builder: staged tree + generated manifest +
/// flatpak-builder + build-bundle. SDK/runtime must pre-exist (gate);
/// nothing heavy is ever fetched silently.
fn build_flatpak(
    project: &str,
    name: &str,
    _version: &str,
    tmp: &Path,
    ctl: &Arc<BuildCtl>,
    runner: &crate::container::Runner,
) -> Result<(PathBuf, u64), String> {
    let pkgname = sanitize_pkgname(name);
    let app_id = flatpak_app_id(name);
    let stage = tmp.join("stage");
    let payload = stage.join("usr").join("share").join(&pkgname);
    copy_recursive(Path::new(project), &payload)
        .map_err(|e| format!("copying project: {e}"))?;
    let entry = flatpak_entry(&stage, &stage)
        .ok_or_else(|| "no executable entry point found".to_string())?;
    let manifest = flatpak_manifest(&app_id, &entry, &pkgname, &stage);
    std::fs::write(tmp.join("app.json"), manifest).map_err(|e| format!("manifest: {e}"))?;
    let build = |cmd: &str, args: &[&str]| {
        run_logged(cmd, args, tmp, tmp, BUILD_TIMEOUT_SECS, ctl, &[], runner)
    };
    let (ok, cancelled) = build(
        "flatpak-builder",
        &["--force-clean", "--repo=repo", "build", "app.json"],
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        return Err(format!("flatpak-builder failed.\nstderr:\n{tail}"));
    }
    let out_name = flatpak_bundle_name(&app_id);
    let out = tmp.join(&out_name);
    // NOTE: arg order is LOCATION FILENAME NAME (NAME last); swapping
    // them makes flatpak read the filename as the app id.
    let (ok, cancelled) = build(
        "flatpak",
        &["build-bundle", "repo", &out_name, &app_id],
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        return Err(format!("flatpak build-bundle failed.\nstderr:\n{tail}"));
    }
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    if size == 0 || !out.exists() {
        return Err("flatpak OK but no .flatpak artifact".to_string());
    }
    Ok((out, size))
}

/// WiX Product Version needs numeric X.Y.Z: "1.2.0-beta" → "1.2.0".
fn wix_version(s: &str) -> String {
    let mut parts: Vec<String> = s
        .split('.')
        .map(|p| {
            let num: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
            if num.is_empty() {
                "0".to_string()
            } else {
                num
            }
        })
        .collect();
    while parts.len() < 3 {
        parts.push("0".to_string());
    }
    parts.truncate(3);
    parts.join(".")
}

/// Test-only GUID-ish (8-4-4-4-12 hex). Unique per build, so test MSIs
/// never clash on Product/Upgrade identity. Not for production signing.
fn guidish(seed: u128) -> String {
    let h = format!("{seed:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Walks the staged tree emitting nested WiX Directory XML; collects
/// (file_id, component_id, guid, absolute_source) for Feature refs.
fn wxs_walk(
    dir: &Path,
    out: &mut String,
    files: &mut Vec<(String, String, String, PathBuf)>,
    idc: &mut u32,
    seed: u128,
) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let fname = e.file_name().to_string_lossy().into_owned();
        if e.file_type()?.is_dir() {
            let id = format!("d{idc}");
            *idc += 1;
            out.push_str(&format!(
                "<Directory Id=\"{id}\" Name=\"{}\">",
                xml_escape(&fname)
            ));
            wxs_walk(&p, out, files, idc, seed)?;
            out.push_str("</Directory>");
        } else {
            let n = *idc;
            *idc += 1;
            let fid = format!("f{n}");
            let cid = format!("c{n}");
            let guid = guidish(seed ^ (n as u128));
            out.push_str(&format!(
                "<Component Id=\"{cid}\" Guid=\"{guid}\"><File Id=\"{fid}\" Source=\"{}\" /></Component>",
                xml_escape(&p.to_string_lossy())
            ));
            files.push((fid, cid, guid, p));
        }
    }
    Ok(())
}

/// Maps host arch to xbps arch names. Test payloads may hold binaries,
/// so prefer the real arch; unknown hosts fall back to noarch.
fn xbps_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => "noarch",
    }
}

/// Real .xbps builder: staged tree + xbps-create + verification.
/// Flags verified against xbps 0.60.7:
/// `xbps-create -A arch -n pkgver -s desc -m maintainer destdir`
/// writes `<pkgver>.<arch>.xbps` into the cwd.
fn build_xbps(
    project: &str,
    name: &str,
    version: &str,
    tmp: &Path,
    ctl: &Arc<BuildCtl>,
    runner: &crate::container::Runner,
) -> Result<(PathBuf, u64), String> {
    let pkgname = sanitize_pkgname(name);
    let pkgver = format!("{}_1", sanitize_pkgver(version));
    let arch = xbps_arch();
    let stage = tmp.join("stage");
    let payload = stage.join("usr").join("share").join(&pkgname);
    copy_recursive(Path::new(project), &payload)
        .map_err(|e| format!("copying project: {e}"))?;
    let pkgver_full = format!("{pkgname}-{pkgver}");
    let (ok, cancelled) = run_logged(
        "xbps-create",
        &[
            "-A",
            arch,
            "-n",
            &pkgver_full,
            "-s",
            "BundleForge test build",
            "-m",
            "BundleForge",
            stage.to_str().unwrap_or("."),
        ],
        tmp,
        tmp,
        BUILD_TIMEOUT_SECS,
        ctl,
        &[],
        runner,
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        return Err(format!("xbps-create failed.\nstderr:\n{tail}"));
    }
    let out = tmp.join(format!("{pkgver_full}.{arch}.xbps"));
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    if size == 0 || !out.exists() {
        return Err("xbps-create OK but no .xbps artifact".to_string());
    }
    Ok((out, size))
}

/// Picks Host vs Container for a format.
/// Container when forced via env, or automatically when the native tool is
/// missing but a runtime + image exist (Linux only).
fn resolve_runner(format: &str, gate_ok: bool) -> crate::container::Runner {
    use crate::container as C;
    let want_container = C::force_container() || !gate_ok;
    if want_container && crate::detectors::os_name() == "linux" {
        if let (Some(rt), Some(spec)) = (C::container_runtime(), C::container_spec(format)) {
            return C::Runner::Container {
                runtime: rt,
                image: spec.image,
                setup: spec.setup,
            };
        }
    }
    C::Runner::Host
}

/// True when `dir` holds an ELF binary (recursive, magic bytes only).
/// Pure enough to unit-test; used to pick an honest BuildArch.
fn has_elf(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if has_elf(&p) {
                return true;
            }
        } else if p.is_file() {
            if let Ok(f) = std::fs::File::open(&p) {
                let mut magic = [0u8; 4];
                use std::io::Read as _;
                if f.take(4).read_exact(&mut magic).is_ok() && magic == [0x7f, b'E', b'L', b'F'] {
                    return true;
                }
            }
        }
    }
    false
}

/// RPM arch for the payload: `noarch` for data/scripts, native arch when
/// an ELF binary rides along (Fedora refuses arch-dependent binaries in
/// noarch packages — rightly so, no override hacks).
fn rpm_build_arch(payload: &Path) -> &'static str {
    if !has_elf(payload) {
        return "noarch";
    }
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "x86" => "i686",
        _ => "noarch", // unknown: let rpmbuild judge honestly
    }
}

/// Builder .rpm real: .spec generado + rpmbuild -bb + verificación.
/// Todo contenido con --define _topdir dentro del temp (se borra siempre).
fn build_rpm(
    project: &str,
    name: &str,
    version: &str,
    tmp: &Path,
    ctl: &Arc<BuildCtl>,
    runner: &crate::container::Runner,
) -> Result<(PathBuf, u64), String> {
    let pkgname = sanitize_pkgname(name);
    let pkgver = sanitize_pkgver(version);
    let srcdir = tmp.join("src");
    copy_recursive(Path::new(project), &srcdir)
        .map_err(|e| format!("copying project: {e}"))?;
    let rpmdir = tmp.join("rpm");
    for d in ["BUILD", "RPMS", "SOURCES", "SPECS", "SRPMS"] {
        std::fs::create_dir_all(rpmdir.join(d)).map_err(|e| format!("rpm tree: {e}"))?;
    }
    let spec = format!(
        "Name:           {pkgname}\nVersion:        {pkgver}\nRelease:        1\nSummary:        BundleForge test build\nLicense:        Apache-2.0\nBuildArch:      {arch}\n\n%description\nBundleForge test build.\n\n%install\nmkdir -p \"%{{buildroot}}/usr/share/{pkgname}\"\ncp -a \"{src}/.\" \"%{{buildroot}}/usr/share/{pkgname}/\"\n\n%files\n/usr/share/{pkgname}/\n",
        pkgname = pkgname,
        pkgver = pkgver,
        arch = rpm_build_arch(&srcdir),
        src = srcdir.to_string_lossy()
    );
    let spec_path = tmp.join("pkg.spec");
    std::fs::write(&spec_path, spec).map_err(|e| format!("spec: {e}"))?;
    let topdir = format!("_topdir {}", rpmdir.to_string_lossy());
    let (ok, cancelled) = run_logged(
        "rpmbuild",
        &["-bb", "--define", &topdir, &spec_path.to_string_lossy()],
        tmp,
        tmp,
        BUILD_TIMEOUT_SECS,
        ctl,
        &[],
        runner,
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        let out = tail_lines(&tmp.join("stdout.log"), 10);
        return Err(format!("rpmbuild failed.\nstderr:\n{tail}\nstdout:\n{out}"));
    }
    let found = find_artifact(&rpmdir.join("RPMS"), ".rpm").ok_or_else(|| {
        "rpmbuild OK but no .rpm artifact (see stdout.log)".to_string()
    })?;
    let size = std::fs::metadata(&found).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Err("empty artifact".to_string());
    }
    Ok((found, size))
}

/// Real AppRun: exec the detected entry point with the user's args.
/// Pure (tested): no hardcoded paths — both segments come from the
/// caller (payload-relative entry via `flatpak_entry`, same rule as
/// the snap/flatpak builders).
fn apprun_script(pkgdir: &str, entry: &str) -> String {
    format!("#!/bin/sh\nexec \"$APPDIR/{pkgdir}/{entry}\" \"$@\"\n")
}

/// Builder .AppImage real: AppDir + desktop + icono + linuxdeploy.
/// AppRun launches the detected entry (first-executable rule, shared
/// with snap/flatpak) — a real launcher, not a listing stub.
/// Requiere linuxdeploy (Help→Install) + FUSE + red (descarga appimagetool).
fn build_appimage(
    project: &str,
    name: &str,
    version: &str,
    tmp: &Path,
    ctl: &Arc<BuildCtl>,
    runner: &crate::container::Runner,
) -> Result<(PathBuf, u64), String> {
    const ICON: &[u8] = include_bytes!("../assets/icon-32.png");
    let pkgname = sanitize_pkgname(name);
    let appdir = tmp.join("AppDir");
    let payload = appdir.join("usr").join("share").join(&pkgname);
    copy_recursive(Path::new(project), &payload)
        .map_err(|e| format!("copying project: {e}"))?;
    std::fs::write(tmp.join("icon.png"), ICON).map_err(|e| format!("icon: {e}"))?;
    let desktop = format!(
        "[Desktop Entry]\nName={pkgname}\nExec=AppRun\nIcon=icon\nType=Application\nCategories=Utility;\n"
    );
    std::fs::write(tmp.join("app.desktop"), &desktop).map_err(|e| format!("desktop: {e}"))?;
    let entry = flatpak_entry(&payload, &payload)
        .ok_or_else(|| "no executable entry point found".to_string())?;
    let apprun_path = appdir.join("AppRun");
    std::fs::write(
        &apprun_path,
        apprun_script(&format!("usr/share/{pkgname}"), &entry),
    )
    .map_err(|e| format!("apprun: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&apprun_path)
            .map_err(|e| format!("stat: {e}"))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&apprun_path, perms).map_err(|e| format!("chmod: {e}"))?;
    }
    let arch = std::env::consts::ARCH.to_string();
    let pkgver = sanitize_pkgver(version);
    let (ok, cancelled) = run_logged(
        "linuxdeploy",
        &[
            "--appdir",
            "AppDir",
            "-d",
            "app.desktop",
            "-i",
            "icon.png",
            "--output",
            "appimage",
        ],
        tmp,
        tmp,
        BUILD_TIMEOUT_SECS,
        ctl,
        &[("ARCH", arch.as_str()), ("VERSION", pkgver.as_str())],
        runner,
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        let out = tail_lines(&tmp.join("stdout.log"), LOG_TAIL_LINES);
        return Err(format!("linuxdeploy failed.\nstderr:\n{tail}\nstdout:\n{out}"));
    }
    let found = find_artifact(tmp, ".AppImage").ok_or_else(|| {
        "linuxdeploy OK but no .AppImage artifact (see stdout.log)".to_string()
    })?;
    let size = std::fs::metadata(&found).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Err("empty artifact".to_string());
    }
    Ok((found, size))
}

/// Builder .zip real (portable): payload copy + `zip -qr` with entries
/// at the archive root. One `sh -c` (cd + zip) so host and container
/// share the path — the container only bind-mounts the cwd, so the
/// output name stays relative to tmp. THE single compression exception
/// (portables); rar/7z/tar stay OUT per PLAN.
fn build_zip(
    project: &str,
    name: &str,
    version: &str,
    tmp: &Path,
    ctl: &Arc<BuildCtl>,
    runner: &crate::container::Runner,
) -> Result<(PathBuf, u64), String> {
    let pkgname = sanitize_pkgname(name);
    let pkgver = sanitize_pkgver(version);
    let payload = tmp.join("payload");
    copy_recursive(Path::new(project), &payload)
        .map_err(|e| format!("copying project: {e}"))?;
    let outfile = format!("{pkgname}_{pkgver}.zip");
    let script = format!("cd payload && zip -qr ../{outfile} .");
    let (ok, cancelled) = run_logged(
        "sh",
        &["-c", &script],
        tmp,
        tmp,
        BUILD_TIMEOUT_SECS,
        ctl,
        &[],
        runner,
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        let out = tail_lines(&tmp.join("stdout.log"), 10);
        return Err(format!("zip failed.\nstderr:\n{tail}\nstdout:\n{out}"));
    }
    let found = find_artifact(tmp, ".zip")
        .ok_or_else(|| "zip OK but no .zip artifact (see stdout.log)".to_string())?;
    let size = std::fs::metadata(&found).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Err("empty artifact".to_string());
    }
    Ok((found, size))
}

/// Builder .exe real: script NSIS generado + makensis.
/// makensis corre nativo en Linux; el instalador resultante solo corre en
/// Windows (test builds: instala payload + desinstalador, sin entry point).
fn build_exe(
    project: &str,
    name: &str,
    version: &str,
    tmp: &Path,
    ctl: &Arc<BuildCtl>,
    runner: &crate::container::Runner,
) -> Result<(PathBuf, u64), String> {
    let pkgname = sanitize_pkgname(name);
    let payload = tmp.join("payload");
    copy_recursive(Path::new(project), &payload)
        .map_err(|e| format!("copying project: {e}"))?;
    // VIProductVersion exige X.X.X.X numérico.
    let mut nums: Vec<String> = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .take(4)
        .map(|s| s.to_string())
        .collect();
    while nums.len() < 4 {
        nums.push("0".to_string());
    }
    let viproduct = nums.join(".");
    let pkgver = sanitize_pkgver(version);
    let out_exe = tmp.join(format!("{pkgname}-{pkgver}-setup.exe"));
    let nsi = format!(
        "Unicode True\n\
         Name \"{pkgname}\"\n\
         OutFile \"{out_exe}\"\n\
         InstallDir \"$PROGRAMFILES\\{pkgname}\"\n\
         RequestExecutionLevel admin\n\
         BrandingText \"BundleForge test build\"\n\
         VIProductVersion \"{viproduct}\"\n\
         Section \"Main\"\n\
           SetOutPath \"$INSTDIR\"\n\
           File /r \"{payload}\\*\"\n\
           WriteUninstaller \"$INSTDIR\\uninstall.exe\"\n\
           CreateDirectory \"$SMPROGRAMS\\{pkgname}\"\n\
           CreateShortcut \"$SMPROGRAMS\\{pkgname}\\Uninstall.lnk\" \"$INSTDIR\\uninstall.exe\"\n\
         SectionEnd\n\
         Section \"Uninstall\"\n\
           Delete \"$INSTDIR\\uninstall.exe\"\n\
           RMDir /r \"$INSTDIR\"\n\
           Delete \"$SMPROGRAMS\\{pkgname}\\Uninstall.lnk\"\n\
           RMDir \"$SMPROGRAMS\\{pkgname}\"\n\
         SectionEnd\n",
        pkgname = pkgname,
        out_exe = out_exe.to_string_lossy(),
        payload = payload.to_string_lossy(),
        viproduct = viproduct,
    );
    std::fs::write(tmp.join("setup.nsi"), nsi).map_err(|e| format!("nsi: {e}"))?;
    let (ok, cancelled) = run_logged(
        "makensis",
        &["setup.nsi"],
        tmp,
        tmp,
        BUILD_TIMEOUT_SECS,
        ctl,
        &[],
        runner,
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        let out = tail_lines(&tmp.join("stdout.log"), LOG_TAIL_LINES);
        return Err(format!("makensis failed.\nstderr:\n{tail}\nstdout:\n{out}"));
    }
    let size = std::fs::metadata(&out_exe).map(|m| m.len()).unwrap_or(0);
    if size == 0 || !out_exe.exists() {
        return Err("makensis OK but no .exe artifact".to_string());
    }
    Ok((out_exe, size))
}
fn find_artifact(dir: &Path, ext: &str) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(d).ok()?;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.to_string_lossy().ends_with(ext) {
                return Some(p);
            }
        }
    }
    None
}

/// Keep a built artifact into <out_base>/packages/ (auto-rename).
/// Shared by package_format callers.
fn keep_artifact(
    artifact: &Path,
    size: u64,
    format: &str,
    out_base: &Path,
    via: &str,
    extra: &str,
) -> crate::TestResult {
    use crate::TestResult;
    let dest_dir = out_base.join("packages");
    match std::fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("output dir: {e}"))
        .and_then(|()| copy_unique(artifact, &dest_dir))
    {
        Ok(dest) => {
            let kept_size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(size);
            TestResult {
                format: format.to_string(),
                state: "SAFE".to_string(),
                reason: format!("saved · {}{}{}", human_size(kept_size), via, extra),
                test_type: "package".to_string(),
                success: Some(true),
                error_log: None,
                command_output: None,
                artifact_path: Some(dest.to_string_lossy().to_string()),
            }
        }
        Err(e) => TestResult {
            format: format.to_string(),
            state: "MEH".to_string(),
            reason: format!("built but not saved: {e}"),
            test_type: "package".to_string(),
            success: Some(false),
            error_log: None,
            command_output: None,
            artifact_path: None,
        },
    }
}

/// Orchestrates a test build: quick gate + real builder + verification +
/// temp cleanup ALWAYS (success, failure or cancelled).
pub fn build_format(
    project: &str,
    name: &str,
    version: &str,
    format: &str,
    ctl: &Arc<BuildCtl>,
) -> crate::TestResult {
    use crate::TestResult;
    let mut gate = crate::tester::check_format(format);
    gate.test_type = "normal".to_string();
    let runner = resolve_runner(format, gate.state == "SAFE");
    // Host without a passing gate: return the honest gate result.
    // Container: the gate only checked the host; the container has its own tools.
    if matches!(runner, crate::container::Runner::Host) && gate.state != "SAFE" {
        return gate;
    }
    if !has_builder(format) {
        return TestResult {
            format: format.to_string(),
            state: "MEH".to_string(),
            reason: "no builder for this format".to_string(),
            test_type: "normal".to_string(),
            success: None,
            error_log: None,
            command_output: None,
            artifact_path: None,
        };
    }
    let tmp = temp_dir();
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        return TestResult {
            format: format.to_string(),
            state: "MEH".to_string(),
            reason: format!("no temp dir: {e}"),
            test_type: "normal".to_string(),
            success: Some(false),
            error_log: None,
            command_output: None,
            artifact_path: None,
        };
    }
    if let Err(e) = prepare_container(&runner, &tmp, ctl) {
        let res = TestResult {
            format: format.to_string(),
            state: "MEH".to_string(),
            reason: e,
            test_type: "normal".to_string(),
            success: Some(false),
            error_log: None,
            command_output: None,
            artifact_path: None,
        };
        cleanup(&tmp);
        return res;
    }
    let via = match &runner {
        crate::container::Runner::Container { .. } => " (container)",
        _ => "",
    };
    let build_out = match format {
        "rpm" => build_rpm(project, name, version, &tmp, ctl, &runner),
        "deb" => build_deb(project, name, version, &tmp, ctl, &runner),
        "appimage" => build_appimage(project, name, version, &tmp, ctl, &runner),
        "exe" => build_exe(project, name, version, &tmp, ctl, &runner),
        "xbps" => build_xbps(project, name, version, &tmp, ctl, &runner),
        "msi" => build_msi(project, name, version, &tmp, ctl, &runner),
        "flatpak" => build_flatpak(project, name, version, &tmp, ctl, &runner),
        "snap" => build_snap(project, name, version, &tmp, ctl, &runner),
        "zip" => build_zip(project, name, version, &tmp, ctl, &runner),
        _ => build_pacman(project, name, version, &tmp, ctl, &runner),
    };
    let res = match build_out {
        Ok((_p, size)) => TestResult {
            format: format.to_string(),
            state: "SAFE".to_string(),
            reason: format!("OK · {}{}", human_size(size), via),
            test_type: "normal".to_string(),
            success: Some(true),
            error_log: None,
            command_output: None,
            artifact_path: None,
        },
        Err(e) => {
            let cancelled = e == "cancelled by user";
            TestResult {
                format: format.to_string(),
                state: "MEH".to_string(),
                reason: if cancelled {
                    "cancelled by user".to_string()
                } else {
                    "build failed (see details)".to_string()
                },
                test_type: "normal".to_string(),
                success: Some(false),
                error_log: Some(e),
                command_output: None,
                artifact_path: None,
            }
        }
    };
    cleanup(&tmp);
    res
}

/// Real packaging: like build_format, but the artifact is KEPT into
/// `<out_base>/packages/` (auto-renamed on collision) instead of deleted.
/// The temp dir is still always cleaned up.
pub fn package_format(
    project: &str,
    name: &str,
    version: &str,
    format: &str,
    out_base: &Path,
    ctl: &Arc<BuildCtl>,
) -> crate::TestResult {
    use crate::TestResult;
    let mut gate = crate::tester::check_format(format);
    gate.test_type = "package".to_string();
    let runner = resolve_runner(format, gate.state == "SAFE");
    if matches!(runner, crate::container::Runner::Host) && gate.state != "SAFE" {
        return gate;
    }
    if !has_builder(format) {
        return TestResult {
            format: format.to_string(),
            state: "MEH".to_string(),
            reason: "no builder for this format".to_string(),
            test_type: "package".to_string(),
            success: None,
            error_log: None,
            command_output: None,
            artifact_path: None,
        };
    }
    let tmp = temp_dir();
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
    let via = match &runner {
        crate::container::Runner::Container { .. } => " (container)",
        _ => "",
    };
    if let Err(e) = prepare_container(&runner, &tmp, ctl) {
        let res = TestResult {
            format: format.to_string(),
            state: "MEH".to_string(),
            reason: e,
            test_type: "package".to_string(),
            success: Some(false),
            error_log: None,
            command_output: None,
            artifact_path: None,
        };
        cleanup(&tmp);
        return res;
    }
    let build_out = match format {
        "rpm" => build_rpm(project, name, version, &tmp, ctl, &runner),
        "deb" => build_deb(project, name, version, &tmp, ctl, &runner),
        "appimage" => build_appimage(project, name, version, &tmp, ctl, &runner),
        "exe" => build_exe(project, name, version, &tmp, ctl, &runner),
        "xbps" => build_xbps(project, name, version, &tmp, ctl, &runner),
        "msi" => build_msi(project, name, version, &tmp, ctl, &runner),
        "flatpak" => build_flatpak(project, name, version, &tmp, ctl, &runner),
        "snap" => build_snap(project, name, version, &tmp, ctl, &runner),
        "zip" => build_zip(project, name, version, &tmp, ctl, &runner),
        _ => build_pacman(project, name, version, &tmp, ctl, &runner),
    };
    let res = match build_out {
        Ok((artifact, size)) => keep_artifact(&artifact, size, format, out_base, via, ""),
        Err(e) => {
            let cancelled = e == "cancelled by user";
            TestResult {
                format: format.to_string(),
                state: "MEH".to_string(),
                reason: if cancelled {
                    "cancelled by user".to_string()
                } else {
                    "build failed (see details)".to_string()
                },
                test_type: "package".to_string(),
                success: Some(false),
                error_log: Some(e),
                command_output: None,
                artifact_path: None,
            }
        }
    };
    cleanup(&tmp);
    res
}

/// Copy src into dir, auto-renaming (stem-1.ext, stem-2.ext, ...) on collision.
/// Keeps compound extensions like `.pkg.tar.zst` together.
pub(crate) fn copy_unique(src: &Path, dir: &Path) -> Result<PathBuf, String> {
    let fname = src
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "bad artifact name".to_string())?;
    let (stem, ext) = if let Some(pos) = fname.find(".pkg.tar.") {
        (fname[..pos].to_string(), fname[pos..].to_string())
    } else {
        match fname.rfind('.') {
            Some(i) => (fname[..i].to_string(), fname[i..].to_string()),
            None => (fname.to_string(), String::new()),
        }
    };
    let candidate = dir.join(fname);
    if !candidate.exists() {
        std::fs::copy(src, &candidate).map_err(|e| format!("copy: {e}"))?;
        return Ok(candidate);
    }
    let mut n = 1u32;
    loop {
        let dest = dir.join(format!("{stem}-{n}{ext}"));
        if !dest.exists() {
            std::fs::copy(src, &dest).map_err(|e| format!("copy: {e}"))?;
            return Ok(dest);
        }
        n += 1;
        if n > 9999 {
            return Err("too many collisions".to_string());
        }
    }
}

/// Borra el temp siempre. Llamar al salir de build_format.

/// AUR fetch+build+install with zero sudo-in-a-pipe (a helper's inner
/// sudo dies instantly without TTY on some sudoers configs — verified
/// the hard way). Proven primitives only: `helper -G` (download, user)
/// + `makepkg` (build, user, deps from .SRCINFO via pkexec) +
/// `pkexec pacman -U` (own GUI prompt).
pub(crate) fn aur_sync(helper: &str, package: &str, tmp: &Path, ctl: &Arc<BuildCtl>) -> Result<(), String> {
    append_log(&tmp.join("stdout.log"), &format!("fetching {package} via {helper} -G (AUR)…"));
    let aurdir = tmp.join("aur");
    std::fs::create_dir_all(&aurdir).map_err(|e| format!("aur dir: {e}"))?;
    let (ok, cancelled) = run_logged(
        helper,
        &["-G", package],
        &aurdir,
        tmp,
        BUILD_TIMEOUT_SECS,
        ctl,
        &[],
        &crate::container::Runner::Host,
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        return Err(format!("AUR fetch failed.\n{tail}"));
    }
    let pkgdir = aurdir.join(package);
    // Deps declared by the PKGBUILD go through pkexec (own GUI prompt):
    // makepkg itself never sudos, so this is the only privileged step.
    let deps = aur_makedeps(&pkgdir);
    if !deps.is_empty() {
        if !crate::install::has_gui_sudo() {
            return Err("needs pkexec for build deps (not found)".to_string());
        }
        let mut args: Vec<String> =
            vec!["pacman".to_string(), "-S".to_string(), "--needed".to_string(), "--noconfirm".to_string()];
        args.extend(deps);
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        append_log(&tmp.join("stdout.log"), "$ pkexec pacman -S (build deps)…");
        let (ok, cancelled) = run_logged(
            "pkexec",
            &arg_refs,
            &aurdir,
            tmp,
            BUILD_TIMEOUT_SECS,
            ctl,
            &[],
            &crate::container::Runner::Host,
        );
        if cancelled {
            return Err("cancelled by user".to_string());
        }
        if !ok {
            let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
            return Err(format!("build deps failed.\n{tail}"));
        }
    }
    // Same trust level as helpers' --noconfirm (which skips PGP with a warning).
    let (ok, cancelled) = run_logged(
        "makepkg",
        &["--noconfirm", "--skippgpcheck"],
        &pkgdir,
        tmp,
        BUILD_TIMEOUT_SECS,
        ctl,
        &[],
        &crate::container::Runner::Host,
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        return Err(format!("AUR build failed.\n{tail}"));
    }
    let built = pick_aur_package(&pkgdir)
        .ok_or_else(|| "AUR build produced no package".to_string())?;
    if !crate::install::has_gui_sudo() {
        return Err(format!(
            "built {} — install it manually (needs pkexec, not found)",
            built.to_string_lossy()
        ));
    }
    append_log(&tmp.join("stdout.log"), "installing via pkexec (GUI prompt)…");
    let (ok, cancelled) = run_logged(
        "pkexec",
        &["pacman", "-U", "--noconfirm", built.to_str().unwrap_or("pkg")],
        &aurdir,
        tmp,
        BUILD_TIMEOUT_SECS,
        ctl,
        &[],
        &crate::container::Runner::Host,
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), LOG_TAIL_LINES);
        return Err(format!("pkexec install failed.\n{tail}"));
    }
    Ok(())
}

/// Picks the built AUR package: sorted, non-debug preferred.
fn pick_aur_package(dir: &Path) -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().map(|x| x == "zst").unwrap_or(false)
                && p.to_string_lossy().contains(".pkg.tar.")
        })
        .collect();
    cands.sort();
    cands
        .iter()
        .find(|p| !p.to_string_lossy().contains("-debug"))
        .or_else(|| cands.first())
        .cloned()
}

/// Build + runtime deps declared in an AUR .SRCINFO (makedepends,
/// checkdepends, depends, any arch suffix). pacman accepts versioned
/// specs as-is.
fn aur_makedeps(pkgdir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(data) = std::fs::read_to_string(pkgdir.join(".SRCINFO")) else {
        return out;
    };
    for line in data.lines() {
        let t = line.trim();
        for key in ["makedepends", "checkdepends", "depends"] {
            if t.starts_with(key) {
                let rest = &t[key.len()..];
                if let Some((_, val)) = rest.split_once('=') {
                    let v = val.trim();
                    if !v.is_empty() && !out.contains(&v.to_string()) {
                        out.push(v.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Pure constructor for AUR recipes (helper resolved by the caller).
pub(crate) fn aur_recipe(helper: &str, package: &str, post_user: Vec<String>, command: String, note: String) -> crate::install::InstallRecipe {
    crate::install::InstallRecipe {
        kind: crate::install::RecipeKind::AurBuild {
            helper: helper.to_string(),
            package: package.to_string(),
            post_user,
        },
        command,
        note: Some(note),
        needs_password: false,
    }
}

fn cleanup(tmp: &Path) {
    let _ = std::fs::remove_dir_all(tmp);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wix_version_coercion() {
        assert_eq!(wix_version("1.2.0"), "1.2.0");
        assert_eq!(wix_version("2"), "2.0.0");
        assert_eq!(wix_version("1.2.0-beta"), "1.2.0");
        assert_eq!(wix_version("a.b"), "0.0.0");
        assert_eq!(wix_version("1.2.3.4"), "1.2.3");
    }

    #[test]
    fn guidish_shape() {
        let g = guidish(0x1234);
        assert_eq!(g.len(), 36);
        assert_eq!(g.chars().filter(|&c| c == '-').count(), 4);
        assert!(g.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_ne!(guidish(1), guidish(2));
    }

    #[test]
    fn flatpak_app_id_rules() {
        assert_eq!(flatpak_app_id("demo-selftest"), "io.bundleforge.demo_selftest");
        assert_eq!(flatpak_app_id("9lives"), "io.bundleforge.app9lives");
        assert!(flatpak_app_id("x").starts_with("io.bundleforge."));
    }

    #[test]
    fn flatpak_bundle_name_is_ref_valid() {
        // Flathub convention: app-id as filename (version segments would
        // break ref rules — segments can't start with a digit).
        let n = flatpak_bundle_name("io.bundleforge.demo_selftest");
        assert_eq!(n, "io.bundleforge.demo_selftest.flatpak");
    }

    #[test]
    fn flatpak_manifest_shape() {
        let m = flatpak_manifest("io.bundleforge.x", "usr/bin/run", "bftest-x", Path::new("/tmp/stage"));
        assert!(m.contains("\"app-id\": \"io.bundleforge.x\""));
        assert!(m.contains("\"runtime\": \"org.freedesktop.Platform\""));
        assert!(m.contains("\"command\": \"/app/usr/bin/run\""));
        assert!(m.contains("\"type\": \"dir\""));
        assert!(m.contains("cp -a . /app/"));
    }

    #[test]
    fn snap_name_rules() {
        assert_eq!(snap_name("demo-selftest"), "demo-selftest");
        assert_eq!(snap_name("My App_2.0"), "my-app-2-0");
        assert_eq!(snap_name("9lives"), "snap-9lives");
        assert_eq!(snap_name("---"), "app");
    }

    #[test]
    fn snapcraft_bin_is_snapcraft() {
        let b = snapcraft_bin();
        assert!(b.ends_with("snapcraft"));
        assert!(!b.is_empty());
    }

    #[test]
    fn snapcraft_yaml_shape() {
        let y = snapcraft_yaml("bftest-x", "0.0.0", "bf-selftest");
        assert!(y.contains("name: bftest-x"));
        assert!(y.contains("base: core24"));
        assert!(y.contains("grade: devel"));
        assert!(y.contains("confinement: devmode"));
        assert!(y.contains("command: bf-selftest"));
        assert!(y.contains("plugin: dump"));
        assert!(y.contains("source: ./payload"));
    }

    #[test]
    fn has_elf_spots_binaries() {
        let d = std::env::temp_dir().join("bf-test-has-elf");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("readme.txt"), "plain").unwrap();
        assert!(!has_elf(&d));
        std::fs::write(d.join("sub").join("app"), [0x7f, b'E', b'L', b'F', 0x02]).unwrap();
        assert!(has_elf(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn newest_snap_picks_snap_files_only() {
        let d = std::env::temp_dir().join("bf-test-newest-snap");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // A decoy directory named like a snap must NOT match (this was
        // the real bug: `snap pack <src> <name>` nests the artifact).
        std::fs::create_dir_all(d.join("decoy_0.0.0_amd64.snap")).unwrap();
        std::fs::write(d.join("notes.txt"), "hi").unwrap();
        std::fs::write(d.join("a_0.0.0_amd64.snap"), "snap-a").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(d.join("b_0.0.0_amd64.snap"), "snap-b").unwrap();
        let got = newest_snap(&d).unwrap();
        assert_eq!(got.file_name().unwrap(), "b_0.0.0_amd64.snap");
        assert!(list_dir(&d).contains("decoy_0.0.0_amd64.snap/"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn snap_yaml_mirrors_snapcraft_yaml() {
        // Container path (`snap pack`) must carry the same identity as
        // the native path (snapcraft.yaml): same name/version/entry,
        // devmode + devel, no store-only fields.
        let a = snapcraft_yaml("bftest-x", "0.0.0", "bf-selftest");
        let b = snap_yaml("bftest-x", "0.0.0", "bf-selftest");
        for needle in [
            "name: bftest-x",
            "base: core24",
            "version: '0.0.0'",
            "grade: devel",
            "confinement: devmode",
            "command: bf-selftest",
        ] {
            assert!(a.contains(needle), "snapcraft.yaml missing {needle}");
            assert!(b.contains(needle), "snap.yaml missing {needle}");
        }
        assert!(!b.contains("plugin:"), "snap.yaml takes no parts");
    }

























    #[test]
    fn apprun_is_real_launcher_not_stub() {
        let s = apprun_script("usr/share/bftest-x", "bf-selftest");
        assert!(s.starts_with("#!/bin/sh\n"), "shebang");
        assert!(
            s.contains("exec \"$APPDIR/usr/share/bftest-x/bf-selftest\" \"$@\""),
            "exec entry with args: {s}"
        );
        assert!(!s.contains("test artifact"), "no stub text");
        assert!(!s.contains("ls "), "no listing stub");
    }

    #[test]
    fn flatpak_entry_needs_executable() {
        let base = std::env::temp_dir().join(format!(
            "bf-flatpak-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("data.txt"), "x").unwrap();
        // data-only tree: no entry (honest failure downstream)
        assert!(flatpak_entry(&base, &base).is_none());
        let run = base.join("run.sh");
        std::fs::write(&run, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&run).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&run, p).unwrap();
        }
        let found = flatpak_entry(&base, &base).unwrap();
        assert!(found.ends_with("run.sh"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn msi_doc_embeds_cab() {
        // Regression: external cabs break standalone installs
        // ("source file not found ... cab1.cab" on Windows).
        let d = msi_doc("bftest-x", "1.2.0", "UPG", "<Directory />", "<ComponentRef />");
        assert!(d.contains("<MediaTemplate EmbedCab=\"yes\" />"));
        assert!(d.contains("Version=\"1.2.0\""));
        assert!(d.contains("Language=\"1033\""));
    }

    #[test]
    fn wxs_walk_nests_and_escapes() {
        let base = std::env::temp_dir().join(format!(
            "bf-wxs-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let sub = base.join("sub dir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(base.join("a&b.txt"), "x").unwrap();
        std::fs::write(sub.join("c.txt"), "y").unwrap();
        let mut out = String::new();
        let mut files = Vec::new();
        let mut idc = 0u32;
        wxs_walk(&base, &mut out, &mut files, &mut idc, 42).unwrap();
        assert_eq!(files.len(), 2);
        // nested Directory + escaped Name
        assert!(out.contains("<Directory"));
        assert!(out.contains("sub dir"));
        assert!(out.contains("a&amp;b.txt"));
        // every collected component has a File element
        for (fid, cid, guid, _) in &files {
            assert!(out.contains(&format!("<File Id=\"{fid}\"")));
            assert!(out.contains(&format!("<Component Id=\"{cid}\" Guid=\"{guid}\"")));
            assert_eq!(guid.len(), 36);
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
