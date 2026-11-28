//! Pult updates. `https://pult.hitkey.io/latest.json` describes the newest version and says where
//! its installer is (a GitHub release); `scripts/publish-update.sh` writes both. No key is needed.
//!
//! Before replacing anything, the installer checksum, the app signature, the developer team and the
//! version number are checked; any mismatch — nothing is installed. The swap happens after Pult quits: a short
//! script waits for the process to exit, puts the new app in place of the old one and opens it. Claude sessions
//! run in the background and don't notice the update.

use std::{
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogFooter,
    h_flex,
    notification::Notification,
    progress::Progress,
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, Global, IntoElement, ParentElement as _, SharedString,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::i18n::t;
use crate::sync;

/// Developer team that signs Pult. A different signature means it's not our app.
const TEAM: &str = "P2J4M43S93";
const DAY: u64 = 86_400;

/// The published version.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Release {
    pub version: String,
    pub sha256: String,
    pub size: u64,
    /// Where its installer is downloaded from (HTTPS only).
    pub url: String,
    /// What's new — for people, one line per item.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Where the check came from: decides how the result is reported.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Origin {
    /// Pult itself: at launch and once a day. A new version — as a notification, everything else silently.
    Auto,
    /// "Pult → Check for updates…".
    Menu,
    /// The "Check" button in Settings: the result shows right there.
    Settings,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    Checking,
    Latest,
    Available,
    Downloading,
    Verifying,
    Installing,
    /// Check failed (the text says what to do).
    CheckFailed(SharedString),
    /// Install failed.
    Failed(SharedString),
}

pub enum UpdateEvent {
    Found(Origin),
    Latest(Origin),
    CheckFailed(Origin, SharedString),
}

pub struct Updater {
    pub phase: Phase,
    pub release: Option<Release>,
    pub checked_at: u64,
    received: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
}

impl EventEmitter<UpdateEvent> for Updater {}

struct UpdaterGlobal(Entity<Updater>);
impl Global for UpdaterGlobal {}

// ───────────────────────── settings ─────────────────────────

/// `update.json` next to the rest of Pult's data.
#[derive(Default, Serialize, Deserialize)]
struct Prefs {
    /// Check automatically. Field missing — yes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auto: Option<bool>,
    #[serde(default)]
    checked_at: u64,
    /// Version just installed: on next launch Pult says it was updated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    installed: Option<String>,
}

fn prefs_path() -> PathBuf {
    sync::Paths::real().app.join("update.json")
}

fn prefs() -> Prefs {
    fs::read_to_string(prefs_path()).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn save_prefs(prefs: &Prefs) {
    let path = prefs_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(prefs) {
        let _ = fs::write(path, text);
    }
}

pub fn auto() -> bool {
    prefs().auto.unwrap_or(true)
}

pub fn set_auto(on: bool) {
    let mut prefs = prefs();
    prefs.auto = Some(on);
    save_prefs(&prefs);
}

/// Version Pult was just updated to (once — then forgotten).
pub fn take_installed() -> Option<String> {
    let mut prefs = prefs();
    let installed = prefs.installed.take()?;
    save_prefs(&prefs);
    (installed == env!("CARGO_PKG_VERSION")).then_some(installed)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|time| time.as_secs()).unwrap_or_default()
}

/// `a` is newer than `b`: compares dot-separated numbers.
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |version: &str| version.split('.').map(|part| part.trim().parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    let (a, b) = (parts(a), parts(b));
    (0..a.len().max(b.len())).map(|ix| (a.get(ix).copied().unwrap_or(0), b.get(ix).copied().unwrap_or(0))).find(|(x, y)| x != y).is_some_and(|(x, y)| x > y)
}

// ───────────────────────── server ─────────────────────────

fn agent(timeout: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_global(timeout)
        .timeout_per_read(Some(Duration::from_secs(120)))
        .http_status_as_error(false)
        .user_agent(concat!("pult/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn no_network() -> String {
    t("update.can_t_reach_server_check").to_string()
}

/// The description of the newest published version.
const UPDATES: &str = "https://pult.hitkey.io/latest.json";

/// The description as published. An installer that isn't fetched over HTTPS is refused.
fn parse_release(text: &str) -> Option<Release> {
    let release: Release = serde_json::from_str(text).ok()?;
    release.url.starts_with("https://").then_some(release)
}

/// A newer published version, if any.
fn latest() -> Result<Option<Release>, String> {
    let mut response = agent(Some(Duration::from_secs(30))).get(UPDATES).call().map_err(|_| no_network())?;
    match response.status().as_u16() {
        200 => {}
        404 => return Ok(None),
        _ => return Err(t("update.couldn_t_check_for_new").into()),
    }
    let text = response.body_mut().read_to_string().map_err(|_| no_network())?;
    let release = parse_release(&text).ok_or_else(|| t("update.server_sent_version_description_pult").to_string())?;
    Ok(newer(&release.version, env!("CARGO_PKG_VERSION")).then_some(release))
}

const CANCELLED: &str = "\u{0}cancelled";

/// Download the installer into the cache, checking the checksum on the fly.
fn download(release: &Release, received: &AtomicU64, cancel: &AtomicBool) -> Result<PathBuf, String> {
    let dir = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("pult/update");
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let file = dir.join(format!("Pult-{}.dmg", release.version));
    let part = dir.join(format!("Pult-{}.dmg.part", release.version));
    let mut response = agent(None)
        .get(&release.url)
        .call()
        .map_err(|_| t("update.couldn_t_download_update_check").to_string())?;
    if response.status().as_u16() != 200 {
        return Err(t("update.server_didn_t_send_update").into());
    }
    let mut body = response.body_mut().as_reader();
    let mut out = fs::File::create(&part).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 << 10];
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = fs::remove_file(&part);
            return Err(CANCELLED.into());
        }
        let read = body.read(&mut buffer).map_err(|_| t("update.download_was_interrupted_check_your").to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        out.write_all(&buffer[..read]).map_err(|error| error.to_string())?;
        received.fetch_add(read as u64, Ordering::Relaxed);
    }
    drop(out);
    let sha: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    if sha != release.sha256 {
        let _ = fs::remove_file(&part);
        return Err(t("update.downloaded_file_doesn_t_match").into());
    }
    fs::rename(&part, &file).map_err(|error| error.to_string())?;
    Ok(file)
}

/// The app Pult runs from (`…/Pult.app`) — if it was started from the installed app
/// and its folder is writable.
fn installed_bundle() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().and_then(|exe| exe.canonicalize()).map_err(|error| error.to_string())?;
    let bundle = exe.ancestors().nth(3).filter(|path| path.extension().is_some_and(|ext| ext == "app")).map(Path::to_path_buf);
    let bundle = bundle.ok_or(t("update.only_installed_pult_in_applications"))?;
    let parent = bundle.parent().ok_or(t("update.couldn_t_find_app_s"))?;
    let probe = parent.join(".pult-write-test");
    fs::write(&probe, b"").map_err(|_| tr!("update.no_permission_to_change_parent", parent = parent.display()))?;
    let _ = fs::remove_file(probe);
    Ok(bundle)
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program).args(args).output().map_err(|error| error.to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    if output.status.success() { Ok(text) } else { Err(text) }
}

/// Extract the app from the installer next to the installed one and verify it. Returns its path.
fn prepare(dmg: &Path, release: &Release, bundle: &Path) -> Result<PathBuf, String> {
    let mount = dmg.with_extension("mount");
    let _ = run("/usr/bin/hdiutil", &["detach", "-force", &mount.to_string_lossy()]);
    fs::create_dir_all(&mount).map_err(|error| error.to_string())?;
    run("/usr/bin/hdiutil", &["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint", &mount.to_string_lossy(), &dmg.to_string_lossy()])
        .map_err(|_| t("update.couldn_t_open_installer").to_string())?;
    let staged = bundle.with_file_name(".Pult-update.app");
    let _ = fs::remove_dir_all(&staged);
    let copied = run("/usr/bin/ditto", &[&mount.join("Pult.app").to_string_lossy(), &staged.to_string_lossy()]);
    let _ = run("/usr/bin/hdiutil", &["detach", "-force", &mount.to_string_lossy()]);
    let _ = fs::remove_dir(&mount);
    copied.map_err(|_| t("update.installer_doesn_t_contain_pult").to_string())?;

    let checked = (|| {
        let staged_text = staged.to_string_lossy();
        run("/usr/bin/codesign", &["--verify", "--deep", "--strict", &staged_text])
            .map_err(|_| t("update.app_s_signature_didn_t").to_string())?;
        let info = run("/usr/bin/codesign", &["-dv", "--verbose=2", &staged_text])
            .map_err(|_| t("update.app_s_signature_can_t").to_string())?;
        if !info.lines().any(|line| line.trim() == format!("TeamIdentifier={TEAM}")) {
            return Err(t("update.app_is_signed_by_different").to_string());
        }
        let plist = staged.join("Contents/Info.plist");
        let version = run("/usr/libexec/PlistBuddy", &["-c", "Print CFBundleShortVersionString", &plist.to_string_lossy()]).unwrap_or_default();
        if version.trim() != release.version {
            return Err(tr!("update.installer_has_version_version_not", version = version.trim(), version2 = release.version));
        }
        Ok(())
    })();
    if let Err(error) = checked {
        let _ = fs::remove_dir_all(&staged);
        return Err(error);
    }
    Ok(staged)
}

/// Swap script: waits for Pult to quit, puts the new app in place of the old one, refreshes its icon for the Dock
/// and Finder and opens it. If the swap fails, the old app is put back.
const SWAP: &str = r#"
pid=$1; old=$2; new=$3
while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
if mv "$old" "$old.pult-old"; then
  if mv "$new" "$old"; then rm -rf "$old.pult-old"; else mv "$old.pult-old" "$old"; fi
fi
# The Dock and Finder keep the old icon until LaunchServices sees the bundle anew.
touch "$old"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$old" >/dev/null 2>&1
open "$old"
killall Dock >/dev/null 2>&1
"#;

fn swap(bundle: &Path, staged: &Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;
    Command::new("/bin/sh")
        .args(["-c", SWAP, "pult-swap", &std::process::id().to_string(), &bundle.to_string_lossy(), &staged.to_string_lossy()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

// ───────────────────────── state ─────────────────────────

pub fn init(cx: &mut App) {
    // For checking the view without a server: `PULT_FAKE_UPDATE=<version>` pretends that version is available.
    let fake = std::env::var("PULT_FAKE_UPDATE").ok().map(|version| Release {
        version,
        sha256: String::new(),
        size: 0,
        url: String::new(),
        notes: Vec::new(),
    });
    let updater = cx.new(|_| Updater {
        phase: if fake.is_some() { Phase::Available } else { Phase::Idle },
        release: fake,
        checked_at: prefs().checked_at,
        received: Arc::new(AtomicU64::new(0)),
        cancel: Arc::new(AtomicBool::new(false)),
    });
    cx.set_global(UpdaterGlobal(updater.clone()));
    // At launch and once a day — if enabled.
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_secs(15)).await;
        let mut first = true;
        loop {
            let due = first || now().saturating_sub(prefs().checked_at) >= DAY;
            if due && auto() {
                updater.update(cx, |updater, cx| updater.check(Origin::Auto, cx));
            }
            first = false;
            cx.background_executor().timer(Duration::from_secs(3600)).await;
        }
    })
    .detach();
}

pub fn updater(cx: &App) -> Option<Entity<Updater>> {
    cx.try_global::<UpdaterGlobal>().map(|global| global.0.clone())
}

pub fn check_now(origin: Origin, cx: &mut App) {
    if let Some(updater) = updater(cx) {
        updater.update(cx, |updater, cx| updater.check(origin, cx));
    }
}

impl Updater {
    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Downloading | Phase::Verifying | Phase::Installing)
    }

    pub fn check(&mut self, origin: Origin, cx: &mut Context<Self>) {
        if self.busy() || self.phase == Phase::Checking {
            return;
        }
        self.phase = Phase::Checking;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async { latest() }).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(found) => {
                        this.checked_at = now();
                        let mut prefs = prefs();
                        prefs.checked_at = this.checked_at;
                        save_prefs(&prefs);
                        match found {
                            Some(release) => {
                                this.release = Some(release);
                                this.phase = Phase::Available;
                                cx.emit(UpdateEvent::Found(origin));
                            }
                            None => {
                                this.release = None;
                                this.phase = Phase::Latest;
                                cx.emit(UpdateEvent::Latest(origin));
                            }
                        }
                    }
                    Err(error) => {
                        let error = SharedString::from(error);
                        this.phase = Phase::CheckFailed(error.clone());
                        cx.emit(UpdateEvent::CheckFailed(origin, error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn install(&mut self, cx: &mut Context<Self>) {
        let Some(mut release) = self.release.clone() else {
            return;
        };
        if self.busy() {
            return;
        }
        let bundle = match installed_bundle() {
            Ok(bundle) => bundle,
            Err(error) => {
                self.phase = Phase::Failed(error.into());
                cx.notify();
                return;
            }
        };
        self.phase = Phase::Downloading;
        self.received.store(0, Ordering::Relaxed);
        self.cancel.store(false, Ordering::Relaxed);
        cx.notify();
        // The progress bar is redrawn while downloading.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let going = this.update(cx, |this, cx| {
                    cx.notify();
                    this.phase == Phase::Downloading
                });
                if !matches!(going, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
        let (received, cancel) = (self.received.clone(), self.cancel.clone());
        cx.spawn(async move |this, cx| {
            let fail = |this: &mut Updater, error: String, cx: &mut Context<Updater>| {
                this.phase = if error == CANCELLED { Phase::Available } else { Phase::Failed(error.into()) };
                cx.notify();
            };
            // Ask again which version is the newest: one published since the last check is the one to install.
            match cx.background_spawn(async { latest() }).await {
                Ok(Some(fresh)) => release = fresh,
                Ok(None) | Err(_) => {}
            }
            let _ = this.update(cx, |this, cx| {
                this.release = Some(release.clone());
                cx.notify();
            });
            let dmg = cx.background_spawn({
                let release = release.clone();
                async move { download(&release, &received, &cancel) }
            });
            let dmg = match dmg.await {
                Ok(dmg) => dmg,
                Err(error) => {
                    let _ = this.update(cx, |this, cx| fail(this, error, cx));
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                this.phase = Phase::Verifying;
                cx.notify();
            });
            let staged = cx.background_spawn({
                let (release, bundle) = (release.clone(), bundle.clone());
                async move { prepare(&dmg, &release, &bundle) }
            });
            let staged = match staged.await {
                Ok(staged) => staged,
                Err(error) => {
                    let _ = this.update(cx, |this, cx| fail(this, error, cx));
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                this.phase = Phase::Installing;
                cx.notify();
            });
            let mut prefs = prefs();
            prefs.installed = Some(release.version.clone());
            save_prefs(&prefs);
            if let Err(error) = swap(&bundle, &staged) {
                let _ = fs::remove_dir_all(&staged);
                let _ = this.update(cx, |this, cx| fail(this, error, cx));
                return;
            }
            // The window gets to show "Installing", then Pult quits — the script installs the new one and opens it.
            cx.background_executor().timer(Duration::from_millis(800)).await;
            cx.update(|cx| cx.quit());
        })
        .detach();
    }

    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn progress(&self) -> (u64, u64) {
        (self.received.load(Ordering::Relaxed), self.release.as_ref().map(|release| release.size).unwrap_or_default())
    }
}

// ───────────────────────── notification and window ─────────────────────────

/// "Version … available" notification with an "Update…" button. Doesn't go away on its own.
pub fn notice(cx: &App) -> Notification {
    let release = updater(cx).and_then(|updater| updater.read(cx).release.clone());
    let version = release.as_ref().map(|release| release.version.clone()).unwrap_or_default();
    let note = release.and_then(|release| release.notes.into_iter().next()).unwrap_or_default();
    Notification::new()
        .title(tr!("update.version_version_is_available", version = version))
        .message(note)
        .icon(Icon::new(AssetIcon::Download))
        .action(|_, _, cx| {
            let notice = cx.entity();
            Button::new("update-open").outline().label(t("common.update")).on_click(move |_, window, cx| {
                notice.update(cx, |notice, cx| notice.dismiss(window, cx));
                open_dialog(window, cx);
            })
        })
}

/// Megabytes with one decimal: a decimal comma and Russian units in Russian, "12.4 MB" in English.
fn megabytes(bytes: u64) -> String {
    let value = bytes as f64 / 1_048_576.;
    crate::i18n::decimal(&tr!("update.megabytes", value = value : {:.1}))
}

/// "Update Pult to …?" window → download → verification → install; errors show in the same window.
pub fn open_dialog(window: &mut Window, cx: &mut App) {
    let Some(updater) = updater(cx) else {
        return;
    };
    window.open_dialog(cx, move |dialog, _, cx| {
        let state = updater.read(cx);
        let busy = state.busy();
        let version = state.release.as_ref().map(|release| release.version.clone()).unwrap_or_default();
        let title = match &state.phase {
            Phase::Failed(_) => t("update.update_not_installed").to_string(),
            Phase::Downloading | Phase::Verifying | Phase::Installing => tr!("update.updating_to_version", version = version),
            _ => tr!("update.update_pult_to_version", version = version),
        };
        let (body, footer) = dialog_parts(&updater, cx);
        dialog
            .w(px(420.))
            .close_button(false)
            .keyboard(!busy)
            .overlay_closable(!busy)
            .title(div().text_base().font_weight(gpui_kit::FontWeight::SEMIBOLD).child(title))
            .child(body)
            .when_some(footer, |dialog, footer| dialog.footer(footer))
    });
}

fn dialog_parts(updater: &Entity<Updater>, cx: &App) -> (AnyElement, Option<AnyElement>) {
    let state = updater.read(cx);
    let muted = cx.theme().muted_foreground;
    match &state.phase {
        Phase::Downloading | Phase::Verifying | Phase::Installing => {
            let (got, total) = state.progress();
            let percent = if state.phase == Phase::Downloading && total > 0 { got as f32 * 100. / total as f32 } else { 100. };
            let line: SharedString = match state.phase {
                Phase::Downloading => tr!("update.downloading_got_of_total", got = megabytes(got), total = megabytes(total)).into(),
                Phase::Verifying => t("update.verifying_signature").into(),
                _ => t("update.installing_pult_will_restart").into(),
            };
            let spinning = state.phase != Phase::Downloading;
            let body = v_flex()
                .gap_2()
                .child(Progress::new("update-progress").value(percent))
                .child(h_flex().gap_1p5().text_sm().text_color(muted).when(spinning, |row| row.child(Spinner::new().small())).child(line))
                .into_any_element();
            let footer = (state.phase == Phase::Downloading).then(|| {
                let updater = updater.clone();
                DialogFooter::new()
                    .child(Button::new("update-cancel").outline().label(t("common.cancel")).on_click(move |_, _, cx| {
                        updater.update(cx, |updater, _| updater.cancel());
                    }))
                    .into_any_element()
            });
            (body, footer)
        }
        Phase::Failed(error) => {
            let danger = cx.theme().danger;
            let body = h_flex()
                .items_start()
                .gap_1p5()
                .text_sm()
                .text_color(danger)
                .child(Icon::new(AssetIcon::TriangleAlert).small().mt_0p5().text_color(danger))
                .child(div().flex_1().min_w_0().child(error.clone()))
                .into_any_element();
            let retry = updater.clone();
            let footer = DialogFooter::new()
                .child(Button::new("update-close").outline().label(t("update.close")).on_click(|_, window, cx| window.close_dialog(cx)))
                .child(Button::new("update-retry").primary().label(t("common.try_again")).on_click(move |_, _, cx| {
                    retry.update(cx, |updater, cx| updater.install(cx));
                }))
                .into_any_element();
            (body, Some(footer))
        }
        _ => {
            let notes = state.release.as_ref().map(|release| release.notes.clone()).unwrap_or_default();
            let body = v_flex()
                .gap_2()
                .text_sm()
                .text_color(muted)
                .when(!notes.is_empty(), |body| {
                    body.child(t("update.what_s_new")).child(
                        v_flex().gap_0p5().children(notes.into_iter().map(|note| h_flex().items_start().gap_2().child("•").child(div().flex_1().min_w_0().child(note)))),
                    )
                })
                .child(t("update.pult_will_close_and_reopen"))
                .into_any_element();
            let start = updater.clone();
            let available = state.phase == Phase::Available;
            let footer = DialogFooter::new()
                .child(Button::new("update-later").outline().label(t("update.later")).on_click(|_, window, cx| window.close_dialog(cx)))
                .child(Button::new("update-start").primary().label(t("update.update")).disabled(!available).on_click(move |_, _, cx| {
                    start.update(cx, |updater, cx| updater.install(cx));
                }))
                .into_any_element();
            (body, Some(footer))
        }
    }
}

// ───────────────────────── commands ─────────────────────────

/// `pult update check | install` — the same without a window: for testing and for maintenance over SSH.
/// Install waits only for this process to exit: close a running Pult beforehand.
pub fn cli(args: &[String]) -> i32 {
    let result = (|| -> Result<String, String> {
        match args.first().map(String::as_str) {
            Some("check") => Ok(match latest()? {
                Some(release) => tr!("update.version_version_is_available_size", version = release.version, size = release.size),
                None => tr!("update.you_have_latest_version_env", env = env!("CARGO_PKG_VERSION")),
            }),
            Some("install") => {
                let release = latest()?
                    .ok_or_else(|| tr!("update.you_have_latest_version_env", env = env!("CARGO_PKG_VERSION")))?;
                let bundle = installed_bundle()?;
                let (received, cancel) = (AtomicU64::new(0), AtomicBool::new(false));
                let dmg = download(&release, &received, &cancel)?;
                eprintln!("{}", tr!("update.downloaded_load_checksum_matches", load = megabytes(received.load(Ordering::Relaxed))));
                let staged = prepare(&dmg, &release, &bundle)?;
                eprintln!("{}", t("update.signature_and_version_verified"));
                let mut prefs = prefs();
                prefs.installed = Some(release.version.clone());
                save_prefs(&prefs);
                swap(&bundle, &staged)?;
                Ok(tr!("update.pult_version_will_replace_bundle", version = release.version, bundle = bundle.display()))
            }
            _ => Err("pult update check | install".into()),
        }
    })();
    match result {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{newer, parse_release};

    #[test]
    fn a_release_is_read_and_must_come_over_https() {
        let json = r#"{"version": "1.2.3", "sha256": "ab", "size": 7, "url": "https://example.com/a.dmg", "notes": ["One"]}"#;
        let release = parse_release(json).unwrap();
        assert_eq!((release.version.as_str(), release.size, release.notes.len()), ("1.2.3", 7, 1));
        assert!(parse_release(&json.replace("https://", "http://")).is_none());
        assert!(parse_release(r#"{"version": "1.2.3", "sha256": "ab", "size": 7}"#).is_none());
    }

    #[test]
    fn versions_compare_by_numbers() {
        assert!(newer("0.3.0", "0.2.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0", "0.99.99"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.2.0", "0.3.0"));
        assert!(newer("0.2.1", "0.2"));
    }
}
