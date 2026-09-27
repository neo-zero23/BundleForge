//! Container fallback (Option 2): when the native tool is missing but a
//! container runtime exists, builds run inside a distro image automatically.
//! Same temp dir bind-mounted at the SAME path, so all builder code paths
//! work unchanged. Honest logging tells the user it went containerized.
//!
//! Supported matrix (deliberately narrow):
//! - deb     -> debian:stable-slim, no setup (dpkg preinstalled)
//! - rpm     -> fedora:latest, setup: dnf install rpm-build
//! - pacman  -> archlinux:latest, setup: keyring + base-devel (SLOW first run)
//! - exe     -> debian:stable-slim, setup: apt-get update + install nsis
//! NOT covered: appimage (needs FUSE inside the container),
//! flatpak (nested bubblewrap sandboxing breaks inside containers).
//! Snap: ubuntu image + snapd/squashfs-tools, packed with the official
//! `snap pack` (no daemon, no systemd, no LXD — native off Ubuntu uses
//! this same path via Isolated).

use std::path::Path;
use std::sync::OnceLock;

/// Where a tool invocation runs.
pub enum Runner {
    /// Directly on the host (current behavior everywhere).
    Host,
    /// Single ephemeral `run --rm` invocation: setup steps + build command
    /// chained in ONE `sh -c` script, so installed tools persist for the
    /// build (separate `run --rm` calls would discard them with the
    /// container). Runs as container root: rootless maps to the host user
    /// outside; a trailing chown keeps rootful/docker artifacts readable.
    Container {
        runtime: &'static str,
        image: &'static str,
        setup: &'static [&'static [&'static str]],
    },
}

/// Which container engine is available, if any.
pub fn container_runtime() -> Option<&'static str> {
    if crate::detectors::tool_exists("podman") {
        Some("podman")
    } else if crate::detectors::tool_exists("docker") {
        Some("docker")
    } else {
        None
    }
}

/// Image + one-time setup commands (run inside, same wrapper) per format.
pub struct ContainerSpec {
    pub image: &'static str,
    pub setup: &'static [&'static [&'static str]],
}

pub fn container_spec(format: &str) -> Option<ContainerSpec> {
    match format {
        "deb" => Some(ContainerSpec {
            image: "debian:stable-slim",
            setup: &[],
        }),
        "rpm" => Some(ContainerSpec {
            image: "fedora:latest",
            setup: &[&["dnf", "install", "-y", "rpm-build"]],
        }),
        "pacman" => Some(ContainerSpec {
            image: "archlinux:latest",
            setup: &[
                &["pacman-key", "--init"],
                &["pacman-key", "--populate", "archlinux"],
                &["pacman", "-Sy", "--needed", "--noconfirm", "base-devel"],
            ],
        }),
        "exe" => Some(ContainerSpec {
            image: "debian:stable-slim",
            setup: &[&["apt-get", "update"], &["apt-get", "install", "-y", "nsis"]],
        }),
        "xbps" => Some(ContainerSpec {
            // Official Void image (has xbps tools); ensure present, idempotent.
            image: "ghcr.io/void-linux/void-glibc-full:latest",
            setup: &[&["xbps-install", "-Sy", "xbps"]],
        }),
        "msi" => Some(ContainerSpec {
            // Fedora, NOT debian: debian's msitools ships 5 tools but NO
            // wixl (verified); Fedora's does (/usr/bin/wixl, verified).
            image: "fedora:latest",
            setup: &[&["dnf", "install", "-y", "msitools"]],
        }),
        "zip" => Some(ContainerSpec {
            image: "debian:stable-slim",
            setup: &[&["apt-get", "update"], &["apt-get", "install", "-y", "zip"]],
        }),
        "snap" => Some(ContainerSpec {
            // No snapcraft deb (transitional -> snap store, needs a daemon),
            // no PyPI (stuck at 4.8). Official local packer: `snap pack`
            // from the snapd deb + squashfs-tools. No daemon, no systemd.
            image: "ubuntu:24.04",
            setup: &[
                &["apt-get", "update"],
                &[
                    "apt-get",
                    "install",
                    "-y",
                    "snapd",
                    "squashfs-tools",
                ],
            ],
        }),
        _ => None,
    }
}

/// Force container path even when the native tool exists.
/// Useful for testing and for reproducible builds.
/// Env `BUNDLEFORGE_CONTAINER=1` or the in-process override below
/// (the UI Isolated action sets it around one run, then clears it).
pub fn force_container() -> bool {
    use std::sync::atomic::Ordering;
    FORCE_CONTAINER.load(Ordering::SeqCst)
        || std::env::var("BUNDLEFORGE_CONTAINER").as_deref() == Ok("1")
}

static FORCE_CONTAINER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// In-process force flag (UI use). Always paired with a reset.
pub fn set_force_container(on: bool) {
    use std::sync::atomic::Ordering;
    FORCE_CONTAINER.store(on, Ordering::SeqCst);
}

/// Short human line for the Help view, or None when not an option here.
/// Containers only make sense on a Linux host (paths + tooling assumptions).
pub fn container_option(format: &str) -> Option<String> {
    if crate::detectors::os_name() != "linux" {
        return None;
    }
    let rt = container_runtime()?;
    let spec = container_spec(format)?;
    Some(format!(
        "automatic container build ({} {}, slower first run, no install needed)",
        rt, spec.image
    ))
}

fn uid_gid() -> Option<(String, String)> {
    static IDS: OnceLock<Option<(String, String)>> = OnceLock::new();
    IDS.get_or_init(|| {
        let u = std::process::Command::new("id")
            .args(["-u"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let g = std::process::Command::new("id")
            .args(["-g"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        match (u, g) {
            (Some(u), Some(g)) => Some((u, g)),
            _ => None,
        }
    })
    .clone()
}

/// Quote one argv element for `sh -c`. Bare safe words pass through;
/// anything else gets single-quoted (with embedded quotes escaped).
pub(crate) fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'-' | b'_' | b'.' | b'/' | b'=' | b':' | b'+' | b',' | b'@')
        })
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Build the podman/docker argv for ONE ephemeral run: setup steps + build
/// command chained via `sh -c`, work dir bind-mounted at the same path
/// (so absolute paths keep working). A trailing chown keeps artifacts
/// owned by the host user on rootful/docker hosts (no-op on rootless).
/// Pure function: unit-tested, no side effects.
pub fn container_argv(
    runtime: &str,
    image: &str,
    dir: &Path,
    setup: &[&[&str]],
    cmd: &str,
    args: &[&str],
    extra_env: &[(&str, &str)],
    chown_ids: Option<(&str, &str)>,
) -> Vec<String> {
    let dir_s = dir.to_string_lossy().to_string();
    let mut argv = vec![runtime.to_string(), "run".to_string(), "--rm".to_string()];
    argv.push("-v".to_string());
    argv.push(format!("{dir_s}:{dir_s}"));
    argv.push("-w".to_string());
    argv.push(dir_s.clone());
    for (k, v) in extra_env {
        argv.push("-e".to_string());
        argv.push(format!("{k}={v}"));
    }
    let mut steps: Vec<String> = setup
        .iter()
        .map(|s| {
            s.iter()
                .map(|a| sh_quote(a))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    let mut build: Vec<String> = vec![sh_quote(cmd)];
    build.extend(args.iter().map(|a| sh_quote(a)));
    steps.push(build.join(" "));
    // Ownership backstop for rootful/docker hosts (rootless: harmless no-op).
    if let Some((u, g)) = chown_ids {
        steps.push(format!("chown -R {u}:{g} {}", sh_quote(&dir_s)));
    }
    argv.push(image.to_string());
    argv.push("sh".to_string());
    argv.push("-c".to_string());
    argv.push(steps.join(" && "));
    argv
}

/// Inner build script for pacman-in-container: the single container run
/// executes as root (keeps setup state), but makepkg refuses root and
/// current makepkg has no `--asroot`. So: hand the workdir to uid 1000,
/// drop privileges with setpriv (util-linux, in Arch base) just for the
/// build, with HOME inside the writable workdir. The runner's trailing
/// chown restores host ownership afterwards.
pub(crate) fn pacman_container_script(dir: &Path) -> String {
    let d = sh_quote(&dir.to_string_lossy());
    format!(
        "chown -R 1000:1000 {d} && env -C {d} HOME={d} setpriv --reuid=1000 --regid=1000 --clear-groups makepkg --noconfirm"
    )
}

/// Free space (KiB) on the filesystem holding `path`, via `df`.
/// Used by the Isolated confirm step: a real number, no guessed image
/// sizes (images vary by version and runtime).
pub fn disk_free_kb(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("df")
        .args(["-k", "--output=avail"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_df_avail(&String::from_utf8_lossy(&out.stdout))
}

fn parse_df_avail(text: &str) -> Option<u64> {
    // `df --output=avail <path>`: header line + exactly one value line.
    text.lines()
        .skip(1)
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .next()
}

/// Image present locally? (`podman image exists` / `docker image inspect`).
pub fn image_present(runtime: &str, image: &str) -> bool {
    let (prog, args): (&str, &[&str]) = match runtime {
        "podman" => ("podman", &["image", "exists", image]),
        _ => ("docker", &["image", "inspect", image]),
    };
    std::process::Command::new(prog)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn current_uid_gid() -> Option<(String, String)> {
    uid_gid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_run_chains_setup_and_build() {
        let setup: &[&[&str]] = &[&["dnf", "install", "-y", "rpm-build"]];
        let argv = container_argv(
            "podman",
            "fedora:latest",
            Path::new("/tmp/bf-1"),
            setup,
            "rpmbuild",
            &["-bb", "pkg.spec"],
            &[],
            Some(("1000", "1000")),
        );
        assert_eq!(
            argv,
            vec![
                "podman",
                "run",
                "--rm",
                "-v",
                "/tmp/bf-1:/tmp/bf-1",
                "-w",
                "/tmp/bf-1",
                "fedora:latest",
                "sh",
                "-c",
                "dnf install -y rpm-build && rpmbuild -bb pkg.spec && chown -R 1000:1000 /tmp/bf-1",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        // No per-step containers: exactly one `run`, no userns/user flags.
        assert_eq!(argv.iter().filter(|a| a.as_str() == "run").count(), 1);
        assert!(!argv.contains(&"--userns=keep-id".to_string()));
        assert!(!argv.contains(&"--user".to_string()));
    }

    #[test]
    fn quoting_and_env_passthrough() {
        let argv = container_argv(
            "docker",
            "debian:stable-slim",
            Path::new("/tmp/bf 2"),
            &[],
            "makensis",
            &["setup file.nsi"],
            &[("ARCH", "x86_64")],
            None,
        );
        // -e passes env through; no chown without ids.
        assert!(argv.contains(&"-e".to_string()));
        assert!(argv.contains(&"ARCH=x86_64".to_string()));
        assert!(!argv.iter().any(|a| a.contains("chown")));
        let script = argv.last().unwrap();
        assert_eq!(
            script,
            "makensis 'setup file.nsi'",
            "args with spaces must be quoted"
        );
        // dir with a space is quoted in the chown-less script but the
        // -v/-w mounts keep the raw path (same path both sides).
        assert!(argv.contains(&"/tmp/bf 2:/tmp/bf 2".to_string()));
    }

    #[test]
    fn sh_quote_basics() {
        assert_eq!(sh_quote("--bb"), "--bb");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("o'clock"), "'o'\\''clock'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn force_override_round_trip() {
        set_force_container(false);
        let env_forced =
            std::env::var("BUNDLEFORGE_CONTAINER").as_deref() == Ok("1");
        assert_eq!(force_container(), env_forced);
        set_force_container(true);
        assert!(force_container());
        set_force_container(false);
        assert_eq!(force_container(), env_forced);
    }

    #[test]
    fn pacman_script_drops_privileges() {
        let s = pacman_container_script(Path::new("/tmp/bf-9"));
        assert!(s.contains("setpriv --reuid=1000 --regid=1000 --clear-groups makepkg --noconfirm"));
        assert!(!s.contains("--asroot"), "current makepkg rejects --asroot");
        // Spaced dirs stay one quoted unit everywhere.
        let s2 = pacman_container_script(Path::new("/tmp/b f"));
        assert!(s2.contains("'/tmp/b f'"));
    }

    #[test]
    fn parse_df_avail_cases() {
        assert_eq!(parse_df_avail("Avail\n  12345678\n"), Some(12_345_678));
        assert_eq!(parse_df_avail("Avail\n0\n"), Some(0));
        assert_eq!(parse_df_avail("Avail\n"), None);
        assert_eq!(parse_df_avail(""), None);
    }

    #[test]
    fn spec_matrix() {
        assert!(container_spec("deb").is_some());
        assert!(container_spec("rpm").is_some());
        assert!(container_spec("pacman").is_some());
        assert!(container_spec("exe").is_some());
        assert!(container_spec("xbps").is_some());
        assert!(container_spec("msi").is_some());
        assert!(container_spec("appimage").is_none());
        assert!(container_spec("flatpak").is_none());
        assert!(container_spec("snap").is_some());
        assert!(container_spec("zip").is_some());
        // msi must stay on Fedora: debian's msitools has no wixl (verified).
        assert_eq!(container_spec("msi").unwrap().image, "fedora:latest");
        assert!(container_spec("blah").is_none());
        assert_eq!(container_spec("deb").unwrap().setup.len(), 0);
        assert!(!container_spec("rpm").unwrap().setup.is_empty());
        assert!(!container_spec("xbps").unwrap().setup.is_empty());
    }
}
