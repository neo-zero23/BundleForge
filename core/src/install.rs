//! Native tool installation (Option 1: install builder + re-test).
//! Explicit user permission via the UI [Install] button. Uses pkexec
//! (graphical sudo) when available; otherwise returns the exact command
//! for manual install. Never installs silently.
use crate::builders::{run_logged, tail_lines, BuildCtl};
use std::path::PathBuf;
use std::sync::Arc;

const INSTALL_TIMEOUT_SECS: u64 = 900;
const SOURCE_BUILD_TIMEOUT_SECS: u64 = 1800;
/// User-scope post steps can pull GBs (flatpak SDK); budget accordingly.
const INSTALL_POST_TIMEOUT_SECS: u64 = 3600;
const LINUXDEPLOY_URL: &str =
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage";

#[derive(Debug, Clone)]
pub enum RecipeKind {
    /// Install OS packages (needs pkexec, unless as_root=false e.g. yay).
    /// `post_user` runs afterwards as the user (no root) via sh: for
    /// heavy user-scope provisioning (flatpak SDK from flathub).
    Packages {
        manager: String,
        packages: Vec<String>,
        as_root: bool,
        post_user: Vec<String>,
    },
    /// Download a binary into ~/.local/bin (needs curl or wget).
    Download { url: String, dest_name: String },
    /// Build a tool from source (user-local, no root for the build
    /// itself; deps go through the native manager with pkexec).
    /// `build_script` runs under `sh -c` with cwd in the install temp
    /// (the clone lands at ./src); $HOME is the user's home.
    SourceBuild {
        manager: String,
        dep_packages: Vec<String>,
        git_url: String,
        build_script: String,
    },
    /// AUR package via helper + makepkg + pkexec (zero sudo-in-a-pipe:
    /// helpers' inner sudo dies without TTY on some sudoers configs).
    AurBuild {
        helper: String,
        package: String,
        post_user: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub struct InstallRecipe {
    pub kind: RecipeKind,
    /// Exact command shown to the user BEFORE running (permission).
    pub command: String,
    /// Extra note (e.g. large download). Shown next to the button.
    pub note: Option<String>,
    /// True when the flow needs sudo password up front (yay/paru run as
    /// user; their inner sudo has no TTY). UI asks before starting.
    pub needs_password: bool,
}

/// Detect the native package manager by probing. Order matters little:
// host normally has exactly one of these.
pub fn detect_manager() -> Option<&'static str> {
    for (tool, manager) in [
        ("pacman", "pacman"),
        ("apt-get", "apt"),
        ("dnf", "dnf"),
        ("xbps-install", "xbps"),
        ("zypper", "zypper"),
        ("apk", "apk"),
    ] {
        if crate::detectors::tool_exists(tool) {
            return Some(manager);
        }
    }
    None
}

fn pkg_args(manager: &str, packages: &[String]) -> Option<Vec<String>> {
    // NOTE: first element must be the manager binary itself (pkexec runs it).
    let mut args: Vec<String> = vec![manager.to_string()];
    args.extend(match manager {
        "pacman" => vec!["-S".into(), "--needed".into(), "--noconfirm".into()],
        "yay" | "paru" => vec![
            "-S".into(),
            "--needed".into(),
            "--noconfirm".into(),
            "--answerclean=None".into(),
            "--answerdiff=None".into(),
            "--answeredit=None".into(),
        ],
        "apt" => vec!["install".into(), "-y".into()],
        "dnf" => vec!["install".into(), "-y".into()],
        "xbps" => vec!["-Sy".into()],
        "zypper" => vec!["--non-interactive".into(), "install".into()],
        "apk" => vec!["add".into()],
        _ => return None,
    });
    args.extend(packages.iter().cloned());
    Some(args)
}

/// Package names per (manager, needed tool). Only well-known mappings;
// unknown combos return None (= stays BLOCKED, honest).
fn packages_for(manager: &str, tool: &str) -> Option<Vec<String>> {
    let v: Vec<&str> = match (manager, tool) {
        // dpkg: NOT universal! Verified: Arch (extra) and Debian (essential)
        // have it; Fedora repos do NOT (audited) → container covers dnf.
        ("pacman", "dpkg-deb") => vec!["dpkg"],
        ("apt", "dpkg-deb") => vec!["dpkg"],
        ("apt", "rpmbuild") => vec!["rpm"],
        ("dnf", "rpmbuild") => vec!["rpm-build"],
        ("zypper", "rpmbuild") => vec!["rpm-build"],
        ("pacman", "makepkg") => vec!["base-devel"],
        ("pacman", "makensis") => vec!["nsis"],
        ("apt", "makensis") => vec!["nsis"],
        ("pacman", "wixl") => vec!["msitools"],
        ("apt", "wixl") => vec!["msitools"],
        ("dnf", "wixl") => vec!["msitools"],
        ("pacman", "flatpak-builder") => vec!["flatpak-builder"],
        ("apt", "flatpak-builder") => vec!["flatpak-builder"],
        ("dnf", "flatpak-builder") => vec!["flatpak-builder"],
        ("zypper", "flatpak-builder") => vec!["flatpak-builder"],
        // zip ships everywhere (extra/core/main): the portable exception.
        ("pacman", "zip") => vec!["zip"],
        ("apt", "zip") => vec!["zip"],
        ("dnf", "zip") => vec!["zip"],
        ("zypper", "zip") => vec!["zip"],
        ("apk", "zip") => vec!["zip"],
        _ => return None,
    };
    Some(v.into_iter().map(|s| s.to_string()).collect())
}

/// Build deps per manager for compiling xbps from source.
/// Pure function: unit-tested. Unknown managers → None (container covers).
fn source_deps(manager: &str) -> Option<Vec<String>> {
    let list: &[&str] = match manager {
        // NOTE: no explicit `zlib` here — on current Arch it conflicts with
        // the installed zlib-ng, and libarchive's dep chain already pulls
        // the right provider.
        "pacman" => &["base-devel", "git", "libarchive", "openssl"],
        "apt" => &[
            "git",
            "build-essential",
            "autoconf",
            "automake",
            "libtool",
            "zlib1g-dev",
            "libarchive-dev",
            "libssl-dev",
        ],
        "dnf" => &[
            "git",
            "gcc",
            "make",
            "autoconf",
            "automake",
            "libtool",
            "zlib-devel",
            "libarchive-devel",
            "openssl-devel",
        ],
        "zypper" => &[
            "git",
            "gcc",
            "make",
            "autoconf",
            "automake",
            "libtool",
            "zlib-devel",
            "libarchive-devel",
            "libopenssl-devel",
        ],
        _ => return None,
    };
    Some(list.iter().map(|s| s.to_string()).collect())
}

/// snapd daemon reachable? Checks the SOCKET unit (persistent), not the
/// service (socket-activated, idles to inactive). Shared by gate + recipe.
pub(crate) fn snapd_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", "snapd.socket"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Recipe for snap: tool + running daemon (snapd needed to install/test
/// even when building Isolated). Per-manager because snapcraft lives in
/// different places (AUR on Arch, snap store on apt/dnf). Native builds
/// only run on Ubuntu — elsewhere use Isolated (container) or Remote,
/// no LXD. Offered whenever any piece is missing.
fn snap_recipe() -> Option<InstallRecipe> {
    let need_tool = !crate::detectors::tool_exists("snapcraft");
    let need_service = !snapd_active();
    if !need_tool && !need_service {
        return None; // fully provisioned
    }
    let manager = detect_manager()?;
    match manager {
        // Arch: snapd from AUR (guide-verified); snapcraft itself has NO
        // AUR package, so it comes from the snap store afterwards.
        // No sudo-in-a-pipe anywhere (fetch+build as user, pkexec GUI).
        "pacman" => {
            let aur = if crate::detectors::tool_exists("yay") {
                "yay"
            } else if crate::detectors::tool_exists("paru") {
                "paru"
            } else {
                return None;
            };
            Some(crate::builders::aur_recipe(
                aur,
                "snapd",
                vec![
                    "pkexec ln -sf /var/lib/snapd/snap /snap".to_string(),
                    "pkexec systemctl enable --now snapd.socket".to_string(),
                    "(snap list snapcraft >/dev/null 2>&1 || pkexec snap install snapcraft --classic)".to_string(),
                ],
                format!("{aur} fetch+build+install snapd (AUR) + socket + snapcraft from store"),
                "Provisions snapd + snapcraft (one time, for install/test). Native builds need Ubuntu; elsewhere use Isolated or Remote.".to_string(),
            ))
        }
        // apt: snapd native; snapcraft from the store itself.
        "apt" => Some(InstallRecipe {
            kind: RecipeKind::Packages {
                manager: "apt".to_string(),
                packages: vec!["snapd".to_string()],
                as_root: true,
                post_user: vec![
                    "pkexec ln -sf /var/lib/snapd/snap /snap".to_string(),
                    "pkexec systemctl enable --now snapd.socket".to_string(),
                    "(snap list snapcraft >/dev/null 2>&1 || pkexec snap install snapcraft --classic)".to_string(),
                ],
            },
            command: "sudo apt install snapd + snapcraft from store (~100 MB, one time)".to_string(),
            note: Some(
                "Provisions snapd + snapcraft (one time, for install/test). Native builds need Ubuntu; elsewhere use Isolated or Remote.".to_string(),
            ),
            needs_password: false,
        }),
        // dnf: snapd native + classic /snap link; snapcraft from the store.
        "dnf" => Some(InstallRecipe {
            kind: RecipeKind::Packages {
                manager: "dnf".to_string(),
                packages: vec!["snapd".to_string()],
                as_root: true,
                post_user: vec![
                    "pkexec ln -sf /var/lib/snapd/snap /snap".to_string(),
                    "pkexec systemctl enable --now snapd.socket".to_string(),
                    "(snap list snapcraft >/dev/null 2>&1 || pkexec snap install snapcraft --classic)".to_string(),
                ],
            },
            command: "sudo dnf install snapd + /snap link + snapcraft from store (~100 MB, one time)".to_string(),
            note: Some(
                "Provisions snapd + snapcraft (one time, for install/test). Native builds need Ubuntu; elsewhere use Isolated or Remote.".to_string(),
            ),
            needs_password: false,
        }),
        _ => None,
    }
}

/// Recipe for xbps: no distro ships xbps tools natively (Arch has it in
/// AUR only), so the universal native path is a user-local source build.
/// Container fallback covers the rest automatically.
fn xbps_recipe() -> Option<InstallRecipe> {
    if crate::detectors::tool_exists("xbps-create") {
        return None; // already present
    }
    let home_ok = std::env::var("HOME").map(|h| !h.is_empty()).unwrap_or(false);
    if !home_ok {
        return None; // nowhere user-local to install into
    }
    let manager = detect_manager()?;
    let deps = source_deps(manager)?;
    let url = "https://github.com/void-linux/xbps";
    Some(InstallRecipe {
        kind: RecipeKind::SourceBuild {
            manager: manager.to_string(),
            dep_packages: deps.clone(),
            git_url: url.to_string(),
            build_script: "cd src && (test -f configure || autoreconf -fi) && ./configure --prefix=$HOME/.local && make -j$(nproc) && (make install || true) && test -x $HOME/.local/bin/xbps-create && for t in xbps-create xbps-query xbps-rindex; do mv $HOME/.local/bin/$t $HOME/.local/bin/.$t.real && printf '#!/bin/sh\\nexec env LD_LIBRARY_PATH=\"$HOME/.local/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}\" \"$HOME/.local/bin/.'$t'.real\" \"$@\"\\n' > $HOME/.local/bin/$t && chmod +x $HOME/.local/bin/$t; done && $HOME/.local/bin/xbps-create -V".to_string(),
        },
        command: format!(
            "sudo install build deps via {manager}: {}\ngit clone --depth 1 {url}\n./configure --prefix=$HOME/.local && make && (make install || true) + launcher wrappers + smoke test  (user-local, no root)",
            deps.join(" ")
        ),
        note: Some(
            "Builds xbps tools from source (~5-10 min one time; user-local install). Arch shortcut: yay -S xbps".to_string(),
        ),
        needs_password: false,
    })
}

/// Flatpak Platform/Sdk presence (shared by gate + recipe).
pub(crate) fn flatpak_runtime_present(which: &str) -> bool {
    if !crate::detectors::tool_exists("flatpak") {
        return false;
    }
    std::process::Command::new("flatpak")
        .args(["info", which])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Recipe for flatpak: builder package + user-scope SDK provisioning
/// (flathub remote + Platform + Sdk, GBs one time). Offered whenever any
/// piece is missing, so Download always provisions everything.
fn flatpak_recipe() -> Option<InstallRecipe> {
    let ready = crate::detectors::tool_exists("flatpak-builder")
        && flatpak_runtime_present("org.freedesktop.Platform")
        && flatpak_runtime_present("org.freedesktop.Sdk");
    if ready {
        return None; // fully provisioned
    }
    let manager = detect_manager()?;
    let packages = packages_for(manager, "flatpak-builder")?;
    let post = vec![
        "flatpak remote-add --user --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo".to_string(),
        "flatpak install --user -y flathub org.freedesktop.Platform//24.08 org.freedesktop.Sdk//24.08".to_string(),
    ];
    Some(InstallRecipe {
        kind: RecipeKind::Packages {
            manager: manager.to_string(),
            packages,
            as_root: true,
            post_user: post,
        },
        command: format!(
            "sudo install flatpak-builder via {manager}\nflatpak remote-add --user flathub + install Platform//24.08 + Sdk//24.08  (~2 GB one time, as you, no root)"
        ),
        note: Some(
            "Provisions everything (builder + ~2 GB SDK one time). Afterwards flatpak cards go ready.".to_string(),
        ),
        needs_password: false,
    })
}

/// Recipe for a format, or None if no honest native path exists.
pub fn recipe_for(format: &str) -> Option<InstallRecipe> {
    if format == "xbps" {
        return xbps_recipe();
    }
    if format == "snap" {
        return snap_recipe();
    }
    if format == "flatpak" {
        return flatpak_recipe();
    }
    // Tool needed per format (mirrors tester::tool_for, minus appimage).
    let tool = match format {
        "deb" => "dpkg-deb",
        "rpm" => "rpmbuild",
        "pacman" => "makepkg",
        "appimage" => "linuxdeploy",
        "exe" => "makensis",
        "msi" => "wixl",
        "flatpak" => "flatpak-builder",
        "zip" => "zip",
        _ => return None, // unknown: no native install path
    };
    if tool == "linuxdeploy"
        && (crate::detectors::tool_exists("linuxdeploy")
            || crate::detectors::tool_exists("appimagetool"))
    {
        return None; // already present, nothing to install
    }
    if tool != "linuxdeploy" && crate::detectors::tool_exists(tool) {
        return None; // already present
    }
    if tool == "linuxdeploy" {
        return Some(InstallRecipe {
            kind: RecipeKind::Download {
                url: LINUXDEPLOY_URL.to_string(),
                dest_name: "linuxdeploy".to_string(),
            },
            command: format!(
                "curl -L -o ~/.local/bin/linuxdeploy {LINUXDEPLOY_URL} && chmod +x ~/.local/bin/linuxdeploy"
            ),
            note: Some("~13 MB download, user-local install".to_string()),
            needs_password: false,
        });
    }
    let manager = detect_manager()?;
    if tool == "makepkg" && manager != "pacman" {
        return None; // makepkg outside Arch: container only
    }
    // makensis on Arch: official repos lack it → AUR fetch+build+pkexec
    // (no sudo-in-a-pipe: helpers' inner sudo dies without TTY).
    if tool == "makensis" && manager == "pacman" {
        let aur = if crate::detectors::tool_exists("yay") {
            "yay"
        } else if crate::detectors::tool_exists("paru") {
            "paru"
        } else {
            return None; // manual: install yay/paru first
        };
        return Some(crate::builders::aur_recipe(
            aur,
            "nsis",
            vec![],
            format!("{aur} fetch + makepkg build + pkexec install nsis (AUR)"),
            "AUR build, takes a few minutes. Non-interactive: skips PGP verification like manual --noconfirm would.".to_string(),
        ));
    }
    let packages = packages_for(manager, tool)?;
    let mut cmd = match manager {
        "pacman" => format!("sudo pacman -S --needed {}", packages.join(" ")),
        "apt" => format!("sudo apt install {}", packages.join(" ")),
        "dnf" => format!("sudo dnf install -y {}", packages.join(" ")),
        "xbps" => format!("sudo xbps-install -Sy {}", packages.join(" ")),
        "zypper" => format!("sudo zypper --non-interactive install {}", packages.join(" ")),
        "apk" => format!("sudo apk add {}", packages.join(" ")),
        _ => return None,
    };
    let mut note = None;
    if tool == "makepkg" {
        cmd.push_str("   # base-devel is a large download (~200 MB)");
        note = Some("large download (~200 MB)".to_string());
    }
    Some(InstallRecipe {
        kind: RecipeKind::Packages {
            manager: manager.to_string(),
            packages,
            as_root: true,
            post_user: vec![],
        },
        command: cmd,
        note,
        needs_password: false,
    })
}

/// Non-interactive check: is there a fresh sudo timestamp already?
/// No password involved; instant when the user authed recently elsewhere.
pub fn sudo_fresh() -> bool {
    use std::process::{Command, Stdio};
    Command::new("sudo")
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// sudo timestamp warm-up: `echo pw | sudo -S -v`.
/// Used before flows whose inner sudo has no TTY (yay/paru).
/// Password lives only in this call frame, never logged.
pub fn sudo_preauth(password: &str, ctl: &Arc<BuildCtl>) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("sudo")
        .args(["-S", "-v"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("sudo: {e}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = writeln!(stdin, "{password}");
    }
    drop(child.stdin.take());
    let start = std::time::Instant::now();
    loop {
        if BuildCtl::cancelled(ctl) {
            let _ = child.kill();
            return Err("cancelled by user".to_string());
        }
        match child.try_wait() {
            Ok(Some(s)) => {
                if s.success() {
                    return Ok(());
                }
                // Distinguish wrong password (retryable) from other failures.
                let out = child
                    .stderr
                    .take()
                    .and_then(|mut h| {
                        use std::io::Read;
                        let mut buf = String::new();
                        h.read_to_string(&mut buf).ok().map(|_| buf)
                    })
                    .unwrap_or_default()
                    .to_lowercase();
                if out.contains("incorrect password")
                    || out.contains("sorry, try again")
                    || out.contains("incorrecta")
                {
                    return Err(
                        "wrong password — fix it in the field and press Install again".to_string(),
                    );
                }
                return Err(format!(
                    "sudo failed (not a password issue): {}",
                    out.lines().last().unwrap_or("unknown error")
                ));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("sudo wait: {e}")),
        }
        if start.elapsed() > std::time::Duration::from_secs(20) {
            let _ = child.kill();
            return Err("sudo timed out".to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

pub fn has_gui_sudo() -> bool {
    crate::detectors::tool_exists("pkexec")
}

fn home_bin() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".local").join("bin"))
}

/// Runs the recipe (pkexec for packages, curl/wget for download).
/// Recipe to install podman itself (Option 2 setup step).
/// None when not on Linux, runtime already present, or no manager.
pub fn runtime_recipe() -> Option<InstallRecipe> {
    if crate::detectors::os_name() != "linux" {
        return None;
    }
    if crate::container::container_runtime().is_some() {
        return None; // already have a runtime, nothing to install
    }
    let manager = detect_manager()?;
    Some(InstallRecipe {
        kind: RecipeKind::Packages {
            manager: manager.to_string(),
            packages: vec!["podman".to_string()],
            as_root: true,
            post_user: vec![],
        },
        command: format!("sudo install podman via {manager}"),
        note: Some("enables automatic container builds".to_string()),
        needs_password: false,
    })
}

/// Install the container runtime itself (podman) via native manager.
/// Returns Ok(log) or Err(reason). Used by the Help view's runtime button.
pub fn install_runtime(ctl: &Arc<BuildCtl>) -> Result<String, String> {
    let recipe = runtime_recipe().ok_or_else(|| "no runtime install path".to_string())?;
    run_recipe(&recipe, ctl)
}

/// Returns combined log tail on success, error text on failure.
pub fn install_format(format: &str, ctl: &Arc<BuildCtl>) -> Result<String, String> {
    let recipe = recipe_for(format).ok_or_else(|| "no native install path".to_string())?;
    run_recipe(&recipe, ctl)
}

/// Packages via manager (pkexec GUI when as_root). Shared by the
/// Packages recipe and the dep stage of SourceBuild. Afterwards runs
/// `post_user` shell steps as the user (no root): heavy user-scope
/// provisioning with explicit consent (flatpak SDK from flathub).
fn install_packages(
    manager: &str,
    packages: &[String],
    as_root: bool,
    post_user: &[String],
    command_text: &str,
    tmp: &std::path::Path,
    ctl: &Arc<BuildCtl>,
) -> Result<String, String> {
    let args =
        pkg_args(manager, packages).ok_or_else(|| "unsupported manager".to_string())?;
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    // as_root → pkexec (GUI prompt). yay/paru run as user.
    let (prog, full_args): (&str, Vec<&str>) = if as_root {
        if !has_gui_sudo() {
            return Err(format!(
                "needs pkexec (not found). Run manually:\n{command_text}"
            ));
        }
        let mut v = vec!["pkexec"];
        v.extend(arg_refs);
        ("pkexec", v)
    } else {
        (arg_refs[0], arg_refs[1..].to_vec())
    };
    let (ok, cancelled) = run_logged(
        prog,
        &full_args,
        tmp,
        tmp,
        INSTALL_TIMEOUT_SECS,
        ctl,
        &[],
        &crate::container::Runner::Host,
    );
    if cancelled {
        return Err("cancelled by user".to_string());
    }
    if !ok {
        let tail = tail_lines(&tmp.join("stderr.log"), 30);
        return Err(format!("install failed.\n{tail}"));
    }
    // User-scope post steps (explicit consent via the recipe screen).
    for step in post_user {
        let (ok, cancelled) = run_logged(
            "sh",
            &["-c", step],
            tmp,
            tmp,
            INSTALL_POST_TIMEOUT_SECS,
            ctl,
            &[],
            &crate::container::Runner::Host,
        );
        if cancelled {
            return Err("cancelled by user".to_string());
        }
        if !ok {
            let tail = tail_lines(&tmp.join("stderr.log"), 15);
            return Err(format!("post-install step failed.\n{tail}"));
        }
    }
    Ok(tail_lines(&tmp.join("stdout.log"), 5))
}

/// Shared install mechanics (used by install_format).
fn run_recipe(recipe: &InstallRecipe, ctl: &Arc<BuildCtl>) -> Result<String, String> {
    let tmp = std::env::temp_dir().join(format!(
        "bundleforge-install-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("temp dir: {e}"))?;
    let res: Result<String, String> = (|| {
        match &recipe.kind {
            RecipeKind::Packages {
                manager,
                packages,
                as_root,
                post_user,
            } => install_packages(manager, packages, *as_root, post_user, &recipe.command, &tmp, ctl),
            RecipeKind::SourceBuild {
                manager,
                dep_packages,
                git_url,
                build_script,
            } => {
                // 1) Build deps through the native manager (pkexec GUI).
                install_packages(
                    manager,
                    dep_packages,
                    true,
                    &[],
                    &recipe.command,
                    &tmp,
                    ctl,
                )?;
                // 2) Clone + run the recipe's build script (user-local).
                if !crate::detectors::tool_exists("git") {
                    return Err("git missing after dep install".to_string());
                }
                let src = tmp.join("src");
                let (ok, cancelled) = run_logged(
                    "git",
                    &["clone", "--depth", "1", git_url, src.to_str().unwrap_or("src")],
                    &tmp,
                    &tmp,
                    INSTALL_TIMEOUT_SECS,
                    ctl,
                    &[],
                    &crate::container::Runner::Host,
                );
                if cancelled {
                    return Err("cancelled by user".to_string());
                }
                if !ok {
                    let tail = tail_lines(&tmp.join("stderr.log"), 10);
                    return Err(format!("git clone failed.\n{tail}"));
                }
                let (ok, cancelled) = run_logged(
                    "sh",
                    &["-c", build_script],
                    &tmp,
                    &tmp,
                    SOURCE_BUILD_TIMEOUT_SECS,
                    ctl,
                    &[],
                    &crate::container::Runner::Host,
                );
                if cancelled {
                    return Err("cancelled by user".to_string());
                }
                if !ok {
                    // Generous tail: SDK builds fail in cascades and the
                    // root error sits above make's own Error lines.
                    let tail = tail_lines(&tmp.join("stderr.log"), 50);
                    return Err(format!("source build failed.\n{tail}"));
                }
                Ok(tail_lines(&tmp.join("stdout.log"), 5))
            }
            RecipeKind::AurBuild {
                helper,
                package,
                post_user,
            } => {
                crate::builders::aur_sync(helper, package, &tmp, ctl)?;
                for step in post_user {
                    let (ok, cancelled) = run_logged(
                        "sh",
                        &["-c", step],
                        &tmp,
                        &tmp,
                        INSTALL_POST_TIMEOUT_SECS,
                        ctl,
                        &[],
                        &crate::container::Runner::Host,
                    );
                    if cancelled {
                        return Err("cancelled by user".to_string());
                    }
                    if !ok {
                        let tail = tail_lines(&tmp.join("stderr.log"), 15);
                        return Err(format!("post-install step failed.\n{tail}"));
                    }
                }
                Ok(tail_lines(&tmp.join("stdout.log"), 5))
            }
            RecipeKind::Download { url, dest_name } => {
                let bindir = home_bin().ok_or_else(|| "no HOME".to_string())?;
                std::fs::create_dir_all(&bindir).map_err(|e| format!("mkdir ~/.local/bin: {e}"))?;
                let dest = bindir.join(dest_name);
                let downloader = if crate::detectors::tool_exists("curl") {
                    vec!["curl", "-L", "-o"]
                } else if crate::detectors::tool_exists("wget") {
                    vec!["wget", "-O"]
                } else {
                    return Err("needs curl or wget".to_string());
                };
                let (dl, args): (&str, Vec<&str>) = if downloader[0] == "curl" {
                    ("curl", vec!["-L", "-o", dest.to_str().unwrap_or("linuxdeploy"), url])
                } else {
                    ("wget", vec!["-O", dest.to_str().unwrap_or("linuxdeploy"), url])
                };
                let (ok, cancelled) = run_logged(dl, &args, &tmp, &tmp, INSTALL_TIMEOUT_SECS, ctl, &[], &crate::container::Runner::Host);
                if cancelled {
                    return Err("cancelled by user".to_string());
                }
                if !ok {
                    return Err("download failed".to_string());
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = std::fs::metadata(&dest)
                        .map_err(|e| format!("stat: {e}"))?
                        .permissions();
                    perms.set_mode(0o755);
                    std::fs::set_permissions(&dest, perms).map_err(|e| format!("chmod: {e}"))?;
                }
                Ok(format!("installed to {}", dest.to_string_lossy()))
            }
        }
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_deps_per_manager() {
        let pac = source_deps("pacman").unwrap();
        assert!(pac.contains(&"git".to_string()));
        assert!(pac.contains(&"base-devel".to_string()));
        let apt = source_deps("apt").unwrap();
        assert!(apt.contains(&"build-essential".to_string()));
        assert!(apt.contains(&"libarchive-dev".to_string()));
        let dnf = source_deps("dnf").unwrap();
        assert!(dnf.contains(&"gcc".to_string()));
        assert!(dnf.contains(&"openssl-devel".to_string()));
        assert!(source_deps("brew").is_none());
        assert!(source_deps("yay").is_none());
    }

    #[test]
    fn dpkg_mapping_audited() {
        // pacman: dpkg in repos (CachyOS verified). apt: essential.
        // dnf: NO dpkg in Fedora repos (audited) → None, container covers.
        assert!(packages_for("pacman", "dpkg-deb").is_some());
        assert!(packages_for("apt", "dpkg-deb").is_some());
        assert!(packages_for("dnf", "dpkg-deb").is_none());
        assert!(packages_for("zypper", "dpkg-deb").is_none());
    }

    #[test]
    fn rpm_mapping_audited() {
        assert!(packages_for("pacman", "rpmbuild").is_none());
        assert_eq!(
            packages_for("apt", "rpmbuild").unwrap(),
            vec!["rpm".to_string()]
        );
        assert_eq!(
            packages_for("dnf", "rpmbuild").unwrap(),
            vec!["rpm-build".to_string()]
        );
    }

    #[test]
    fn pacman_mapping_audited() {
        // makepkg is Arch-only (CachyOS: present + base-devel; antiX and
        // Fedora: absent → container-only, enforced in recipe_for).
        assert_eq!(
            packages_for("pacman", "makepkg").unwrap(),
            vec!["base-devel".to_string()]
        );
    }

    #[test]
    fn exe_msi_mapping_audited() {
        // exe: AUR nsis on Arch (proven), apt nsis on Debian/Ubuntu
        // (antiX slim repos lack it → auto-container), dnf none.
        assert_eq!(
            packages_for("pacman", "makensis").unwrap(),
            vec!["nsis".to_string()]
        );
        assert_eq!(
            packages_for("apt", "makensis").unwrap(),
            vec!["nsis".to_string()]
        );
        assert!(packages_for("dnf", "makensis").is_none());
        // msi: msitools on all three (dnf presence verified on Fedora).
        assert_eq!(
            packages_for("pacman", "wixl").unwrap(),
            vec!["msitools".to_string()]
        );
        assert_eq!(
            packages_for("apt", "wixl").unwrap(),
            vec!["msitools".to_string()]
        );
        assert_eq!(
            packages_for("dnf", "wixl").unwrap(),
            vec!["msitools".to_string()]
        );
    }

    #[test]
    fn flatpak_mapping_audited() {
        // flatpak-builder in all three managers' repos (dnf presence
        // verified on Fedora; neither tool on clean antiX/Fedora).
        for m in ["pacman", "apt", "dnf"] {
            assert_eq!(
                packages_for(m, "flatpak-builder").unwrap(),
                vec!["flatpak-builder".to_string()]
            );
        }
    }

    #[test]
    fn xbps_recipe_shape_when_offered() {
        // Shape check only when this host has a known manager; otherwise
        // (unknown manager) the honest answer is None — both are valid.
        if let Some(r) = recipe_for("xbps") {
            match &r.kind {
                RecipeKind::SourceBuild {
                    manager,
                    dep_packages,
                    git_url,
                    build_script,
                } => {
                    assert!(!manager.is_empty());
                    assert!(!dep_packages.is_empty());
                    assert!(git_url.contains("void-linux/xbps"));
                    assert!(build_script.contains("configure"));
                }
                _ => panic!("xbps must offer SourceBuild"),
            }
            assert!(r.command.contains("configure"));
            assert!(!r.needs_password);
        }
    }

    #[test]
    fn flatpak_recipe_provisions_sdk() {
        // When offered, Download must provision everything (builder +
        // flathub Platform + Sdk), never just the builder.
        if let Some(r) = recipe_for("flatpak") {
            match &r.kind {
                RecipeKind::Packages {
                    packages,
                    post_user,
                    ..
                } => {
                    assert!(packages.contains(&"flatpak-builder".to_string()));
                    assert_eq!(post_user.len(), 2);
                    assert!(post_user[0].contains("flathub"));
                    assert!(post_user[1].contains("org.freedesktop.Sdk"));
                }
                _ => panic!("flatpak must offer Packages+post"),
            }
            assert!(r.command.contains("2 GB") || r.command.contains("~2 GB"));
        }
    }







    #[test]
    fn aur_recipe_constructor() {
        let r = crate::builders::aur_recipe("yay", "foo", vec!["echo hi".to_string()], "cmd".to_string(), "note".to_string());
        match &r.kind {
            RecipeKind::AurBuild { helper, package, post_user } => {
                assert_eq!(helper, "yay");
                assert_eq!(package, "foo");
                assert_eq!(post_user.len(), 1);
            }
            _ => panic!("must be AurBuild"),
        }
        assert!(!r.needs_password);
    }

    #[test]
    fn snap_recipe_shape_when_offered() {
        // Offered whenever snapcraft is missing or snapd is down.
        // Kind varies by manager (AUR on pacman, Packages+post elsewhere).
        if let Some(r) = recipe_for("snap") {
            match &r.kind {
                RecipeKind::Packages {
                    packages,
                    post_user,
                    ..
                } => {
                    assert!(!packages.is_empty());
                    assert!(!post_user.is_empty());
                }
                RecipeKind::AurBuild {
                    helper, package, ..
                } => {
                    assert!(!helper.is_empty());
                    assert!(!package.is_empty());
                }
                _ => panic!("snap must offer Packages+post or AurBuild"),
            }
        }
    }
}
