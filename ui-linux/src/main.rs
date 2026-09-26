//! BundleForge Linux UI (Iced) — wizard: form → loading (animated forge +
//! progress + details + cancel) → results.
//! Quick = sync checks per tick. Build = background threads with cancellation.
use bundleforge_core::{
    builders::{build_format, package_format, BuildCtl},
    config::{self, Config},
    detectors, install, remote::{self, Remote}, tester, ProjectInfo, TestResult,
};
use iced::{
    event::{self, Event},
    keyboard::{self, key::Named, Key},
    time,
    widget::{
        button, checkbox, column, container, horizontal_space, image,
        progress_bar, row, scrollable, text, text_input,
        tooltip::{self, Tooltip},
    },
    Application, Command, Element, Length, Settings, Subscription, Theme,
};
use std::sync::{
    atomic::Ordering,
    mpsc::{self, Receiver},
    Arc,
};
use std::collections::HashMap;
use std::time::Duration;

const FORMATS: &[(&str, &str)] = &[
    ("deb", "Linux"),
    ("rpm", "Linux"),
    ("pacman", "Arch"),
    ("xbps", "Void (exp.)"),
    ("appimage", "Linux"),
    ("exe", "Windows"),
    ("msi", "Windows"),
    ("flatpak", "Linux"),
    ("snap", "Linux"),
    ("zip", "Portable"),
];

/// Badge PNG per format (generated, trademark-free).
#[allow(unreachable_patterns)]
fn fmt_icon(name: &str) -> &'static [u8] {
    match name {
        "deb" => include_bytes!("../../assets/formats/deb.png"),
        "rpm" => include_bytes!("../../assets/formats/rpm.png"),
        "pacman" => include_bytes!("../../assets/formats/pacman.png"),
        "xbps" => include_bytes!("../../assets/formats/xbps.png"),
        "appimage" => include_bytes!("../../assets/formats/appimage.png"),
        "exe" => include_bytes!("../../assets/formats/exe.png"),
        "msi" => include_bytes!("../../assets/formats/msi.png"),
        "flatpak" => include_bytes!("../../assets/formats/flatpak.png"),
        "zip" => include_bytes!("../../assets/formats/zip.png"),
        _ => include_bytes!("../../assets/formats/snap.png"),
    }
}

/// One-line card state (full text lives in Detail).
fn short_state(state: &CardState) -> String {
    match state {
        CardState::Ready => "ready".to_string(),
        CardState::ViaContainer => "container".to_string(),
        CardState::ViaRemote(r) => format!("remote {r}"),
        CardState::NeedDownload => "needs download".to_string(),
        CardState::Blocked => "blocked".to_string(),
    }
}

// NOTE: single source of truth for real builders is core::has_builder;
// always query it instead of hardcoding formats here.

// Title logo (user-provided PNG, not a font file). Swap TITLE to
// TITLE_ALT to try the alternative.
#[allow(dead_code)]
const TITLE: &[u8] = include_bytes!("../../assets/fonts/bundleforge.png");
const TITLE_ALT: &[u8] = include_bytes!("../../assets/fonts/bundleforgealternative.png");
// Anvil sprite frames (32x32), compiled into the binary.
const FRAME1: &[u8] = include_bytes!("../../assets/loading/frame1.png");
const FRAME2: &[u8] = include_bytes!("../../assets/loading/frame2.png");

fn window_icon() -> Option<iced::window::Icon> {
    iced::window::icon::from_file_data(include_bytes!("../../assets/logo/BundleForge.png"), None)
        .ok()
}

fn main() -> iced::Result {
    Ui::run(Settings {
        // App ID: en Wayland el icono sale del .desktop que coincida con esto.
        id: Some(String::from("bundleforge")),
        window: iced::window::Settings {
            size: iced::Size::new(690.0, 707.0),
            // Fixed layout by design (all views target ~680px): maximizing
            // only adds empty space. Revert to resizable if views go fluid.
            resizable: false,
            icon: window_icon(),
            ..Default::default()
        },
        ..Settings::default()
    })
}

#[derive(Debug, Clone)]
enum Step {
    Scan,
    Check(String),
    Build(String),
    Package(String),
    Install(String),
    InstallRuntime,
    RemotePackage { format: String, remote: String },
}

/// House button: default (primary) look with rounder corners.
/// One constructor for all 28 buttons instead of per-call styling.
struct Pill;
impl iced::widget::button::StyleSheet for Pill {
    type Style = Theme;
    fn active(&self, style: &Self::Style) -> iced::widget::button::Appearance {
        // Swapped by design: resting shows the hover tone, hovering drops
        // to the resting tone.
        let base = style.hovered(&iced::theme::Button::Primary);
        iced::widget::button::Appearance {
            border: iced::Border {
                radius: 9.0.into(),
                ..base.border
            },
            ..base
        }
    }
    fn hovered(&self, style: &Self::Style) -> iced::widget::button::Appearance {
        let base = style.active(&iced::theme::Button::Primary);
        iced::widget::button::Appearance {
            border: iced::Border {
                radius: 9.0.into(),
                ..base.border
            },
            ..base
        }
    }
    fn pressed(&self, style: &Self::Style) -> iced::widget::button::Appearance {
        let base = style.pressed(&iced::theme::Button::Primary);
        iced::widget::button::Appearance {
            border: iced::Border {
                radius: 9.0.into(),
                ..base.border
            },
            ..base
        }
    }
    fn disabled(&self, style: &Self::Style) -> iced::widget::button::Appearance {
        let base = style.disabled(&iced::theme::Button::Primary);
        iced::widget::button::Appearance {
            border: iced::Border {
                radius: 9.0.into(),
                ..base.border
            },
            ..base
        }
    }
}

fn pill<'a>(
    content: impl Into<iced::Element<'a, Message>>,
) -> iced::widget::Button<'a, Message> {
    button(content).style(iced::theme::Button::Custom(Box::new(Pill)))
}

/// Hover tip for a button, only when the user kept them enabled
/// (`show_button_tips` in config). No delay API in iced 0.12, so tips
/// appear on hover immediately. Solid background (default is transparent).
fn tip<'a>(
    btn: iced::widget::Button<'a, Message>,
    s: &'static str,
    on: bool,
) -> Element<'a, Message> {
    if on {
        Tooltip::new(btn, text(s).size(12), tooltip::Position::Top)
            .style(iced::theme::Container::Custom(Box::new(TipBg)))
            .into()
    } else {
        btn.into()
    }
}

/// Solid background for tooltips (house dark, rounded).
struct TipBg;
impl container::StyleSheet for TipBg {
    type Style = Theme;
    fn appearance(&self, _style: &Self::Style) -> container::Appearance {
        container::Appearance {
            background: Some(iced::Color::from_rgb8(0x26, 0x26, 0x2A).into()),
            border: iced::Border {
                color: iced::Color::from_rgb8(0x55, 0x55, 0x5A).into(),
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        }
    }
}

struct Loading {
    steps: Vec<Step>,
    done: usize,
    log: Vec<String>,
    frame: usize,
    frames: [image::Handle; 2],
    show_detail: bool,
    path: String,
    out_dir: String,
    info: Option<ProjectInfo>,
    results: Vec<TestResult>,
    rx: Option<Receiver<(TestResult, bool)>>,
    ctl: Arc<BuildCtl>,
    cancelled: bool,
    pwd_open: bool,
    upfront_pw: String,
}

enum View {
    Store,
    Detail {
        format: String,
    },
    Config,
    Loading(Loading),
    Results {
        info: ProjectInfo,
        results: Vec<TestResult>,
        log: Vec<String>,
    },
    Remotes {
        back: RemoteBack,
    },
}

/// Where the Remotes view returns to.
#[derive(Debug, Clone)]
enum RemoteBack {
    Store,
    Detail {
        format: String,
    },
}

/// Capability state of one format card. Computed live from gates +
/// container runtime + configured remotes (always fresh, plus an
/// explicit re-check button).
#[derive(Debug, Clone, PartialEq)]
enum CardState {
    Ready,
    ViaContainer,
    ViaRemote(String),
    NeedDownload,
    Blocked,
}

struct Ui {
    path_input: String,
    out_dir: String,
    info: Option<ProjectInfo>,
    view: View,
    note: String,
    install_pwd: String,
    pwd_input: String,
    detail_status: String,
    pending_isolated: Option<String>,
    last_err: HashMap<String, String>,
    remotes: Vec<Remote>,
    r_name: String,
    r_host: String,
    r_user: String,
    r_os: String,
    remote_note: String,
    remote_rx: Option<Receiver<(String, bool)>>,
    remote_testing: Option<String>,
    remote_ctl: Option<Arc<BuildCtl>>,
    config: Config,
}

impl Default for Ui {
    fn default() -> Self {
        // Config loads here so last folders prefill the inputs (unless
        // that folder is session-only); missing or corrupt file =
        // defaults, never a crash (core guarantee).
        let config = config::load();
        Self {
            path_input: if config.persist_project_folder {
                config.last_project_folder.clone()
            } else {
                String::new()
            },
            out_dir: if config.persist_output_folder {
                config.last_output_folder.clone()
            } else {
                String::new()
            },
            info: None,
            view: View::Store,
            note: String::new(),
            install_pwd: String::new(),
            pwd_input: String::new(),
            detail_status: String::new(),
            pending_isolated: None,
            last_err: HashMap::new(),
            remotes: Vec::new(),
            r_name: String::new(),
            r_host: String::new(),
            r_user: String::new(),
            r_os: String::from("linux"),
            remote_note: String::new(),
            remote_rx: None,
            remote_testing: None,
            remote_ctl: None,
            config,
        }
    }
}

#[derive(Debug, Clone)]
enum Message {
    PathChanged(String),
    OutDirChanged(String),
    RefreshStore,
    OpenDetail(String),
    BrowseProject,
    BrowseOut,
    DetailTest(String),
    DetailPackage(String),
    DetailIsolated(String),
    RunQuick,
    RunBuild,
    InstallPwdChanged(String),
    DoInstall(String),
    DoInstallRuntime,
    RemotePackage { format: String, remote: String },
    ShowRemotes,
    ShowRemotesFromDetail,
    RemoteBack,
    RNameChanged(String),
    RHostChanged(String),
    RUserChanged(String),
    ROsChanged(String),
    AddRemote,
    RemoveRemote(usize),
    TestRemote(usize),
    StopRemoteTest,
    RemoteTick,
    Tick,
    ToggleDetail,
    Cancel,
    BackToStore,
    PwdChanged(String),
    PwdSubmit,
    OpenConfig,
    CloseConfig,
    ConfigBrowseProject,
    ConfigBrowseOut,
    TipsToggled(bool),
    PersistProjectToggled(bool),
    PersistOutputToggled(bool),
    EscPressed,
}

/// All known formats (Store cards + Test/Build all).
fn selected_formats_all() -> Vec<String> {
    FORMATS.iter().map(|(f, _)| f.to_string()).collect()
}

/// Batch report: header + per-format lines + full console log.
/// Written for multi-format runs instead of a result screen.
/// Returns the written path.
fn write_report(
    base: &str,
    info: &ProjectInfo,
    results: &[TestResult],
    log: &[String],
) -> Result<String, String> {
    let dir = std::path::Path::new(if base.trim().is_empty() {
        info.path.as_str()
    } else {
        base
    });
    std::fs::create_dir_all(dir).map_err(|e| format!("report dir: {e}"))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("bundleforge-report-{stamp}.txt"));
    let mut out = format!(
        "BundleForge report — {} · {} {}\n\n",
        info.name, info.stack, info.version
    );
    for r in results {
        out.push_str(&format!("[{}] {} — {}\n", r.state, r.format, r.reason));
        if let Some(a) = &r.artifact_path {
            out.push_str(&format!("  artifact: {a}\n"));
        }
        if let Some(e) = &r.error_log {
            if !e.is_empty() {
                out.push_str(&format!("  error:\n{e}\n"));
            }
        }
    }
    out.push_str("\n--- LOG ---\n");
    for line in log {
        out.push_str(line);
        out.push('\n');
    }
    std::fs::write(&path, out).map_err(|e| format!("writing report: {e}"))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Capability of one format, computed live (gates + runtime + remotes).
fn capability(format: &str, remotes: &[Remote]) -> (CardState, String) {
    let gate = tester::check_format(&format.to_string());
    if gate.state == "SAFE" {
        return (CardState::Ready, format!("ready — {}", gate.reason));
    }
    if let Some(line) = bundleforge_core::container::container_option(format) {
        return (CardState::ViaContainer, format!("container-ready — {line}"));
    }
    if let Some(r) = remote::matching_remote(format, remotes) {
        return (
            CardState::ViaRemote(r.name.clone()),
            format!("via remote {} ({}@{})", r.name, r.user, r.host),
        );
    }
    if install::recipe_for(format).is_some() {
        return (CardState::NeedDownload, gate.reason.clone());
    }
    (CardState::Blocked, gate.reason.clone())
}

/// One-line install summary shared by Help and Detail views.
fn install_summary(recipe: &install::InstallRecipe) -> String {
    match &recipe.kind {
        install::RecipeKind::Packages {
            manager,
            packages,
            post_user,
            ..
        } => {
            let mut s = format!("Install with {manager}: {}", packages.join(" "));
            if !post_user.is_empty() {
                s.push_str(" (+ user-scope provisioning afterwards)");
            }
            s
        }
        install::RecipeKind::Download { url, .. } => {
            format!("Download from:\n{url}")
        }
        install::RecipeKind::SourceBuild {
            manager,
            dep_packages,
            git_url,
            ..
        } => {
            format!(
                "Build deps with {manager}: {}\nClone:\n{git_url}\n(then user-local build into ~/.local/bin)",
                dep_packages.join(" ")
            )
        }
        install::RecipeKind::AurBuild {
            helper, package, ..
        } => {
            format!("Fetch + build + install {package} via {helper} (AUR, pkexec GUI for install)")
        }
    }
}

/// Native folder picker without heavy deps: kdialog → zenity → qarma.
/// Blocks briefly while the dialog is open. None when unavailable
/// (user types the path) or the dialog is cancelled.
fn browse_folder() -> Option<String> {
    let attempts: &[(&str, &[&str])] = &[
        ("kdialog", &["--getexistingdirectory", "--title", "Choose folder"]),
        (
            "zenity",
            &["--file-selection", "--directory", "--title=Choose folder"],
        ),
        (
            "qarma",
            &["--file-selection", "--directory", "--title", "Choose folder"],
        ),
    ];
    for (prog, args) in attempts {
        if !detectors::tool_exists(prog) {
            continue;
        }
        if let Ok(out) = std::process::Command::new(prog).args(*args).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

impl Application for Ui {
    type Executor = iced::executor::Default;
    type Message = Message;
    type Theme = Theme;
    type Flags = ();

    fn new(_flags: ()) -> (Self, Command<Message>) {
        (Self::default(), Command::none())
    }

    fn title(&self) -> String {
        String::from("BundleForge")
    }

    fn subscription(&self) -> Subscription<Message> {
        // ESC closes the config modal (global, cheap: only ESC produces
        // a message, everything else is filtered out here).
        let keys = event::listen_with(|e, _status| match e {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: Key::Named(Named::Escape),
                ..
            }) => Some(Message::EscPressed),
            _ => None,
        });
        match &self.view {
            View::Loading(_) => Subscription::batch(vec![
                keys,
                time::every(Duration::from_millis(200)).map(|_| Message::Tick),
            ]),
            View::Remotes { .. } if self.remote_rx.is_some() => Subscription::batch(vec![
                keys,
                time::every(Duration::from_millis(300)).map(|_| Message::RemoteTick),
            ]),
            _ => keys,
        }
    }

    fn update(&mut self, msg: Message) -> Command<Message> {
        match msg {
            Message::PathChanged(s) => {
                self.path_input = s;
                self.sync_config();
            }
            Message::OutDirChanged(s) => {
                self.out_dir = s;
                self.sync_config();
            }
            Message::RefreshStore => {
                // No-op by design: every processed message re-renders, and
                // card states probe live — so this IS the re-check.
            }
            Message::RunQuick | Message::RunBuild => {
                let mode = if matches!(msg, Message::RunQuick) {
                    "quick"
                } else {
                    "build"
                };
                let p = self.path_input.trim();
                if p.is_empty() {
                    self.note = String::from("paste the project folder");
                    return Command::none();
                }
                let formats = selected_formats_all();
                if formats.is_empty() {
                    self.note = String::from("no formats known");
                    return Command::none();
                }
                self.note.clear();
                self.start_loading(formats, mode);
            }
            Message::Tick => {
                let mut finished: Option<(ProjectInfo, Vec<TestResult>, Vec<String>)> = None;
                if let View::Loading(st) = &mut self.view {
                    st.frame = (st.frame + 1) % 2;
                    // Password dialog follows the worker flag (mid-run sudo prompt).
                    if !st.pwd_open && st.ctl.password_needed.load(Ordering::SeqCst) {
                        st.pwd_open = true;
                    }
                    if st.pwd_open && !st.ctl.password_needed.load(Ordering::SeqCst) {
                        st.pwd_open = false;
                    }
                    if st.cancelled {
                        let info = st.info.clone().unwrap_or(ProjectInfo {
                            path: st.path.clone(),
                            stack: "unknown".into(),
                            name: "project".into(),
                            version: "0.0.0".into(),
                        });
                        finished = Some((info, std::mem::take(&mut st.results), std::mem::take(&mut st.log)));
                    } else if st.done < st.steps.len() {
                        match st.steps[st.done].clone() {
                            Step::Scan => {
                                let (stack, name, version) =
                                    detectors::detect_stack(std::path::Path::new(&st.path));
                                st.log.push(format!("stack={stack} name={name} v{version}"));
                                st.info = Some(ProjectInfo {
                                    path: st.path.clone(),
                                    stack,
                                    name,
                                    version,
                                });
                                st.done += 1;
                            }
                            Step::Check(fmt) => {
                                st.log.push(format!("$ check {fmt}"));
                                let r = tester::check_format(&fmt);
                                st.log.push(format!(
                                    "{} — {}",
                                    r.state,
                                    if r.reason.is_empty() {
                                        "(ok)".to_string()
                                    } else {
                                        r.reason.clone()
                                    }
                                ));
                                st.results.push(r);
                                st.done += 1;
                            }
                            Step::InstallRuntime => {
                                if st.rx.is_none() {
                                    st.log.push("$ install podman (running…)".to_string());
                                    let (tx, rx) = mpsc::channel();
                                    st.rx = Some(rx);
                                    let ctl = st.ctl.clone();
                                    std::thread::spawn(move || {
                                        let r = match install::install_runtime(&ctl) {
                                            Ok(log) => TestResult {
                                                format: "podman".to_string(),
                                                state: "SAFE".to_string(),
                                                reason: format!("installed. {log}"),
                                                test_type: "install".to_string(),
                                                success: Some(true),
                                                error_log: None,
                                                command_output: None,
                                                artifact_path: None,
                                            },
                                            Err(e) => TestResult {
                                                format: "podman".to_string(),
                                                state: "MEH".to_string(),
                                                reason: e,
                                                test_type: "install".to_string(),
                                                success: Some(false),
                                                error_log: None,
                                                command_output: None,
                                                artifact_path: None,
                                            },
                                        };
                                        let _ = tx.send((r, true));
                                    });
                                } else {
                                    use std::sync::mpsc::TryRecvError;
                                    match st.rx.as_ref().map(|rx| rx.try_recv()) {
                                        Some(Ok((r, _))) => {
                                            st.log.push(format!(
                                                "{} — {}",
                                                r.state,
                                                if r.reason.is_empty() {
                                                    "(ok)".to_string()
                                                } else {
                                                    r.reason.clone()
                                                }
                                            ));
                                            let ok_install = r.success == Some(true);
                                            st.results.push(r);
                                            st.rx = None;
                                            st.done += 1;
                                            if ok_install {
                                                st.log.push(
                                                    "installed, re-testing…".to_string(),
                                                );
                                                st.steps = vec![Step::Scan];
                                                st.steps.extend(
                                                    selected_formats_all()
                                                        .into_iter()
                                                        .map(Step::Check),
                                                );
                                                st.done = 0;
                                                st.results.clear();
                                            }
                                        }
                                        Some(Err(TryRecvError::Empty)) => {}
                                        Some(Err(TryRecvError::Disconnected)) => {
                                            st.log.push("build thread died".to_string());
                                            st.results.push(TestResult {
                                                format: "podman".to_string(),
                                                state: "MEH".to_string(),
                                                reason: "build thread died".to_string(),
                                                test_type: "normal".to_string(),
                                                success: Some(false),
                                                error_log: None,
                                                command_output: None,
                                                artifact_path: None,
                                            });
                                            st.rx = None;
                                            st.done += 1;
                                        }
                                        None => {}
                                    }
                                }
                            }
                            Step::RemotePackage { format, remote } => {
                                if st.rx.is_none() {
                                    st.log.push(format!(
                                        "$ remote package {format} on {remote} (running…)"
                                    ));
                                    let (tx, rx) = mpsc::channel();
                                    st.rx = Some(rx);
                                    let ctl = st.ctl.clone();
                                    let path = st.path.clone();
                                    let out_dir = st.out_dir.clone();
                                    let info = st.info.clone().unwrap_or(ProjectInfo {
                                        path: st.path.clone(),
                                        stack: "unknown".into(),
                                        name: "project".into(),
                                        version: "0.0.0".into(),
                                    });
                                    std::thread::spawn(move || {
                                        let rem = remote::load_remotes()
                                            .into_iter()
                                            .find(|x| x.name == remote);
                                        let r = match rem {
                                            Some(rem) => remote::package_remote_format(
                                                &path,
                                                &info.name,
                                                &info.version,
                                                &format,
                                                &rem,
                                                std::path::Path::new(&out_dir),
                                                &ctl,
                                            ),
                                            None => TestResult {
                                                format: format.clone(),
                                                state: "MEH".to_string(),
                                                reason: "remote removed".to_string(),
                                                test_type: "package".to_string(),
                                                success: Some(false),
                                                error_log: None,
                                                command_output: None,
                                                artifact_path: None,
                                            },
                                        };
                                        let _ = tx.send((r, false));
                                    });
                                } else {
                                    use std::sync::mpsc::TryRecvError;
                                    match st.rx.as_ref().map(|rx| rx.try_recv()) {
                                        Some(Ok((r, _))) => {
                                            st.log.push(format!(
                                                "{} — {}",
                                                r.state,
                                                if r.reason.is_empty() {
                                                    "(ok)".to_string()
                                                } else {
                                                    r.reason.clone()
                                                }
                                            ));
                                            st.results.push(r);
                                            st.rx = None;
                                            st.done += 1;
                                        }
                                        Some(Err(TryRecvError::Empty)) => {}
                                        Some(Err(TryRecvError::Disconnected)) => {
                                            st.log.push("build thread died".to_string());
                                            st.results.push(TestResult {
                                                format: format.clone(),
                                                state: "MEH".to_string(),
                                                reason: "build thread died".to_string(),
                                                test_type: "package".to_string(),
                                                success: Some(false),
                                                error_log: None,
                                                command_output: None,
                                                artifact_path: None,
                                            });
                                            st.rx = None;
                                            st.done += 1;
                                        }
                                        None => {}
                                    }
                                }
                            }
                            Step::Build(fmt) | Step::Package(fmt) | Step::Install(fmt) => {
                                let kind = if matches!(st.steps[st.done], Step::Package(_)) {
                                    "package"
                                } else if matches!(st.steps[st.done], Step::Install(_)) {
                                    "install"
                                } else {
                                    "build"
                                };
                                if st.rx.is_none() {
                                    // Launches the build on a thread; following ticks poll it.
                                    st.log.push(format!("$ {kind} {fmt} (running…)"));
                                    let (tx, rx) = mpsc::channel();
                                    st.rx = Some(rx);
                                    let ctl = st.ctl.clone();
                                    let path = st.path.clone();
                                    let out_dir = st.out_dir.clone();
                                    let info = st.info.clone().unwrap_or(ProjectInfo {
                                        path: st.path.clone(),
                                        stack: "unknown".into(),
                                        name: "project".into(),
                                        version: "0.0.0".into(),
                                    });
                                    let install_mode = kind == "install";
                                    let upfront_pw = st.upfront_pw.clone();
                                    std::thread::spawn(move || {
                                        let r = if install_mode {
                                            // 1) Fresh timestamp already? No password needed.
                                            // 2) Typed password? Pre-auth now — fail FAST
                                            //    on typo instead of dying minutes later.
                                            // 3) AUR recipe but no password and stale?
                                            //    Refuse: yay's sudo dies instantly w/o TTY.
                                            let mut authed = install::sudo_fresh();
                                            if !authed && upfront_pw.is_empty() {
                                                let needs_pw = install::recipe_for(&fmt)
                                                    .map(|r| r.needs_password)
                                                    .unwrap_or(false);
                                                if needs_pw {
                                                    let _ = tx.send((
                                                        TestResult {
                                                            format: fmt.clone(),
                                                            state: "MEH".to_string(),
                                                            reason: "type your sudo password first (AUR needs it upfront)".to_string(),
                                                            test_type: "install".to_string(),
                                                            success: Some(false),
                                                            error_log: None,
                                                            command_output: None,
                                                            artifact_path: None,
                                                        },
                                                        true,
                                                    ));
                                                    return;
                                                }
                                            }
                                            let mut authed = install::sudo_fresh();
                                            if !authed && !upfront_pw.is_empty() {
                                                match install::sudo_preauth(&upfront_pw, &ctl) {
                                                    Ok(()) => authed = true,
                                                    Err(e) => {
                                                        let _ = tx.send((
                                                            TestResult {
                                                                format: fmt.clone(),
                                                                state: "MEH".to_string(),
                                                                reason: e,
                                                                test_type: "install".to_string(),
                                                                success: Some(false),
                                                                error_log: None,
                                                                command_output: None,
                                                                artifact_path: None,
                                                            },
                                                            true,
                                                        ));
                                                        return;
                                                    }
                                                }
                                            }
                                            // 4) Refresher: re-warm the timestamp every 4 min
                                            // while the install runs (AUR builds outlive it).
                                            let stop =
                                                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                                                    false,
                                                ));
                                            let refresher = if !upfront_pw.is_empty() {
                                                let stop_c = stop.clone();
                                                let ctl_c = ctl.clone();
                                                let pw = upfront_pw.clone();
                                                Some(std::thread::spawn(move || {
                                                    use std::time::Duration;
                                                    loop {
                                                        std::thread::sleep(Duration::from_secs(240));
                                                        if stop_c.load(
                                                            std::sync::atomic::Ordering::SeqCst,
                                                        ) || BuildCtl::cancelled(&ctl_c)
                                                        {
                                                            break;
                                                        }
                                                        let _ = install::sudo_preauth(&pw, &ctl_c);
                                                    }
                                                }))
                                            } else {
                                                None
                                            };
                                            let out = match install::install_format(&fmt, &ctl) {
                                                Ok(log) => TestResult {
                                                    format: fmt.clone(),
                                                    state: "SAFE".to_string(),
                                                    reason: format!("installed. {log}"),
                                                    test_type: "install".to_string(),
                                                    success: Some(true),
                                                    error_log: None,
                                                    command_output: None,
                                                    artifact_path: None,
                                                },
                                                Err(e) => TestResult {
                                                    format: fmt.clone(),
                                                    state: "MEH".to_string(),
                                                    reason: e,
                                                    test_type: "install".to_string(),
                                                    success: Some(false),
                                                    error_log: None,
                                                    command_output: None,
                                                    artifact_path: None,
                                                },
                                            };
                                            stop.store(true, std::sync::atomic::Ordering::SeqCst);
                                            drop(refresher);
                                            let _ = authed;
                                            out
                                        } else if kind == "package" {
                                            package_format(
                                                &path,
                                                &info.name,
                                                &info.version,
                                                &fmt,
                                                std::path::Path::new(&out_dir),
                                                &ctl,
                                            )
                                        } else {
                                            build_format(
                                                &path, &info.name, &info.version, &fmt, &ctl,
                                            )
                                        };
                                        let _ = tx.send((r, install_mode));
                                    });
                                } else {
                                    use std::sync::mpsc::TryRecvError;
                                    match st.rx.as_ref().map(|rx| rx.try_recv()) {
                                        Some(Ok((r, was_install))) => {
                                            st.log.push(format!(
                                                "{} — {}",
                                                r.state,
                                                if r.reason.is_empty() {
                                                    "(ok)".to_string()
                                                } else {
                                                    r.reason.clone()
                                                }
                                            ));
                                            let ok_install =
                                                was_install && r.success == Some(true);
                                            if ok_install {
                                                self.install_pwd.clear();
                                            }
                                            st.results.push(r);
                                            st.rx = None;
                                            st.done += 1;
                                            if ok_install {
                                                // Tool installed: auto re-test everything checked.
                                                st.log.push(
                                                    "installed, re-testing…".to_string(),
                                                );
                                                st.steps = vec![Step::Scan];
                                                st.steps.extend(
                                                    selected_formats_all()
                                                        .into_iter()
                                                        .map(Step::Check),
                                                );
                                                st.done = 0;
                                                st.results.clear();
                                            }
                                        }
                                        // Still working: just animate, touch nothing.
                                        Some(Err(TryRecvError::Empty)) => {}
                                        Some(Err(TryRecvError::Disconnected)) => {
                                            st.log.push("build thread died".to_string());
                                            st.results.push(TestResult {
                                                format: fmt.clone(),
                                                state: "MEH".to_string(),
                                                reason: "build thread died".to_string(),
                                                test_type: "normal".to_string(),
                                                success: Some(false),
                                                error_log: None,
                                                command_output: None,
                                                artifact_path: None,
                                            });
                                            st.rx = None;
                                            st.done += 1;
                                        }
                                        None => {}
                                    }
                                }
                            }
                        }
                        if st.done >= st.steps.len() {
                            let info = st.info.clone().unwrap_or(ProjectInfo {
                                path: st.path.clone(),
                                stack: "unknown".into(),
                                name: "project".into(),
                                version: "0.0.0".into(),
                            });
                            finished = Some((info, std::mem::take(&mut st.results), std::mem::take(&mut st.log)));
                        }
                    }
                }
                if let Some((info, results, log)) = finished {
                    // Any finished run clears a transient force-container
                    // flag (the Isolated action sets it for one run only).
                    bundleforge_core::container::set_force_container(false);
                    // Feed the store's red retry lines (cleared on success).
                    for r in &results {
                        if r.success == Some(false) {
                            let line = r.reason.lines().next().unwrap_or("failed").to_string();
                            self.last_err.insert(r.format.clone(), line);
                        } else if r.success == Some(true) {
                            self.last_err.remove(&r.format);
                        }
                    }
                    self.info = Some(info.clone());
                    if results.len() > 1 {
                        // Batch runs report to a .txt file and return home;
                        // single runs get the result screen.
                        let base = if self.out_dir.trim().is_empty() {
                            info.path.clone()
                        } else {
                            self.out_dir.trim().to_string()
                        };
                        match write_report(&base, &info, &results, &log) {
                            Ok(p) => {
                                self.note = format!("report saved: {p}");
                            }
                            Err(e) => {
                                self.note = format!("report failed: {e}");
                            }
                        }
                        self.view = View::Store;
                    } else {
                        self.view = View::Results { info, results, log };
                    }
                }
            }
            Message::ToggleDetail => {
                if let View::Loading(st) = &mut self.view {
                    st.show_detail = !st.show_detail;
                }
            }
            Message::Cancel => {
                if let View::Loading(st) = &mut self.view {
                    st.cancelled = true;
                    st.ctl.cancel.store(true, Ordering::SeqCst);
                    st.log.push("cancelling…".to_string());
                }
            }
            Message::PwdChanged(s) => {
                self.pwd_input = s;
            }
            Message::PwdSubmit => {
                if let View::Loading(st) = &mut self.view {
                    let pw = std::mem::take(&mut self.pwd_input);
                    if let Ok(mut g) = st.ctl.password.lock() {
                        *g = Some(pw);
                    }
                    st.ctl.password_needed.store(false, Ordering::SeqCst);
                    st.pwd_open = false;
                    st.log.push("password sent (memory only, never logged)".to_string());
                }
            }
            Message::BackToStore => {
                self.install_pwd.clear();
                self.pending_isolated = None;
                self.view = View::Store;
            }
            Message::OpenDetail(format) => {
                self.detail_status.clear();
                self.pending_isolated = None;
                self.view = View::Detail { format };
            }
            Message::BrowseProject => {
                match browse_folder() {
                    Some(p) => {
                        self.path_input = p.clone();
                        self.sync_config();
                        let (stack, name, version) =
                            detectors::detect_stack(std::path::Path::new(&p));
                        self.detail_status = format!("{name} · {stack} {version}");
                    }
                    None => {
                        self.detail_status =
                            String::from("no folder chosen (no file dialog found?)");
                    }
                }
            }
            Message::BrowseOut => {
                if let Some(p) = browse_folder() {
                    self.out_dir = p;
                    self.sync_config();
                }
            }
            Message::OpenConfig => {
                self.view = View::Config;
            }
            Message::CloseConfig => {
                self.view = View::Store;
            }
            Message::EscPressed => {
                if matches!(self.view, View::Config) {
                    self.view = View::Store;
                }
            }
            Message::ConfigBrowseProject => {
                if let Some(p) = browse_folder() {
                    self.path_input = p;
                    self.sync_config();
                }
            }
            Message::ConfigBrowseOut => {
                if let Some(p) = browse_folder() {
                    self.out_dir = p;
                    self.sync_config();
                }
            }
            Message::TipsToggled(b) => {
                self.config.show_button_tips = b;
                let _ = config::save(&self.config);
            }
            Message::PersistProjectToggled(b) => {
                self.config.persist_project_folder = b;
                if b {
                    self.config.last_project_folder = self.path_input.clone();
                } else {
                    self.config.last_project_folder.clear();
                }
                let _ = config::save(&self.config);
            }
            Message::PersistOutputToggled(b) => {
                self.config.persist_output_folder = b;
                if b {
                    self.config.last_output_folder = self.out_dir.clone();
                } else {
                    self.config.last_output_folder.clear();
                }
                let _ = config::save(&self.config);
            }
            Message::DetailTest(fmt) => {
                if self.path_input.trim().is_empty() {
                    self.detail_status = String::from("choose a project folder first");
                    return Command::none();
                }
                self.start_loading(vec![fmt], "build");
            }
            Message::DetailPackage(fmt) => {
                if self.path_input.trim().is_empty() {
                    self.detail_status = String::from("choose a project folder first");
                    return Command::none();
                }
                if self.out_dir.trim().is_empty() {
                    self.detail_status = String::from("choose an output folder first");
                    return Command::none();
                }
                self.start_loading(vec![fmt], "package");
            }
            Message::DetailIsolated(fmt) => {
                if self.path_input.trim().is_empty() {
                    self.detail_status = String::from("choose a project folder first");
                    return Command::none();
                }
                if self.out_dir.trim().is_empty() {
                    self.detail_status = String::from("choose an output folder first");
                    return Command::none();
                }
                if bundleforge_core::container::container_runtime().is_none() {
                    self.detail_status = String::from(
                        "isolated needs podman/docker first (use Install podman on this page when offered)",
                    );
                    return Command::none();
                }
                if bundleforge_core::container::container_spec(&fmt).is_none() {
                    self.detail_status = format!(
                        "isolated unavailable for {fmt} (no image: nested sandbox or SDK rebuild)"
                    );
                    return Command::none();
                }
                // Two-step confirm when the image is NOT cached: the first
                // Isolated run downloads it (hundreds of MB). Show the real
                // free space; the second press confirms.
                let rt =
                    bundleforge_core::container::container_runtime().unwrap_or_default();
                let image = bundleforge_core::container::container_spec(&fmt)
                    .map(|s| s.image.to_string())
                    .unwrap_or_default();
                if !bundleforge_core::container::image_present(&rt, &image)
                    && self.pending_isolated.as_deref() != Some(&fmt)
                {
                    self.pending_isolated = Some(fmt.clone());
                    let anchor = if self.out_dir.trim().is_empty() {
                        self.path_input.trim()
                    } else {
                        self.out_dir.trim()
                    };
                    let free = std::path::Path::new(anchor);
                    let free_s =
                        match bundleforge_core::container::disk_free_kb(free) {
                            Some(kb) if kb >= 1024 * 1024 => {
                                format!("{:.1} GB free", kb as f64 / (1024.0 * 1024.0))
                            }
                            Some(kb) => format!("{} MB free", kb / 1024),
                            None => "unknown free space".to_string(),
                        };
                    self.detail_status = format!(
                        "image {image} not cached: first run downloads it (hundreds of MB, {free_s} here). Press Isolated again to confirm."
                    );
                    return Command::none();
                }
                self.pending_isolated = None;
                bundleforge_core::container::set_force_container(true);
                self.start_loading(vec![fmt], "package");
            }
            Message::DoInstall(fmt) => {
                let p = self.path_input.trim();
                // Clone (don't clear): on pre-auth failure the field keeps
                // the password so the user just fixes the typo and retries.
                // Cleared on success and when leaving the views below.
                let pw = self.install_pwd.clone();
                self.view = View::Loading(Loading {
                    steps: vec![Step::Install(fmt)],
                    done: 0,
                    log: vec![format!("$ install tool")],
                    frame: 0,
                    frames: [
                        image::Handle::from_memory(FRAME1.to_vec()),
                        image::Handle::from_memory(FRAME2.to_vec()),
                    ],
                    show_detail: false,
                    path: p.to_string(),
                    out_dir: self.out_dir.clone(),
                    info: self.info.clone(),
                    results: Vec::new(),
                    rx: None,
                    ctl: BuildCtl::fresh(),
                    cancelled: false,
                    pwd_open: false,
                    upfront_pw: pw,
                });
            }
            Message::DoInstallRuntime => {
                let p = self.path_input.trim();
                self.view = View::Loading(Loading {
                    steps: vec![Step::InstallRuntime],
                    done: 0,
                    log: vec!["$ install podman (container runtime)".to_string()],
                    frame: 0,
                    frames: [
                        image::Handle::from_memory(FRAME1.to_vec()),
                        image::Handle::from_memory(FRAME2.to_vec()),
                    ],
                    show_detail: false,
                    path: p.to_string(),
                    out_dir: self.out_dir.clone(),
                    info: self.info.clone(),
                    results: Vec::new(),
                    rx: None,
                    ctl: BuildCtl::fresh(),
                    cancelled: false,
                    pwd_open: false,
                    upfront_pw: String::new(),
                });
            }
            Message::RemotePackage { format, remote } => {
                if self.out_dir.trim().is_empty() {
                    self.note = String::from("choose an output folder first");
                    return Command::none();
                }
                let p = self.path_input.trim();
                self.view = View::Loading(Loading {
                    steps: vec![Step::RemotePackage {
                        format: format.clone(),
                        remote: remote.clone(),
                    }],
                    done: 0,
                    log: vec![format!("$ remote package {format} on {remote}")],
                    frame: 0,
                    frames: [
                        image::Handle::from_memory(FRAME1.to_vec()),
                        image::Handle::from_memory(FRAME2.to_vec()),
                    ],
                    show_detail: false,
                    path: p.to_string(),
                    out_dir: self.out_dir.clone(),
                    info: self.info.clone(),
                    results: Vec::new(),
                    rx: None,
                    ctl: BuildCtl::fresh(),
                    cancelled: false,
                    pwd_open: false,
                    upfront_pw: String::new(),
                });
            }
            Message::InstallPwdChanged(s) => {
                self.install_pwd = s;
            }
            Message::ShowRemotes => {
                self.remotes = remote::load_remotes();
                self.remote_note.clear();
                self.view = View::Remotes { back: RemoteBack::Store };
            }
            Message::ShowRemotesFromDetail => {
                let back = if let View::Detail { format } = &self.view {
                    RemoteBack::Detail {
                        format: format.clone(),
                    }
                } else {
                    RemoteBack::Store
                };
                self.remotes = remote::load_remotes();
                self.remote_note.clear();
                self.view = View::Remotes { back };
            }
            Message::RemoteBack => {
                self.remote_rx = None;
                self.remote_testing = None;
                self.remote_ctl = None;
                if let View::Remotes { back } = &self.view {
                    match back {
                        RemoteBack::Store => self.view = View::Store,
                        RemoteBack::Detail { format } => {
                            self.view = View::Detail {
                                format: format.clone(),
                            };
                        }
                    }
                } else {
                    self.view = View::Store;
                }
            }
            Message::RNameChanged(s) => self.r_name = s,
            Message::RHostChanged(s) => self.r_host = s,
            Message::RUserChanged(s) => self.r_user = s,
            Message::ROsChanged(s) => self.r_os = s,
            Message::AddRemote => {
                let r = Remote::new(&self.r_name, &self.r_host, &self.r_user, &self.r_os);
                if !r.valid() {
                    self.remote_note =
                        String::from("fill name/host/user, os linux|windows");
                } else if self.remotes.iter().any(|e| e.name == r.name) {
                    self.remote_note = format!("a remote named '{}' exists", r.name);
                } else {
                    self.remotes.push(r);
                    match remote::save_remotes(&self.remotes) {
                        Ok(()) => {
                            self.remote_note = String::from("saved");
                            self.r_name.clear();
                            self.r_host.clear();
                            self.r_user.clear();
                        }
                        Err(e) => self.remote_note = e,
                    }
                }
            }
            Message::RemoveRemote(i) => {
                if i < self.remotes.len() {
                    self.remotes.remove(i);
                    self.remote_note = match remote::save_remotes(&self.remotes) {
                        Ok(()) => String::from("removed"),
                        Err(e) => e,
                    };
                }
            }
            Message::TestRemote(i) => {
                if self.remote_rx.is_some() {
                    // One probe at a time.
                } else if let Some(r) = self.remotes.get(i).cloned() {
                    let (tx, rx) = mpsc::channel();
                    self.remote_rx = Some(rx);
                    self.remote_testing = Some(r.name.clone());
                    self.remote_note = format!("probing {}…", r.target());
                    let ctl = BuildCtl::fresh();
                    self.remote_ctl = Some(ctl.clone());
                    std::thread::spawn(move || {
                        let out = match remote::test_remote(&r, &ctl) {
                            Ok(s) => (format!("OK: {s}"), true),
                            Err(e) => (format!("FAIL: {e}"), false),
                        };
                        let _ = tx.send(out);
                    });
                }
            }
            Message::StopRemoteTest => {
                if let Some(ctl) = &self.remote_ctl {
                    ctl.cancel.store(true, Ordering::SeqCst);
                }
                self.remote_rx = None;
                self.remote_testing = None;
                self.remote_ctl = None;
                self.remote_note = String::from("stopped");
            }
            Message::RemoteTick => {
                use std::sync::mpsc::TryRecvError;
                match self.remote_rx.as_ref().map(|rx| rx.try_recv()) {
                    Some(Ok((msg, _))) => {
                        self.remote_note = msg;
                        self.remote_rx = None;
                        self.remote_testing = None;
                        self.remote_ctl = None;
                    }
                    Some(Err(TryRecvError::Empty)) => {}
                    Some(Err(TryRecvError::Disconnected)) => {
                        self.remote_note = String::from("test thread died");
                        self.remote_rx = None;
                        self.remote_testing = None;
                        self.remote_ctl = None;
                    }
                    None => {}
                }
            }
        }
        Command::none()
    }

    fn view(&self) -> Element<Message> {
        match &self.view {
            View::Store => self.view_store(),
            View::Detail { format } => self.view_detail(format),
            View::Config => self.view_config(),
            View::Loading(st) => self.view_loading(st),
            View::Results { info, results, log } => self.view_results(info, results, log),
            View::Remotes { .. } => self.view_remotes(),
        }
    }

    fn theme(&self) -> Theme {
        // House palette on Dark: background #151517, buttons #272727.
        // Every default (primary) button follows it, no per-button edits.
        Theme::custom(
            "BundleForge".to_string(),
            iced::theme::Palette {
                background: iced::Color::from_rgb8(0x15, 0x15, 0x17),
                primary: iced::Color::from_rgb8(0x27, 0x27, 0x27),
                ..Theme::Dark.palette()
            },
        )
    }
}

impl Ui {
    /// Folders live in the inputs AND in config.toml (written immediately,
    /// best effort) — but only when that folder is set to persist;
    /// session-only folders never touch the file.
    fn sync_config(&mut self) {
        if self.config.persist_project_folder {
            self.config.last_project_folder = self.path_input.clone();
        }
        if self.config.persist_output_folder {
            self.config.last_output_folder = self.out_dir.clone();
        }
        let _ = config::save(&self.config);
    }

    /// Shared run launcher (batch and per-format detail actions).
    /// Guards live at the call sites (note vs detail_status).
    fn start_loading(&mut self, formats: Vec<String>, kind: &str) {
        let p = self.path_input.trim().to_string();
        let mut steps = vec![Step::Scan];
        for f in formats {
            steps.push(match kind {
                "quick" => Step::Check(f),
                "build" => Step::Build(f),
                _ => Step::Package(f),
            });
        }
        self.view = View::Loading(Loading {
            steps,
            done: 0,
            log: vec![format!("$ scan {p}")],
            frame: 0,
            frames: [
                image::Handle::from_memory(FRAME1.to_vec()),
                image::Handle::from_memory(FRAME2.to_vec()),
            ],
            show_detail: false,
            path: p,
            out_dir: self.out_dir.clone(),
            info: None,
            results: Vec::new(),
            rx: None,
            ctl: BuildCtl::fresh(),
            cancelled: false,
            pwd_open: false,
            upfront_pw: String::new(),
        });
    }

    /// Config view (full page, like Detail): last folders (readonly
    /// display + Browse, persisted immediately) + tips toggle.
    fn view_config(&self) -> Element<Message> {
        let col = column![
            row![
                text("Settings").size(20),
                horizontal_space(),
                pill(text("X")).on_press(Message::CloseConfig),
            ]
            .align_items(iced::Alignment::Center),
            text("Project folder:").size(14),
            row![
                text_input("", &self.path_input).padding(8),
                pill("Browse").on_press(Message::ConfigBrowseProject),
            ]
            .spacing(10),
            text("Output folder:").size(14),
            row![
                text_input("", &self.out_dir).padding(8),
                pill("Browse").on_press(Message::ConfigBrowseOut),
            ]
            .spacing(10),
            checkbox("Show button tips", self.config.show_button_tips)
                .on_toggle(Message::TipsToggled),
            checkbox(
                "Remember project folder",
                self.config.persist_project_folder,
            )
            .on_toggle(Message::PersistProjectToggled),
            checkbox(
                "Remember output folder",
                self.config.persist_output_folder,
            )
            .on_toggle(Message::PersistOutputToggled),
            text("Unchecked = session only (starts empty next launch).").size(11),
        ]
        .spacing(12)
        .padding(20);
        let mut col = col;
        col = col.push(pill("Back").on_press(Message::CloseConfig));
        container(scrollable(col))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_store(&self) -> Element<Message> {
        let tips = self.config.show_button_tips;
        let mut col = column![
            row![
                image(image::Handle::from_memory(TITLE_ALT.to_vec()))
                    .width(Length::Fixed(300.0))
                    .height(Length::Fixed(68.0)),
                horizontal_space(),
                pill(
                    image(image::Handle::from_memory(
                        include_bytes!("../../assets/formats/config.png").to_vec(),
                    ))
                    .width(Length::Fixed(30.0))
                    .height(Length::Fixed(30.0)),
                )
                .on_press(Message::OpenConfig),
            ]
            .align_items(iced::Alignment::Center),
            text("Package only your own or trusted code: builds run project scripts (RCE by design).").size(11),
            text("Project folder:").size(14),
            row![
                text_input("/path/to/project", &self.path_input)
                    .on_input(Message::PathChanged)
                    .padding(8),
                pill("Browse").on_press(Message::BrowseProject),
            ]
            .spacing(10),
            text("Output folder (packages land here):").size(14),
            row![
                text_input("/path/to/output", &self.out_dir)
                    .on_input(Message::OutDirChanged)
                    .padding(8),
                pill("Browse").on_press(Message::BrowseOut),
            ]
            .spacing(10),
            row![
                tip(pill("Test all").on_press(Message::RunQuick),
                    "Runs a quick compatibility test on all 10 formats (a few seconds per format).",
                    tips),
                tip(pill("Build all").on_press(Message::RunBuild),
                    "Builds packages for all 10 formats in temp folders, verifies them, then auto-deletes. Nothing is saved to output.",
                    tips),
                tip(pill("Re-check").on_press(Message::RefreshStore),
                    "Re-detects available tools and dependencies for all formats. Use after installing something new.",
                    tips),
                tip(pill("Remotes").on_press(Message::ShowRemotes),
                    "Configure remote SSH builders for formats that can't be built natively on this machine.",
                    tips),
            ]
            .spacing(10),
            text(&self.note).size(13),
        ]
        .spacing(10)
        .padding(16);
        let remotes = remote::load_remotes();
        for chunk in FORMATS.chunks(4) {
            let mut r = row![].spacing(10);
            for (name, _os) in chunk {
                let (state, _line) = capability(name, &remotes);
                let icon =
                    image(image::Handle::from_memory(fmt_icon(name).to_vec()))
                        .width(Length::Fixed(48.0))
                        .height(Length::Fixed(48.0));
                let mut card = column![
                    icon,
                    text(*name).size(13),
                    text(short_state(&state)).size(10),
                ]
                .spacing(1)
                .align_items(iced::Alignment::Center)
                .width(Length::Fixed(110.0));
                if let Some(err) = self.last_err.get(*name) {
                    card = card.push(text(err).size(10));
                }
                match &state {
                    CardState::NeedDownload => {
                        let retry = self.last_err.contains_key(*name);
                        let label = if retry { "Retry" } else { "Download" };
                        let b = pill(text(label))
                            .on_press(Message::DoInstall(name.to_string()));
                        card = card.push(if retry {
                            b.style(iced::theme::Button::Destructive)
                        } else {
                            b
                        });
                    }
                    _ => {
                        // Blocked: nothing actionable — still openable for
                        // info, but labeled honestly.
                        let label = if state == CardState::Blocked {
                            "Info"
                        } else {
                            "Open"
                        };
                        card = card.push(
                            pill(label).on_press(Message::OpenDetail(name.to_string())),
                        );
                    }
                }
                r = r.push(card);
            }
            col = col.push(container(r).width(Length::Fill).center_x());
        }
        container(scrollable(
            container(col)
                .max_width(680.0)
                .width(Length::Fill)
                .center_x(),
        ))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn view_detail(&self, format: &str) -> Element<Message> {
        let remotes = remote::load_remotes();
        let tips = self.config.show_button_tips;
        let (state, stateline) = capability(format, &remotes);
        let _ = &state;
        let mut col = column![
            text(format!(".{format} packaging")).size(22),
            text(stateline).size(12),
            text("Project folder:").size(14),
            row![
                text_input("/path/to/project", &self.path_input)
                    .on_input(Message::PathChanged)
                    .padding(8),
                pill("Browse").on_press(Message::BrowseProject),
            ]
            .spacing(10),
            text(&self.detail_status).size(12),
            text("Save to:").size(14),
            row![
                text_input("output folder", &self.out_dir)
                    .on_input(Message::OutDirChanged)
                    .padding(8),
                pill("Browse").on_press(Message::BrowseOut),
            ]
            .spacing(10),
        ]
        .spacing(10)
        .padding(20);
        if let Some(recipe) = install::recipe_for(format) {
            col = col.push(text(format!("Needs: {}", install_summary(&recipe))).size(12));
            col = col.push(
                text("Exact command (runs only if you press Download):").size(12),
            );
            col = col.push(text(&recipe.command).size(11));
            if let Some(note) = &recipe.note {
                col = col.push(text(note).size(11));
            }
            if recipe.needs_password {
                col = col.push(
                    text("Needs your sudo password up front:").size(12),
                );
                col = col.push(
                    text_input("password", &self.install_pwd)
                        .secure(true)
                        .on_input(Message::InstallPwdChanged)
                        .padding(6),
                );
            }
        } else {
            col = col.push(
                text("No install action available for this format on this host.").size(12),
            );
        }
        if bundleforge_core::container::container_option(format).is_none()
            && bundleforge_core::container::container_spec(format).is_some()
        {
            if let Some(rt) = install::runtime_recipe() {
                col = col.push(text(format!("Needs podman first: {}", rt.command)).size(12));
            }
        } else if let Some(line) = bundleforge_core::container::container_option(format) {
            col = col.push(text(format!("Container: {line}")).size(12));
        }
        let m_remote = remote::matching_remote(format, &remotes);
        match &m_remote {
            Some(r) => {
                col = col.push(
                    text(format!("Remote: {} ({}@{})", r.name, r.user, r.host)).size(12),
                );
            }
            None => {
                col = col.push(text("Remote: none configured").size(12));
            }
        }
        col = col.push(text(remote::recommendation(format, detectors::os_name())).size(12));
        let have_path = !self.path_input.trim().is_empty();
        let have_out = !self.out_dir.trim().is_empty();
        let fmt = format.to_string();
        let mut btns = row![
            pill("Test").on_press_maybe(if have_path {
                Some(Message::DetailTest(fmt.clone()))
            } else {
                None
            }),
            pill("Package").on_press_maybe(if have_path && have_out {
                Some(Message::DetailPackage(fmt.clone()))
            } else {
                None
            }),
            tip(pill("Isolated").on_press_maybe(if have_path && have_out {
                Some(Message::DetailIsolated(fmt.clone()))
            } else {
                None
            }),
            "Builds inside an automatic podman/docker container. First run downloads the image (hundreds of MB); nothing is installed on your host.",
            tips),
        ]
        .spacing(10);
        match m_remote {
            Some(r) => {
                btns = btns.push(
                    pill(text(format!("Remote ({})", r.name))).on_press(
                        Message::RemotePackage {
                            format: fmt.clone(),
                            remote: r.name.clone(),
                        },
                    ),
                );
            }
            None => {
                btns = btns.push(pill("Remotes").on_press(Message::ShowRemotesFromDetail));
            }
        }
        let gate = tester::check_format(&fmt);
        if install::recipe_for(format).is_some() {
            // Always offered (Download or Re-download): local provisioning
            // may lag behind (new recipe steps) even when a remote exists.
            let label = if gate.state != "SAFE" {
                "Download deps"
            } else {
                "Re-download"
            };
            btns = btns.push(
                pill(label).on_press(Message::DoInstall(fmt.clone())),
            );
        }
        if bundleforge_core::container::container_option(format).is_none()
            && bundleforge_core::container::container_spec(format).is_some()
            && install::runtime_recipe().is_some()
        {
            btns = btns.push(pill("Install podman").on_press(Message::DoInstallRuntime));
        }
        btns = btns.push(pill("Back").on_press(Message::BackToStore));
        col = col.push(btns);
        container(scrollable(col))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_loading(&self, st: &Loading) -> Element<Message> {
        if st.pwd_open {
            return container(
                column![
                    text("Administrator password needed").size(18),
                    text("The installer asked for sudo access. Kept in memory only, never stored or logged.").size(12),
                    text_input("password", &self.pwd_input)
                        .secure(true)
                        .on_input(Message::PwdChanged)
                        .on_submit(Message::PwdSubmit)
                        .padding(8),
                    row![
                        pill("Send").on_press(Message::PwdSubmit),
                        pill("Cancel").on_press(Message::Cancel),
                    ]
                    .spacing(10),
                ]
                .spacing(10)
                .padding(24)
                .align_items(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x()
            .center_y()
            .into();
        }
        let total = st.steps.len().max(1);
        let pct = st.done as f32 / total as f32;
        let current = if st.done < st.steps.len() {
            match &st.steps[st.done] {
                Step::Scan => "scanning project…".to_string(),
                Step::Check(f) => format!("checking {f}…"),
                Step::Build(f) => format!("packaging {f}…"),
                Step::Package(f) => format!("packaging {f} → packages/…"),
                Step::Install(f) => format!("installing tool for {f}…"),
                Step::InstallRuntime => "installing podman…".to_string(),
                Step::RemotePackage { format, remote } => {
                    format!("packaging {format} on {remote}…")
                }
            }
        } else {
            "finishing…".to_string()
        };
        let mut col = column![
            image(st.frames[st.frame].clone())
                .filter_method(image::FilterMethod::Nearest)
                .width(Length::Fixed(128.0))
                .height(Length::Fixed(128.0)),
            text(current).size(15),
            progress_bar(0.0..=1.0, pct).height(Length::Fixed(10.0)),
            text(format!("{}/{}", st.done.min(total), total)).size(12),
            row![
                pill(if st.show_detail {
                    "Hide details"
                } else {
                    "Show details"
                })
                .on_press(Message::ToggleDetail),
                pill("Cancel").on_press(Message::Cancel),
            ]
            .spacing(10),
        ]
        .spacing(10)
        .padding(24)
        .align_items(iced::Alignment::Center);

        if st.show_detail {
            let mut log = column![].spacing(2);
            for line in &st.log {
                log = log.push(text(line).size(12));
            }
            col = col.push(scrollable(log).height(Length::Fixed(180.0)));
        }

        container(col)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x()
            .center_y()
            .into()
    }

    fn view_results(
        &self,
        info: &ProjectInfo,
        results: &[TestResult],
        log: &[String],
    ) -> Element<Message> {
        let failed = results.iter().any(|r| r.success == Some(false));
        let title = if failed {
            "Packaging failed"
        } else {
            "Packaging successful"
        };
        let mut col = column![
            text(title).size(24),
            text(format!("{} · {} {}", info.name, info.stack, info.version)).size(13),
        ]
        .spacing(10)
        .padding(20);
        let mut logcol = column![].spacing(2);
        for r in results {
            logcol = logcol.push(
                text(format!("[{}] {} — {}", r.state, r.format, r.reason)).size(13),
            );
            if let Some(a) = &r.artifact_path {
                logcol = logcol.push(text(a).size(11));
            }
            if let Some(e) = &r.error_log {
                if !e.is_empty() {
                    logcol = logcol.push(text(e).size(11));
                }
            }
        }
        logcol = logcol.push(text("Console log:").size(14));
        for line in log {
            logcol = logcol.push(text(line).size(11));
        }
        col = col.push(scrollable(logcol).height(Length::Fill));
        col = col.push(pill("Back to store").on_press(Message::BackToStore));
        container(col)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_remotes(&self) -> Element<Message> {
        let mut col = column![
            text("Remote builders (SSH, keys only)").size(22),
            text("A remote is any machine with bundleforge-core + SSH. Offload lands next; today this only configures + probes.").size(12),
        ]
        .spacing(10)
        .padding(20);
        if self.remotes.is_empty() {
            col = col.push(text("No remotes yet. Add your other PC below.").size(13));
        }
        for (i, r) in self.remotes.iter().enumerate() {
            let testing = self.remote_testing.as_deref() == Some(r.name.as_str());
            let rrow = row![
                text(format!("{} ({}) {}@{}", r.name, r.os, r.user, r.host)).size(13),
                pill(if testing { "…" } else { "Test" }).on_press_maybe(
                    if testing || self.remote_rx.is_some() {
                        None
                    } else {
                        Some(Message::TestRemote(i))
                    }
                ),
                pill("Remove").on_press(Message::RemoveRemote(i)),
            ]
            .spacing(10);
            col = col.push(rrow);
        }
        if self.remote_testing.is_some() {
            col = col.push(
                row![
                    text(format!(
                        "probing {}…",
                        self.remote_testing.as_deref().unwrap_or("")
                    ))
                    .size(12),
                    pill("Stop").on_press(Message::StopRemoteTest),
                ]
                .spacing(10),
            );
        }
        col = col.push(text(&self.remote_note).size(13));
        col = col.push(text("Add remote:").size(14));
        col = col.push(
            text_input("name (e.g. lab-pc)", &self.r_name)
                .on_input(Message::RNameChanged)
                .padding(6),
        );
        col = col.push(
            text_input("host (e.g. 192.168.1.5)", &self.r_host)
                .on_input(Message::RHostChanged)
                .padding(6),
        );
        col = col.push(
            text_input("user (e.g. dev)", &self.r_user)
                .on_input(Message::RUserChanged)
                .padding(6),
        );
        col = col.push(
            text_input("os: linux|windows", &self.r_os)
                .on_input(Message::ROsChanged)
                .padding(6),
        );
        let buttons = row![
            pill("Add").on_press(Message::AddRemote),
            pill("Back").on_press(Message::RemoteBack),
        ]
        .spacing(10);
        col = col.push(buttons);
        container(scrollable(col))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}
