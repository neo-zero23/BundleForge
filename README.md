# BundleForge

> Wizard packaging source into executables and portables.

BundleForge packages desktop apps into native formats with honest states
(SAFE / MEH / BLOCKED). It never modifies your project, never installs
anything without permission, and auto-deletes test builds. Apache-2.0.

## Formats (10)

| Format | Via |
|---|---|
| `.deb`, `.rpm`, `.pacman`, `.xbps` | native tooling / container |
| `.AppImage` (real launcher), `.flatpak` | linuxdeploy / flatpak-builder |
| `.snap` | Ubuntu native, `snap pack` container elsewhere |
| `.exe` (NSIS), `.msi` (WiX) | makensis / wixl, built on Linux |
| `.zip` | portable |

Each format builds **natively**, in an **Isolated** container
(podman/docker, automatic), or on a **Remote** SSH builder (keys only).

## Run (Linux)

```bash
cd ui-linux && cargo run
```

Core CLI also works standalone (`core/`). Needs Rust stable.

## Layout

- `core/` — zero-dependency Rust CLI (scan / test / package / install hints)
- `ui-linux/` — native Iced Store UI
- `demo-selftest/` — fixture project with a 4-point self-test
- `assets/` — badges, logo, title art
- `PLAN.md` — milestones and decisions (Spanish/English)

Status: v0.0.1-alpha. Windows UI (WTL) planned; no macOS.
