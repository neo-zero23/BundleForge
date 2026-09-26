# BundleForge — PLAN

> Wizard that packages desktop apps into multiple formats.
> Stack: **Rust core CLI + native per-OS UI** (Windows: C++ with WTL,
> Linux: Iced). License: Apache-2.0. No Mac.

## 1. Philosophy

- 5-step wizard. It doesn't prepare your project, it only packages it.
- Test build = temporary + auto-delete. Packaging = final, into `dist/`.
- The user always knows what's going on: honest states, terminal on demand.

## 2. Scope

### v1 — Local (current phase)
- Desktop apps only. **"Language packages" track: OUT.**
- Only formats buildable on the host OS. **No macOS.**
- No auth, no accounts, no GitHub Releases, no signing.

### Build order
1. **Core CLI first** (works without UI; validated by packaging real projects).
2. Linux UI (Iced, no reboots needed to develop) ← NOW.
3. Windows UI (C++ WTL, classic dialogs).

### v2 — Cloud (later)
- Fan-out to runners with the **user's** GitHub account (BYO infra).
- "Upload to GitHub Releases" button.
- Secure signing-secret handoff design (or stays blocked).

## 3. v1 formats

| Format | Where | Note |
|---|---|---|
| `.deb` | Linux | standard |
| `.rpm` | Linux | standard |
| `.pacman` | Arch/derivatives | requires `makepkg` + `base-devel` |
| `.xbps` | Void (experimental) | see rule below |
| `.AppImage` | Linux | via linuxdeploy or similar |
| `.exe` (NSIS) | Windows | — |
| `.msi` (WiX) | Windows | — |
| `.zip` | portable | single compression exception (M19) |

**`.xbps` rule (experimental, MEH by default):**
- `xbps-create` installed + `void-packages` present → SAFE.
- `xbps-create` without `void-packages` → MEH + warning ("first run ~1h, GBs").
- Nothing installed → BLOCKED with info.
- "Compression" track: OUT, redundant — **sole exception `.zip` (M19,
  portables; rar/7z/tar stay OUT).**

## 4. Wizard — 5 screens

### P1. Pick type
Big buttons. Desktop apps only in v1.

### P2. Configure
- Project folder picker (path input for now, native picker later).
- Format checkboxes (host-OS ones + experimental xbps).
- Note: "Fewer formats = faster results".
- Mode: ⦿ Quick simulation (~2s: checks tools + OS + deps, does NOT build)
  ○ Normal build (real per-project time, measured and shown).
- [Test selected] button.
- >4 formats → estimated-time warning, no hard cap.

### P3. Loading
- Logo + current format (i/n) + progress bar.
- [Advanced ▼] collapsible (collapsed by default):
  `$ <command>`, live stdout/stderr, [Copy].
- [Cancel]: kills the in-flight build, finishes with partial results.

### P4. Results
- Disclaimer per mode.
- Per format: status icon + name + reason + button per state +
  [Advanced] + ℹ️ details.
- [Re-run all] / [Normal build all] (if sim) / [Package all] (if build).

### P5. Post-packaging
- File list in `dist/`. [Open folder].

## 5. Cloud flow (v2, designed, not implemented)

When a format isn't available on the current OS:
1. Detect `gh` CLI; if missing, per-OS install guide.
2. GUI auth: connect button → opens `github.com/login/device` + copyable
   code + polling of `gh auth status`. Extra `delete_repo` scope only if
   disposable (`gh auth refresh -s delete_repo`).
3. Repo options: default **private + disposable**, auto name
   `bundleforge-job-<timestamp>-<hash>` (reusable = advanced option).
4. Warning if public/closed-source (strong if no LICENSE or proprietary).
5. [Start packaging] → reused screen 3 (live log) → download
   artifact → delete repo/instance → file into `dist/`.
6. 30–45 min timeout per job; cancel cleans everything (no zombie repos).
7. Per-format workflow templates, versioned in the repo.
8. Future alternative: user self-hosted runners.

## 6. Hard rules

1. **Never modify the user's project files.** Cache in own data dir:
   `~/.config/bundleforge/cache/` (never `.packit-cache.json` inside).
2. **Never install dependencies without explicit permission.**
3. Test builds are ALWAYS deleted when finished.
4. Honest states: when in doubt → MEH. Never promise what wasn't verified.
5. Responsive UI; Advanced collapsed by default.
6. Only package own/trusted code (packaging = RCE by design):
   warning on open.
7. Signing (Windows .pfx, Apple notarization, GPG): BLOCKED in v1 with info.
   No secrets passed anywhere.

## 7. Structure

The core is a JSON-speaking CLI; UIs are dumb (spawn, parse, display).
No C++↔Rust FFI.

```
bundleforge/
├── core/                    # Rust: lib + CLI binary
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs          # CLI: scan/test/package
│       ├── lib.rs           # ProjectInfo/TestResult/PackageResult types
│       ├── detectors.rs     # stack/tools/OS
│       ├── builders.rs      # pacman real, rest in M2
│       ├── tester.rs        # quick + normal
│       └── cache.rs         # own data dir (~/.config/bundleforge/)
├── ui-windows/              # C++ WTL: 5 classic dialogs (.rc) [later]
└── ui-linux/                # Iced: form → loading → results [now]
    ├── Cargo.toml
    ├── src/main.rs
    └── assets -> ../assets/loading/ (anvil sprite)
```

## 8. CLI commands (JSON on stdout)

- `bundleforge-core scan <path>` → ProjectInfo
- `bundleforge-core test [--quick] --formats deb,rpm` → [TestResult]
- `bundleforge-core package --formats ...` → [PackageResult] (stub until M2)

## 9. Types

```rust
struct ProjectInfo { path: String, stack: String, name: String, version: String }

struct TestResult {
    format: String, state: String, reason: String,
    test_type: String, success: Option<bool>,
    error_log: Option<String>, command_output: Option<String>
}

struct PackageResult { format: String, path: String, size: u64, success: bool }
```

## 10. Milestones

- [x] M0: repo + PLAN + stack (Rust core CLI + WTL + Iced)
- [x] M1: real core CLI `scan` (stack/tools/OS) + `quick_test` (SAFE/MEH/BLOCKED)
- [x] M2a: real `.pacman` builder (generated PKGBUILD + makepkg + verify + auto-delete) + `normal_test` + Cancel
- [x] M3a: Linux UI wizard (form → animated loading + progress + detail → results)
- [x] M2b: real `.deb` + `.rpm` + `.AppImage`
  (linuxdeploy, ARCH required, icon-name matching) builders + `package`
  (kept artifacts + auto-rename) + auto-delete
- [x] M2c: container fallback — automatic podman/docker build when the
  native tool is missing (deb/rpm/pacman/exe images + setup), `BUNDLEFORGE_CONTAINER=1`
  to force, honest `(container)` suffix, unit-tested argv.
  Single `run --rm` per build (setup+build in one `sh -c`, tools persist;
  as-root inside, setpriv drop for makepkg, trailing chown for docker/rootful).
- [ ] M3b: native folder picker, persisted Advanced toggle, UI polish
- [ ] M4: Windows UI (WTL) + release v1
- [ ] M5: Linux UI release polish
- [x] M6a: phase 3 slice 1 — remote builders over SSH (keys only, no daemon):
  persisted remote config + keys-only probe (3 honest states) + OS-aware
  Help matrix (Option 3) + Remotes view with back-target. No offload yet.
- [x] M6b: phase 3 slice 2 — SSH offload (verified loopback, remote .pacman saved).
- [x] M7: xbps support — Help Option 1 (user-local source-build recipe
  with launcher wrappers; upstream keys quirk tolerated, static broken
  upstream, LDFLAGS ignored) + real `build_xbps` (flags verified e2e:
  create→rindex→query round-trip) + void container spec + gate +
  remote matching. Verified native end-to-end via CLI.
- [ ] M8: msi via wixl — builder written (hand-rolled WiX XML, no heat;
  32-bit package, GUIDs per build), gate flipped to wixl, debian
  container spec, per-manager recipes, remote matching. 18 tests + UI
  compile. wixl acceptance NOT verifiable here (no msitools) — user
  tests natively.
- [x] M9: full matrix verified on real systems (demo-selftest 4/4 or
  ALL PASS everywhere): pacman (Cachy), AppImage (runs, stub launcher),
  exe + msi (Windows dual-boot), deb (antiX), xbps (Void VM +
  xbps-install), rpm (Fedora VM + rpm -i).
- [x] M10: store redesign — home is a per-format store (live capability
  states: ready/container/remote/download + red retry + inline last
  error), detail view per format (browse pickers via kdialog/zenity,
  auto-scan, Test/Package/Isolated/Remote/Download), batch Test/Build
  all kept, re-check button, force-container override for Isolated.
  20 core tests + UI compile. User verifies live.
- [x] M11: screen diet — Results slimmed to status + console log + back;
  Help view deleted (Detail absorbed exact command, password field,
  podman install); batch runs write bundleforge-report-<ts>.txt and
  return home. Views left: Store, Detail, Loading, Results, Remotes.
- [x] M12: horizontal store — icon rows (4+3) with generated badges
  (trademark-free), name + one-line state + Download/Retry/Open per
  card. UI compiles.
- [x] M13: flatpak — gate (builder + Platform + Sdk, no silent SDK),
  builder (generated manifest, first-executable entry rule, builder +
  bundle), per-manager recipes + user-scope SDK provisioning via
  Download (flathub, ~2 GB one time), no container (nested sandbox),
  remote Linux. 24 core tests + UI compile.
- [x] M16: snap — gate (snapcraft + running snapd; native Ubuntu only,
  elsewhere Isolated/Remote), builder (generated snapcraft.yaml +
  --destructive-mode natively; generated meta/snap.yaml + official
  `snap pack` in the automatic ubuntu:24.04 container via Isolated),
  per-manager recipes (snapd + snapcraft for install/test, no LXD),
  remote Linux. 38 core tests + UI compile. LIVE-VERIFIED 2026-09-25 on
  CachyOS: Isolated (ubuntu:24.04 + snap pack) → install
  --dangerous --devmode → snap run self-test 4/4 PASS.
- [x] M17: safety UX (DeepSeek audit) — RCE warning under the Store
  title (always visible), Isolated two-step confirm when the image is
  not cached (real free-space via df, no guessed sizes; second press
  confirms the hundreds-of-MB download). 39 core tests + UI compile.
- [x] M18: AppImage real launcher — AppRun generated via shared
  first-executable rule (`flatpak_entry`, no dup, no hardcode):
  `exec "$APPDIR/usr/share/<pkg>/<entry>" "$@"` (+x kept). 40 core
  tests + UI compile. User verifies live (run + unsquashfs).
  LIVE-VERIFIED 2026-09-26 on CachyOS: AppImage runs the real payload
  self-test 4/4 PASS (stub could never do that).
- [x] M19: zip (sole compression exception, portables) — gate (tool-only,
  any host OS), builder (`zip -qr` at archive root via one sh -c so host
  + container share the path), per-manager recipes (zip ships everywhere),
  debian container spec, remote any-Linux, Store card + generated badge.
  40 core tests + UI compile. LIVE-VERIFIED 2026-09-26 on CachyOS
  (native, user badge).
- [x] M20: clean naming (DeepSeek T1) — dropped the `bftest-` prefix in all
  9 builders (tool-generated names follow automatically), version added to
  exe (`-setup` kept) and AppImage (via `$VERSION` for linuxdeploy).
  Untouched ecosystem standards: deb `_all`, rpm `-1.noarch`, pacman
  `-1-any`, snap `_all`, flatpak reverse-DNS. 40 core tests + UI compile.
  User verifies live (filenames + AppImage VERSION honored).
- [x] M21: config UI + tooltips (DeepSeek brief) — gear opens a modal
  (X/ESC/click-dim closes) with last folders (readonly + Browse,
  persisted immediately) + tips toggle; `~/.config/bundleforge/
  config.toml` (TOML-valid schema, zero-dep line parser, corrupt =
  defaults); hover tips on Test/Build/Re-check/Remotes gated by the
  flag. Deviations: no Stack in iced 0.12 (modal replaces view, same
  focus), no 500ms delay API (tips on hover). 44 core tests + UI
  compile. M21b: config moved to a full view (no modal) with Back,
  gear uses config.png badge, tooltips got a solid background.
  M21c: Isolated tooltip (Detail) + per-format enable/disable checks in
  Settings (`disabled_formats` in config.toml, hides cards and skips
  batch). 44 core tests + UI compile. M21d: checks refocused to folder
  persistence (Remember project/output folder; unchecked = session-only,
  saved value cleared). Format checks removed (not wanted).
- [x] M22: iced 0.12 → 0.14 migration (proven on a throwaway branch first,
  then applied clean, no merge) — Catalog styles (pill/danger/tips as
  closures), Task replaces Command, boot/update/view builder, Space
  spacer, checkbox labels, align_x/align_y split, borrowed text
  (lifetimes unified). Dropped: window icon (API removed upstream),
  fixed progress height, App ID setting. 44 core tests + UI compile.
  Stack available now → Yin-style glow retake possible.
- [x] M23: house theme — Yin-style baked glow backdrop (Stack), steel
  blue buttons (#2b3f52), Space Grotesk (Regular+Bold loaded, Bold
  default; `with_name` weight gotcha fixed). 44 core tests + UI compile.
  LIVE-VERIFIED visually on CachyOS.
- [x] M14: msix — REMOVED 2026-09-24 (Windows rejects signed installs
  at GetManifestReader despite everything locally verifiable being
  correct; better nothing than frustration).
- [x] M15: compat audit (CachyOS/pacman, antiX/apt, Fedora Server/dnf):
  dpkg (pacman+apt, dnf none), rpm (apt+dnf, pacman none — rpm-tools
  is Cachy-overlay only), makepkg (Arch-only), xbps (recipe-only
  everywhere), appimage (download everywhere), exe (AUR/apt, dnf
  none), msi (msitools everywhere), flatpak-builder (everywhere).
  AntiX slim repos lack rpm/nsis (auto-container there).
