//! Pult window: title bar, project list, the Claude session terminal and the Connected to Claude panel.
//!
//! Each project has one pinned Claude Code session (see `sessions`). Every 3 seconds Pult
//! asks Claude Code what the sessions are doing and uses that to draw the list icons and the
//! Session section, and to decide what to show in place of the terminal.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use gpui_kit::component::{
    ActiveTheme as _, Collapsible, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, Theme, ThemeMode,
    TitleBar, WindowExt as _,
    accordion::Accordion,
    button::{Button, ButtonVariant, ButtonVariants as _},
    h_flex,
    kbd::Kbd,
    menu::{DropdownMenu as _, PopupMenuItem},
    notification::Notification,
    radio::Radio,
    resizable::{h_resizable, resizable_panel},
    sidebar::{Sidebar, SidebarCollapsible, SidebarGroup, SidebarItem, SidebarMenu, SidebarMenuItem},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    AnyElement, AnyWindowHandle, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, Keystroke, MouseButton, ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _,
    StatefulInteractiveElement as _, Window, div, prelude::FluentBuilder as _, px, rems,
};

use crate::actions::{
    AddFolders, ClearTerminal, CloneSubproject, CopyPath, GoBack, GoForward, NewProject, NewSubproject, OpenModels, OpenPalette, OpenSettings, PauseOthers,
    RemoveProject, RenameProject, RestartSession, RevealInFinder, SplitProject, SwitchSession, ToggleInfo, TogglePause, ToggleSidebar, ToggleTheme, UseClaude, UseGlm, ZoomIn,
    ZoomOut, ZoomReset,
};
use gpui_kit::assets::IconName as AssetIcon;

use crate::claude::{ClaudeSetup, Scoped};
use crate::i18n::t;
use crate::dialogs::{self, PaletteAction};
use crate::migrate::{self, Move};
use crate::projects::{Project, ProjectStore, ThemePref, display_path, key_for, split_key};
use crate::rename::{self, Target};
use crate::sessions::{self, Activity, Agent, StartError, Transcript};
use crate::program_status::{Report, State};
use crate::terminal::{self, ClearBlocked, FontSizeChanged, ProgramStatus, Terminal};

/// How often the live registry is read (`~/.claude/sessions`): files only, no process is started.
const LIVE_TICK: Duration = Duration::from_secs(1);
/// How often Claude Code's supervisor is asked for the whole list (`claude agents` — a process each time). It is the
/// only source for stopped and failed sessions and for sessions nobody has seen yet; what the running ones are doing
/// comes from the registry in between.
const FULL: Duration = Duration::from_secs(15);
/// The same while macOS is short of memory.
const FULL_PRESSED: Duration = Duration::from_secs(45);
/// A list requested early (a process came or went) is still asked for no more often than this.
const FULL_SOON: Duration = Duration::from_secs(3);

/// What one full poll brings: the sessions (`None` — Claude Code didn't answer), the memory of their processes,
/// whether `claude` is missing, and the live registry as it was at that moment.
type Polled = (Option<Vec<Agent>>, HashMap<u32, u64>, bool, HashMap<u32, sessions::Live>);

/// Folder move progress — shown by the move window.
pub struct MoveState {
    pub labels: Vec<&'static str>,
    /// Step in progress; `None` — the move finished or was rolled back.
    pub step: Option<usize>,
    pub error: Option<String>,
    pub done: bool,
}

/// A project the folder move will affect — a row in the move window.
pub struct Affected {
    /// The project folder: a split project gives one row per subproject.
    pub folder: String,
    pub name: String,
    pub state: &'static str,
    pub tone: Tone,
}

#[derive(Clone, Copy)]
pub enum Tone {
    Muted,
    Warning,
}

pub struct Workspace {
    store: ProjectStore,
    projects: Vec<Project>,
    selected: Option<String>,
    history: Vec<String>,
    history_index: usize,
    sidebar_open: bool,
    info_open: bool,
    /// Expanded sections of the Connected to Claude panel.
    info_sections: Vec<usize>,
    terminal: Entity<Terminal>,
    focus_handle: FocusHandle,

    /// Where sessions run: here (`presence.json`) and on other computers (`presence.rs`).
    presence: crate::presence::Presence,
    remote: std::collections::BTreeMap<String, crate::presence::Remote>,
    /// What the program in an open project terminal last reported about itself (OSC 7501), by project. Only
    /// while that terminal is connected to it: the terminal says when the program is gone.
    program_status: HashMap<String, Report>,
    /// Projects whose session is being moved here from another computer: the session and when it was requested.
    handoff: HashMap<String, (String, u64)>,
    /// Projects whose session came here with "Continue here": from which computer (the steps stay on screen
    /// until it has started).
    moved_from: HashMap<String, String>,
    /// Projects whose code from another computer is being laid down before the session starts.
    restoring: HashMap<String, crate::code::Manifest>,
    /// Projects whose code from another computer didn't go into place: why, and the snapshot.
    code_stuck: HashMap<String, (crate::code::Stuck, crate::code::Manifest)>,
    /// Notifications from background work, shown at the next frame (it has no window at hand).
    notices: Vec<Notification>,
    /// Projects whose session is being stopped here for a move: waiting for the process to exit.
    releasing: HashMap<String, String>,
    /// When the server was last asked for updates during a move.
    pulled_for_move: Option<std::time::Instant>,

    /// Who Claude Code talks to (read from its settings), whether a GLM key is saved and the model GLM gets by default:
    /// for the chip in the title bar — cached, it is drawn every frame.
    provider: crate::models::Provider,
    /// A switch asked for from Settings (a separate window, no access to this one): the main window starts it.
    pending_provider: Option<crate::models::Provider>,
    glm_key: bool,
    glm_main: String,

    /// Last response of `claude agents --json`.
    agents: Vec<Agent>,
    /// The session list has been received at least once: before that, "Claude Code has no such session" means nothing.
    agents_loaded: bool,
    /// A `claude agents` poll is under way: no second one is started meanwhile.
    polling: bool,
    /// When the last full poll ended, and whether the registry showed a reason to ask again soon.
    last_full: Option<std::time::Instant>,
    want_full: bool,
    /// What the running processes wrote about themselves at the last read (see `sessions::Live`).
    live: HashMap<u32, sessions::Live>,
    /// The start time of each process as of the last full poll: a pid with another start time is another process,
    /// whose registry file must not be applied to the list's entry for the old one.
    proc_starts: HashMap<u32, String>,
    /// Registry processes the supervisor's list didn't contain at the last full poll (not asked about again).
    unlisted: HashSet<u32>,
    /// Empty lists in a row from Claude Code after a non-empty one (an overloaded supervisor answers `[]`).
    empty_polls: u8,
    /// The "Code" row of the session panel, refreshed with every poll (no file reads while drawing).
    code_rows: HashMap<String, crate::code::Summary>,
    /// Sessions pinned during this run: a new one may still be empty and missing from Claude
    /// Code's list — such sessions are not dropped as nonexistent.
    bound_here: HashSet<String>,
    /// Memory of session processes, MB, by pid.
    memory: HashMap<u32, u64>,
    /// Projects whose session is starting right now.
    starting: HashSet<String>,
    /// Error from the last session start — shown in the placeholder.
    errors: HashMap<String, (Failure, String)>,
    /// Session started right in the project terminal (the folder wasn't trusted yet, or a pick from
    /// Claude Code's list): its id is found by the terminal process and pinned.
    pending_bind: HashMap<String, u32>,
    /// Projects whose pinned session turned out empty and was dropped: the placeholder explains why.
    dropped: HashSet<String>,
    /// Claude Code not found: the session list is empty and the shell can't find `claude`.
    claude_missing: bool,
    /// Projects paused during this run: don't attach to their session until they are continued.
    paused: HashSet<String>,
    /// Projects continued after a pause: while Claude Code still lists the session as stopped.
    resumed: HashSet<String>,
    /// When `/rename` was last sent to the project's session — at most once every 30 s.
    renamed: HashMap<String, std::time::Instant>,
    /// Projects where another session is being chosen.
    switching: HashSet<String>,
    /// Session selected in the list (not pinned yet).
    choice: HashMap<String, String>,
    /// The folder's conversations to choose from; read when the chooser opens.
    transcripts: HashMap<String, Vec<Transcript>>,
    previous_open: bool,
    /// Folder being moved right now: don't attach to its sessions.
    moving: Option<PathBuf>,
    /// Projects in the folder being moved that weren't paused — they continue after the move.
    moving_live: HashSet<String>,
    settings_window: Option<AnyWindowHandle>,
    move_state: Option<MoveState>,
    /// The subproject last opened in each split project: clicking the project row goes back to it.
    last_sub: HashMap<String, String>,
    /// A split project whose subprojects must be unfolded in the sidebar: one of them was just opened.
    reveal: Option<String>,
    /// `/rename` commands Pult typed into a session's input and when: checked a few seconds later.
    typed: HashMap<String, (String, std::time::Instant)>,
    /// Projects whose session did not take Pult's `/rename`: not tried again while Pult runs.
    rename_failed: HashSet<String>,
    /// What the current subproject's session knows about its neighbours (`journal::summary`), shown in the session
    /// panel; empty for an ordinary project.
    neighbours: String,
    /// Project keys that changed (split, merge, rename): a session start that was running at that moment
    /// still reports under the key it began with.
    rekeyed: HashMap<String, String>,
    _appearance: Option<gpui_kit::Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = ProjectStore::load();
        let projects = store.list();
        // The project that was open when Pult quit; otherwise the first one.
        let selected = store
            .last_selected()
            .map(str::to_string)
            .or_else(|| projects.first().map(|project| project.id().to_string()));
        // For checking the look without clicks: `PULT_SHOW=<project key>` opens that project (`MAPLE/Finance`).
        let selected = std::env::var("PULT_SHOW")
            .ok()
            .filter(|key| projects.iter().any(|project| project.id() == key.as_str()))
            .or(selected);
        // Split projects need the journal hooks (they may have been split before, or on another computer).
        if projects.iter().any(|project| project.sub().is_some()) {
            crate::journal::ensure_hooks();
        }
        let font_size = store.terminal_font_size();
        let terminal = cx.new(|cx| Terminal::new(font_size, cx));
        cx.subscribe(&terminal, |this, _, event: &FontSizeChanged, _| {
            this.store.set_terminal_font_size(event.0);
        })
        .detach();
        cx.subscribe(&terminal, |this, _, event: &ProgramStatus, cx| {
            let changed = match &event.report {
                // The program withdrew its record: nothing is known about it now.
                Some(report) if report.state == State::Clear => this.program_status.remove(&event.id).is_some(),
                Some(report) => this.program_status.insert(event.id.clone(), report.clone()) != Some(report.clone()),
                None => this.program_status.remove(&event.id).is_some(),
            };
            if changed {
                cx.notify();
            }
        })
        .detach();
        cx.subscribe_in(&terminal, window, |_, _, event: &ClearBlocked, window, cx| {
            window.push_notification(
                Notification::info(tr!("workspace.event_is_turned_off_in", event = event.0)),
                cx,
            );
        })
        .detach();
        let mut this = Self {
            store,
            projects,
            selected: None,
            history: Vec::new(),
            history_index: 0,
            sidebar_open: true,
            info_open: false,
            info_sections: Vec::new(),
            terminal,
            focus_handle: cx.focus_handle(),
            presence: crate::presence::Presence::load(),
            remote: Default::default(),
            handoff: HashMap::new(),
            moved_from: HashMap::new(),
            restoring: HashMap::new(),
            code_stuck: HashMap::new(),
            notices: Vec::new(),
            releasing: HashMap::new(),
            pulled_for_move: None,
            provider: crate::models::Provider::Claude,
            pending_provider: None,
            glm_key: false,
            glm_main: String::new(),
            agents: Vec::new(),
            agents_loaded: false,
            polling: false,
            last_full: None,
            want_full: false,
            live: HashMap::new(),
            proc_starts: HashMap::new(),
            unlisted: HashSet::new(),
            empty_polls: 0,
            code_rows: HashMap::new(),
            bound_here: HashSet::new(),
            memory: HashMap::new(),
            starting: HashSet::new(),
            program_status: HashMap::new(),
            errors: HashMap::new(),
            pending_bind: HashMap::new(),
            dropped: HashSet::new(),
            claude_missing: false,
            paused: HashSet::new(),
            resumed: HashSet::new(),
            switching: HashSet::new(),
            renamed: HashMap::new(),
            choice: HashMap::new(),
            transcripts: HashMap::new(),
            previous_open: false,
            moving: None,
            moving_live: HashSet::new(),
            settings_window: None,
            move_state: None,
            last_sub: HashMap::new(),
            reveal: None,
            rekeyed: HashMap::new(),
            neighbours: String::new(),
            typed: HashMap::new(),
            rename_failed: HashSet::new(),
            _appearance: None,
        };
        this.apply_theme(cx);
        // System: the theme follows the macOS appearance.
        this._appearance = Some(cx.observe_window_appearance(window, |this, _, cx| {
            if this.store.theme() == ThemePref::System {
                this.apply_theme(cx);
            }
        }));
        // Pult remembers its window frame like any Mac app (Apple HIG: Remember Window State).
        cx.observe_window_bounds(window, |this, window, _| {
            let bounds = window.bounds();
            this.store.set_window_frame([
                bounds.origin.x / px(1.),
                bounds.origin.y / px(1.),
                bounds.size.width / px(1.),
                bounds.size.height / px(1.),
            ]);
        })
        .detach();
        // Updates: found automatically — a notification; from the Check for Updates… menu — the window or an answer right away.
        if let Some(updater) = crate::update::updater(cx) {
            // The update button in the sidebar follows the updater's state.
            cx.observe(&updater, |_, _, cx| cx.notify()).detach();
            cx.subscribe_in(&updater, window, |this, _, event: &crate::update::UpdateEvent, window, cx| {
                use crate::update::{Origin, UpdateEvent};
                match event {
                    // Found automatically: the sidebar shows a button at its bottom; a notification only when
                    // the sidebar is hidden and the button is out of sight.
                    UpdateEvent::Found(Origin::Auto) => {
                        if !this.sidebar_open {
                            window.push_notification(crate::update::notice(cx), cx);
                        }
                    }
                    UpdateEvent::Found(Origin::Menu) => crate::update::open_dialog(window, cx),
                    UpdateEvent::Latest(Origin::Menu) => {
                        window.push_notification(tr!("workspace.you_have_latest_version_env", env = env!("CARGO_PKG_VERSION")), cx)
                    }
                    UpdateEvent::CheckFailed(Origin::Menu, error) => window.push_notification(Notification::error(error.clone()), cx),
                    _ => {}
                }
            })
            .detach();
        }
        if let Some(version) = crate::update::take_installed() {
            cx.defer_in(window, move |_, window, cx| window.push_notification(tr!("workspace.pult_updated_to_version", version = version), cx));
        }
        if let Some(id) = selected {
            this.select(&id, window, cx);
            // The launch selection is not the user opening a subproject: a folded project stays folded.
            this.reveal = None;
        }
        // Polls run in the background only — the window never waits for `claude`: under memory pressure a slow
        // answer would freeze it. The whole list from `claude agents` comes right away,
        // then every `FULL` (less often when memory is short), sooner when the registry shows a process that came
        // or went; never two at once. In between, every second, the live registry is read from files: what the
        // running sessions do is as fresh as before without a process started every few seconds.
        cx.spawn(async move |this, cx| {
            let mut first = true;
            loop {
                if !first {
                    cx.background_executor().timer(LIVE_TICK).await;
                }
                first = false;
                let pressed = cx.background_spawn(async { sessions::memory_pressure() >= 2 }).await;
                let every = if pressed { FULL_PRESSED } else { FULL };
                let Ok(full) = this.update(cx, |this, _| {
                    let since = this.last_full.map(|at| at.elapsed());
                    let go = since.is_none_or(|since| since >= every || (this.want_full && since >= FULL_SOON));
                    if go && this.polling {
                        return false;
                    }
                    if go {
                        this.want_full = false;
                        this.polling = true;
                    }
                    go
                }) else {
                    break;
                };
                if full {
                    let result = cx.background_spawn(async { poll_agents() }).await;
                    if this.update(cx, |this, cx| this.on_agents(result, cx)).is_err() {
                        break;
                    }
                } else {
                    let live = cx.background_spawn(async { sessions::live_sessions() }).await;
                    if this.update(cx, |this, cx| this.on_live(live, cx)).is_err() {
                        break;
                    }
                }
            }
        })
        .detach();
        // The key the `@z_ai/coding-helper` utility saved is taken once, at launch: the GLM choice works at once.
        crate::models::adopt_helper_key(&crate::sync::Paths::real());
        // Before 0.2.0 GLM was switched on for everything in `~/.claude/settings.json`: now it is per project.
        crate::models::migrate_global(&crate::sync::Paths::real());
        this.refresh_models();
        this
    }

    fn current(&self) -> Option<&Project> {
        let id = self.selected.as_ref()?;
        self.projects.iter().find(|project| project.id() == id)
    }

    fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|project| project.id() == id)
    }

    /// The session as Claude Code sees it. One that failed at startup (`failed`, e.g. "source session … not found")
    /// Pult ignores: it can't be attached, only started again.
    /// The pinned session failed at Claude Code (and wasn't paused here): its short id.
    fn failed(&self, project: &Project) -> Option<String> {
        if self.paused.contains(project.id()) || self.starting.contains(project.id()) {
            return None;
        }
        let session = project.session()?;
        let failed = |agent: &&Agent| agent.session_id == session && agent.state.as_deref() == Some("failed") && agent.pid.is_none();
        // Only when no live entry of the same session exists.
        if self.agent(session).is_some_and(Agent::is_running) {
            return None;
        }
        self.agents.iter().find(failed).and_then(|agent| agent.id.clone())
    }

    fn agent(&self, session: &str) -> Option<&Agent> {
        self.agents.iter().find(|agent| agent.session_id == session && agent.state.as_deref() != Some("failed"))
    }

    /// The project's session runs on another computer (and not here): that computer's name.
    fn elsewhere(&self, project: &Project) -> Option<String> {
        let session = project.session()?;
        if self.agent(session).is_some_and(Agent::is_running) || self.handoff.contains_key(project.id()) {
            return None;
        }
        let remote = self.remote.get(session)?;
        let now = crate::presence::unix_now();
        // Another computer just asked for the session (it was already released here) — also "there",
        // even before it has started it: Continue here at that moment would launch a second one.
        let claimed = self.presence.claimed.get(session).copied().unwrap_or(0);
        let taking = remote.takeover_at > claimed && now.saturating_sub(remote.takeover_at) < 600;
        // Went silent while running there, and not taken here since: maybe still running there (a computer that
        // only lost its connection must not have its sessions started a second time).
        let silent = remote.silent(now) && remote.active_at > claimed;
        (remote.active(now) || taking || silent).then(|| remote.device.clone())
    }

    /// Continue here: ask the other computer for the session. It pauses it, the whole conversation
    /// arrives here — then the session starts here (`track_presence`).
    fn take_over(&mut self, project: &Project, cx: &mut Context<Self>) {
        let Some(session) = project.session().map(str::to_string) else {
            return;
        };
        let now = crate::presence::unix_now();
        self.errors.remove(project.id());
        self.handoff.insert(project.id().to_string(), (session.clone(), now));
        if let Some(remote) = self.remote.get(&session) {
            self.moved_from.insert(project.id().to_string(), remote.device.clone());
        }
        self.code_stuck.remove(project.id());
        self.presence.takeover.insert(session, now);
        self.presence.save();
        crate::sync::pull_now();
        self.pulled_for_move = Some(std::time::Instant::now());
        cx.notify();
    }

    /// The code snapshot the other computer made after stopping the session has arrived (or code isn't carried here).
    fn code_arrived(&self, session: &str, released_at: u64) -> bool {
        if !crate::code::enabled() {
            return true;
        }
        let paths = crate::sync::Paths::real();
        crate::code::manifest(&paths, session).is_some_and(|manifest| manifest.at >= released_at && crate::code::ready(&paths, &manifest))
    }

    /// Lay a session's code from another computer down, then start the session in it.
    fn restore_code(&mut self, project: &Project, manifest: crate::code::Manifest, cx: &mut Context<Self>) {
        let id = project.id().to_string();
        self.restoring.insert(id.clone(), manifest.clone());
        self.errors.remove(&id);
        self.moved_from.entry(id.clone()).or_insert_with(|| manifest.device_name.clone());
        cx.notify();
        let folder = project.path().to_path_buf();
        let transcript = project.session().and_then(|session| sessions::transcript_path(project.path(), session));
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let paths = crate::sync::Paths::real();
                    // The bundle may still be on its way after its description.
                    let mut waited = 0;
                    while !crate::code::ready(&paths, &manifest) {
                        if waited >= 60 {
                            return Err((crate::code::Stuck::NotArrived, manifest));
                        }
                        let _ = crate::sync::pull(&paths);
                        std::thread::sleep(Duration::from_secs(3));
                        waited += 3;
                    }
                    crate::code::restore(&paths, &folder, &manifest, transcript.as_deref()).map_err(|stuck| (stuck, manifest))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let id = this.live_key(&id);
                let device = this.restoring.remove(&id).map(|manifest| manifest.device_name).unwrap_or_default();
                match result {
                    Ok(restored) => {
                        if let Some((wanted, used)) = restored.renamed {
                            this.notices.push(Notification::info(tr!(
                                "code.branch_renamed",
                                device = device,
                                wanted = wanted,
                                used = used
                            )));
                        }
                        if let Some(project) = this.project(&id).cloned() {
                            this.resume(&project, cx);
                        }
                    }
                    Err((stuck, manifest)) => {
                        this.code_stuck.insert(id, (stuck, manifest));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// "Continue without code": the snapshot isn't asked about again, the session starts with the code that's here.
    fn continue_without_code(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some((stuck, manifest)) = self.code_stuck.remove(id) else { return };
        crate::code::skip(&crate::sync::Paths::real(), &manifest);
        let text = match stuck {
            crate::code::Stuck::Dirty { branch, .. } => {
                tr!("code.continues_without_code_kept", device = manifest.device_name, branch = branch)
            }
            _ => tr!("code.continues_without_code", device = manifest.device_name),
        };
        self.notices.push(Notification::info(text));
        if let Some(project) = self.project(id).cloned() {
            self.resume(&project, cx);
        }
        cx.notify();
    }

    /// "Cancel" on stuck code: the session stays paused here; its code is where it was.
    fn keep_code_there(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some((_, manifest)) = self.code_stuck.remove(id) else { return };
        self.moved_from.remove(id);
        self.paused.insert(id.to_string());
        self.notices.push(Notification::info(tr!("code.paused_code_stayed", device = manifest.device_name)));
        cx.notify();
    }

    /// "Try again" on stuck code.
    fn retry_code(&mut self, id: &str, cx: &mut Context<Self>) {
        self.code_stuck.remove(id);
        if let Some(project) = self.project(id).cloned() {
            self.resume(&project, cx);
        }
        cx.notify();
    }

    /// The code step of a move: what comes, or why nothing does.
    fn code_step(&self, session: &str) -> (SharedString, terminal::Step) {
        let paths = crate::sync::Paths::real();
        if !crate::code::enabled() {
            return (t("code.step_off_here").into(), terminal::Step::Skip);
        }
        let manifest = crate::code::manifest(&paths, session).filter(|manifest| crate::sync::config(&paths).is_none_or(|config| config.device != manifest.device));
        let Some(manifest) = manifest else {
            return (t("code.step_receiving").into(), terminal::Step::Wait);
        };
        let label: SharedString = match manifest.status {
            crate::code::Status::Off => tr!("code.step_off_there", device = manifest.device_name).into(),
            crate::code::Status::Nogit => t("code.step_nogit").into(),
            crate::code::Status::Big => t("code.step_big").into(),
            crate::code::Status::Ok if manifest.worktree => {
                tr!("code.step_worktree", name = manifest.worktree_name, files = crate::i18n::count(manifest.files as u64, "code.changed_files")).into()
            }
            crate::code::Status::Ok if manifest.files > 0 => {
                tr!("code.step_files", files = crate::i18n::count(manifest.files as u64, "code.changed_files")).into()
            }
            crate::code::Status::Ok => t("code.step_no_changes").into(),
        };
        let carried = manifest.status == crate::code::Status::Ok && (manifest.worktree || manifest.files > 0 || !manifest.bundle.is_empty());
        (label, if carried || manifest.status == crate::code::Status::Big { terminal::Step::Wait } else { terminal::Step::Skip })
    }

    /// The four steps of a move, `current` — the one in progress.
    fn move_steps(&self, session: &str, device: &str, current: usize) -> Vec<(SharedString, terminal::Step)> {
        let (code_label, code_kind) = self.code_step(session);
        let labels: [(SharedString, bool); 4] = [
            (tr!("code.step_pause_there", device = device).into(), false),
            (t("code.step_conversation").into(), false),
            (code_label, code_kind == terminal::Step::Skip),
            (t("code.step_continue").into(), false),
        ];
        labels
            .into_iter()
            .enumerate()
            .map(|(index, (label, skip))| {
                let step = if skip {
                    terminal::Step::Skip
                } else if index < current {
                    terminal::Step::Done
                } else if index == current {
                    terminal::Step::Now
                } else {
                    terminal::Step::Wait
                };
                (label, step)
            })
            .collect()
    }

    fn cancel_take_over(&mut self, id: &str, cx: &mut Context<Self>) {
        self.moved_from.remove(id);
        if let Some((session, _)) = self.handoff.remove(id) {
            self.presence.takeover.remove(&session);
            self.presence.save();
        }
        cx.notify();
    }

    /// Once per poll: mark sessions running here, hand over a session another computer asked for,
    /// and start the one being moved here once its conversation has fully arrived.
    fn track_presence(&mut self, cx: &mut Context<Self>) {
        let paths = crate::sync::Paths::real();
        let Some(own) = crate::sync::own_snapshot(&paths) else {
            return;
        };
        let now = crate::presence::unix_now();
        self.remote = crate::presence::remote(&crate::sync::shared_dir(&paths), &own);
        let mut changed = false;

        // Running here: pinned sessions with a live process. A just-started one is "claimed".
        let running: Vec<String> = self
            .projects
            .iter()
            .filter_map(Project::session)
            .filter(|session| self.agent(session).is_some_and(Agent::is_running))
            .map(str::to_string)
            .collect();
        for session in &running {
            if !self.presence.active.contains_key(session) {
                self.presence.claim(session, now);
                // The move is done once the session is running here: until then the request stays, so
                // the other computer doesn't start it again while it is starting here.
                self.presence.takeover.remove(session);
                changed = true;
            }
        }
        changed |= self.presence.beat(&running, now);

        // Another computer asked for the session after it was started here — pause it.
        let asked: Vec<(String, String)> = self
            .projects
            .iter()
            .filter_map(|project| {
                let session = project.session()?;
                let remote = self.remote.get(session)?;
                let claimed = self.presence.claimed.get(session).copied().unwrap_or(0);
                (running.iter().any(|running| running == session)
                    && remote.takeover_at > claimed
                    && now.saturating_sub(remote.takeover_at) < 600
                    && !self.releasing.contains_key(project.id()))
                .then(|| (project.id().to_string(), session.to_string()))
            })
            .collect();
        for (id, session) in asked {
            if let Some(short) = self.agent(&session).filter(|agent| agent.is_background()).and_then(|agent| agent.id.clone()) {
                cx.background_spawn(async move { sessions::stop(&short) }).detach();
            } else {
                self.terminal.update(cx, |terminal, cx| terminal.close(&id, cx));
            }
            self.paused.insert(id.clone());
            self.releasing.insert(id, session);
        }
        // A session stopped for a move has exited: its conversation is now final.
        let released: Vec<(String, String)> = self
            .releasing
            .iter()
            .filter(|(_, session)| !self.agent(session).is_some_and(Agent::is_running))
            .map(|(id, session)| (id.clone(), session.clone()))
            .collect();
        for (id, session) in released {
            self.releasing.remove(&id);
            let Some(project) = self.project(&id).cloned() else { continue };
            let len = sessions::transcript_len(project.path(), &session);
            self.presence.active.remove(&session);
            self.presence.released.insert(session.clone(), crate::presence::Released { at: now, len, code: true });
            if let Some(transcript) = sessions::transcript_path(project.path(), &session) {
                crate::sync::push_transcript_now(transcript);
            }
            changed = true;
        }

        // Moving here: the other computer released the session (or is silent) and the conversation fully arrived.
        let moving: Vec<(String, String, u64)> =
            self.handoff.iter().map(|(id, (session, since))| (id.clone(), session.clone(), *since)).collect();
        for (id, session, since) in moving {
            let Some(project) = self.project(&id).cloned() else {
                self.handoff.remove(&id);
                continue;
            };
            let remote = self.remote.get(&session).cloned().unwrap_or_default();
            let len = sessions::transcript_len(project.path(), &session);
            let released = remote.released.filter(|released| released.at >= since);
            let ready = match released {
                Some(released) => len >= released.len && (!released.code || self.code_arrived(&session, released.at)),
                None => !remote.active(now),
            };
            if ready {
                self.handoff.remove(&id);
                self.presence.claim(&session, now);
                changed = true;
                self.paused.remove(&id);
                self.resumed.remove(&id);
                self.resume(&project, cx);
            } else if now.saturating_sub(since) > 180 {
                self.handoff.remove(&id);
                self.presence.takeover.remove(&session);
                changed = true;
                self.moved_from.remove(&id);
                self.errors.insert(
                    id,
                    (
                        Failure::Move,
                        tr!("workspace.device_didn_t_respond_for", device = remote.device),
                    ),
                );
            } else if self.pulled_for_move.is_none_or(|last| last.elapsed().as_secs() >= 5) {
                crate::sync::pull_now();
                self.pulled_for_move = Some(std::time::Instant::now());
            }
        }
        if changed {
            self.presence.save();
        }
    }

    /// What the project's pinned session is doing; `None` — no session, or the folder is gone.
    fn activity(&self, project: &Project) -> Option<Activity> {
        if !project.exists() {
            return None;
        }
        if self.starting.contains(project.id()) || self.handoff.contains_key(project.id()) || self.restoring.contains_key(project.id()) {
            return Some(Activity::Working);
        }
        // The session failed: the person decides (Try Again or a new session).
        if self.failed(project).is_some() {
            return Some(Activity::Waiting);
        }
        // The code from another computer didn't go into place: the person decides.
        if self.code_stuck.contains_key(project.id()) {
            return Some(Activity::Waiting);
        }
        let session = project.session()?;
        if self.is_paused(project) {
            return Some(Activity::Stopped);
        }
        // An open terminal hears the program itself: at once, and not a poll later.
        if let Some(report) = self.program_status.get(project.id()) {
            return Some(activity_of(report.state));
        }
        Some(self.agent(session).map(Agent::activity).unwrap_or(Activity::Stopped))
    }

    /// Why projects went on pause by themselves — `pause.log` in the data folder (a pause the person asked for
    /// is in `paused` and isn't logged). `None` — Claude Code didn't answer the poll.
    fn log_pauses(&self, before: Option<&HashSet<String>>) {
        let path = crate::sync::Paths::real().app.join("pause.log");
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|time| time.as_secs()).unwrap_or_default();
        let mut lines = Vec::new();
        match before {
            None => lines.push(format!("{stamp} claude agents didn't answer; previous list kept")),
            Some(before) => {
                for project in &self.projects {
                    if before.contains(project.id()) || self.paused.contains(project.id()) || !self.is_paused(project) {
                        continue;
                    }
                    let session = project.session().unwrap_or_default();
                    let why = match self.agents.iter().find(|agent| agent.session_id == session) {
                        Some(agent) => format!("claude reports state={:?} status={:?} pid={:?}", agent.state, agent.status, agent.pid),
                        None => format!("session not in claude agents ({} listed)", self.agents.len()),
                    };
                    lines.push(format!("{stamp} {} {}: {why}", project.id(), sessions::short_id(session)));
                }
            }
        }
        if lines.is_empty() {
            return;
        }
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 256 << 10) {
            let _ = std::fs::rename(&path, path.with_extension("log.old"));
        }
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            use std::io::Write as _;
            let _ = writeln!(file, "{}", lines.join("\n"));
        }
    }

    /// Project is paused: paused here, or its session was stopped (`claude stop`), and
    /// Continue hasn't been pressed yet. Pult doesn't start a session Claude Code doesn't know either.
    /// Opening such a project doesn't continue it: only the Continue button does.
    fn is_paused(&self, project: &Project) -> bool {
        let id = project.id();
        if self.paused.contains(id) {
            return true;
        }
        match project.session().and_then(|session| self.agent(session)) {
            Some(agent) => agent.is_stopped() && !self.resumed.contains(id),
            None => !self.resumed.contains(id),
        }
    }

    fn reload(&mut self) {
        self.projects = self.store.list();
    }

    fn is_moving(&self, project: &Project) -> bool {
        self.moving.as_deref().is_some_and(|dir| migrate::is_under(project.path(), dir))
    }

    pub(crate) fn root(&self) -> PathBuf {
        self.store.root().to_path_buf()
    }

    /// How many Pult projects are in the main folder (a split project is one).
    pub(crate) fn root_count(&self) -> usize {
        self.projects
            .iter()
            .filter(|project| self.store.in_root(project.path()))
            .map(Project::folder)
            .collect::<HashSet<_>>()
            .len()
    }

    pub(crate) fn font_size(&self, cx: &gpui_kit::App) -> f32 {
        self.terminal.read(cx).font_size()
    }

    pub(crate) fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| terminal.set_font_size(size, cx));
    }

    /// Pult projects inside the folder and what will happen to their session during the move.
    pub(crate) fn affected(&self, dir: &Path) -> Vec<Affected> {
        self.projects
            .iter()
            .filter(|project| migrate::is_under(project.path(), dir))
            .map(|project| {
                let (state, tone) = match (project.session(), self.activity(project)) {
                    (None, _) => (t("workspace.no_session"), Tone::Muted),
                    (_, Some(Activity::Working)) => (t("workspace.claude_is_working_current_turn"), Tone::Warning),
                    (_, Some(Activity::Stopped)) => (t("workspace.paused"), Tone::Muted),
                    _ => (t("workspace.will_pause_and_continue"), Tone::Muted),
                };
                Affected { folder: project.folder().to_string(), name: project.title(), state, tone }
            })
            .collect()
    }

    /// Claude Code sessions inside the folder that are open outside Pult (in another terminal or window).
    pub(crate) fn blockers(&self, dir: &Path, cx: &gpui_kit::App) -> Vec<PathBuf> {
        let terminal = self.terminal.read(cx);
        let ours: Vec<u32> = self.projects.iter().filter_map(|project| terminal.pid(project.id())).collect();
        migrate::blockers(&self.agents, dir, &ours)
    }

    pub(crate) fn refresh_agents(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
    }

    /// Before a move: close the terminals of the folder's projects and don't attach to them while the move runs.
    pub(crate) fn begin_move(&mut self, dir: &Path, cx: &mut Context<Self>) {
        self.moving = Some(dir.to_path_buf());
        let inside: Vec<Project> =
            self.projects.iter().filter(|project| migrate::is_under(project.path(), dir)).cloned().collect();
        self.moving_live = inside
            .iter()
            .filter(|project| project.session().is_some() && !self.is_paused(project))
            .map(|project| project.id().to_string())
            .collect();
        for project in &inside {
            self.terminal.update(cx, |terminal, cx| terminal.close(project.id(), cx));
            self.pending_bind.remove(project.id());
        }
        self.sync_terminal(cx);
        cx.notify();
    }

    /// Folder and Claude data moved: Pult switches to the new paths.
    pub(crate) fn finish_move(&mut self, to: &Path, target: &Target, cx: &mut Context<Self>) {
        match target {
            Target::Root => {
                self.store.move_root(to.to_path_buf());
                self.reload();
            }
            Target::Project(id) => {
                let folder = split_key(id).0.to_string();
                let before: Vec<String> = self.siblings(&folder).iter().map(|project| project.id().to_string()).collect();
                if let Ok(new_id) = self.store.relocate(id, to.to_path_buf()) {
                    self.reload();
                    let new_folder = split_key(&new_id).0.to_string();
                    for old in before {
                        self.rekey(&old, &key_for(&new_folder, split_key(&old).1));
                    }
                    if let Some(last) = self.last_sub.remove(&folder) {
                        self.last_sub.insert(new_folder.clone(), key_for(&new_folder, split_key(&last).1));
                    }
                }
            }
        }
        crate::journal::reset(&crate::journal::data_dir());
        self.moving = Some(to.to_path_buf());
        cx.notify();
    }

    /// Move finished: sessions are up — show them.
    pub(crate) fn end_move(&mut self, cx: &mut Context<Self>) {
        self.moving = None;
        for id in std::mem::take(&mut self.moving_live) {
            self.paused.remove(&id);
            self.resumed.insert(id);
        }
        self.refresh(cx);
        cx.notify();
    }

    /// Move failed and was rolled back.
    pub(crate) fn abort_move(&mut self, cx: &mut Context<Self>) {
        self.moving = None;
        self.moving_live.clear();
        self.refresh(cx);
        cx.notify();
    }

    /// The project's folder was renamed — its id (the folder name) changes to the new one everywhere.
    fn rekey(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        // Nothing lives under the new key yet; leftovers of a removed project with that name go.
        self.forget_state(new);
        if let Some(report) = self.program_status.remove(old) {
            self.program_status.insert(new.to_string(), report);
        }
        self.rekeyed.remove(new);
        self.rekeyed.insert(old.to_string(), new.to_string());
        if self.selected.as_deref() == Some(old) {
            self.selected = Some(new.to_string());
        }
        for entry in &mut self.history {
            if entry == old {
                *entry = new.to_string();
            }
        }
        for set in [&mut self.paused, &mut self.resumed, &mut self.switching, &mut self.starting, &mut self.moving_live] {
            if set.remove(old) {
                set.insert(new.to_string());
            }
        }
        if let Some(value) = self.handoff.remove(old) {
            self.handoff.insert(new.to_string(), value);
        }
        if let Some(value) = self.releasing.remove(old) {
            self.releasing.insert(new.to_string(), value);
        }
        if let Some(value) = self.pending_bind.remove(old) {
            self.pending_bind.insert(new.to_string(), value);
        }
        self.renamed.remove(old);
        self.choice.remove(old);
        self.transcripts.remove(old);
        self.errors.remove(old);
    }

    /// The key a project has now, given the key it had when something started (see `rekeyed`).
    fn live_key(&self, id: &str) -> String {
        let mut id = id.to_string();
        for _ in 0..4 {
            match self.rekeyed.get(&id) {
                Some(next) if self.project(&id).is_none() => id = next.clone(),
                _ => break,
            }
        }
        id
    }

    /// A project left Pult: nothing of its state may reach a project that gets the same key later.
    /// `starting` is left to the start that is running — it clears the flag itself.
    fn forget_state(&mut self, id: &str) {
        for set in [&mut self.paused, &mut self.resumed, &mut self.switching, &mut self.dropped, &mut self.moving_live] {
            set.remove(id);
        }
        self.pending_bind.remove(id);
        self.renamed.remove(id);
        self.choice.remove(id);
        self.transcripts.remove(id);
        self.errors.remove(id);
        self.program_status.remove(id);
        self.releasing.remove(id);
        if let Some((session, _)) = self.handoff.remove(id) {
            self.presence.takeover.remove(&session);
            self.presence.save();
        }
    }

    /// The projects of one folder: the project itself, or its subprojects in order.
    fn siblings(&self, folder: &str) -> Vec<&Project> {
        self.projects.iter().filter(|project| project.folder() == folder).collect()
    }

    /// The sidebar reports that a split project was folded or unfolded: remember it.
    fn set_folder_open(&mut self, folder: &str, open: bool, cx: &mut Context<Self>) {
        self.store.set_collapsed(folder, !open);
        if open && self.reveal.as_deref() == Some(folder) {
            self.reveal = None;
        }
        cx.notify();
    }

    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.open_settings_at(None, cx);
    }

    /// Settings on a given page (an index in `settings::PAGES`): a window that is already open is closed and
    /// opened again — a Settings window can't be told to switch its page.
    pub(crate) fn open_settings_at(&mut self, page: Option<usize>, cx: &mut Context<Self>) {
        if let Some(handle) = self.settings_window {
            if page.is_none() && handle.update(cx, |_, window, _| window.activate_window()).is_ok() {
                return;
            }
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        // The window opens after Pult is updated: its first frame reads Pult.
        let weak = cx.entity().downgrade();
        cx.spawn(async move |this, cx| {
            let handle = cx.update(|cx| crate::settings::open(weak, page, cx));
            let _ = this.update(cx, |this, _| this.settings_window = handle);
        })
        .detach();
    }

    /// Reread who Claude Code talks to and what is saved for GLM (a few small files).
    pub(crate) fn refresh_models(&mut self) {
        let paths = crate::sync::Paths::real();
        let config = crate::models::load(&paths);
        self.provider = if config.default_glm { crate::models::Provider::Glm } else { crate::models::Provider::Claude };
        self.glm_key = config.has_key();
        self.glm_main = config.main;
    }

    pub(crate) fn provider(&self) -> crate::models::Provider {
        self.provider
    }

    /// The models a project's sessions use: its own choice, else Pult's default.
    pub(crate) fn provider_of(&self, project: &Project) -> crate::models::Provider {
        project.provider().and_then(crate::models::provider_from_key).unwrap_or(self.provider)
    }

    /// The `--settings` value a session of this project is started with. Without a GLM key a GLM project can't
    /// start: the error says where to add it.
    fn models_flag(&self, project: &Project) -> Result<String, String> {
        crate::models::session_flag(&crate::sync::Paths::real(), self.provider_of(project)).map_err(|_| t("models.need_key").to_string())
    }

    /// Asked for from Settings: the switch runs in the main window at its next frame.
    pub(crate) fn request_provider(&mut self, to: crate::models::Provider, cx: &mut Context<Self>) {
        self.pending_provider = Some(to);
        cx.notify();
    }

    fn use_claude(&mut self, _: &UseClaude, window: &mut Window, cx: &mut Context<Self>) {
        self.use_provider(crate::models::Provider::Claude, window, cx);
    }

    fn use_glm(&mut self, _: &UseGlm, window: &mut Window, cx: &mut Context<Self>) {
        self.use_provider(crate::models::Provider::Glm, window, cx);
    }

    fn open_models(&mut self, _: &OpenModels, _: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_at(Some(crate::settings::MODELS_PAGE), cx);
    }

    /// The chip, the menu and ⌘K: the current project's models.
    pub(crate) fn use_provider(&mut self, to: crate::models::Provider, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_models();
        if let Some(id) = self.selected.clone() {
            self.switch_project_provider(&id, to, false, window, cx);
        }
    }

    /// Move one project to `to`: its choice is saved (and shared), and its session comes back on the new models —
    /// a running one at once (a turn in progress asks first), a stopped one when it is continued.
    fn switch_project_provider(&mut self, id: &str, to: crate::models::Provider, confirmed: bool, window: &mut Window, cx: &mut Context<Self>) {
        use crate::models::Provider;
        let Some(project) = self.project(id).cloned() else {
            return;
        };
        if self.provider_of(&project) == to {
            return;
        }
        if to == Provider::Glm && !self.glm_key {
            self.open_settings_at(Some(crate::settings::MODELS_PAGE), cx);
            window.push_notification(t("models.need_key"), cx);
            return;
        }
        let flag = match crate::models::session_flag(&crate::sync::Paths::real(), to) {
            Ok(flag) => flag,
            Err(_) => {
                window.push_notification(t("models.need_key"), cx);
                return;
            }
        };
        let agent = project.session().and_then(|session| self.agent(session)).cloned();
        let job = agent.as_ref().filter(|agent| agent.is_background()).and_then(|agent| agent.id.clone());
        let running = agent.as_ref().is_some_and(|agent| agent.is_background() && agent.is_running()) && !self.is_paused(&project);
        let working = running && agent.as_ref().is_some_and(|agent| matches!(agent.activity(), Activity::Working | Activity::Waiting));
        let name = crate::switch::provider_name(to);
        if working && !confirmed {
            let workspace = cx.entity().downgrade();
            let id = id.to_string();
            let title = tr!("models.project_ask_title", name = project.name(), to = name);
            window.open_alert_dialog(cx, move |alert, _, _| {
                let (workspace, id) = (workspace.clone(), id.clone());
                alert
                    .title(SharedString::from(title.clone()))
                    .description(t("workspace.claude_is_working_right_now_2"))
                    .show_cancel(true)
                    .cancel_text(t("common.cancel"))
                    .ok_text(t("models.project_switch"))
                    .ok_variant(ButtonVariant::Danger)
                    .on_ok(move |_, window, cx| {
                        let _ = workspace.update(cx, |this, cx| this.switch_project_provider(&id, to, true, window, cx));
                        true
                    })
            });
            return;
        }
        self.store.set_provider(id, crate::models::provider_key(to));
        self.reload();
        let title = project.name().to_string();
        match job {
            Some(short) if running => {
                if !self.starting.insert(id.to_string()) {
                    return;
                }
                self.terminal.update(cx, |terminal, cx| terminal.close(id, cx));
                self.errors.remove(id);
                let (id, pid) = (id.to_string(), agent.and_then(|agent| agent.pid));
                cx.spawn_in(window, async move |this, cx| {
                    let result = cx.background_spawn(async move { sessions::switch_settings(&short, pid, &flag) }).await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        let id = this.live_key(&id);
                        this.starting.remove(&id);
                        match result {
                            Ok(()) => window.push_notification(tr!("models.project_done", name = title, to = name), cx),
                            Err(StartError::NotTrusted) => window.push_notification(t("workspace.claude_code_doesn_t_trust"), cx),
                            Err(StartError::Failed(message)) => {
                                this.errors.insert(id.clone(), (Failure::Resume, message));
                            }
                        }
                        this.sync_terminal(cx);
                        this.refresh(cx);
                        cx.notify();
                    });
                })
                .detach();
            }
            Some(short) => {
                cx.background_spawn(async move { sessions::set_settings_flag(&short, &flag) }).detach();
                window.push_notification(tr!("models.project_later", name = title, to = name), cx);
            }
            None => window.push_notification(tr!("models.project_later", name = title, to = name), cx),
        }
        cx.notify();
    }

    /// Settings → Models: every project moves to `to`, and new projects get it too. Running sessions restart
    /// (`switch`), stopped ones come back on `to` when they are continued.
    pub(crate) fn use_provider_everywhere(&mut self, to: crate::models::Provider, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_models();
        if to == crate::models::Provider::Glm && !self.glm_key {
            self.open_settings_at(Some(crate::settings::MODELS_PAGE), cx);
            window.push_notification(t("models.need_key"), cx);
            return;
        }
        let paths = crate::sync::Paths::real();
        let flag = match crate::switch::prepare(&paths, to) {
            Ok(flag) => flag,
            Err(reason) => {
                window.push_notification(tr!("models.write_failed", reason = reason), cx);
                return;
            }
        };
        let targets = self.model_targets();
        self.models_everywhere(to, &flag, &targets);
        self.refresh_models();
        if targets.is_empty() {
            window.push_notification(tr!("models.done", to = crate::switch::provider_name(to)), cx);
        } else {
            crate::switch::start(cx.entity().downgrade(), to, flag, targets, window, cx);
        }
        cx.notify();
    }

    /// Every project's choice becomes `to`; the stopped sessions are told to come back on it.
    fn models_everywhere(&mut self, to: crate::models::Provider, flag: &str, running: &[crate::switch::Target]) {
        self.store.set_providers(crate::models::provider_key(to), true);
        for project in &self.projects {
            let Some(agent) = project.session().and_then(|session| self.agent(session)) else {
                continue;
            };
            if let Some(short) = agent.id.clone().filter(|_| agent.is_background())
                && !running.iter().any(|target| target.short == short)
            {
                sessions::set_settings_flag(&short, flag);
            }
        }
        self.reload();
    }

    /// The running background sessions of Pult's projects: what a switch of models restarts.
    fn model_targets(&self) -> Vec<crate::switch::Target> {
        self.projects
            .iter()
            .filter_map(|project| {
                let agent = self.agent(project.session()?)?;
                let short = agent.id.clone().filter(|_| agent.is_background() && agent.is_running())?;
                Some(crate::switch::Target {
                    id: project.id().to_string(),
                    name: project.name().to_string(),
                    short,
                    pid: agent.pid,
                    working: agent.activity() != Activity::Ready,
                })
            })
            .collect()
    }

    /// The switch is about to restart these projects' sessions: their terminals attach to processes that are
    /// about to end, and the placeholder "starting" shows meanwhile.
    pub(crate) fn models_restarting(&mut self, ids: &[String], cx: &mut Context<Self>) {
        for id in ids {
            self.terminal.update(cx, |terminal, cx| terminal.close(id, cx));
            self.paused.remove(id);
            self.starting.insert(id.clone());
        }
        self.sync_terminal(cx);
        cx.notify();
    }

    /// The switch is over (or failed): the terminals attach again once the sessions are seen running.
    pub(crate) fn models_switched(&mut self, ids: &[String], cx: &mut Context<Self>) {
        for id in ids {
            self.starting.remove(id);
        }
        self.refresh_models();
        self.refresh(cx);
        cx.notify();
    }

    fn rename_project(&mut self, _: &RenameProject, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.current().cloned().filter(Project::exists) else {
            return;
        };
        rename::open(
            cx.entity().downgrade(),
            Target::Project(project.id().to_string()),
            project.path().to_path_buf(),
            window,
            cx,
        );
    }

    fn select(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(id) {
            return;
        }
        self.history.truncate(self.history_index + 1);
        self.history.push(id.to_string());
        self.history_index = self.history.len() - 1;
        self.show(Some(id.to_string()), window, cx);
    }

    fn show(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        // Attaching to a background session is a separate claude process. When leaving the project we close
        // it: the session lives on, and Claude Code can unload an idle one from memory.
        if let Some(previous) = self.selected.clone()
            && Some(&previous) != id.as_ref()
            && !self.pending_bind.contains_key(&previous)
        {
            let attached = self
                .project(&previous)
                .and_then(Project::session)
                .and_then(|session| self.agent(session))
                .is_some_and(Agent::is_background);
            if attached {
                self.terminal.update(cx, |terminal, cx| terminal.close(&previous, cx));
            }
        }
        self.selected = id;
        if let Some(selected) = self.selected.clone() {
            self.store.set_last_selected(&selected);
        }
        if let Some(project) = self.current()
            && project.sub().is_some()
        {
            let (folder, id) = (project.folder().to_string(), project.id().to_string());
            if self.store.is_collapsed(&folder) {
                self.reveal = Some(folder.clone());
            }
            self.last_sub.insert(folder, id);
        }
        // Session list to choose from — fresh each time a project without a session is opened.
        if let Some(project) = self.current().cloned()
            && (project.session().is_none() || self.switching.contains(project.id()))
        {
            self.load_transcripts(project.id());
        }
        self.sync_terminal(cx);
        self.refresh_neighbours();
        self.terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        cx.notify();
    }

    /// Bring the current project's terminal in line with its session.
    fn sync_terminal(&mut self, cx: &mut Context<Self>) {
        // Opened a project whose session came from another computer: Claude Code here doesn't know it,
        // but the conversation is already here — start it right away, as if it ran here. One paused
        // in this Pult is left alone.
        if let Some(project) = self.current().cloned()
            && let Some(session) = project.session()
            && self.agents_loaded
            && project.exists()
            && !self.is_moving(&project)
            && self.agent(session).is_none()
            && !self.paused.contains(project.id())
            && !self.starting.contains(project.id())
            && !self.resumed.contains(project.id())
            && !self.errors.contains_key(project.id())
            && !self.pending_bind.contains_key(project.id())
            && !self.handoff.contains_key(project.id())
            && !self.restoring.contains_key(project.id())
            && !self.code_stuck.contains_key(project.id())
            && self.elsewhere(&project).is_none()
            && sessions::has_transcript(project.path(), session)
        {
            self.resume(&project, cx);
            return;
        }
        // A session Claude Code here knows, coming back from another computer with newer code: lay it down before
        // `attach` starts the session.
        if let Some(project) = self.current().cloned()
            && let Some(session) = project.session()
            && self.agents_loaded
            && project.exists()
            && !self.is_moving(&project)
            && !self.is_paused(&project)
            && !self.terminal.read(cx).is_open(project.id())
            && !self.restoring.contains_key(project.id())
            && !self.code_stuck.contains_key(project.id())
            && !self.handoff.contains_key(project.id())
            && self.elsewhere(&project).is_none()
            && self.agent(session).is_some_and(|agent| agent.is_background() && !agent.is_running())
            && let Some(manifest) = crate::code::pending(&crate::sync::Paths::real(), session)
        {
            self.restore_code(&project, manifest, cx);
            return;
        }
        let target = self.current().cloned().and_then(|project| {
            if !project.exists() || self.is_moving(&project) {
                return None;
            }
            // The session runs on another computer or is being moved here — don't attach: `attach`
            // would start it a second time.
            // Being restarted (a switch of models, a restart without prompts): `attach` would start the session
            // that is being stopped and race with the restart.
            if self.handoff.contains_key(project.id())
                || self.restoring.contains_key(project.id())
                || self.code_stuck.contains_key(project.id())
                || self.starting.contains(project.id())
                || self.elsewhere(&project).is_some()
            {
                return None;
            }
            let id = project.id().to_string();
            if self.pending_bind.contains_key(&id) {
                return Some((id, None));
            }
            let agent = self.agent(project.session()?)?.clone();
            // Claude Code keeps a background session itself, even unloaded from memory: `attach` starts it.
            if agent.is_background() && !self.is_paused(&project) {
                let short = agent.id.clone()?;
                return Some((id, Some((project.path().to_path_buf(), short))));
            }
            if !agent.is_running() {
                return None;
            }
            // Open in a window: show it only if that is our own terminal's process.
            let ours = self.terminal.read(cx).pid(&id) == agent.pid;
            ours.then_some((id, None))
        });
        self.terminal.update(cx, |terminal, cx| match target {
            Some((id, Some((cwd, short)))) => {
                terminal.open(&id, cwd, sessions::shell_args(&["attach", &short]), cx);
                terminal.show(Some(&id), cx);
            }
            Some((id, None)) => terminal.show(Some(&id), cx),
            None => terminal.show(None, cx),
        });
    }

    /// The live registry was read (every second between full polls): what the running sessions say about
    /// themselves goes over the list, and a process that came or went asks for a new list sooner.
    fn on_live(&mut self, live: HashMap<u32, sessions::Live>, cx: &mut Context<Self>) {
        self.live = live;
        let listed: HashSet<u32> = self.agents.iter().filter_map(|agent| agent.pid).collect();
        let new_process = self.live.keys().any(|pid| !listed.contains(pid) && !self.unlisted.contains(pid));
        let gone = self.agents.iter().filter_map(|agent| agent.pid).any(|pid| !sessions::alive(pid));
        if new_process || gone {
            self.want_full = true;
        }
        if self.overlay_live() {
            self.enforce_names(cx);
            cx.notify();
        }
    }

    /// Lay the registry over the list. A pid whose start time isn't the one the list was read with is another
    /// process: it is skipped and a new list is asked for. Returns whether anything shown changed.
    fn overlay_live(&mut self) -> bool {
        let mut changed = false;
        let mut switched: Vec<(String, String)> = Vec::new();
        let mut renewed = false;
        for agent in &mut self.agents {
            let Some(pid) = agent.pid else { continue };
            let Some(live) = self.live.get(&pid) else { continue };
            match self.proc_starts.get(&pid) {
                Some(start) if *start == live.proc_start => {}
                _ => {
                    renewed = true;
                    continue;
                }
            }
            // `/clear`, `/resume` of another session and `/branch` move a running process to another session.
            if live.session_id != agent.session_id {
                switched.push((agent.session_id.clone(), live.session_id.clone()));
                agent.session_id = live.session_id.clone();
                changed = true;
            }
            changed |= agent.apply_live(live);
        }
        if renewed {
            self.want_full = true;
        }
        for (old, new) in switched {
            changed |= self.follow_switch(&old, &new);
        }
        changed
    }

    /// The process of a project's pinned session went on in another session (`/clear`, `/branch`, `/resume`
    /// typed in a window of its own): the project follows it, as it would otherwise show a pause while Claude is
    /// at work in the next one and Continue would start a copy. A session pinned elsewhere is never taken.
    fn follow_switch(&mut self, old: &str, new: &str) -> bool {
        if self.moving.is_some() {
            return false;
        }
        let Some(project) = self.projects.iter().find(|project| project.session() == Some(old)).cloned() else {
            return false;
        };
        if self.projects.iter().any(|other| other.session() == Some(new)) {
            return false;
        }
        self.store.bind(project.id(), new);
        self.bound_here.insert(new.to_string());
        self.reload();
        self.notices.push(Notification::info(tr!("workspace.session_followed", name = project.title())));
        true
    }

    fn on_agents(&mut self, (agents, memory, missing, live): Polled, cx: &mut Context<Self>) {
        self.polling = false;
        self.refresh_models();
        self.last_full = Some(std::time::Instant::now());
        // An empty list right after a non-empty one is believed only the third time: `--all` lists stopped
        // sessions too, so all of them vanishing at once means an overloaded supervisor, not a fact.
        let agents = agents.filter(|agents| {
            let doubtful = agents.is_empty() && !self.agents.is_empty() && self.empty_polls < 2;
            self.empty_polls = if agents.is_empty() { self.empty_polls.saturating_add(1) } else { 0 };
            !doubtful
        });
        // Claude Code didn't answer this time: keep what it said last, instead of showing every session paused.
        let Some(agents) = agents else {
            self.log_pauses(None);
            return;
        };
        let before: HashSet<String> =
            self.projects.iter().filter(|project| self.is_paused(project)).map(|project| project.id().to_string()).collect();
        self.agents = agents;
        // The registry as it was when the list was read: the start times to tell processes apart by, and the
        // processes the list doesn't know about (not worth another list).
        self.proc_starts = live.iter().map(|(pid, live)| (*pid, live.proc_start.clone())).collect();
        let listed: HashSet<u32> = self.agents.iter().filter_map(|agent| agent.pid).collect();
        self.unlisted = live.keys().copied().filter(|pid| !listed.contains(pid)).collect();
        self.live = live;
        self.overlay_live();
        self.log_pauses(Some(&before));
        self.memory = memory;
        self.claude_missing = missing;
        self.agents_loaded = true;
        let paths = crate::sync::Paths::real();
        self.code_rows = self
            .projects
            .iter()
            .filter_map(Project::session)
            .map(|session| (session.to_string(), crate::code::summary(&paths, session)))
            .collect();
        self.track_presence(cx);
        // Started: the steps of a move are no longer needed.
        self.moved_from.retain(|id, _| {
            self.starting.contains(id) || self.restoring.contains_key(id) || self.handoff.contains_key(id) || self.code_stuck.contains_key(id)
        });
        self.share_states();
        // Projects and sessions from other computers (they arrive with sync). Not during a folder
        // move: the list briefly disagrees with the disk then.
        if self.moving.is_none() {
            let adopted = self.store.adopt_shared(&crate::sync::shared_dir(&crate::sync::Paths::real()), sessions::session_has_conversation);
            for id in &adopted.removed {
                self.forget_state(id);
                self.history.retain(|entry| entry != id);
                self.terminal.update(cx, |terminal, cx| terminal.close(id, cx));
            }
            self.history_index = self.history.len().saturating_sub(1);
            if adopted.changed {
                self.reload();
                if self.projects.iter().any(|project| project.sub().is_some()) {
                    crate::journal::ensure_hooks();
                }
                if let Some(gone) = self.selected.clone().filter(|id| self.project(id).is_none()) {
                    let same_folder = self.siblings(split_key(&gone).0).first().map(|project| project.id().to_string());
                    self.selected = same_folder
                        .or_else(|| self.history.last().cloned())
                        .or_else(|| self.projects.first().map(|project| project.id().to_string()));
                }
            }
        }
        // A pinned session that exists neither in Claude Code nor as a conversation on disk (started and closed without
        // writing anything) is dropped: it can't be started, and Continue would hit "session not found".
        let empty: Vec<(String, String)> = self
            .projects
            .iter()
            .filter(|project| project.exists() && !self.is_moving(project))
            .filter(|project| !self.starting.contains(project.id()) && !self.pending_bind.contains_key(project.id()))
            .filter_map(|project| {
                let session = project.session()?;
                (!self.bound_here.contains(session)
                    && self.agent(session).is_none()
                    && !sessions::has_transcript(project.path(), session))
                    .then(|| (project.id().to_string(), session.to_string()))
            })
            .collect();
        for (id, session) in &empty {
            self.store.forget_session(id, session);
            self.dropped.insert(id.clone());
        }
        if !empty.is_empty() {
            self.reload();
        }
        // Pult creates project folders that don't exist here. Not during a folder move: the old
        // path is already free there, and an empty folder would take its place.
        if self.moving.is_none() && !self.store.ensure_folders().is_empty() {
            self.reload();
        }
        // A session started in the project terminal showed up in the list — pin it.
        let found: Vec<(String, String)> = self
            .pending_bind
            .iter()
            .filter_map(|(id, pid)| {
                let agent = self.agents.iter().find(|agent| agent.pid == Some(*pid))?;
                Some((id.clone(), agent.session_id.clone()))
            })
            .collect();
        for (id, session) in found {
            self.pending_bind.remove(&id);
            self.store.bind(&id, &session);
            self.bound_here.insert(session.clone());
            self.reload();
        }
        // The project terminal closed and no session was chosen — forget the pending bind.
        let closed: Vec<String> = self
            .pending_bind
            .keys()
            .filter(|id| !self.terminal.read(cx).is_open(id))
            .cloned()
            .collect();
        for id in closed {
            self.pending_bind.remove(&id);
        }
        self.sync_terminal(cx);
        self.enforce_names(cx);
        cx.notify();
    }

    /// A project's main session is always named `<project>-main`. Pult fixes any other name itself:
    /// when the session is free (turn finished), attached in Pult's terminal, and the input line is empty,
    /// it sends `/rename`. While Claude is working it stays out of the way.
    fn enforce_names(&mut self, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        // A command typed earlier: it worked (the name is right now), or it is still sitting in the input — then
        // it is taken out again and not tried any more. Nothing of Pult's stays in the user's input.
        let checks: Vec<(String, String, bool)> = self
            .typed
            .iter()
            .filter(|(_, (_, at))| now.duration_since(*at).as_secs() >= 8)
            .map(|(id, (command, _))| {
                let done = self
                    .project(id)
                    .and_then(|project| Some((project.session_name(), self.agent(project.session()?)?.name.clone()?)))
                    .is_some_and(|(expected, name)| expected == name);
                (id.clone(), command.clone(), done)
            })
            .collect();
        for (id, command, done) in checks {
            self.typed.remove(&id);
            if !done {
                self.terminal.update(cx, |terminal, _| terminal.erase_typed(&id, &command));
                self.rename_failed.insert(id);
            }
        }
        // Leftovers of earlier versions: `/rename <this project's name>` stuck in the input of a session (typed
        // while it was starting). Only Pult types exactly that, so it is taken out once the terminal has settled —
        // in its first half minute only: later the same text may be the user's own.
        let leftovers: Vec<(String, Vec<String>)> = self
            .projects
            .iter()
            .filter(|project| !self.rename_failed.contains(project.id()) && !self.typed.contains_key(project.id()))
            .filter(|project| self.terminal.read(cx).age(project.id()).is_some_and(|age| (5..30).contains(&age.as_secs())))
            .map(|project| {
                let names = [project.session_name(), sessions::main_name(project.folder())];
                (project.id().to_string(), names.iter().map(|name| format!("/rename {name}")).collect())
            })
            .collect();
        for (id, commands) in leftovers {
            for command in commands {
                if self.terminal.update(cx, |terminal, _| terminal.erase_typed(&id, &command)) {
                    break;
                }
            }
        }
        let targets: Vec<(String, String)> = self
            .projects
            .iter()
            .filter(|project| !self.is_moving(project) && !self.rename_failed.contains(project.id()))
            .filter_map(|project| {
                let agent = self.agent(project.session()?)?;
                let expected = project.session_name();
                let ours = agent.is_background() || self.terminal.read(cx).pid(project.id()) == agent.pid;
                let due = self
                    .renamed
                    .get(project.id())
                    .is_none_or(|last| now.duration_since(*last).as_secs() >= 30);
                // Only a session that is up and waiting for input, whose name Claude Code knows and which has been
                // on screen for a few seconds. A paused or just-starting session reports no name (and comes back
                // with its own name from the conversation), and keys typed while it starts get lost.
                let settled = self.terminal.read(cx).age(project.id()).is_some_and(|age| age.as_secs() >= 5);
                (ours
                    && due
                    && settled
                    && agent.is_running()
                    && agent.status.as_deref() == Some("idle")
                    && agent.name.as_deref().is_some_and(|name| name != expected))
                .then(|| (project.id().to_string(), expected))
            })
            .collect();
        for (id, name) in targets {
            let command = format!("/rename {name}");
            let sent = self.terminal.update(cx, |terminal, _| terminal.send_command(&id, &command));
            if sent {
                self.renamed.insert(id.clone(), now);
                self.typed.insert(id, (command, now));
            }
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.polling, true) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async { poll_agents() }).await;
            let _ = this.update(cx, |this, cx| this.on_agents(result, cx));
        })
        .detach();
    }

    /// Start the pinned session in the background from where it left off.
    fn resume(&mut self, project: &Project, cx: &mut Context<Self>) {
        let Some(session) = project.session().map(str::to_string) else {
            return;
        };
        let id = project.id().to_string();
        if self.elsewhere(project).is_some() {
            self.take_over(project, cx);
            return;
        }
        if self.restoring.contains_key(&id) || self.code_stuck.contains_key(&id) {
            return;
        }
        // Code from another computer comes first: the session starts in it.
        if let Some(manifest) = crate::code::pending(&crate::sync::Paths::real(), &session) {
            self.restore_code(project, manifest, cx);
            return;
        }
        self.paused.remove(&id);
        self.resumed.insert(id.clone());
        // The Claude Code supervisor has the session (even unloaded) — attach, `attach` will start it.
        // `--resume --bg` would create a copy with a new id in that case.
        if self.agent(&session).is_some_and(Agent::is_background) {
            self.errors.remove(&id);
            self.sync_terminal(cx);
            cx.notify();
            return;
        }
        if !self.starting.insert(id.clone()) {
            return;
        }
        self.errors.remove(&id);
        let flag = match self.models_flag(&project) {
            Ok(flag) => flag,
            Err(message) => {
                self.starting.remove(&id);
                self.errors.insert(id, (Failure::Resume, message));
                cx.notify();
                return;
            }
        };
        let cwd = project.path().to_path_buf();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let (cwd, session) = (cwd.clone(), session.clone());
                    async move { sessions::resume(&cwd, &session, &flag) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let id = this.live_key(&id);
                this.starting.remove(&id);
                match result {
                    Ok(None) => {}
                    // Claude Code started a copy of the conversation under a new id: pin it, so the next Continue
                    // doesn't make another one.
                    Ok(Some(copy)) => {
                        this.store.bind(&id, &copy);
                        this.reload();
                    }
                    // The folder isn't trusted yet: Claude Code will ask itself — run it right in the terminal.
                    Err(StartError::NotTrusted) => this.run_in_terminal(&id, cwd, &["--resume", &session, sessions::SKIP_PERMISSIONS], cx),
                    Err(StartError::Failed(message)) => {
                        this.errors.insert(id.clone(), (Failure::Resume, message));
                    }
                }
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Claude Code right in the project terminal; its session id gets pinned once it appears.
    fn run_in_terminal(&mut self, id: &str, cwd: PathBuf, args: &[&str], cx: &mut Context<Self>) {
        let args = sessions::shell_args(args);
        let pid = self.terminal.update(cx, |terminal, cx| {
            terminal.close(id, cx);
            terminal.open(id, cwd, args, cx);
            terminal.pid(id)
        });
        if let Some(pid) = pid {
            self.pending_bind.insert(id.to_string(), pid);
        }
        self.sync_terminal(cx);
    }

    fn new_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project(id).cloned() else {
            return;
        };
        let flag = match self.models_flag(&project) {
            Ok(flag) => flag,
            Err(message) => {
                window.push_notification(message, cx);
                return;
            }
        };
        if !self.starting.insert(id.to_string()) {
            return;
        }
        self.switching.remove(id);
        self.errors.remove(id);
        self.dropped.remove(id);
        self.terminal.update(cx, |terminal, cx| terminal.close(id, cx));
        let old = project.session().map(str::to_string);
        let (id, cwd, name) = (id.to_string(), project.path().to_path_buf(), project.session_name());
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn({
                    let (cwd, name) = (cwd.clone(), name.clone());
                    async move { sessions::start_new(&cwd, &name, &flag) }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let id = this.live_key(&id);
                this.starting.remove(&id);
                match result {
                    Ok(session) => {
                        this.store.bind(&id, &session);
                        this.bound_here.insert(session.clone());
                        this.reload();
                        this.notify_bound(&id, old, window, cx);
                    }
                    Err(StartError::NotTrusted) => this.run_in_terminal(&id, cwd, &["-n", &name, sessions::SKIP_PERMISSIONS], cx),
                    Err(StartError::Failed(message)) => {
                        this.errors.insert(id.clone(), (Failure::New, message));
                    }
                }
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Pin the session selected in the list.
    fn bind_choice(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.choice.remove(id) else {
            return;
        };
        // One conversation belongs to one subproject.
        let folder = split_key(id).0;
        if let Some(owner) = self
            .siblings(folder)
            .into_iter()
            .find(|other| other.id() != id && other.session() == Some(session.as_str()))
        {
            window.push_notification(tr!("workspace.session_belongs_to_subproject", name = owner.name()), cx);
            return;
        }
        let old = self.project(id).and_then(Project::session).map(str::to_string);
        self.switching.remove(id);
        self.terminal.update(cx, |terminal, cx| terminal.close(id, cx));
        self.store.bind(id, &session);
        self.bound_here.insert(session.clone());
        self.reload();
        self.notify_bound(id, old, window, cx);
        if let Some(project) = self.project(id).cloned()
            && self.activity(&project) == Some(Activity::Stopped)
        {
            self.resume(&project, cx);
        }
        self.sync_terminal(cx);
        cx.notify();
    }

    fn notify_bound(&mut self, id: &str, old: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        match old {
            Some(old) => {
                let workspace = cx.entity().downgrade();
                let id = id.to_string();
                window.push_notification(
                    Notification::new()
                        .message(t("workspace.pinned_another_session_previous_one"))
                        .action(move |_, _, _| {
                            let (workspace, id, old) = (workspace.clone(), id.clone(), old.clone());
                            Button::new("restore-session").outline().small().label(t("workspace.restore")).on_click(
                                move |_, window, cx| {
                                    let _ = workspace
                                        .update(cx, |this, cx| this.restore(&id, &old, window, cx));
                                },
                            )
                        }),
                    cx,
                );
            }
            None => {
                let name = self.project(id).map(|project| project.name().to_string()).unwrap_or_default();
                window.push_notification(tr!("workspace.session_pinned_to_name", name = name), cx);
            }
        }
    }

    /// Restore a previous session.
    fn restore(&mut self, id: &str, session: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.choice.insert(id.to_string(), session.to_string());
        self.bind_choice(id, window, cx);
    }

    /// Reread the folder's conversations for choosing a session.
    fn load_transcripts(&mut self, id: &str) {
        if let Some(project) = self.project(id) {
            let taken: HashSet<&str> = self
                .siblings(project.folder())
                .into_iter()
                .filter(|other| other.id() != id)
                .filter_map(|other| other.session())
                .collect();
            let mut list = sessions::transcripts(project.path());
            list.retain(|transcript| !taken.contains(transcript.id.as_str()));
            self.transcripts.insert(id.to_string(), list);
        }
    }

    fn start_switch(&mut self, id: &str, cx: &mut Context<Self>) {
        self.load_transcripts(id);
        if self.project(id).is_some_and(|project| project.session().is_some()) {
            self.switching.insert(id.to_string());
        }
        self.info_open = true;
        cx.notify();
    }

    fn pause(&mut self, id: &str, confirmed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project(id).cloned() else {
            return;
        };
        let Some(agent) = project.session().and_then(|session| self.agent(session)).cloned() else {
            return;
        };
        if self.is_paused(&project) {
            return;
        }
        if agent.activity() == Activity::Working && !confirmed {
            let workspace = cx.entity().downgrade();
            let id = id.to_string();
            let title = tr!("workspace.stop_claude_in_name", name = project.name());
            window.open_alert_dialog(cx, move |alert, _, _| {
                let (workspace, id) = (workspace.clone(), id.clone());
                alert
                    .title(SharedString::from(title.clone()))
                    .description(t("workspace.claude_is_working_right_now"))
                    .show_cancel(true)
                    .cancel_text(t("common.cancel"))
                    .ok_text(t("workspace.stop"))
                    .ok_variant(ButtonVariant::Danger)
                    .on_ok(move |_, window, cx| {
                        let _ = workspace.update(cx, |this, cx| this.pause(&id, true, window, cx));
                        true
                    })
            });
            return;
        }
        self.terminal.update(cx, |terminal, cx| terminal.close(id, cx));
        self.pending_bind.remove(id);
        self.paused.insert(id.to_string());
        self.resumed.remove(id);
        if let Some(short) = agent.id.clone().filter(|_| agent.is_background()) {
            cx.background_spawn(async move { sessions::stop(&short) }).detach();
        }
        self.sync_terminal(cx);
        window.push_notification(tr!("workspace.name_is_paused_conversation_is", name = project.name()), cx);
        self.refresh(cx);
        cx.notify();
    }

    /// Whether the project's session can be restarted: a background session that runs or idles here, and nothing
    /// else is being done to it (starting, handing over, moving to another computer).
    fn can_restart(&self, project: &Project) -> bool {
        if self.elsewhere(project).is_some() || self.is_paused(project) {
            return false;
        }
        if self.starting.contains(project.id()) || self.handoff.contains_key(project.id()) {
            return false;
        }
        project
            .session()
            .and_then(|session| self.agent(session))
            .is_some_and(|agent| agent.is_background() && agent.id.is_some() && agent.activity() != Activity::Stopped)
    }

    fn restart_current(&mut self, _: &RestartSession, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.selected.clone() {
            self.restart_session(&id, false, window, cx);
        }
    }

    /// Restart the project's session: `claude stop`, wait for the process, `claude respawn` — the same id, name,
    /// flags and worktree; the conversation goes on. A turn that runs or waits for a decision is cut off, so it
    /// asks first.
    fn restart_session(&mut self, id: &str, confirmed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project(id).cloned() else {
            return;
        };
        if !self.can_restart(&project) {
            return;
        }
        let Some(agent) = project.session().and_then(|session| self.agent(session)).cloned() else {
            return;
        };
        let Some(short) = agent.id.clone() else {
            return;
        };
        if matches!(agent.activity(), Activity::Working | Activity::Waiting) && !confirmed {
            let workspace = cx.entity().downgrade();
            let id = id.to_string();
            let title = tr!("workspace.restart_name", name = project.name());
            window.open_alert_dialog(cx, move |alert, _, _| {
                let (workspace, id) = (workspace.clone(), id.clone());
                alert
                    .title(SharedString::from(title.clone()))
                    .description(t("workspace.claude_is_working_right_now_2"))
                    .show_cancel(true)
                    .cancel_text(t("common.cancel"))
                    .ok_text(t("workspace.restart"))
                    .ok_variant(ButtonVariant::Danger)
                    .on_ok(move |_, window, cx| {
                        let _ = workspace.update(cx, |this, cx| this.restart_session(&id, true, window, cx));
                        true
                    })
            });
            return;
        }
        if !self.starting.insert(id.to_string()) {
            return;
        }
        self.terminal.update(cx, |terminal, cx| terminal.close(id, cx));
        self.errors.remove(id);
        let (id, pid, name) = (id.to_string(), agent.pid, project.name().to_string());
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { sessions::restart(&short, pid) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let id = this.live_key(&id);
                this.starting.remove(&id);
                match result {
                    Ok(()) => window.push_notification(tr!("workspace.name_restarted", name = name), cx),
                    Err(StartError::NotTrusted) => window.push_notification(t("workspace.claude_code_doesn_t_trust"), cx),
                    Err(StartError::Failed(message)) => {
                        this.errors.insert(id.clone(), (Failure::Resume, message));
                    }
                }
                this.sync_terminal(cx);
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Restart the project's session without permission prompts (a working one — after confirmation).
    fn skip_permissions(&mut self, id: &str, confirmed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project(id).cloned() else {
            return;
        };
        let Some(agent) = project.session().and_then(|session| self.agent(session)).cloned() else {
            return;
        };
        let Some(short) = agent.id.clone().filter(|_| agent.is_background()) else {
            return;
        };
        if agent.activity() == Activity::Working && !confirmed {
            let workspace = cx.entity().downgrade();
            let id = id.to_string();
            let title = tr!("workspace.restart_name_without_prompts", name = project.name());
            window.open_alert_dialog(cx, move |alert, _, _| {
                let (workspace, id) = (workspace.clone(), id.clone());
                alert
                    .title(SharedString::from(title.clone()))
                    .description(t("workspace.claude_is_working_right_now_2"))
                    .show_cancel(true)
                    .cancel_text(t("common.cancel"))
                    .ok_text(t("workspace.restart"))
                    .ok_variant(ButtonVariant::Danger)
                    .on_ok(move |_, window, cx| {
                        let _ = workspace.update(cx, |this, cx| this.skip_permissions(&id, true, window, cx));
                        true
                    })
            });
            return;
        }
        if !self.starting.insert(id.to_string()) {
            return;
        }
        self.terminal.update(cx, |terminal, cx| terminal.close(id, cx));
        self.paused.remove(id);
        let (id, pid) = (id.to_string(), agent.pid);
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { sessions::relaunch_skipping_permissions(&short, pid) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let id = this.live_key(&id);
                this.starting.remove(&id);
                match result {
                    Ok(()) => window.push_notification(t("workspace.session_restarted_without_permission_prompts"), cx),
                    Err(StartError::NotTrusted) => window.push_notification(
                        t("workspace.claude_code_doesn_t_trust"),
                        cx,
                    ),
                    Err(StartError::Failed(message)) => {
                        this.errors.insert(id.clone(), (Failure::Resume, message));
                    }
                }
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Label and availability of the pause command for a project (the … menu, context menu, Session).
    fn pause_command(&self, project: &Project) -> (&'static str, bool) {
        if self.elsewhere(project).is_some() {
            return (t("workspace.continue_here"), true);
        }
        let busy = self.starting.contains(project.id()) || self.handoff.contains_key(project.id());
        match self.activity(project) {
            Some(Activity::Stopped) => (t("workspace.continue"), !busy),
            Some(_) => (t("workspace.pause"), !busy),
            None => (t("workspace.pause"), false),
        }
    }

    fn toggle_pause(&mut self, _: &TogglePause, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.current().cloned() else {
            return;
        };
        match self.activity(&project) {
            Some(Activity::Stopped) => self.resume(&project, cx),
            Some(_) => self.pause(project.id(), false, window, cx),
            None => {}
        }
    }

    fn pause_others(&mut self, _: &PauseOthers, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.selected.clone();
        let targets: Vec<(String, String)> = self
            .projects
            .iter()
            .filter(|project| Some(project.id()) != current.as_deref())
            .filter_map(|project| {
                let agent = self.agent(project.session()?)?;
                let idle = matches!(agent.activity(), Activity::Ready | Activity::Waiting);
                let short = agent.id.clone()?;
                (agent.is_background() && idle).then(|| (project.id().to_string(), short))
            })
            .collect();
        for (id, short) in &targets {
            self.paused.insert(id.clone());
            self.resumed.remove(id);
            self.terminal.update(cx, |terminal, cx| terminal.close(id, cx));
            let short = short.clone();
            cx.background_spawn(async move { sessions::stop(&short) }).detach();
        }
        let message = if targets.is_empty() {
            t("workspace.nothing_to_pause").to_string()
        } else {
            crate::i18n::count(targets.len() as u64, "workspace.paused_count_session_sessions_that")
        };
        window.push_notification(message, cx);
        self.refresh(cx);
    }

    fn switch_session(&mut self, _: &SwitchSession, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.selected.clone() {
            self.start_switch(&id, cx);
        }
    }

    fn go_back(&mut self, _: &GoBack, window: &mut Window, cx: &mut Context<Self>) {
        if self.history_index > 0 {
            self.history_index -= 1;
            let id = self.history[self.history_index].clone();
            self.show(Some(id), window, cx);
        }
    }

    fn go_forward(&mut self, _: &GoForward, window: &mut Window, cx: &mut Context<Self>) {
        if self.history_index + 1 < self.history.len() {
            self.history_index += 1;
            let id = self.history[self.history_index].clone();
            self.show(Some(id), window, cx);
        }
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    fn toggle_theme(&mut self, _: &ToggleTheme, _: &mut Window, cx: &mut Context<Self>) {
        let pref = if cx.theme().is_dark() { ThemePref::Light } else { ThemePref::Dark };
        self.set_theme(pref, cx);
    }

    pub(crate) fn language_pref(&self) -> crate::i18n::LangPref {
        self.store.language()
    }

    /// Change the language: strings, the macOS menu and all windows redraw right away.
    pub(crate) fn set_language(&mut self, pref: crate::i18n::LangPref, cx: &mut Context<Self>) {
        self.store.set_language(pref);
        crate::i18n::apply(pref);
        crate::install_menus(cx);
        cx.refresh_windows();
    }

    pub(crate) fn theme_pref(&self) -> ThemePref {
        self.store.theme()
    }

    pub(crate) fn set_theme(&mut self, pref: ThemePref, cx: &mut Context<Self>) {
        self.store.set_theme(pref);
        self.apply_theme(cx);
    }

    /// App-wide theme (the Pult window and the settings window).
    fn apply_theme(&mut self, cx: &mut Context<Self>) {
        match self.store.theme() {
            ThemePref::System => Theme::sync_system_appearance(None, cx),
            ThemePref::Light => Theme::change(ThemeMode::Light, None, cx),
            ThemePref::Dark => Theme::change(ThemeMode::Dark, None, cx),
        }
        cx.refresh_windows();
    }

    fn toggle_info(&mut self, _: &ToggleInfo, _: &mut Window, cx: &mut Context<Self>) {
        self.info_open = !self.info_open;
        if self.info_open
            && let Some(project) = self.current().cloned()
            && project.session().is_none()
        {
            self.load_transcripts(project.id());
        }
        cx.notify();
    }

    fn copy_path(&mut self, _: &CopyPath, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(project) = self.current() {
            let path = project.path().display().to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(path));
            window.push_notification(t("workspace.path_copied"), cx);
        }
    }

    fn reveal_in_finder(&mut self, _: &RevealInFinder, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(project) = self.current() {
            cx.reveal_path(project.path());
        }
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| terminal.zoom(1., cx));
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| terminal.zoom(-1., cx));
    }

    fn zoom_reset(&mut self, _: &ZoomReset, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| terminal.reset_zoom(cx));
    }

    fn clear_terminal(&mut self, _: &ClearTerminal, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| terminal.clear_current(cx));
    }

    fn new_project(&mut self, _: &NewProject, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.needs_root() {
            self.pick_root(AfterRoot::NewProject, window, cx);
            return;
        }
        let root = display_path(self.store.root());
        dialogs::open_new_project(cx.entity().downgrade(), root, window, cx);
    }

    /// First launch: ask for the main folder (any folder on the computer), then go on with what was asked for.
    fn pick_root(&mut self, after: AfterRoot, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t("common.choose").into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = paths.await
                && let Some(path) = paths.pop()
            {
                let _ = this.update_in(cx, |this, window, cx| {
                    if let Err(error) = this.set_root(path) {
                        window.push_notification(error, cx);
                        return;
                    }
                    cx.notify();
                    match after {
                        AfterRoot::NewProject => this.new_project(&NewProject, window, cx),
                        // An empty folder has nothing to offer: the "No projects" placeholder takes over.
                        AfterRoot::AddFolders if !this.store.candidates().is_empty() => {
                            this.add_folders(&AddFolders, window, cx)
                        }
                        AfterRoot::AddFolders => {}
                    }
                });
            }
        })
        .detach();
    }

    /// Make `path` the main folder without moving anything (only while Pult has no projects).
    pub(crate) fn set_root(&mut self, path: PathBuf) -> Result<(), String> {
        self.store.choose_root(path)?;
        self.reload();
        Ok(())
    }

    pub(crate) fn needs_root(&self) -> bool {
        self.store.needs_root()
    }

    pub(crate) fn has_projects(&self) -> bool {
        !self.projects.is_empty()
    }

    fn add_folders(&mut self, _: &AddFolders, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.needs_root() {
            self.pick_root(AfterRoot::AddFolders, window, cx);
            return;
        }
        let candidates = self
            .store
            .candidates()
            .into_iter()
            .map(|path| {
                let count = sessions::transcript_count(&path);
                (path, count)
            })
            .collect();
        let root = display_path(self.store.root());
        dialogs::open_add_folders(cx.entity().downgrade(), root, candidates, window, cx);
    }

    pub(crate) fn add_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let added = self.store.add(&paths);
        self.reload();
        match added.as_slice() {
            [] => window.push_notification(t("workspace.these_folders_are_already_in"), cx),
            [one] => {
                window.push_notification(tr!("workspace.added_folder_one", one = one), cx);
                let one = one.clone();
                self.select(&one, window, cx);
            }
            many => {
                window.push_notification(
                    crate::i18n::count(many.len() as u64, "workspace.added_count_folder"),
                    cx,
                );
                let first = many[0].clone();
                self.select(&first, window, cx);
            }
        }
        cx.notify();
    }

    /// The project folder was moved by hand: point to its new location. Projects live only in the main folder.
    /// Claude Code's conversation, memory and trust follow it; the session stays the same.
    fn relocate(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t("common.choose").into()),
        });
        let id = id.to_string();
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = paths.await
                && let Some(path) = paths.pop()
            {
                let _ = this.update_in(cx, |this, window, cx| this.relocate_to(&id, path, window, cx));
            }
        })
        .detach();
    }

    fn relocate_to(&mut self, id: &str, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project(id).cloned() else {
            return;
        };
        if !self.store.in_root(&path) {
            let root = display_path(self.store.root());
            window.push_notification(tr!("workspace.pult_projects_live_only_in", root = root), cx);
            return;
        }
        let new_name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        if new_name != project.folder() && !self.siblings(&new_name).is_empty() {
            window.push_notification(tr!("workspace.project_new_name_is_already", new_name = new_name), cx);
            return;
        }
        let from = project.path().to_path_buf();
        self.start_move(Target::Project(id.to_string()), from, path, false, window, cx);
    }

    /// Move a folder (the main one or a project's) together with Claude Code data: one step at a time,
    /// progress goes to `move_state` (shown by the move window). An error before the last step rolls back.
    /// `move_folder: false` — the folder was already moved by hand, only Claude data moves.
    pub(crate) fn start_move(
        &mut self,
        target: Target,
        from: PathBuf,
        to: PathBuf,
        move_folder: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.move_state.as_ref().is_some_and(|state| state.step.is_some()) {
            return;
        }
        let names = match &target {
            Target::Project(id) => {
                let new_name = to.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                self.siblings(split_key(id).0)
                    .into_iter()
                    .map(|project| (project.session_name(), sessions::session_name(&new_name, project.sub())))
                    .collect()
            }
            Target::Root => Vec::new(),
        };
        let mut job = Move::new(from.clone(), to.clone(), move_folder, names);
        self.move_state = Some(MoveState {
            labels: migrate::step_labels(move_folder, target == Target::Root),
            step: Some(0),
            error: None,
            done: false,
        });
        self.begin_move(&from, cx);
        cx.spawn_in(window, async move |this, cx| {
            let mut failed = None;
            for step in 0..job.steps() {
                let _ = this.update(cx, |this, cx| {
                    if let Some(state) = &mut this.move_state {
                        state.step = Some(step);
                    }
                    // Folder and data are moved — Pult switches to the new paths before
                    // sessions come up, so it recognizes them right away.
                    if job.is_last(step) {
                        this.finish_move(&to, &target, cx);
                    }
                    cx.notify();
                });
                let (back, result) = cx
                    .background_spawn(async move {
                        let result = job.run_step(step);
                        (job, result)
                    })
                    .await;
                job = back;
                if let Err(error) = result {
                    if !job.is_last(step) {
                        let _ = cx.background_spawn(async move { job.rollback() }).await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.abort_move(cx);
                            let message = tr!("workspace.error_everything_was_put_back", error = sentence(&error));
                            if let Some(state) = &mut this.move_state {
                                state.step = None;
                                state.error = Some(message.clone());
                            }
                            window.push_notification(message, cx);
                            cx.notify();
                        });
                        return;
                    }
                    // Data is already in its new place: a session that didn't start is no reason to roll back.
                    failed = Some(error);
                }
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.end_move(cx);
                if let Some(state) = &mut this.move_state {
                    state.step = None;
                    state.done = true;
                }
                let message = failed.unwrap_or_else(|| match target {
                    Target::Root => tr!("workspace.main_folder_is_now_to", to = display_path(&to)),
                    Target::Project(_) => tr!("workspace.project_is_now_to_session", to = display_path(&to)),
                });
                window.push_notification(message, cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn move_state(&self) -> Option<&MoveState> {
        self.move_state.as_ref()
    }

    fn open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        let projects = self
            .projects
            .iter()
            .map(|project| (project.id().to_string(), project.title(), display_path(project.path())))
            .collect();
        let pause = self.current().cloned().and_then(|project| {
            let label = match self.activity(&project)? {
                Activity::Stopped => tr!("workspace.continue_name", name = project.name()),
                _ => tr!("workspace.pause_name", name = project.name()),
            };
            Some(label)
        });
        let rename = self
            .current()
            .filter(|project| project.exists())
            .map(|project| tr!("workspace.rename_name", name = project.folder()));
        let subproject = self.current().filter(|project| project.exists()).and_then(|project| {
            if project.sub().is_some() {
                let label = tr!("workspace.new_subproject_in_name_cmd", name = project.folder());
                Some((label, PaletteAction::NewSubproject))
            } else if project.session().is_some() {
                let label = tr!("workspace.split_name_into_subprojects_cmd", name = project.name());
                Some((label, PaletteAction::SplitProject))
            } else {
                None
            }
        });
        let restart = self
            .current()
            .filter(|project| self.can_restart(project))
            .map(|project| tr!("workspace.restart_name_cmd", name = project.name()));
        let clone = self
            .current()
            .filter(|project| self.can_clone(project))
            .map(|project| tr!("workspace.clone_name_cmd", name = project.name()));
        dialogs::open_palette(
            cx.entity().downgrade(),
            projects,
            dialogs::CurrentCommands { pause, restart, rename, subproject, clone, provider: self.provider },
            self.sidebar_open,
            self.info_open,
            window,
            cx,
        );
    }

    /// Action chosen in the search window.
    pub(crate) fn run_palette(&mut self, action: PaletteAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            PaletteAction::Project(id) => self.select(&id, window, cx),
            PaletteAction::NewProject => self.new_project(&NewProject, window, cx),
            PaletteAction::AddFolders => self.add_folders(&AddFolders, window, cx),
            PaletteAction::TogglePause => self.toggle_pause(&TogglePause, window, cx),
            PaletteAction::RestartSession => self.restart_current(&RestartSession, window, cx),
            PaletteAction::PauseOthers => self.pause_others(&PauseOthers, window, cx),
            PaletteAction::ToggleSidebar => self.toggle_sidebar(&ToggleSidebar, window, cx),
            PaletteAction::ToggleInfo => self.toggle_info(&ToggleInfo, window, cx),
            PaletteAction::Settings => self.open_settings(cx),
            PaletteAction::UseProvider(to) => self.use_provider(to, window, cx),
            PaletteAction::ModelsSettings => self.open_settings_at(Some(crate::settings::MODELS_PAGE), cx),
            PaletteAction::RenameProject => self.rename_project(&RenameProject, window, cx),
            PaletteAction::SplitProject => self.split_current(&SplitProject, window, cx),
            PaletteAction::NewSubproject => self.new_subproject(&NewSubproject, window, cx),
            PaletteAction::CloneSubproject => self.clone_current(&CloneSubproject, window, cx),
            PaletteAction::Theme(pref) => self.set_theme(pref, cx),
        }
    }

    /// New project: a folder in the main folder and a new session right away.
    pub(crate) fn create_project(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match self.store.create(name) {
            Ok(project) => {
                let id = project.id().to_string();
                self.reload();
                self.select(&id, window, cx);
                self.new_session(&id, window, cx);
                true
            }
            Err(message) => {
                window.push_notification(message, cx);
                false
            }
        }
    }

    /// "Split into subprojects…": the current conversation becomes the first subproject.
    fn split_current(&mut self, _: &SplitProject, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.current().cloned() else {
            return;
        };
        if project.sub().is_some() {
            return self.open_new_subproject(project.folder(), window, cx);
        }
        if !project.exists() || project.session().is_none() {
            window.push_notification(t("workspace.choose_session_before_split"), cx);
            return;
        }
        dialogs::open_split(
            cx.entity().downgrade(),
            project.id().to_string(),
            project.folder().to_string(),
            self.store.subs(project.folder()),
            window,
            cx,
        );
    }

    /// "New subproject…": one more conversation in a split project; an ordinary project is split first.
    fn new_subproject(&mut self, _: &NewSubproject, window: &mut Window, cx: &mut Context<Self>) {
        match self.current().cloned() {
            Some(project) if project.sub().is_some() => self.open_new_subproject(project.folder(), window, cx),
            Some(_) => self.split_current(&SplitProject, window, cx),
            None => {}
        }
    }

    fn open_new_subproject(&mut self, folder: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.siblings(folder).first().map(|project| display_path(project.path())) else {
            return;
        };
        dialogs::open_new_subproject(
            cx.entity().downgrade(),
            folder.to_string(),
            path,
            self.store.subs(folder),
            window,
            cx,
        );
    }

    /// Split the project: its conversation stays as the first subproject, the second one starts new.
    pub(crate) fn split_project(
        &mut self,
        id: &str,
        (first, first_about): (&str, &str),
        (second, second_about): (&str, &str),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.store.split(id, first, second) {
            Ok((kept, new)) => {
                let folder = split_key(id).0.to_string();
                self.store.set_about(&kept, first_about);
                self.store.set_about(&new, second_about);
                crate::journal::ensure_hooks();
                self.reload();
                self.rekey(id, &kept);
                self.terminal.update(cx, |terminal, _| terminal.rekey(id, &kept));
                self.select(&new, window, cx);
                self.new_session(&new, window, cx);
                window.push_notification(
                    tr!("workspace.name_split_into_first_and_second", name = folder, first = first.trim(), second = second.trim()),
                    cx,
                );
                true
            }
            Err(message) => {
                window.push_notification(message, cx);
                false
            }
        }
    }

    /// One more subproject in a split project, with a new session right away.
    pub(crate) fn create_subproject(
        &mut self,
        folder: &str,
        name: &str,
        about: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.store.add_sub(folder, name) {
            Ok(id) => {
                self.store.set_about(&id, about);
                crate::journal::ensure_hooks();
                self.reload();
                self.select(&id, window, cx);
                self.new_session(&id, window, cx);
                true
            }
            Err(message) => {
                window.push_notification(message, cx);
                false
            }
        }
    }

    /// A subproject's conversation can be copied: it has one here, its folder is in place, and the session
    /// isn't running on another computer or still starting (the copy could miss the latest turns).
    fn can_clone(&self, project: &Project) -> bool {
        let Some(session) = project.session() else {
            return false;
        };
        project.sub().is_some()
            && project.exists()
            && !self.starting.contains(project.id())
            && self.elsewhere(project).is_none()
            && sessions::has_transcript(project.path(), session)
    }

    /// "Clone…": the window asking for the new subproject's name.
    fn clone_current(&mut self, _: &CloneSubproject, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.current().cloned().filter(|project| self.can_clone(project)) else {
            return;
        };
        let busy = matches!(self.activity(&project), Some(Activity::Working | Activity::Waiting));
        dialogs::open_clone_subproject(
            cx.entity().downgrade(),
            project.id().to_string(),
            project.name().to_string(),
            project.folder().to_string(),
            self.store.subs(project.folder()),
            busy,
            window,
            cx,
        );
    }

    /// A new subproject in the same project that starts from a copy of `source`'s conversation and goes on
    /// separately; `source` is left as it is.
    pub(crate) fn clone_subproject(
        &mut self,
        source: &str,
        name: &str,
        about: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(original) = self.project(source).cloned() else {
            return false;
        };
        let Some(session) = original.session().map(str::to_string) else {
            return false;
        };
        let id = match self.store.add_sub(original.folder(), name) {
            Ok(id) => id,
            Err(message) => {
                window.push_notification(message, cx);
                return false;
            }
        };
        self.store.set_about(&id, about);
        // The copy goes on with the models of the original.
        let provider = self.provider_of(&original);
        self.store.set_provider(&id, crate::models::provider_key(provider));
        crate::journal::ensure_hooks();
        self.reload();
        self.select(&id, window, cx);
        let Some(project) = self.project(&id).cloned() else {
            return true;
        };
        let flag = match self.models_flag(&project) {
            Ok(flag) => flag,
            Err(message) => {
                window.push_notification(message, cx);
                return true;
            }
        };
        self.starting.insert(id.clone());
        let (cwd, session_name) = (project.path().to_path_buf(), project.session_name());
        let (new_name, source_name) = (project.name().to_string(), original.name().to_string());
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn({
                    let cwd = cwd.clone();
                    async move { sessions::clone_session(&cwd, &session, &session_name, &flag) }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let id = this.live_key(&id);
                this.starting.remove(&id);
                match result {
                    Ok(copy) => {
                        this.store.bind(&id, &copy);
                        this.bound_here.insert(copy);
                        this.reload();
                        window.push_notification(
                            tr!("workspace.cloned_from", name = new_name, source = source_name),
                            cx,
                        );
                    }
                    Err(StartError::NotTrusted) => {
                        this.errors.insert(id.clone(), (Failure::New, t("sessions.folder_not_trusted").into()));
                    }
                    Err(StartError::Failed(message)) => {
                        this.errors.insert(id.clone(), (Failure::New, message));
                    }
                }
                this.refresh(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
        true
    }

    /// Write what the subproject does; the neighbours' sessions will read it.
    pub(crate) fn set_about(&mut self, id: &str, text: &str, cx: &mut Context<Self>) {
        self.store.set_about(id, text);
        self.reload();
        self.refresh_neighbours();
        cx.notify();
    }

    fn open_about_dialog(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project(id) else {
            return;
        };
        dialogs::open_about(
            cx.entity().downgrade(),
            id.to_string(),
            tr!("workspace.what_it_does_title", name = project.name()),
            project.about().to_string(),
            window,
            cx,
        );
    }

    /// Tell the journal hooks how the sessions of split projects are doing (they can't ask Claude Code
    /// themselves), and refresh what the current session knows about its neighbours.
    fn share_states(&mut self) {
        if self.projects.iter().any(|project| project.sub().is_some()) {
            let states: std::collections::BTreeMap<String, &'static str> = self
                .projects
                .iter()
                .filter(|project| project.sub().is_some() && project.session().is_some())
                .map(|project| {
                    let state = if self.elsewhere(project).is_some() {
                        "remote"
                    } else {
                        match self.activity(project) {
                            Some(Activity::Working) => "working",
                            Some(Activity::Waiting) => "waiting",
                            Some(Activity::Stopped) => "paused",
                            _ => "idle",
                        }
                    };
                    (project.id().to_string(), state)
                })
                .collect();
            crate::journal::write_status(&crate::journal::data_dir(), &states);
        }
        self.refresh_neighbours();
    }

    fn refresh_neighbours(&mut self) {
        self.neighbours = match self.current() {
            Some(project) if project.sub().is_some() => crate::journal::summary(&crate::journal::data_dir(), project.id()),
            _ => String::new(),
        };
    }

    /// Stop the project's background session if it is running: the project is leaving Pult.
    fn stop_session_of(&self, id: &str, cx: &mut Context<Self>) {
        if let Some(short) = self
            .project(id)
            .and_then(Project::session)
            .and_then(|session| self.agent(session))
            .filter(|agent| agent.is_background() && agent.is_running())
            .and_then(|agent| agent.id.clone())
        {
            cx.background_spawn(async move { sessions::stop(&short) }).detach();
        }
    }

    /// For checking without clicks: `PULT_DO` runs commands at launch, separated by `;` —
    /// `add:<folder>`, `new:<name>`, `split:<project>:<first>:<second>`, `sub:<project>:<name>`, `about:<key>:<text>`, `remove:<key>`,
    /// `show:<key>`.
    pub(crate) fn run_script(&mut self, script: &str, window: &mut Window, cx: &mut Context<Self>) {
        for command in script.split(';') {
            let parts: Vec<&str> = command.trim().split(':').collect();
            match parts.as_slice() {
                ["add", folder] => self.add_paths(vec![self.store.root().join(folder)], window, cx),
                ["new", name] => {
                    self.create_project(name, window, cx);
                }
                ["split", id, first, second] => {
                    self.split_project(id, (first, ""), (second, ""), window, cx);
                }
                ["sub", folder, name] => {
                    self.create_subproject(folder, name, "", window, cx);
                }
                ["about", id, text] => self.set_about(id, text, cx),
                ["remove", id] => self.forget_project(id, window, cx),
                ["show", id] => self.select(id, window, cx),
                // The stuck-code screen as it looks, without a second computer: `code-stuck:<project>:<dirty|clone|big|…>`.
                ["code-stuck", id, kind] => {
                    let manifest = crate::code::Manifest { device_name: "Test computer".into(), ..Default::default() };
                    let stuck = match *kind {
                        "dirty" => crate::code::Stuck::Dirty { files: 2, branch: "pult/11111111-virtualnaya-mashina".into() },
                        "clone" => crate::code::Stuck::Clone { remote: "https://example.com/hitkey-io/pult.git".into() },
                        "big" => crate::code::Stuck::Big { megabytes: 31 },
                        "norepo" => crate::code::Stuck::NoRepo,
                        "arrived" => crate::code::Stuck::NotArrived,
                        _ => crate::code::Stuck::Fetch,
                    };
                    self.code_stuck.insert(id.to_string(), (stuck, manifest));
                }
                ["info"] => self.info_open = true,
                ["ideas", id] => self.open_ideas(id, window, cx),
                ["resume", id] => {
                    if let Some(project) = self.project(id).cloned() {
                        self.resume(&project, cx);
                    }
                }
                _ => {}
            }
        }
    }

    /// For checking the look without clicks (`PULT_DIALOG=remove-project`): the confirmation of removing the
    /// current project with all its subprojects.
    pub(crate) fn confirm_remove_current_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(folder) = self.current().map(|project| project.folder().to_string()) {
            self.confirm_remove_folder(&folder, window, cx);
        }
    }

    /// "Remove from Pult…" on a split project's row: the project with all its subprojects.
    fn confirm_remove_folder(&mut self, folder: &str, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.siblings(folder).len();
        if count == 0 {
            return;
        }
        let workspace = cx.entity().downgrade();
        let title = tr!("workspace.remove_name_from_pult", name = folder);
        let description = tr!("workspace.project_and_subprojects_will_be_removed", count = count);
        let folder = folder.to_string();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (workspace, folder) = (workspace.clone(), folder.clone());
            alert
                .title(SharedString::from(title.clone()))
                .description(SharedString::from(description.clone()))
                .show_cancel(true)
                .cancel_text(t("common.cancel"))
                .ok_text(t("workspace.remove"))
                .on_ok(move |_, window, cx| {
                    let _ = workspace.update(cx, |this, cx| this.forget_folder(&folder, window, cx));
                    true
                })
        });
    }

    fn forget_folder(&mut self, folder: &str, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.siblings(folder).iter().map(|project| project.id().to_string()).collect();
        for id in &ids {
            self.stop_session_of(id, cx);
            self.forget_state(id);
            self.terminal.update(cx, |terminal, cx| terminal.close(id, cx));
        }
        self.store.remove_folder(folder);
        self.last_sub.remove(folder);
        self.reload();
        self.history.retain(|entry| !ids.contains(entry));
        self.history_index = self.history.len().saturating_sub(1);
        if self.selected.as_ref().is_some_and(|selected| ids.contains(selected)) {
            let next = self
                .history
                .last()
                .cloned()
                .or_else(|| self.projects.first().map(|project| project.id().to_string()));
            if let Some(next) = &next
                && self.history.is_empty()
            {
                self.history.push(next.clone());
                self.history_index = 0;
            }
            self.selected = None;
            self.show(next, window, cx);
        }
        window.push_notification(tr!("workspace.name_removed_with_subprojects", name = folder), cx);
        cx.notify();
    }

    fn remove_project(&mut self, _: &RemoveProject, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.current().cloned() else {
            return;
        };
        let workspace = cx.entity().downgrade();
        let (title, description) = match project.sub() {
            Some(sub) => {
                let mut description: String = t("workspace.subproject_will_be_removed").into();
                // Two subprojects now: after this one goes, the project is ordinary again.
                if let [first, second] = self.siblings(project.folder()).as_slice() {
                    let left = if first.id() == project.id() { second } else { first };
                    description.push(' ');
                    description.push_str(&tr!(
                        "workspace.one_subproject_will_remain",
                        folder = project.folder(),
                        name = left.name()
                    ));
                }
                (tr!("workspace.remove_subproject_name", name = sub), description)
            }
            None => (
                tr!("workspace.remove_name_from_pult", name = project.name()),
                t("workspace.project_will_be_removed_from").to_string(),
            ),
        };
        window.open_alert_dialog(cx, move |alert, _, _| {
            let workspace = workspace.clone();
            let id = project.id().to_string();
            alert
                .title(SharedString::from(title.clone()))
                .description(SharedString::from(description.clone()))
                .show_cancel(true)
                .cancel_text(t("common.cancel"))
                .ok_text(t("workspace.remove"))
                .on_ok(move |_, window, cx| {
                    let _ = workspace.update(cx, |this, cx| this.forget_project(&id, window, cx));
                    true
                })
        });
    }

    fn forget_project(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_session_of(id, cx);
        let name = self.project(id).map(|project| project.name().to_string()).unwrap_or_default();
        let (folder, sub) = split_key(id);
        let (folder, was_sub) = (folder.to_string(), sub.is_some());
        let merged = self.store.remove(id);
        self.reload();
        self.forget_state(id);
        self.history.retain(|entry| entry != id);
        self.terminal.update(cx, |terminal, cx| terminal.close(id, cx));
        if self.last_sub.get(&folder).is_some_and(|last| last == id) {
            self.last_sub.remove(&folder);
        }
        // One subproject left: the project is ordinary again, and its terminal stays as it is.
        if let Some(only) = &merged {
            let old = key_for(&folder, Some(only));
            self.rekey(&old, &folder);
            self.terminal.update(cx, |terminal, _| terminal.rekey(&old, &folder));
            self.last_sub.remove(&folder);
        }
        self.history_index = self.history.len().saturating_sub(1);
        // After a subproject the user stays in the same project.
        let stay = was_sub.then(|| self.siblings(&folder).first().map(|project| project.id().to_string())).flatten();
        let next = stay
            .or_else(|| self.history.last().cloned())
            .or_else(|| self.projects.first().map(|project| project.id().to_string()));
        // The project shown next is part of the navigation history, like any project the user opens.
        if let Some(next) = &next
            && self.history.last() != Some(next)
        {
            self.history.push(next.clone());
        }
        self.history_index = self.history.len().saturating_sub(1);
        self.selected = None;
        self.show(next, window, cx);
        let message = match (was_sub, merged) {
            (true, Some(only)) => {
                tr!("workspace.subproject_removed_project_ordinary", name = name, folder = folder, only = only)
            }
            (true, None) => tr!("workspace.subproject_removed", name = name),
            (false, _) => tr!("workspace.name_removed_from_pult_folder", name = name),
        };
        window.push_notification(message, cx);
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.current();
        let has_project = current.is_some();
        let title: SharedString = current.map(Project::title).unwrap_or_else(|| t("common.pult").into()).into();
        let is_sub = current.is_some_and(|project| project.sub().is_some());
        let can_clone = current.is_some_and(|project| self.can_clone(project));
        let can_split = current.is_some_and(|project| project.sub().is_none() && project.exists() && project.session().is_some());
        let (pause_label, can_pause) = current.map(|project| self.pause_command(project)).unwrap_or((t("workspace.pause"), false));
        let can_switch = current.is_some_and(Project::exists);
        let can_restart = current.is_some_and(|project| self.can_restart(project));
        let switch_label = if current.and_then(Project::session).is_some() {
            t("common.change_session")
        } else {
            t("workspace.choose_session")
        };
        let is_dark = cx.theme().is_dark();
        let provider = current.map(|project| self.provider_of(project)).unwrap_or(self.provider);
        let (glm_key, glm_main) = (self.glm_key, self.glm_main.clone());
        let chip_name = crate::switch::provider_name(provider);
        let chip_dot = if provider == crate::models::Provider::Glm { cx.theme().info } else { cx.theme().muted_foreground };

        TitleBar::new()
            .child(
                h_flex()
                    .gap_1()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new("toggle-sidebar")
                            .ghost()
                            .small()
                            .icon(IconName::PanelLeft)
                            .tooltip(if self.sidebar_open {
                                t("common.hide_sidebar")
                            } else {
                                t("common.show_sidebar")
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ToggleSidebar), cx)
                            }),
                    )
                    .child(
                        Button::new("go-back")
                            .ghost()
                            .small()
                            .icon(IconName::ChevronLeft)
                            .tooltip(t("common.back"))
                            .disabled(self.history_index == 0)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(GoBack), cx)),
                    )
                    .child(
                        Button::new("go-forward")
                            .ghost()
                            .small()
                            .icon(IconName::ChevronRight)
                            .tooltip(t("common.forward"))
                            .disabled(self.history_index + 1 >= self.history.len())
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(GoForward), cx)
                            }),
                    )
                    .child(div().pl_2().min_w_0().truncate().text_sm().font_medium().child(title))
                    .child(
                        Button::new("project-actions")
                            .ghost()
                            .small()
                            .icon(IconName::Ellipsis)
                            .tooltip(t("workspace.project_actions"))
                            .disabled(!has_project)
                            .dropdown_menu(move |menu, _, _| {
                                menu.menu(t("common.copy_path"), Box::new(CopyPath))
                                    .menu(t("common.connected_to_claude"), Box::new(ToggleInfo))
                                    .separator()
                                    .menu_with_disabled(pause_label, Box::new(TogglePause), !can_pause)
                                    .menu_with_disabled(t("common.restart_session"), Box::new(RestartSession), !can_restart)
                                    .menu_with_disabled(switch_label, Box::new(SwitchSession), !can_switch)
                                    .menu(t("common.clear_terminal_screen"), Box::new(ClearTerminal))
                                    .menu_with_disabled(t("common.rename_project"), Box::new(RenameProject), !can_switch)
                                    .separator()
                                    .when(can_split, |menu| {
                                        menu.menu(t("common.split_into_subprojects"), Box::new(SplitProject))
                                    })
                                    .when(is_sub, |menu| menu.menu(t("common.new_subproject"), Box::new(NewSubproject)))
                                    .when(is_sub, |menu| {
                                        menu.menu_with_disabled(t("common.clone_subproject"), Box::new(CloneSubproject), !can_clone)
                                    })
                                    .menu(
                                        if is_sub { t("common.remove_subproject") } else { t("common.remove_from_pult") },
                                        Box::new(RemoveProject),
                                    )
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .pr_2()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new("models-chip")
                            .outline()
                            .small()
                            .dropdown_caret(true)
                            .tooltip(tr!("models.chip_tooltip", name = chip_name))
                            .child(h_flex().gap_1p5().child(div().size_2().rounded_full().bg(chip_dot)).child(chip_name))
                            .dropdown_menu(move |menu, _, _| {
                                let glm_hint = if glm_key { glm_main.clone() } else { t("models.menu_key_needed").to_string() };
                                menu.menu_element_with_check(provider == crate::models::Provider::Claude, Box::new(UseClaude), |_, cx| {
                                    model_menu_row(t("models.claude"), t("models.menu_claude_hint").into(), cx)
                                })
                                .menu_element_with_check(provider == crate::models::Provider::Glm, Box::new(UseGlm), move |_, cx| {
                                    model_menu_row(t("models.glm_option"), glm_hint.clone().into(), cx)
                                })
                                .separator()
                                .menu(t("models.menu_configure"), Box::new(OpenModels))
                            }),
                    )
                    .child(
                        Button::new("search")
                            .outline()
                            .small()
                            .w(rems(12.5))
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenPalette), cx))
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .child(Icon::new(IconName::Search).size_3p5())
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_left()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(t("common.search")),
                                    )
                                    .child(Kbd::new(Keystroke::parse("cmd-k").unwrap())),
                            ),
                    )
                    .child(
                        Button::new("toggle-theme")
                            .ghost()
                            .small()
                            .icon(if is_dark { IconName::Sun } else { IconName::Moon })
                            .tooltip(if is_dark { t("common.light_theme") } else { t("common.dark_theme") })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ToggleTheme), cx)
                            }),
                    )
                    .child(
                        Button::new("open-settings")
                            .ghost()
                            .small()
                            .icon(Icon::new(AssetIcon::Settings))
                            .tooltip(t("common.settings_2"))
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenSettings), cx)),
                    )
                    .child(
                        Button::new("toggle-info")
                            .ghost()
                            .small()
                            .icon(IconName::PanelRight)
                            .tooltip(if self.info_open {
                                t("workspace.hide_connected_to_claude")
                            } else {
                                t("workspace.show_connected_to_claude")
                            })
                            .disabled(!has_project)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleInfo), cx)),
                    ),
            )
    }

    /// A project's (or a subproject's) row in the sidebar, and what is going on in it — for the summary a
    /// folded project shows.
    fn project_row(&self, project: &Project, cx: &mut Context<Self>) -> (SidebarMenuItem, Option<Urgent>) {
        let workspace = cx.entity().downgrade();
        let (muted, info, warning) = (cx.theme().muted_foreground, cx.theme().info, cx.theme().warning);
        let id = project.id().to_string();
        let workspace = workspace.clone();
        let missing = !project.exists();
        let activity = self.activity(project);
        let remote = self.elsewhere(project).is_some();
        let state = if missing {
            t("workspace.folder_not_found")
        } else if remote {
            t("workspace.on_another_computer")
        } else if self.pending_bind.contains_key(project.id()) {
            t("workspace.waiting_for_answer_in_terminal")
        } else if project.session().is_none() {
            t("workspace.no_session_chosen")
        } else {
            match activity {
                Some(Activity::Working) => t("workspace.working"),
                Some(Activity::Waiting) => t("workspace.waiting_for_you"),
                Some(Activity::Stopped) => t("workspace.paused"),
                _ => t("workspace.ready"),
            }
        };
        let asking = self.pending_bind.contains_key(project.id());
        let on = self.selected.as_deref() == Some(project.id());
        // A project where nothing is going on (paused, another computer, no session, no folder) is dimmed,
        // as in the mockup; the selected one isn't — it is highlighted anyway.
        let quiet = !on
            && !asking
            && (missing || remote || project.session().is_none() || activity == Some(Activity::Stopped));
        let tip = remote_tip(state, self.elsewhere(project));
        let mut label_style = gpui_kit::StyleRefinement::default();
        if quiet {
            label_style.text.color = Some(muted);
        }
        // For the folded project's summary: what is going on in this row.
        let urgent = if missing || remote {
            None
        } else if asking || activity == Some(Activity::Waiting) {
            Some(Urgent::Waiting)
        } else if activity == Some(Activity::Working) {
            Some(Urgent::Working)
        } else if activity == Some(Activity::Stopped) {
            Some(Urgent::Paused)
        } else {
            None
        };
        let is_sub = project.sub().is_some();
        let can_clone = self.can_clone(project);
        let can_split = !is_sub && !missing && project.session().is_some();
        let provider = self.provider_of(project);
        let glm = provider == crate::models::Provider::Glm;
        let has_state = missing || remote || asking || activity.is_some_and(|activity| activity != Activity::Ready);
        let item = SidebarMenuItem::new(project.name().to_string())
            .accessibility_label(format!("{}, {tip}", project.name()))
            .label_style(label_style)
            .active(on)
            .when(has_state || glm, |item| {
                let id = id.clone();
                item.suffix(move |_, _| -> AnyElement {
                    // A project on GLM says so in its row: which models answer is part of what the row is.
                    let mark = glm.then(|| div().text_xs().text_color(muted).child(t("models.glm")));
                    let icon = if !has_state {
                        div().into_any_element()
                    } else if missing {
                        Icon::new(AssetIcon::TriangleAlert).xsmall().text_color(warning).into_any_element()
                    } else if remote {
                        Icon::new(AssetIcon::Laptop).xsmall().text_color(muted).into_any_element()
                    } else if asking {
                        div().size_2().rounded_full().bg(info).into_any_element()
                    } else {
                        match activity {
                            Some(Activity::Working) => Spinner::new().xsmall().color(muted).into_any_element(),
                            Some(Activity::Waiting) => div().size_2().rounded_full().bg(info).into_any_element(),
                            _ => Icon::new(AssetIcon::Pause).xsmall().text_color(muted).into_any_element(),
                        }
                    };
                    // State icon with a text tooltip: color and shape are not the only signal.
                    let tip = tip.clone();
                    let state = div()
                        .id(SharedString::from(format!("state-{id}")))
                        .flex()
                        .items_center()
                        .child(icon)
                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx));
                    h_flex().gap_1p5().children(mark).when(has_state, |row| row.child(state)).into_any_element()
                })
            })
            .context_menu({
                // Right click: the project becomes current and the menu commands apply to it (Apple HIG:
                // a context menu on the object; the same commands are in …, File and ⌘K).
                let (workspace, id) = (workspace.clone(), id.clone());
                let (pause, can_pause) = self.pause_command(project);
                let can_restart = self.can_restart(project);
                let switch = if project.session().is_some() { t("common.change_session") } else { t("workspace.choose_session") };
                move |menu, window, cx| {
                    let (workspace, id) = (workspace.clone(), id.clone());
                    window.defer(cx, move |window, cx| {
                        let _ = workspace.update(cx, |this, cx| this.select(&id, window, cx));
                    });
                    menu.menu_with_disabled(pause, Box::new(TogglePause), !can_pause)
                        .menu_with_disabled(t("common.restart_session"), Box::new(RestartSession), !can_restart)
                        .menu_with_disabled(switch, Box::new(SwitchSession), missing)
                        .separator()
                        .menu_with_check(t("models.project_claude"), !glm, Box::new(UseClaude))
                        .menu_with_check(t("models.project_glm"), glm, Box::new(UseGlm))
                        .separator()
                        .menu(t("common.copy_path"), Box::new(CopyPath))
                        .menu_with_disabled(t("common.show_in_finder"), Box::new(RevealInFinder), missing)
                        .separator()
                        .when(can_split, |menu| menu.menu(t("common.split_into_subprojects"), Box::new(SplitProject)))
                        .when(is_sub, |menu| menu.menu(t("common.new_subproject"), Box::new(NewSubproject)))
                        .when(is_sub, |menu| {
                            menu.menu_with_disabled(t("common.clone_subproject"), Box::new(CloneSubproject), !can_clone)
                        })
                        .menu_with_disabled(t("common.rename_project"), Box::new(RenameProject), missing)
                        .menu(
                            if is_sub { t("common.remove_subproject") } else { t("common.remove_from_pult") },
                            Box::new(RemoveProject),
                        )
                }
            })
            .on_click(move |_, window, cx| {
                let _ = workspace.update(cx, |this, cx| this.select(&id, window, cx));
            });
        (item, urgent)
    }

    /// The row of a project split into subprojects: its subprojects go under it (the Kit's submenu). Folded,
    /// it shows the most urgent of what is going on inside.
    fn folder_row(&self, folder: &str, kids: &[&Project], cx: &mut Context<Self>) -> (SharedString, SidebarMenuItem) {
        let workspace = cx.entity().downgrade();
        let (muted, info) = (cx.theme().muted_foreground, cx.theme().info);
        let key: SharedString = format!("project-{folder}/").into();
        let (children, states): (Vec<SidebarMenuItem>, Vec<Option<Urgent>>) =
            kids.iter().map(|kid| self.project_row(kid, cx)).unzip();
        let summary = if states.contains(&Some(Urgent::Waiting)) {
            Some(Urgent::Waiting)
        } else if states.contains(&Some(Urgent::Working)) {
            Some(Urgent::Working)
        } else if states.iter().all(|state| *state == Some(Urgent::Paused)) {
            Some(Urgent::Paused)
        } else {
            None
        };
        let stored_open = !self.store.is_collapsed(folder);
        let reveal = self.reveal.as_deref() == Some(folder);
        let inside = self.current().is_some_and(|project| project.sub().is_some() && project.folder() == folder);
        let count = crate::i18n::count(kids.len() as u64, "workspace.count_subprojects");
        let tip: SharedString = match summary {
            Some(Urgent::Waiting) => format!("{count} · {}", t("workspace.waiting_for_you")),
            Some(Urgent::Working) => format!("{count} · {}", t("workspace.working")),
            Some(Urgent::Paused) => format!("{count} · {}", t("workspace.paused")),
            None => count,
        }
        .into();
        // A click on the project goes back to the subproject opened last.
        let target = self
            .last_sub
            .get(folder)
            .filter(|last| kids.iter().any(|kid| kid.id() == last.as_str()))
            .cloned()
            .unwrap_or_else(|| kids[0].id().to_string());
        let path = kids[0].path().to_path_buf();
        let missing = !kids[0].exists();
        let item = SidebarMenuItem::new(folder.to_string())
            .accessibility_label(format!("{folder}, {tip}"))
            .active(!stored_open && inside)
            .default_open(stored_open)
            .click_to_open(true)
            .children(children)
            .suffix({
                let (workspace, key, folder, tip) = (workspace.clone(), key.clone(), folder.to_string(), tip.clone());
                move |window, cx| -> AnyElement {
                    // The row keeps "unfolded" in its own state, keyed by the row id (GPUI Kit 0.10,
                    // `SidebarMenuItem::render`); this is that same state. Pult reads it to show the summary
                    // only on a folded project, unfolds the project when one of its subprojects is opened,
                    // and remembers the choice across launches.
                    let state = window.use_keyed_state(gpui_kit::ElementId::from(key.clone()), cx, |_, _| stored_open);
                    let mut open = *state.read(cx);
                    if reveal && !open {
                        state.update(cx, |open, cx| {
                            *open = true;
                            cx.notify();
                        });
                        open = true;
                    }
                    if open != stored_open || reveal {
                        let (workspace, folder) = (workspace.clone(), folder.clone());
                        cx.defer(move |cx| {
                            let _ = workspace.update(cx, |this, cx| this.set_folder_open(&folder, open, cx));
                        });
                    }
                    let Some(summary) = summary.filter(|_| !open) else {
                        return div().into_any_element();
                    };
                    let icon = match summary {
                        Urgent::Working => Spinner::new().xsmall().color(muted).into_any_element(),
                        Urgent::Waiting => div().size_2().rounded_full().bg(info).into_any_element(),
                        Urgent::Paused => Icon::new(AssetIcon::Pause).xsmall().text_color(muted).into_any_element(),
                    };
                    let tip = tip.clone();
                    div()
                        .id(SharedString::from(format!("state-{folder}/")))
                        .flex()
                        .items_center()
                        .child(icon)
                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
                        .into_any_element()
                }
            })
            .context_menu({
                // The commands apply to the whole project, not to the subproject that is open.
                let (workspace, folder) = (workspace.clone(), folder.to_string());
                move |menu, _, _| {
                    let new_sub = {
                        let (workspace, folder) = (workspace.clone(), folder.clone());
                        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                            let _ = workspace.update(cx, |this, cx| this.open_new_subproject(&folder, window, cx));
                        }
                    };
                    let copy = {
                        let path = path.clone();
                        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                            cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
                            window.push_notification(t("workspace.path_copied"), cx);
                        }
                    };
                    let show = {
                        let path = path.clone();
                        move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| cx.reveal_path(&path)
                    };
                    let remove = {
                        let (workspace, folder) = (workspace.clone(), folder.clone());
                        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                            let _ = workspace.update(cx, |this, cx| this.confirm_remove_folder(&folder, window, cx));
                        }
                    };
                    menu.item(PopupMenuItem::new(t("common.new_subproject")).disabled(missing).on_click(new_sub))
                        .separator()
                        .item(PopupMenuItem::new(t("common.copy_path")).on_click(copy))
                        .item(PopupMenuItem::new(t("common.show_in_finder")).disabled(missing).on_click(show))
                        .separator()
                        .item(PopupMenuItem::new(t("common.remove_from_pult")).on_click(remove))
                }
            })
            .on_click(move |_, window, cx| {
                let _ = workspace.update(cx, |this, cx| this.select(&target, window, cx));
            });
        (key, item)
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Projects go by folder; the subprojects of one folder are next to each other, in their own order.
        let mut items: Vec<(SharedString, SidebarMenuItem)> = Vec::new();
        let mut index = 0;
        while index < self.projects.len() {
            let project = &self.projects[index];
            if project.sub().is_none() {
                items.push((format!("project-{}", project.id()).into(), self.project_row(project, cx).0));
                index += 1;
                continue;
            }
            let folder = project.folder().to_string();
            let kids: Vec<&Project> = self.projects[index..]
                .iter()
                .take_while(|other| other.sub().is_some() && other.folder() == folder)
                .collect();
            index += kids.len();
            items.push(self.folder_row(&folder, &kids, cx));
        }

        let shortcut = |keys: &'static str| {
            move |_: &mut Window, _: &mut gpui_kit::App| {
                Keystroke::parse(keys)
                    .ok()
                    .map(|stroke| Kbd::new(stroke).into_any_element())
                    .unwrap_or_else(|| div().into_any_element())
            }
        };
        let new_project = SidebarMenuItem::new(t("common.new_project"))
            .icon(IconName::Plus)
            .suffix(shortcut("cmd-n"))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(NewProject), cx));
        let add_folders = SidebarMenuItem::new(t("common.add_folders"))
            .icon(Icon::new(AssetIcon::FolderPlus))
            .suffix(shortcut("cmd-o"))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(AddFolders), cx));

        Sidebar::new("projects")
            .collapsible(SidebarCollapsible::Offcanvas)
            .collapsed(!self.sidebar_open)
            .child(SidebarSection::Actions(Box::new(
                SidebarMenu::new().gap_0p5().children([new_project, add_folders]),
            )))
            // On first launch the empty Projects group is hidden: the placeholder on the right covers that.
            .when(!items.is_empty(), |this| {
                this.child(SidebarSection::Projects(
                    SidebarGroup::new(t("common.projects")).child(ProjectTree { rows: items, collapsed: false }),
                ))
            })
            .when_some(self.render_update_button(cx), |this, button| this.footer(button))
    }

    /// The button at the bottom of the sidebar while a new version is available or being installed; it opens
    /// the update window. Nothing otherwise.
    fn render_update_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::update::Phase;
        let updater = crate::update::updater(cx)?;
        let state = updater.read(cx);
        let version = state.release.as_ref()?.version.clone();
        let busy = state.busy();
        if !busy && !matches!(state.phase, Phase::Available | Phase::Failed(_)) {
            return None;
        }
        let label = if busy {
            tr!("update.updating_to_version", version = version)
        } else {
            tr!("workspace.update_to_version", version = version)
        };
        Some(
            Button::new("sidebar-update")
                .outline()
                .small()
                .w_full()
                .when(busy, |this| this.loading(true))
                .when(!busy, |this| this.icon(Icon::new(AssetIcon::Download)))
                .label(label)
                .tooltip(tr!("workspace.update_tooltip", current = env!("CARGO_PKG_VERSION")))
                .on_click(|_, window, cx| crate::update::open_dialog(window, cx))
                .into_any_element(),
        )
    }

    /// What to show in place of the terminal, if not the terminal itself.
    fn render_placeholder(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let workspace = cx.entity().downgrade();
        let Some(project) = self.current().cloned() else {
            if self.store.needs_root() {
                return Some(terminal::placeholder(
                    t("workspace.where_are_your_projects"),
                    t("workspace.choose_folder_that_holds"),
                    vec![
                        Button::new("empty-root")
                            .label(t("workspace.choose_folder"))
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(AddFolders), cx)),
                    ],
                    cx,
                ));
            }
            return Some(terminal::placeholder(
                t("workspace.no_projects"),
                t("workspace.create_project_or_add_folder"),
                vec![
                    Button::new("empty-add")
                        .label(t("common.add_folders"))
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(AddFolders), cx)),
                    Button::new("empty-new")
                        .label(t("common.new_project"))
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(NewProject), cx)),
                ],
                cx,
            ));
        };
        let id = project.id().to_string();
        if self.is_moving(&project) {
            return Some(terminal::placeholder_with(
                t("workspace.moving_folder"),
                t("workspace.session_is_paused_while_folder"),
                Vec::new(),
                true,
                cx,
            ));
        }
        if !project.exists() {
            let relocate = {
                let (workspace, id) = (workspace.clone(), id.clone());
                move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                    let _ = workspace.update(cx, |this, cx| this.relocate(&id, window, cx));
                }
            };
            return Some(terminal::placeholder(
                t("workspace.folder_not_found_2"),
                tr!("workspace.path_is_no_longer_there", path = display_path(project.path())),
                vec![
                    Button::new("missing-remove")
                        .label(t("common.remove_from_pult"))
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(RemoveProject), cx)),
                    Button::new("missing-relocate").label(t("workspace.locate_folder")).on_click(relocate),
                ],
                cx,
            ));
        }
        if self.pending_bind.contains_key(&id) {
            return None;
        }
        if self.claude_missing {
            let check = Button::new("claude-check").label(t("common.check_again")).on_click({
                let workspace = workspace.clone();
                move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                    let _ = workspace.update(cx, |this, cx| this.refresh(cx));
                }
            });
            return Some(terminal::placeholder_no_claude(check, cx));
        }
        if let Some((failure, error)) = self.errors.get(&id) {
            let failure = *failure;
            let retry = {
                let (workspace, id) = (workspace.clone(), id.clone());
                move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                    let _ = workspace.update(cx, |this, cx| {
                        let Some(project) = this.project(&id).cloned() else { return };
                        match failure {
                            Failure::New => this.new_session(&id, window, cx),
                            Failure::Move => this.take_over(&project, cx),
                            Failure::Resume => this.resume(&project, cx),
                        }
                    });
                }
            };
            let title = match failure {
                Failure::New => t("workspace.new_session_didn_t_start"),
                Failure::Move => t("workspace.couldn_t_move_session"),
                Failure::Resume => t("workspace.couldn_t_start_session"),
            };
            let mut buttons = Vec::new();
            if failure == Failure::New {
                let (workspace, id) = (workspace.clone(), id.clone());
                buttons.push(Button::new("error-choose").label(t("workspace.choose_existing")).on_click(
                    move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                        let _ = workspace.update(cx, |this, cx| {
                            this.errors.remove(&id);
                            this.start_switch(&id, cx);
                        });
                    },
                ));
            }
            buttons.push(Button::new("error-retry").label(t("common.try_again")).on_click(retry));
            return Some(terminal::placeholder(title, error.clone(), buttons, cx));
        }
        let Some(session) = project.session() else {
            let (new_session, choose) = {
                let (workspace, id) = (workspace.clone(), id.clone());
                let workspace2 = workspace.clone();
                let id2 = id.clone();
                (
                    move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                        let _ = workspace.update(cx, |this, cx| this.new_session(&id, window, cx));
                    },
                    move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                        let _ = workspace2.update(cx, |this, cx| this.start_switch(&id2, cx));
                    },
                )
            };
            let starting = self.starting.contains(&id);
            let text = if starting {
                tr!("workspace.starting_new_session_for_name", name = project.name())
            } else if self.dropped.contains(&id) {
                t("workspace.previous_session_was_empty_and").to_string()
            } else {
                tr!("workspace.name_keeps_one_pinned_claude", name = project.name())
            };
            return Some(terminal::placeholder_with(
                if starting { t("workspace.session_is_starting") } else { t("workspace.no_session_chosen_2") },
                text,
                if starting {
                    Vec::new()
                } else {
                    vec![
                        Button::new("unbound-choose").label(t("workspace.choose_existing")).on_click(choose),
                        Button::new("unbound-new").label(t("workspace.new_session")).on_click(new_session),
                    ]
                },
                starting,
                cx,
            ));
        };
        if let Some((_, since)) = self.handoff.get(&id) {
            let device = self.remote.get(session).map(|remote| remote.device.clone()).unwrap_or_default();
            let cancel = {
                let (workspace, id) = (workspace.clone(), id.clone());
                move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                    let _ = workspace.update(cx, |this, cx| this.cancel_take_over(&id, cx));
                }
            };
            let remote = self.remote.get(session).cloned().unwrap_or_default();
            let released = remote.released.filter(|released| released.at >= *since);
            let current = match released {
                None => 0,
                Some(released) if sessions::transcript_len(project.path(), session) < released.len => 1,
                Some(_) => 2,
            };
            let text = crate::sync::is_failing().then(|| SharedString::from(t("workspace.can_t_reach_sync_server")));
            return Some(terminal::placeholder_steps(
                tr!("workspace.moving_session_from_device", device = device),
                text,
                self.move_steps(session, &device, current),
                vec![Button::new("moving-cancel").label(t("common.cancel")).on_click(cancel)],
                cx,
            ));
        }
        if let Some(manifest) = self.restoring.get(&id) {
            let device = self.moved_from.get(&id).cloned().unwrap_or_else(|| manifest.device_name.clone());
            return Some(terminal::placeholder_steps(
                tr!("workspace.moving_session_from_device", device = device),
                None,
                self.move_steps(session, &device, 2),
                Vec::new(),
                cx,
            ));
        }
        if let Some((stuck, manifest)) = self.code_stuck.get(&id) {
            let device = manifest.device_name.clone();
            let action = |name: &'static str, label: &'static str, work: fn(&mut Workspace, &str, &mut Context<Workspace>)| {
                let (workspace, id) = (workspace.clone(), id.clone());
                Button::new(name).label(label).on_click(move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                    let _ = workspace.update(cx, |this, cx| work(this, &id, cx));
                })
            };
            let without = action("code-without", t("code.continue_without_code"), Workspace::continue_without_code);
            let (text, buttons): (String, Vec<Button>) = match stuck {
                crate::code::Stuck::Dirty { files, branch } => (
                    tr!(
                        "code.dirty",
                        name = project.name(),
                        files = crate::i18n::count(*files as u64, "code.files"),
                        branch = branch
                    ),
                    vec![action("code-cancel", t("common.cancel"), Workspace::keep_code_there), without],
                ),
                crate::code::Stuck::Big { megabytes } => (
                    tr!("code.big", device = device, size = megabytes),
                    vec![action("code-cancel", t("common.cancel"), Workspace::keep_code_there), without],
                ),
                crate::code::Stuck::NoRepo => (
                    tr!("code.no_repo", name = project.name()),
                    vec![action("code-cancel", t("common.cancel"), Workspace::keep_code_there), without],
                ),
                crate::code::Stuck::Fetch => (
                    t("code.fetch").to_string(),
                    vec![without, action("code-retry", t("common.try_again"), Workspace::retry_code)],
                ),
                crate::code::Stuck::Clone { remote } => (
                    tr!("code.clone", remote = remote),
                    vec![without, action("code-retry", t("common.try_again"), Workspace::retry_code)],
                ),
                crate::code::Stuck::NotArrived => (
                    tr!("code.not_arrived", device = device),
                    vec![without, action("code-retry", t("common.try_again"), Workspace::retry_code)],
                ),
                crate::code::Stuck::Failed(message) => (
                    tr!("code.failed", message = message),
                    vec![without, action("code-retry", t("common.try_again"), Workspace::retry_code)],
                ),
            };
            return Some(terminal::placeholder(tr!("code.not_in_place", device = device), text, buttons, cx));
        }
        if let Some(device) = self.elsewhere(&project) {
            let take = {
                let (workspace, id) = (workspace.clone(), id.clone());
                move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                    let _ = workspace.update(cx, |this, cx| {
                        if let Some(project) = this.project(&id).cloned() {
                            this.take_over(&project, cx);
                        }
                    });
                }
            };
            let silent = self.remote.get(session).filter(|remote| remote.silent(crate::presence::unix_now()));
            let carries = matches!(self.code_rows.get(session), Some(crate::code::Summary::Carried { .. }));
            let (title, text) = if self.presence.released.contains_key(session) {
                let text = if carries { t("code.paused_here_code_went") } else { t("workspace.it_s_paused_here_so") };
                (tr!("workspace.session_continued_on_device", device = device), text.to_string())
            } else if let Some(remote) = silent {
                let minutes = crate::presence::unix_now().saturating_sub(remote.active_at) / 60;
                (
                    tr!("workspace.session_is_open_on_device", device = device),
                    tr!("workspace.device_silent_session_may_run", device = device, ago = crate::i18n::count(minutes.max(1), "journal.minutes_ago")),
                )
            } else {
                (
                    tr!("workspace.session_is_open_on_device", device = device),
                    if crate::code::enabled() { t("code.session_runs_in_one_place") } else { t("workspace.session_runs_in_one_place") }
                        .to_string(),
                )
            };
            return Some(terminal::placeholder(
                title,
                text,
                vec![Button::new("elsewhere-take").label(t("workspace.continue_here")).on_click(take)],
                cx,
            ));
        }
        if self.starting.contains(&id)
            && let Some(device) = self.moved_from.get(&id)
        {
            return Some(terminal::placeholder_steps(
                tr!("workspace.moving_session_from_device", device = device),
                None,
                self.move_steps(session, device, 3),
                Vec::new(),
                cx,
            ));
        }
        if self.starting.contains(&id) {
            return Some(terminal::placeholder_with(
                t("workspace.session_is_starting"),
                tr!("workspace.picking_up_name_where_it", name = project.name()),
                Vec::new(),
                true,
                cx,
            ));
        }
        if let Some(short) = self.failed(&project) {
            let (resume, new_session) = {
                let (workspace, id) = (workspace.clone(), id.clone());
                let workspace2 = workspace.clone();
                let id2 = id.clone();
                (
                    move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                        let _ = workspace.update(cx, |this, cx| {
                            if let Some(project) = this.project(&id).cloned() {
                                this.resume(&project, cx);
                            }
                        });
                    },
                    move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                        let _ = workspace2.update(cx, |this, cx| this.new_session(&id2, window, cx));
                    },
                )
            };
            let text = sessions::last_failure(&short).unwrap_or_else(|| t("workspace.session_failed_no_reason").to_string());
            return Some(terminal::placeholder(
                t("workspace.session_stopped_with_error"),
                text,
                vec![
                    Button::new("failed-new").label(t("workspace.new_session")).on_click(new_session),
                    Button::new("failed-retry").label(t("common.try_again")).on_click(resume),
                ],
                cx,
            ));
        }
        match self.agent(session) {
            Some(agent) if agent.is_background() && !self.is_paused(&project) => None,
            Some(agent) if agent.is_running() => {
                if self.terminal.read(cx).pid(&id) == agent.pid {
                    None
                } else {
                    let switch = {
                        let (workspace, id) = (workspace.clone(), id.clone());
                        move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                            let _ = workspace.update(cx, |this, cx| this.start_switch(&id, cx));
                        }
                    };
                    Some(terminal::placeholder(
                        t("workspace.session_is_open_in_another"),
                        t("workspace.close_it_there_pult_will"),
                        vec![Button::new("elsewhere-switch").label(t("common.change_session")).on_click(switch)],
                        cx,
                    ))
                }
            }
            _ => {
                let resume = {
                    let (workspace, id) = (workspace.clone(), id.clone());
                    move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                        let _ = workspace.update(cx, |this, cx| {
                            if let Some(project) = this.project(&id).cloned() {
                                this.resume(&project, cx);
                            }
                        });
                    }
                };
                Some(terminal::placeholder(
                    tr!("workspace.name_is_paused", name = project.name()),
                    t("workspace.claude_is_stopped_and_uses"),
                    vec![Button::new("paused-resume").label(t("workspace.continue")).on_click(resume)],
                    cx,
                ))
            }
        }
    }

    /// The Session section of the right panel.
    /// The project's ideas file: notes and bookmarks in `.pult/` that travel with the sessions (`sync::DOT_PULT`).
    fn ideas_path(project: &Project) -> PathBuf {
        project.path().join(crate::sync::DOT_PULT).join("ideas.md")
    }

    /// "Ideas" in the right panel: open the project's ideas file for a quick look, or create it.
    fn render_ideas(&self, project: &Project, cx: &mut Context<Self>) -> AnyElement {
        let (muted, border) = (cx.theme().muted_foreground, cx.theme().border);
        let path = Self::ideas_path(project);
        let exists = path.is_file();
        let workspace = cx.entity().downgrade();
        let id = project.id().to_string();
        let header = h_flex()
            .py_1p5()
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(border)
            .font_medium()
            .child(Icon::new(AssetIcon::Lightbulb).xsmall())
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(t("workspace.ideas"))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_normal()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .text_color(muted)
                            .child(".pult/ideas.md"),
                    ),
            );
        let open = move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
            let _ = workspace.update(cx, |this, cx| this.open_ideas(&id, window, cx));
        };
        let body = v_flex()
            .p_2()
            .gap_2()
            .child(div().text_color(muted).child(if exists { t("workspace.ideas_hint") } else { t("workspace.ideas_none") }))
            .child(
                div().child(
                    Button::new("ideas-open")
                        .outline()
                        .small()
                        .label(if exists { t("workspace.ideas_open") } else { t("workspace.ideas_create") })
                        .disabled(!project.exists())
                        .on_click(open),
                ),
            );
        v_flex()
            .border_1()
            .border_color(border)
            .rounded(cx.theme().radius_lg)
            .text_sm()
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// Show the ideas file in a sheet, read-only (Markdown through the Kit's `TextView`). A missing file is created
    /// first with a heading, so "Create" lands in the same view.
    fn open_ideas(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project(id).cloned() else {
            return;
        };
        let path = Self::ideas_path(&project);
        if !path.is_file() {
            let text = tr!("workspace.ideas_template", name = project.name());
            let created = path.parent().map(std::fs::create_dir_all).unwrap_or(Ok(())).and_then(|()| std::fs::write(&path, text));
            if let Err(error) = created {
                window.push_notification(tr!("workspace.ideas_create_failed", error = error), cx);
                return;
            }
            crate::sync::exclude_from_git(project.path());
            cx.notify();
        }
        let text: SharedString = match std::fs::read_to_string(&path) {
            Ok(text) => text.into(),
            Err(error) => {
                window.push_notification(tr!("workspace.ideas_read_failed", error = error), cx);
                return;
            }
        };
        let title: SharedString = tr!("workspace.ideas_title", name = project.name()).into();
        window.open_sheet(cx, move |sheet, _, _| {
            let (reveal, edit) = (path.clone(), path.clone());
            sheet
                .title(title.clone())
                .size(px(560.))
                .child(
                    div().px_1().child(
                        gpui_kit::component::text::TextView::markdown("ideas-view", text.clone()).selectable(true),
                    ),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("ideas-reveal")
                                .ghost()
                                .label(t("common.show_in_finder"))
                                .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
                        )
                        .child(
                            Button::new("ideas-edit")
                                .outline()
                                .label(t("workspace.ideas_edit"))
                                .on_click(move |_, _, cx| cx.open_with_system(&edit)),
                        ),
                )
        });
    }

    fn render_session(&self, project: &Project, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let workspace = cx.entity().downgrade();
        let id = project.id().to_string();
        let activity = self.activity(project);
        let switching = self.switching.contains(&id);

        let remote = self.elsewhere(project).is_some();
        let state = activity.map(|activity| match activity {
            _ if remote => (t("workspace.on_another_computer_2"), muted),
            Activity::Working if self.handoff.contains_key(&id) => (t("workspace.moving"), muted),
            Activity::Working if self.starting.contains(&id) => (t("workspace.starting"), muted),
            Activity::Working => (t("workspace.working_2"), theme.success),
            Activity::Waiting => (t("workspace.waiting_for_you_2"), theme.info),
            Activity::Ready => (t("workspace.ready_2"), muted),
            Activity::Stopped => (t("workspace.paused_2"), muted),
        });
        // Like an Accordion (small) section header: py 6, px 8, icon gap 4 — aligned with Plugins.
        let header = h_flex()
            .py_1p5()
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(border)
            .font_medium()
            .child(Icon::new(AssetIcon::MessageSquare).xsmall())
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(t("workspace.session"))
                    .when(project.session().is_some() && project.exists() && !switching, |this| {
                        this.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_normal()
                                .font_family(theme.mono_font_family.clone())
                                .text_xs()
                                .text_color(muted)
                                .child(project.session_name()),
                        )
                    }),
            )
            .when_some(state.filter(|_| project.session().is_some() && !switching), |this, (label, color)| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .font_normal()
                        .text_color(muted)
                        .child(div().size_2().rounded_full().bg(color))
                        .child(label),
                )
            });

        let body: AnyElement = if !project.exists() {
            let relocate = {
                let (workspace, id) = (workspace.clone(), id.clone());
                move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                    let _ = workspace.update(cx, |this, cx| this.relocate(&id, window, cx));
                }
            };
            v_flex()
                .p_2()
                .gap_2()
                .child(t("workspace.folder_not_found_2"))
                .child(
                    div()
                        .text_color(muted)
                        .child(t("workspace.session_is_saved_locate_project")),
                )
                .child(
                    div().child(
                        Button::new("session-relocate").outline().small().label(t("workspace.locate_folder")).on_click(relocate),
                    ),
                )
                .into_any_element()
        } else if let (Some(session), false) = (project.session(), switching) {
            self.render_bound(project, session, activity, cx)
        } else {
            self.render_pick(project, switching, cx)
        };

        v_flex()
            .border_1()
            .border_color(border)
            .rounded(cx.theme().radius_lg)
            .text_sm()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_bound(&self, project: &Project, session: &str, activity: Option<Activity>, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, mono) = (theme.muted_foreground, theme.border, theme.mono_font_family.clone());
        let workspace = cx.entity().downgrade();
        let id = project.id().to_string();
        let agent = self.agent(session);
        let memory = agent
            .and_then(|agent| agent.pid)
            .and_then(|pid| self.memory.get(&pid))
            .map(|mb| tr!("workspace.mb_mb", mb = mb))
            .unwrap_or_else(|| "—".into());
        let last = sessions::last_activity(project.path(), session)
            .map(sessions::ago)
            .unwrap_or_else(|| "—".into());
        let row = |label: &'static str, value: AnyElement| {
            h_flex()
                .min_h_6()
                .gap_2()
                .child(div().w_24().flex_none().text_color(muted).child(label))
                .child(div().flex_1().min_w_0().child(value))
        };
        let session_full = session.to_string();
        let skips = agent
            .filter(|agent| agent.is_background())
            .and_then(|agent| agent.id.as_deref())
            .and_then(sessions::skips_permissions);
        let paused = activity == Some(Activity::Stopped);
        let remote = self.elsewhere(project).is_some();
        let starting = self.starting.contains(&id) || self.handoff.contains_key(&id);

        let pause = {
            let (workspace, id) = (workspace.clone(), id.clone());
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                let _ = workspace.update(cx, |this, cx| {
                    if paused {
                        if let Some(project) = this.project(&id).cloned() {
                            this.resume(&project, cx);
                        }
                    } else {
                        this.pause(&id, false, window, cx);
                    }
                });
            }
        };
        let switch = {
            let (workspace, id) = (workspace.clone(), id.clone());
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                let _ = workspace.update(cx, |this, cx| this.start_switch(&id, cx));
            }
        };

        let previous = project.previous();
        v_flex()
            .child(
                v_flex()
                    .px_2()
                    .py_1p5()
                    .gap_1()
                    .child(row(t("workspace.activity"), div().child(last).into_any_element()))
                    .when_some(
                        agent.filter(|_| activity == Some(Activity::Waiting)).and_then(|agent| agent.waiting_for.clone()),
                        |this, reason| {
                            // Claude Code writes the reason in English; the one it always writes is translated.
                            let text: SharedString = match reason.as_str() {
                                "input needed" => t("workspace.waiting_reason_input").into(),
                                _ => reason.into(),
                            };
                            this.child(row(t("workspace.waiting_row"), div().child(text).into_any_element()))
                        },
                    )
                    .when(project.sub().is_some(), |this| {
                        let change = {
                            let (workspace, id) = (workspace.clone(), id.clone());
                            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                                let _ = workspace.update(cx, |this, cx| this.open_about_dialog(&id, window, cx));
                            }
                        };
                        let about = project.about().to_string();
                        this.child(row(
                            t("workspace.what_it_does_row"),
                            // The text takes the whole width, the button goes under it: the value column is narrow.
                            v_flex()
                                .items_start()
                                .gap_1()
                                .child(if about.is_empty() {
                                    div().min_w_0().text_color(muted).child(t("workspace.not_set"))
                                } else {
                                    div().min_w_0().child(SharedString::from(about))
                                })
                                .child(
                                    Button::new("session-about")
                                        .outline()
                                        .xsmall()
                                        .label(t("workspace.change_about"))
                                        .on_click(change),
                                )
                                .into_any_element(),
                        ))
                    })
                    .child(row(t("workspace.memory"), div().child(memory).into_any_element()))
                    .when_some(skips, |this, skips| {
                        let skip = {
                            let (workspace, id) = (workspace.clone(), id.clone());
                            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                                let _ = workspace.update(cx, |this, cx| this.skip_permissions(&id, false, window, cx));
                            }
                        };
                        this.child(row(
                            t("workspace.permissions"),
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().min_w_0().truncate().child(if skips {
                                    t("workspace.no_prompts")
                                } else {
                                    t("workspace.asks")
                                }))
                                .when(!skips, |this| {
                                    this.child(
                                        Button::new("session-skip-permissions")
                                            .outline()
                                            .xsmall()
                                            .label(t("workspace.restart_without_prompts"))
                                            .tooltip(t("workspace.claude_will_stop_asking_permission"))
                                            .disabled(starting)
                                            .on_click(skip),
                                    )
                                })
                                .into_any_element(),
                        ))
                    })
                    .child({
                        let (main, note, warn): (SharedString, SharedString, bool) =
                            match self.code_rows.get(session).cloned().unwrap_or(crate::code::Summary::None) {
                                crate::code::Summary::NoSync => (t("code.row_not_carried").into(), t("code.row_no_sync").into(), false),
                                crate::code::Summary::Off => (t("code.row_not_carried").into(), t("code.row_off").into(), false),
                                crate::code::Summary::Nogit => (t("code.row_not_carried").into(), t("code.row_nogit").into(), false),
                                crate::code::Summary::Big { megabytes } => {
                                    (t("code.row_not_carried").into(), tr!("code.row_big", size = megabytes).into(), true)
                                }
                                crate::code::Summary::None => (t("code.row_no_snapshot").into(), t("code.row_after_answer").into(), false),
                                crate::code::Summary::Carried { worktree, files, at } => {
                                    let place = match worktree {
                                        Some(name) => tr!("code.row_worktree", name = name),
                                        None => t("code.row_checkout").to_string(),
                                    };
                                    let changes = if files == 0 {
                                        t("code.row_no_changes").to_string()
                                    } else {
                                        crate::i18n::count(files as u64, "code.changed_files")
                                    };
                                    let when = sessions::ago(std::time::UNIX_EPOCH + Duration::from_secs(at));
                                    (format!("{place} · {changes}").into(), tr!("code.row_snapshot", when = when).into(), false)
                                }
                            };
                        let warning = cx.theme().warning;
                        // Two lines: the label stands at the first one.
                        h_flex()
                            .items_start()
                            .min_h_6()
                            .gap_2()
                            .child(div().w_24().flex_none().text_color(muted).child(t("code.row")))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().min_w_0().child(main))
                                    .child(
                                        h_flex()
                                            .items_start()
                                            .gap_1()
                                            .text_xs()
                                            .text_color(if warn { cx.theme().foreground } else { muted })
                                            .when(warn, |this| {
                                                this.child(Icon::new(AssetIcon::TriangleAlert).xsmall().mt_0p5().text_color(warning))
                                            })
                                            .child(div().min_w_0().child(note)),
                                    ),
                            )
                    })
                    .child(row(
                        "ID",
                        h_flex()
                            .gap_1()
                            .child(
                                div()
                                    .font_family(mono)
                                    .text_xs()
                                    .child(sessions::short_id(session).to_string()),
                            )
                            .child(
                                Button::new("copy-session-id")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(AssetIcon::Copy))
                                    .tooltip(t("workspace.copy_session_id"))
                                    .on_click(move |_, window, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(session_full.clone()));
                                        window.push_notification(t("workspace.session_id_copied"), cx);
                                    }),
                            )
                            .into_any_element(),
                    )),
            )
            .when(!self.neighbours.is_empty(), |this| {
                this.child(
                    v_flex()
                        .px_2()
                        .pb_2()
                        .gap_1p5()
                        .child(div().text_xs().text_color(muted).child(t("workspace.what_neighbours_know")))
                        .child(
                            div()
                                .p_2()
                                .border_1()
                                .border_color(border)
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().muted)
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_xs()
                                .child(SharedString::from(self.neighbours.clone())),
                        ),
                )
            })
            .child(
                h_flex()
                    .flex_wrap()
                    .px_2()
                    .pb_2()
                    .gap_2()
                    .child(
                        Button::new("session-pause")
                            .outline()
                            .small()
                            .icon(Icon::new(if paused { AssetIcon::Play } else { AssetIcon::Pause }))
                            .label(if remote { t("workspace.continue_here") } else if paused { t("workspace.continue") } else { t("workspace.pause") })
                            .disabled(starting)
                            .on_click(pause),
                    )
                    .child(Button::new("session-switch").outline().small().label(t("common.change_session")).on_click(switch)),
            )
            .when(!previous.is_empty(), |this| {
                let toggle = cx.listener(|this, _: &gpui_kit::ClickEvent, _, cx| {
                    this.previous_open = !this.previous_open;
                    cx.notify();
                });
                this.child(
                    v_flex()
                        .border_t_1()
                        .border_color(border)
                        .child(
                            Button::new("previous-toggle")
                                .ghost()
                                .small()
                                .w_full()
                                .rounded_none()
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_1()
                                        .child(t("workspace.previous_sessions"))
                                        .child(div().text_color(muted).child(previous.len().to_string()))
                                        .child(div().flex_1())
                                        .child(
                                            Icon::new(if self.previous_open {
                                                IconName::ChevronUp
                                            } else {
                                                IconName::ChevronDown
                                            })
                                            .xsmall()
                                            .text_color(muted),
                                        ),
                                )
                                .on_click(toggle),
                        )
                        .when(self.previous_open, |this| {
                            this.child(v_flex().px_1().pb_1p5().children(previous.iter().map(|previous| {
                                let restore = {
                                    let (workspace, id, session) =
                                        (workspace.clone(), id.clone(), previous.id.clone());
                                    move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                                        let _ = workspace
                                            .update(cx, |this, cx| this.restore(&id, &session, window, cx));
                                    }
                                };
                                h_flex()
                                    .min_h_10()
                                    .px_2()
                                    .gap_2()
                                    .rounded(cx.theme().radius)
                                    .hover(|this| this.bg(cx.theme().accent))
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .child(div().truncate().child(sessions::short_id(&previous.id).to_string()))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(muted)
                                                    .child(tr!("workspace.until_until", until = previous.until)),
                                            ),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!("restore-{}", previous.id)))
                                            .ghost()
                                            .xsmall()
                                            .label(t("workspace.restore"))
                                            .on_click(restore),
                                    )
                            })))
                        }),
                )
            })
            .into_any_element()
    }

    fn render_pick(&self, project: &Project, switching: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, warning, foreground) = (theme.muted_foreground, theme.warning, theme.foreground);
        let workspace = cx.entity().downgrade();
        let id = project.id().to_string();
        let running: HashSet<&str> = self
            .agents
            .iter()
            .filter(|agent| agent.is_running())
            .map(|agent| agent.session_id.as_str())
            .collect();
        let list: Vec<&Transcript> = self
            .transcripts
            .get(&id)
            .map(|list| list.iter().filter(|item| Some(item.id.as_str()) != project.session()).collect())
            .unwrap_or_default();
        let chosen = self.choice.get(&id).cloned();
        let starting = self.starting.contains(&id);
        let has_list = !list.is_empty();

        let new_session = {
            let (workspace, id) = (workspace.clone(), id.clone());
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                let _ = workspace.update(cx, |this, cx| this.new_session(&id, window, cx));
            }
        };
        let cancel = {
            let (workspace, id) = (workspace.clone(), id.clone());
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                let _ = workspace.update(cx, |this, cx| {
                    this.switching.remove(&id);
                    this.choice.remove(&id);
                    cx.notify();
                });
            }
        };
        let bind = {
            let (workspace, id) = (workspace.clone(), id.clone());
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                let _ = workspace.update(cx, |this, cx| this.bind_choice(&id, window, cx));
            }
        };

        let new_button = Button::new("session-new")
            .small()
            .w_full()
            .icon(IconName::Plus)
            .label(t("workspace.new_session"))
            .loading(starting)
            .on_click(new_session);
        let new_button = if list.is_empty() { new_button.primary() } else { new_button.outline() };

        v_flex()
            .p_2()
            .gap_3()
            .child(div().text_color(muted).child(if switching {
                t("workspace.current_session_moves_to_previous")
            } else {
                t("workspace.each_project_keeps_one_pinned")
            }))
            .child(new_button)
            .child(
                v_flex()
                    .gap_0p5()
                    .child(h_flex().h_6().text_xs().text_color(muted).child(t("workspace.sessions_in_this_folder")))
                    .when(list.is_empty(), |this| {
                        this.child(div().py_2().text_color(muted).child(t("workspace.no_sessions_in_this_folder")))
                    })
                    .children(list.into_iter().map(|item| {
                        let busy = running.contains(item.id.as_str());
                        let checked = chosen.as_deref() == Some(item.id.as_str());
                        let choose = {
                            let (workspace, id, session) = (workspace.clone(), id.clone(), item.id.clone());
                            move |_: &bool, _: &mut Window, cx: &mut gpui_kit::App| {
                                let _ = workspace.update(cx, |this, cx| {
                                    this.choice.insert(id.clone(), session.clone());
                                    cx.notify();
                                });
                            }
                        };
                        let meta = if busy {
                            t("workspace.open_in_another_window").to_string()
                        } else {
                            format!("{} · {}", sessions::ago(item.modified), sessions::size_label(item.size))
                        };
                        Radio::new(SharedString::from(format!("pick-{}", item.id)))
                            .checked(checked)
                            .disabled(busy)
                            .on_click(choose)
                            .w_full()
                            .py_1()
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(div().truncate().child(item.title.clone()))
                                    .child(
                                        // Yellow text is unreadable on a light background: the color goes on the icon, the text stays normal.
                                        h_flex()
                                            .gap_1()
                                            .text_xs()
                                            .text_color(if busy { foreground } else { muted })
                                            .when(busy, |this| {
                                                this.child(Icon::new(AssetIcon::TriangleAlert).xsmall().text_color(warning))
                                            })
                                            .child(meta),
                                    ),
                            )
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .when(switching, |this| {
                        this.child(Button::new("session-cancel").outline().small().label(t("common.cancel")).on_click(cancel))
                    })
                    .when(has_list, |this| {
                        this.child(
                            Button::new("session-bind")
                                .primary()
                                .small()
                                .label(t("workspace.pin"))
                                .disabled(chosen.is_none())
                                .on_click(bind),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_info(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(project) = self.current().cloned() else {
            return div().into_any_element();
        };
        let setup = ClaudeSetup::read(project.path());
        let open = |index: usize| self.info_sections.contains(&index);
        let sections = [
            (Icon::new(AssetIcon::Puzzle), t("workspace.plugins"), &setup.plugins, false),
            (Icon::new(AssetIcon::Sparkles), t("workspace.skills"), &setup.skills, false),
            (Icon::new(AssetIcon::ScrollText), t("workspace.rules"), &setup.rules, true),
        ];
        let mut accordion = Accordion::new("claude-setup")
            .h_auto()
            .bordered(true)
            .multiple(true)
            .small()
            .on_toggle_click(cx.listener(|this, open: &[usize], _, cx| {
                this.info_sections = open.to_vec();
                cx.notify();
            }));
        for (index, (icon, title, scoped, mono)) in sections.into_iter().enumerate() {
            accordion = accordion.item(|item| {
                item.icon(icon)
                    .open(open(index))
                    .title(
                        h_flex()
                            .gap_1()
                            .child(title)
                            .child(
                                div()
                                    .font_normal()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(scoped.len().to_string()),
                            ),
                    )
                    .child(render_scoped(scoped, mono, cx))
            });
        }
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .h_10()
                    .flex_none()
                    .pl_4()
                    .pr_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .child(div().flex_1().font_medium().child(t("common.connected_to_claude")))
                    .child(
                        Button::new("close-info")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .tooltip(t("workspace.hide_connected_to_claude"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ToggleInfo), cx)
                            }),
                    ),
            )
            .child(
                div()
                    .id("claude-setup-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .p_3()
                            .gap_3()
                            .child(self.render_session(&project, cx))
                            .child(self.render_ideas(&project, cx))
                            .child(accordion),
                    ),
            )
            .into_any_element()
    }
}

/// Poll Claude Code: sessions and the memory of their processes.
fn poll_agents() -> Polled {
    // The registry first: read right before the list, so start times and list belong to one moment.
    let live = sessions::live_sessions();
    let agents = sessions::try_agents();
    let missing = agents.as_ref().is_none_or(Vec::is_empty) && !sessions::installed();
    let agents = if missing { Some(Vec::new()) } else { agents };
    let memory = agents
        .iter()
        .flatten()
        .filter_map(|agent| agent.pid)
        .filter_map(|pid| Some((pid, sessions::memory_mb(pid)?)))
        .collect();
    (agents, memory, missing, live)
}

/// A complete sentence: ends with a period (move messages sometimes lack one).
fn sentence(text: &str) -> String {
    let text = text.trim_end();
    if text.ends_with(['.', '!', '?', '…']) { text.to_string() } else { format!("{text}.") }
}

/// What failed: it decides what Try again does.
/// What to do once the main folder is chosen on a first launch.
#[derive(Clone, Copy)]
enum AfterRoot {
    AddFolders,
    NewProject,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Failure {
    /// Start the pinned session.
    Resume,
    /// Start a new session.
    New,
    /// Move the session from another computer.
    Move,
}

/// What a state the program reported means for the project. `error` is for the person to look at, as a
/// question or a permission is: the program stopped and says why.
fn activity_of(state: State) -> Activity {
    match state {
        State::Working => Activity::Working,
        State::Blocked | State::Error => Activity::Waiting,
        State::Idle | State::Done => Activity::Ready,
        State::Clear => Activity::Ready,
    }
}

/// Tooltip on a list state icon: for a session on another computer — with that computer's name.
fn remote_tip(state: &str, device: Option<String>) -> SharedString {
    match device {
        Some(device) => tr!("workspace.open_on_device", device = device).into(),
        None => {
            let mut chars = state.chars();
            chars.next().map(|first| first.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default().into()
        }
    }
}

/// Sidebar sections, as in the mockup: actions on top separately,
/// below them the Projects group (items 2 px apart). `Sidebar` takes children of a single type.
#[derive(Clone)]
enum SidebarSection {
    Actions(Box<SidebarMenu>),
    Projects(SidebarGroup<ProjectTree>),
}

/// What a folded project reports about its subprojects, most urgent first.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Urgent {
    Waiting,
    Working,
    Paused,
}

/// The project rows of the sidebar. Like the Kit's `SidebarMenu`, but every row is rendered under an id made
/// of the project's name instead of its position in the list: the Kit keeps "subprojects unfolded" by row id,
/// and with positions that state would jump to a neighbour whenever a project is added or removed.
#[derive(Clone)]
struct ProjectTree {
    rows: Vec<(SharedString, SidebarMenuItem)>,
    collapsed: bool,
}

impl Collapsible for ProjectTree {
    fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }

    fn is_collapsed(&self) -> bool {
        self.collapsed
    }
}

impl SidebarItem for ProjectTree {
    fn render(
        self,
        _: impl Into<gpui_kit::ElementId>,
        window: &mut Window,
        cx: &mut gpui_kit::App,
    ) -> impl IntoElement {
        let collapsed = self.collapsed;
        v_flex().gap_0p5().children(
            self.rows
                .into_iter()
                .map(|(key, row)| SidebarItem::render(row.collapsed(collapsed), key, window, cx).into_any_element())
                .collect::<Vec<_>>(),
        )
    }
}

impl Collapsible for SidebarSection {
    fn collapsed(self, collapsed: bool) -> Self {
        match self {
            Self::Actions(menu) => Self::Actions(Box::new(menu.collapsed(collapsed))),
            Self::Projects(group) => Self::Projects(group.collapsed(collapsed)),
        }
    }

    fn is_collapsed(&self) -> bool {
        match self {
            Self::Actions(menu) => menu.is_collapsed(),
            Self::Projects(group) => group.is_collapsed(),
        }
    }
}

impl SidebarItem for SidebarSection {
    fn render(
        self,
        id: impl Into<gpui_kit::ElementId>,
        window: &mut Window,
        cx: &mut gpui_kit::App,
    ) -> impl IntoElement {
        match self {
            Self::Actions(menu) => SidebarItem::render(*menu, id, window, cx).into_any_element(),
            // A gap between sections, like between Sidebar groups (gap_y_3).
            Self::Projects(group) => div()
                .pt_3()
                .child(SidebarItem::render(group, id, window, cx))
                .into_any_element(),
        }
    }
}

/// Two subgroups of a section: In this project and All projects, like sidebar groups.
fn render_scoped(scoped: &Scoped, mono: bool, cx: &Context<Workspace>) -> impl IntoElement {
    let group = |label: &'static str, names: &[String]| {
        v_flex()
            .child(
                h_flex()
                    .h_6()
                    .px_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .when(names.is_empty(), |this| {
                this.child(
                    h_flex()
                        .h_7()
                        .px_2()
                        .text_color(cx.theme().muted_foreground)
                        .child(t("workspace.none")),
                )
            })
            .children(names.iter().map(|name| {
                h_flex()
                    .h_7()
                    .px_2()
                    .when(mono, |this| {
                        this.font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                    })
                    .child(div().truncate().child(name.clone()))
            }))
    };
    v_flex()
        .mx(rems(-0.5))
        .child(group(t("workspace.in_this_project"), &scoped.project))
        .child(group(t("workspace.all_projects"), &scoped.global))
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.notices.is_empty() {
            let notices = std::mem::take(&mut self.notices);
            cx.defer_in(window, move |_, window, cx| {
                for notice in notices {
                    window.push_notification(notice, cx);
                }
            });
        }
        if let Some(to) = self.pending_provider.take() {
            cx.defer_in(window, move |this, window, cx| {
                window.activate_window();
                this.use_provider_everywhere(to, window, cx);
            });
        }
        let placeholder = self.render_placeholder(cx);
        // A placeholder is in place of the terminal but focus stayed on it (it isn't on screen) — button commands
        // and keys then go nowhere. Focus the window.
        // Only if focus is on the hidden terminal or on nothing: an open window (dialog) is left alone.
        let stale = self.terminal.read(cx).focus_handle(cx).is_focused(window) || window.focused(cx).is_none();
        if placeholder.is_some() && stale {
            self.focus_handle.focus(window, cx);
        } else if placeholder.is_none() && self.focus_handle.is_focused(window) {
            self.terminal.update(cx, |terminal, cx| terminal.focus(window, cx));
        }
        v_flex()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_info))
            .on_action(cx.listener(Self::toggle_theme))
            .on_action(cx.listener(Self::copy_path))
            .on_action(cx.listener(Self::reveal_in_finder))
            .on_action(cx.listener(Self::clear_terminal))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::zoom_reset))
            .on_action(cx.listener(Self::new_project))
            .on_action(cx.listener(Self::add_folders))
            .on_action(cx.listener(Self::toggle_pause))
            .on_action(cx.listener(Self::restart_current))
            .on_action(cx.listener(Self::pause_others))
            .on_action(cx.listener(Self::switch_session))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::remove_project))
            .on_action(cx.listener(Self::rename_project))
            .on_action(cx.listener(Self::split_current))
            .on_action(cx.listener(Self::new_subproject))
            .on_action(cx.listener(Self::clone_current))
            .on_action(cx.listener(Self::use_claude))
            .on_action(cx.listener(Self::use_glm))
            .on_action(cx.listener(Self::open_models))
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .items_stretch()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(
                        div().flex_1().min_w_0().h_full().child(
                            h_resizable("main-split")
                                .child(resizable_panel().child(match placeholder {
                                    Some(placeholder) => placeholder,
                                    None => self.terminal.clone().into_any_element(),
                                }))
                                .child(
                                    resizable_panel()
                                        .visible(self.info_open && self.current().is_some())
                                        .size(px(320.))
                                        .size_range(px(260.)..px(480.))
                                        // The width the panel opened with (or was
                                        // dragged to). Without `fixed`, GPUI Kit keeps it still
                                        // only in the first frame, and from the next redraw
                                        // (~1 s) the panel shrinks to 260: the terminal remembers the full
                                        // window width recorded while the panel was hidden.
                                        .fixed()
                                        .child(self.render_info(cx)),
                                ),
                        ),
                    ),
            )
    }
}

/// A two-line row of the models menu: the name and, under it, what it means.
fn model_menu_row(title: &'static str, hint: SharedString, cx: &gpui_kit::App) -> gpui_kit::AnyElement {
    v_flex().child(title).child(div().text_xs().text_color(cx.theme().muted_foreground).child(hint)).into_any_element()
}
