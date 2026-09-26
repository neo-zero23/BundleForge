<div align="center">

<img width="1500" height="500" alt="bannernew" src="https://github.com/user-attachments/assets/9f78efa4-e9bb-47e9-94ff-d87c1e18be28" />

</div>

---
# BundleForge

**Wizard packaging source into executables and portables.**

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

## What is BundleForge?

BundleForge is a desktop wizard that packages **already-built** applications into 10 distribution formats. It does not modify your project, it does not install anything without your explicit permission, and test builds self-destruct.

You point it at a project folder, it detects what's packagable, and it produces the artifacts you ask for — natively, in a container, or on a remote builder.

> **Packaging is not building.** BundleForge takes what you already built and packages it.

---

## Supported formats

| Format | Native on Linux | Notes |
|---|---|---|
| **pacman** | ✅ | PKGBUILD + `makepkg` |
| **deb** | ✅ | `dpkg-deb --build` |
| **rpm** | ✅ | `rpmbuild` |
| **xbps** | ⚠️ experimental | `xbps-create` |
| **AppImage** | ✅ | `linuxdeploy` + real AppRun launcher |
| **exe** | ✅ | NSIS (`makensis`, runs natively on Linux) |
| **msi** | ✅ | WiX subset via `wixl` (msitools) |
| **flatpak** | ✅ | `flatpak-builder` + flathub SDK |
| **snap** | ⚠️ Ubuntu native / `snap pack` container elsewhere | no LXD |
| **zip** | ✅ | portable archive (sole compression exception) |

Each format resolves through one of three paths, in order:

1. **Native** — the toolchain is installed on your system.
2. **Isolated** — automatic podman/docker container (`run --rm`, first run downloads the image).
3. **Remote** — offload to a configured SSH builder (keys only, `BatchMode`, no passwords stored).

If none of the three works, the card says so honestly (`SAFE` / `MEH` / `BLOCKED`). No magic, no silent failures.

---

## Philosophy

- **Packaging ≠ building.** We package what you already built.
- **Your project is never modified.** Ever.
- **Nothing is installed without your permission** (exact command shown first).
- **Honest states.** When in doubt → `MEH`.
- **Test builds self-destruct.** Temp dirs are always cleaned up.
- **If it costs too much maintenance, it gets removed.** (MSIX and LXD died this way.)

---

## ⚠️ Security notice

**BundleForge runs project scripts during packaging. This is RCE by design.**

Only package your own code, or code you fully trust. This warning is displayed in the app at all times. It is not a bug. It is how packaging works.

---

## Installation (from source)

```bash
git clone https://github.com/neo-zero23/BundleForge.git
cd BundleForge/ui-linux
cargo run            # debug, rebuilds automatically
cargo build --release  # binary at ui-linux/target/release/bundleforge
```

Requirements:

- Rust stable, Linux.
- Optional but recommended: podman or docker (Isolated path).

Format toolchains are detected at runtime. Missing tools install via the built-in Download action (shows the exact command before running anything).

## Usage

1. Pick project + output folders (persisted, or session-only — your call in Settings).
2. Cards show live capability: `ready` / `container` / `remote` / `needs download` / `blocked`.
3. `Test all` = quick gates; `Build all` = real builds in temp (auto-deleted); `Open` per format = Test / Package / Isolated / Remote / Download.
4. `Re-check` re-probes after you install something.

Packages land in `<output>/packages/` with clean, ecosystem-correct names:

```
demo-selftest-0.0.0-1-any.pkg.tar.zst    # pacman
demo-selftest-0.0.0-1.noarch.rpm         # rpm
demo-selftest_0.0.0_all.deb              # deb
demo-selftest-0.0.0-x86_64.AppImage      # AppImage
demo-selftest-0.0.0-setup.exe            # exe
demo-selftest-0.0.0.msi                  # msi
demo-selftest-0.0.0_1.x86_64.xbps        # xbps
demo-selftest_0.0.0_all.snap             # snap
io.bundleforge.demo_selftest.flatpak     # flatpak
demo-selftest_0.0.0.zip                  # zip
```

---

## Architecture

- `core/` — zero-dependency Rust: detection, gating, builders, tester, install recipes, container + remote orchestration. Ships as a library **and** a JSON CLI.
- `ui-linux/` — native Iced Store UI, linked against the core as a library. No FFI.
- `demo-selftest/` — fixture project with a 4-point self-test (`bf-selftest`).
- `assets/` — badges, logo, title art.

A Windows frontend (`ui-windows/`, C++ WTL) is planned. No macOS.

## Configuration

Settings live in `~/.config/bundleforge/config.toml`:

```toml
last_project_folder = ""
last_output_folder = ""
show_button_tips = true
persist_project_folder = true
persist_output_folder = true
```

Remote builders live in `~/.config/bundleforge/remotes` (host + user + name only; SSH keys, never passwords).

## Contributing

Issues and PRs welcome if they align with the philosophy above. Before proposing a feature, ask: is it packaging or building? Does it cost more maintenance than the value it adds?

## License

Apache-2.0. See LICENSE.

<div align="center">

"Honesty before features."

</div>
