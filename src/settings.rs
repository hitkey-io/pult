//! "Settings" window (⌘,): a separate window, as is customary on macOS, built on the GPUI Kit `Settings` component.
//! "General" — theme and terminal font size, "Folders" — the main folder (one for all of Pult),
//! "Sync" — the server Claude Code sessions are sent to (`crate::sync`).

use std::time::{Duration, UNIX_EPOCH};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, TitleBar, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    alert::Alert,
    input::{Input, InputEvent, InputState, OtpEvent, OtpInput, OtpState},
    label::Label,
    setting::{NumberFieldOptions, SelectIndex, SettingField, SettingGroup, SettingItem, SettingPage, Settings},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, Focusable as _, IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString,
    Styled as _, Subscription, WeakEntity, Window, WindowBounds, WindowOptions, div, prelude::FluentBuilder as _, px, size,
};
use gpui_kit::assets::IconName as AssetIcon;

use crate::i18n::{LangPref, count, t};
use crate::models::{self, Provider};
use crate::projects::{ThemePref, display_path};
use crate::rename::{self, Target};
use crate::sessions;
use crate::sync;
use crate::terminal::profile;
use crate::update::{self, Phase as UpdatePhase};
use crate::workspace::Workspace;

/// Step of connecting to the sync server.
enum Phase {
    /// Not connected: the address field.
    Off,
    /// Server found and sent the owner a code: the code field.
    Code { address: String, api: String, pairing: String, server: String, via: String, code: Entity<OtpState>, _watch: Subscription },
    On,
}

/// The "Sync" page.
struct SyncUi {
    address: Entity<InputState>,
    phase: Phase,
    /// A request to the server is in progress (lookup, confirmation, disconnect).
    busy: bool,
    /// A push started by the "Send now" button is in progress.
    pushing: bool,
    /// What failed on the current step; shown next to that step's field.
    error: Option<String>,
    /// How much the server stores and how much free space it has.
    server: Option<(u64, u64)>,
    ticks: u32,
    /// Time of the last successful push the window has already seen.
    sent_at: u64,
}

/// Where the check of the GLM key stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    Idle,
    Checking,
    Ok,
    Rejected,
    Offline,
}

/// The "Models" page.
struct ModelsUi {
    key: Entity<InputState>,
    check: Check,
    /// The login shell sets its own `ANTHROPIC_*` variables (found once, in the background).
    shell_env: bool,
}

/// The index of the "Models" page: Settings opened from the chip in the title bar lands on it.
pub const MODELS_PAGE: usize = 1;

pub struct SettingsView {
    workspace: WeakEntity<Workspace>,
    sync: SyncUi,
    models: ModelsUi,
    /// The page the window was opened on.
    start_page: SelectIndex,
    _subscriptions: Vec<Subscription>,
}

pub fn projects_label(n: usize) -> String {
    count(n as u64, "settings.count_project")
}

fn files_label(n: u64) -> String {
    count(n, "settings.count_file").replacen(&n.to_string(), &grouped(n), 1)
}

/// Digit groups: a narrow no-break space in Russian ("1 240"), a comma in English ("1,240").
fn grouped(number: u64) -> String {
    let separator = if crate::i18n::is_ru() { '\u{202f}' } else { ',' };
    let digits = number.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(separator);
        }
        out.push(digit);
    }
    out
}

/// What is happening with sending right now.
enum SyncState {
    Sending(String),
    Failed(String),
    Idle(String),
}

fn sync_state(status: &sync::Status, pushing: bool) -> SyncState {
    if status.pending > 0 {
        return SyncState::Sending(tr!("settings.pending_left_to_send", pending = files_label(status.pending)));
    }
    if pushing {
        return SyncState::Sending(t("settings.sending").into());
    }
    if let Some(error) = &status.error
        && status.error_at >= status.ok_at
    {
        return SyncState::Failed(error.clone());
    }
    match status.ok_at {
        0 => SyncState::Idle(t("settings.nothing_sent_yet").into()),
        at => {
            let when = sessions::ago(UNIX_EPOCH + Duration::from_secs(at));
            let when = if when == t("sessions.just_now") { t("settings.just_now").to_string() } else { when };
            let mut text = tr!("settings.all_sent_when", when = when);
            if status.skipped > 0 {
                text.push_str(&tr!("settings.too_large_to_send_skipped", skipped = files_label(status.skipped)));
            }
            SyncState::Idle(text)
        }
    }
}

impl SettingsView {
    fn general(&self, cx: &mut Context<Self>) -> SettingPage {
        let workspace = self.workspace.clone();
        let (read, write) = (workspace.clone(), workspace.clone());
        let (lang_read, lang_write) = (workspace.clone(), workspace.clone());
        let (theme_read, theme_write) = (workspace.clone(), workspace);
        SettingPage::new(t("settings.general")).icon(Icon::new(AssetIcon::SlidersHorizontal)).group(
            // No group titles: in Settings a group title becomes a sub-item in the sidebar,
            // and we don't want those there.
            SettingGroup::new()
                .item(
                    SettingItem::new(
                        t("settings.theme"),
                        SettingField::dropdown(
                            vec![
                                ("system".into(), t("settings.system").into()),
                                ("light".into(), t("settings.light").into()),
                                ("dark".into(), t("settings.dark").into()),
                            ],
                            move |cx| {
                                let pref = theme_read
                                    .upgrade()
                                    .map(|workspace| workspace.read(cx).theme_pref())
                                    .unwrap_or_default();
                                match pref {
                                    ThemePref::System => "system".into(),
                                    ThemePref::Light => "light".into(),
                                    ThemePref::Dark => "dark".into(),
                                }
                            },
                            move |value, cx| {
                                let pref = match value.as_ref() {
                                    "light" => ThemePref::Light,
                                    "dark" => ThemePref::Dark,
                                    _ => ThemePref::System,
                                };
                                if let Some(workspace) = theme_write.upgrade() {
                                    workspace.update(cx, |workspace, cx| workspace.set_theme(pref, cx));
                                }
                            },
                        ),
                    )
                    .description(t("settings.terminal_always_uses_iterm2_colors")),
                )
                .item(
                    // Language names — each in its own language, so someone who can't read the other one can still find it.
                    SettingItem::new(
                        t("settings.language"),
                        SettingField::dropdown(
                            vec![
                                ("system".into(), t("settings.system").into()),
                                ("ru".into(), "Русский".into()),
                                ("en".into(), "English".into()),
                            ],
                            move |cx| {
                                let pref = lang_read
                                    .upgrade()
                                    .map(|workspace| workspace.read(cx).language_pref())
                                    .unwrap_or_default();
                                match pref {
                                    LangPref::System => "system".into(),
                                    LangPref::Ru => "ru".into(),
                                    LangPref::En => "en".into(),
                                }
                            },
                            move |value, cx| {
                                let pref = match value.as_ref() {
                                    "ru" => LangPref::Ru,
                                    "en" => LangPref::En,
                                    _ => LangPref::System,
                                };
                                if let Some(workspace) = lang_write.upgrade() {
                                    workspace.update(cx, |workspace, cx| workspace.set_language(pref, cx));
                                }
                            },
                        ),
                    )
                    .description(t("settings.system_russian_if_macos_is")),
                )
                .item(
                    SettingItem::new(
                        t("settings.terminal_font_size"),
                        SettingField::number_input(
                            NumberFieldOptions {
                                min: f64::from(profile::MIN_FONT_SIZE),
                                max: f64::from(profile::MAX_FONT_SIZE),
                                step: 1.,
                            },
                            move |cx| {
                                read.upgrade()
                                    .map(|workspace| f64::from(workspace.read(cx).font_size(cx)))
                                    .unwrap_or(f64::from(profile::FONT_SIZE))
                            },
                            move |size, cx| {
                                if let Some(workspace) = write.upgrade() {
                                    workspace.update(cx, |workspace, cx| workspace.set_font_size(size as f32, cx));
                                }
                            },
                        )
                        .default_value(f64::from(profile::FONT_SIZE)),
                    )
                    .description(t("settings.same_as_0_in_terminal")),
                ),
        )
        .group(self.updates(cx))
    }

    /// "Updates": the version and its status, "Check" / "Update…", automatic checks.
    fn updates(&self, cx: &mut Context<Self>) -> SettingGroup {
        let (phase, version, checked_at) = update::updater(cx)
            .map(|updater| {
                let state = updater.read(cx);
                (state.phase.clone(), state.release.as_ref().map(|release| release.version.clone()), state.checked_at)
            })
            .unwrap_or((UpdatePhase::Idle, None, 0));
        let new = version.clone().unwrap_or_default();
        let checked = match checked_at {
            0 => t("settings.not_checked_yet").to_string(),
            at => {
                let when = sessions::ago(UNIX_EPOCH + Duration::from_secs(at));
                if when == t("sessions.just_now") {
                    t("settings.checked_just_now").to_string()
                } else {
                    tr!("settings.checked_when", when = when)
                }
            }
        };
        let (line, problem): (SharedString, bool) = match &phase {
            UpdatePhase::Checking => (t("settings.checking").into(), false),
            UpdatePhase::Available => (tr!("settings.version_new_is_available", new = new).into(), false),
            UpdatePhase::Downloading | UpdatePhase::Verifying | UpdatePhase::Installing => {
                (tr!("settings.updating_to_new", new = new).into(), false)
            }
            UpdatePhase::CheckFailed(error) => (error.clone(), true),
            UpdatePhase::Failed(_) => {
                (tr!("settings.version_new_is_available_last", new = new).into(), false)
            }
            UpdatePhase::Idle if checked_at == 0 => (t("settings.updates_haven_t_been_checked").into(), false),
            UpdatePhase::Latest | UpdatePhase::Idle => (tr!("settings.latest_version_checked", checked = checked).into(), false),
        };
        let spinning = matches!(phase, UpdatePhase::Checking | UpdatePhase::Downloading | UpdatePhase::Verifying | UpdatePhase::Installing);
        let offer = version.is_some() && !matches!(phase, UpdatePhase::Latest | UpdatePhase::Checking | UpdatePhase::CheckFailed(_));
        let row = SettingItem::render(move |_, _, cx| {
            let color = if problem { cx.theme().danger } else { cx.theme().muted_foreground };
            h_flex()
                .w_full()
                .justify_between()
                .gap_3()
                .child(
                    v_flex().flex_1().min_w_0().child(Label::new(tr!("settings.version_env", env = env!("CARGO_PKG_VERSION"))).text_sm()).child(
                        h_flex()
                            .items_start()
                            .gap_1p5()
                            .text_sm()
                            .text_color(color)
                            .when(spinning, |row| row.child(div().mt_0p5().child(Spinner::new().small())))
                            .when(problem, |row| row.child(Icon::new(AssetIcon::TriangleAlert).small().mt_0p5().text_color(color)))
                            .child(div().flex_1().min_w_0().child(line.clone())),
                    ),
                )
                .child(if offer {
                    Button::new("update-offer").primary().label(t("common.update")).on_click(|_, window, cx| update::open_dialog(window, cx))
                } else {
                    Button::new("update-check")
                        .outline()
                        .label(t("settings.check"))
                        .disabled(spinning)
                        .on_click(|_, _, cx| update::check_now(update::Origin::Settings, cx))
                })
        });
        SettingGroup::new()
            .item(row)
            .item(
                SettingItem::new(
                    t("settings.check_automatically"),
                    SettingField::switch(|_| update::auto(), |on, _| update::set_auto(on)),
                )
                .description(t("settings.when_pult_starts_and_once")),
            )
    }

    fn folders(&self) -> SettingPage {
        let workspace = self.workspace.clone();
        SettingPage::new(t("settings.folders")).icon(Icon::new(AssetIcon::Folder)).group(
            SettingGroup::new()
                .title(t("common.main_folder"))
                .description(t("settings.one_for_all_of_pult"))
                .item(SettingItem::render(move |_, _, cx| {
                    let Some(entity) = workspace.upgrade() else {
                        return div().into_any_element();
                    };
                    let (root, count, empty, unchosen) = {
                        let workspace = entity.read(cx);
                        (workspace.root(), workspace.root_count(), !workspace.has_projects(), workspace.needs_root())
                    };
                    let workspace = workspace.clone();
                    let chooser = workspace.clone();
                    let from = root.clone();
                    let choose_label = if unchosen { t("workspace.choose_folder") } else { t("settings.choose_another") };
                    h_flex()
                        .w_full()
                        .gap_3()
                        .child(Icon::new(AssetIcon::Folder).small().text_color(cx.theme().muted_foreground))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .when(unchosen, |this| {
                                    this.child(div().text_sm().text_color(cx.theme().muted_foreground).child(t("settings.not_chosen")))
                                })
                                .when(!unchosen, |this| {
                                    this.child(
                                        div()
                                            .truncate()
                                            .font_family(cx.theme().mono_font_family.clone())
                                            .text_sm()
                                            .child(display_path(&root)),
                                    )
                                    .child(
                                        div().text_sm().text_color(cx.theme().muted_foreground).child(projects_label(count)),
                                    )
                                }),
                        )
                        // With no projects yet the main folder can simply be another one; with projects it is
                        // moved instead, so they stay inside it.
                        .when(empty, |this| {
                            this.child(Button::new("root-choose").outline().label(choose_label).on_click(
                                move |_, window, cx| {
                                    let paths = cx.prompt_for_paths(PathPromptOptions {
                                        files: false,
                                        directories: true,
                                        multiple: false,
                                        prompt: Some(t("common.choose").into()),
                                    });
                                    let workspace = chooser.clone();
                                    window
                                        .spawn(cx, async move |cx| {
                                            if let Ok(Ok(Some(mut paths))) = paths.await
                                                && let Some(path) = paths.pop()
                                            {
                                                let _ = cx.update(|window, cx| {
                                                    match workspace.update(cx, |workspace, cx| {
                                                        let result = workspace.set_root(path);
                                                        cx.notify();
                                                        result
                                                    }) {
                                                        Ok(Err(error)) => window.push_notification(error, cx),
                                                        _ => window.refresh(),
                                                    }
                                                });
                                            }
                                        })
                                        .detach();
                                },
                            ))
                        })
                        // Nothing to move until a folder is chosen.
                        .when(!unchosen, |this| {
                            this.child(Button::new("root-change").outline().label(t("settings.change")).on_click(
                                move |_, window, cx| {
                                    rename::open(workspace.clone(), Target::Root, from.clone(), window, cx);
                                },
                            ))
                        })
                        .into_any_element()
                })),
        )
    }

    // ───────────────────────── sync ─────────────────────────

    /// Find the server by address and ask it for a connection code.
    fn sync_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let address = self.sync.address.read(cx).value().trim().to_string();
        if address.is_empty() || self.sync.busy || !matches!(self.sync.phase, Phase::Off) {
            return;
        }
        self.sync.busy = true;
        self.sync.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let found = cx
                .background_spawn({
                    let address = address.clone();
                    async move {
                        let (found, api) = sync::discover(&address)?;
                        let pairing = sync::pair_start(&api, &sync::computer_name())?;
                        Ok::<_, String>((found.name, api, pairing))
                    }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.sync.busy = false;
                match found {
                    Ok((server, api, pairing)) => {
                        // The code field has as many digits as the server said; a complete code is sent automatically.
                        let code = cx.new(|cx| OtpState::new(pairing.digits, window, cx));
                        let watch = cx.subscribe_in(&code, window, |this, _, event: &OtpEvent, window, cx| match event {
                            OtpEvent::Complete => this.sync_confirm(window, cx),
                            OtpEvent::Change => {
                                this.sync.error = None;
                                cx.notify();
                            }
                            _ => {}
                        });
                        code.read(cx).focus_handle(cx).focus(window, cx);
                        this.sync.phase =
                            Phase::Code { address, api, pairing: pairing.id, server, via: pairing.via, code: code.clone(), _watch: watch };
                        // For checking the layout: `PULT_SYNC_CODE` — the code is already entered and sent.
                        if let Ok(preset) = std::env::var("PULT_SYNC_CODE") {
                            code.update(cx, |code, cx| code.set_value(preset, window, cx));
                            this.sync_confirm(window, cx);
                        }
                    }
                    Err(error) => this.sync.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Exchange the code for this computer's key, install the Claude Code hooks and start sending.
    fn sync_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Phase::Code { address, api, pairing, code, .. } = &self.sync.phase else {
            return;
        };
        let entered = code.read(cx).value().trim().to_string();
        if entered.chars().count() < code.read(cx).len() || self.sync.busy {
            return;
        }
        let (address, api, pairing, code) = (address.clone(), api.clone(), pairing.clone(), code.clone());
        self.sync.busy = true;
        self.sync.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let paths = sync::Paths::real();
                    sync::pair_finish(&paths, &address, &api, &sync::computer_name(), &pairing, &entered)?;
                    sync::install_hooks(&paths, &sync::hook_exe())?;
                    Ok::<_, String>(())
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.sync.busy = false;
                match result {
                    Ok(()) => {
                        this.sync.phase = Phase::On;
                        this.sync_push(cx);
                    }
                    Err(error) => {
                        // Wrong code: the field is cleared and focused again, the reason is shown under it.
                        code.update(cx, |code, cx| code.set_value("", window, cx));
                        code.read(cx).focus_handle(cx).focus(window, cx);
                        this.sync.error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Go back to the server address.
    fn sync_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync.phase = Phase::Off;
        self.sync.error = None;
        self.sync.address.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Send everything new now, without waiting for a hook or the periodic pass.
    fn sync_push(&mut self, cx: &mut Context<Self>) {
        if self.sync.pushing {
            return;
        }
        self.sync.pushing = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let _ = cx.background_spawn(async { sync::push(&sync::Paths::real(), &sync::Scope::All) }).await;
            let _ = this.update(cx, |this, cx| {
                this.sync.pushing = false;
                this.sync_refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn sync_disconnect(&mut self, cx: &mut Context<Self>) {
        self.sync.busy = true;
        self.sync.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async { sync::disconnect(&sync::Paths::real()) }).await;
            let _ = this.update(cx, |this, cx| {
                this.sync.busy = false;
                match result {
                    Ok(()) => {
                        this.sync.server = None;
                        this.sync.phase = Phase::Off;
                    }
                    // Disconnect failed — the computer stays connected, the reason is shown under the server row.
                    Err(error) => this.sync.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Ask the server how much it stores.
    fn sync_refresh(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.sync.phase, Phase::On) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let server = cx.background_spawn(async { sync::server_status(&sync::Paths::real()) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(server) = server {
                    let number = |key: &str| server.get(key).and_then(|value| value.as_u64()).unwrap_or_default();
                    this.sync.server = Some((number("raw_bytes"), number("free_bytes")));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The "Sync" page. Steps: server address → connection code → status.
    /// Row titles and descriptions are the same size as regular settings rows (`SettingItem::new`),
    /// fields and buttons are regular size; a step's error sits under its field.
    fn sync_page(&self, cx: &mut Context<Self>) -> SettingPage {
        let view = cx.entity().downgrade();
        let busy = self.sync.busy;
        let error = self.sync.error.clone().map(SharedString::from);
        let group = SettingGroup::new().title(t("settings.server")).description(t("settings.pult_copies_claude_code_transcripts"));
        let group = match &self.sync.phase {
            Phase::Off => {
                let address = self.sync.address.clone();
                let empty = address.read(cx).value().trim().is_empty();
                group.item(SettingItem::render(move |_, _, cx| {
                    let view = view.clone();
                    v_flex().w_full().gap_3().child(Label::new(t("settings.server_address")).text_sm()).child(
                        v_flex()
                            .gap_2()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().flex_1().min_w_0().child(Input::new(&address).disabled(busy)))
                                    .child(
                                        Button::new("sync-connect")
                                            .primary()
                                            .label(t("settings.get_code"))
                                            .loading(busy)
                                            .disabled(empty || busy)
                                            .on_click(move |_, window, cx| {
                                                let _ = view.update(cx, |this, cx| this.sync_connect(window, cx));
                                            }),
                                    ),
                            )
                            .when_some(error.clone(), |this, error| this.child(problem(error, cx))),
                    )
                }))
            }
            Phase::Code { address, server, via, code, .. } => {
                let code = code.clone();
                let short = code.read(cx).value().chars().count() < code.read(cx).len();
                let (server, address) = (SharedString::from(server.clone()), SharedString::from(bare_address(address)));
                let hint: SharedString = if via == "telegram" {
                    t("settings.server_sent_you_code_in").into()
                } else {
                    t("settings.code_is_in_server_log").into()
                };
                group.item(server_row(server, address, None, None)).item(SettingItem::render(move |_, _, cx| {
                    let (confirm, cancel) = (view.clone(), view.clone());
                    v_flex()
                        .w_full()
                        .gap_3()
                        .child(
                            v_flex().child(Label::new(t("settings.connection_code")).text_sm()).child(
                                div().text_sm().text_color(cx.theme().muted_foreground).child(hint.clone()),
                            ),
                        )
                        .child(
                            v_flex()
                                .gap_2()
                                .child(
                                    // In a narrow window the buttons wrap under the code field and stay at the right edge.
                                    h_flex()
                                        .flex_wrap()
                                        .justify_between()
                                        .gap_3()
                                        .child(OtpInput::new(&code).groups(2).disabled(busy))
                                        .child(
                                            h_flex()
                                                .ml_auto()
                                                .gap_2()
                                                .child(Button::new("sync-cancel").outline().label(t("common.cancel")).disabled(busy).on_click(
                                                    move |_, window, cx| {
                                                        let _ = cancel.update(cx, |this, cx| this.sync_cancel(window, cx));
                                                    },
                                                ))
                                                .child(
                                                    Button::new("sync-confirm")
                                                        .primary()
                                                        .label(t("settings.connect"))
                                                        .loading(busy)
                                                        .disabled(short || busy)
                                                        .on_click(move |_, window, cx| {
                                                            let _ = confirm.update(cx, |this, cx| this.sync_confirm(window, cx));
                                                        }),
                                                ),
                                        ),
                                )
                                .when_some(error.clone(), |this, error| this.child(problem(error, cx))),
                        )
                }))
            }
            Phase::On => {
                let config = sync::config(&sync::Paths::real());
                let (server, address, computer) = config
                    .map(|config| (config.server_name, config.address, config.device_name))
                    .unwrap_or_default();
                let state = sync_state(&sync::status(&sync::Paths::real()), self.sync.pushing);
                let working = matches!(state, SyncState::Sending(_));
                let failed = matches!(state, SyncState::Failed(_));
                let state: SharedString = match state {
                    SyncState::Sending(text) | SyncState::Failed(text) | SyncState::Idle(text) => text.into(),
                };
                let stored: SharedString = match self.sync.server {
                    Some((raw, free)) => tr!("settings.raw_free_free", raw = sync::size_label(raw), free = sync::size_label(free)).into(),
                    None => "—".into(),
                };
                let computer = SharedString::from(computer);
                let (ask, push) = (view.clone(), view.clone());
                let title = SharedString::from(tr!("settings.disconnect_server", server = server));
                let off: RowRun = std::rc::Rc::new(move |window, cx| {
                    let (view, title) = (ask.clone(), title.clone());
                    window.open_alert_dialog(cx, move |alert, _, _| {
                        let view = view.clone();
                        alert
                            .title(title.clone())
                            .description(t("settings.this_computer_will_stop_sending"))
                            .show_cancel(true)
                            .cancel_text(t("common.cancel"))
                            .ok_text(t("settings.disconnect"))
                            .on_ok(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| this.sync_disconnect(cx));
                                true
                            })
                    });
                });
                group
                    .item(server_row(server.into(), bare_address(&address).into(), Some((t("settings.disconnect_2"), busy, off)), error))
                    .item(SettingItem::render(move |_, _, cx| {
                        let push = push.clone();
                        // Yellow text on a light background is unreadable: on failure the text keeps its normal color, the icon warns.
                        let color = if failed { cx.theme().foreground } else { cx.theme().muted_foreground };
                        h_flex()
                            .w_full()
                            .justify_between()
                            .gap_3()
                            .child(
                                v_flex().flex_1().min_w_0().child(Label::new(t("settings.status")).text_sm()).child(
                                    h_flex()
                                        .items_start()
                                        .gap_1p5()
                                        .text_sm()
                                        .text_color(color)
                                        .when(working, |row| row.child(div().mt_0p5().child(Spinner::new().small())))
                                        .when(failed, |row| {
                                            row.child(Icon::new(AssetIcon::TriangleAlert).small().mt_0p5().text_color(cx.theme().warning))
                                        })
                                        .child(div().flex_1().min_w_0().child(state.clone())),
                                ),
                            )
                            .child(Button::new("sync-now").outline().label(t("settings.send_now")).disabled(working).on_click(
                                move |_, _, cx| {
                                    let _ = push.update(cx, |this, cx| this.sync_push(cx));
                                },
                            ))
                    }))
                    .item(
                        SettingItem::new(
                            t("code.setting"),
                            SettingField::switch(|_| crate::code::enabled(), |on, _| crate::code::set_enabled(on)),
                        )
                        .description(t("code.setting_description")),
                    )
                    .item(value_row(t("settings.this_computer"), computer))
                    .item(value_row(t("settings.on_server"), stored))
                    .footer(|_, _| {
                        t("settings.new_data_goes_to_server")
                    })
            }
        };
        SettingPage::new(t("settings.sync")).icon(Icon::new(AssetIcon::RefreshCw)).group(group)
    }
}

/// Change the saved GLM settings. The GLM session file follows (sessions of GLM projects read it when they start;
/// running ones pick it up at their next restart).
fn edit_config(workspace: &WeakEntity<Workspace>, cx: &mut App, change: impl FnOnce(&mut models::Config)) {
    let paths = crate::sync::Paths::real();
    let mut config = models::load(&paths);
    change(&mut config);
    if models::save(&paths, &config).is_ok() {
        models::refresh_glm_session(&paths);
    }
    let _ = workspace.update(cx, |workspace, cx| {
        workspace.refresh_models();
        cx.notify();
    });
}

fn model_options() -> Vec<(SharedString, SharedString)> {
    models::MODELS.iter().map(|name| ((*name).into(), (*name).into())).collect()
}

impl SettingsView {
    /// "Models": who Claude Code talks to, and what GLM is given.
    fn models_page(&self, cx: &mut Context<Self>) -> SettingPage {
        let workspace = self.workspace.clone();
        let view = cx.entity().downgrade();
        let paths = crate::sync::Paths::real();
        let config = models::load(&paths);
        let provider = workspace.upgrade().map(|workspace| workspace.read(cx).provider()).unwrap_or(Provider::Claude);
        let (read, write) = (workspace.clone(), workspace.clone());

        let provider_item = SettingItem::new(
            t("models.provider_title"),
            SettingField::dropdown(
                vec![("claude".into(), t("models.claude").into()), ("glm".into(), t("models.glm_option").into())],
                move |cx| match read.upgrade().map(|workspace| workspace.read(cx).provider()) {
                    Some(Provider::Glm) => "glm".into(),
                    _ => "claude".into(),
                },
                move |value, cx| {
                    let to = if value.as_ref() == "glm" { Provider::Glm } else { Provider::Claude };
                    let _ = write.update(cx, |workspace, cx| workspace.request_provider(to, cx));
                },
            ),
        )
        .description(if provider == Provider::Glm { t("models.status_glm") } else { t("models.status_claude") });

        let (plan_read, plan_write, plan_view) = (config.plan, workspace.clone(), view.clone());
        let plan_item = SettingItem::new(
            t("models.plan_title"),
            SettingField::dropdown(
                vec![("global".into(), t("models.plan_global").into()), ("china".into(), t("models.plan_china").into())],
                move |_| if plan_read == models::Plan::China { "china".into() } else { "global".into() },
                move |value, cx| {
                    let plan = if value.as_ref() == "china" { models::Plan::China } else { models::Plan::Global };
                    edit_config(&plan_write, cx, |config| config.plan = plan);
                    // The key belongs to one of the two services: check it against the new one.
                    let _ = plan_view.update(cx, |view, cx| view.models_key_commit(true, cx));
                },
            ),
        )
        .description(t("models.plan_hint"));

        let key = self.models.key.clone();
        let check = self.models.check;
        let has_key = !key.read(cx).value().trim().is_empty();
        let key_item = SettingItem::render(move |_, _, cx| {
            let muted = cx.theme().muted_foreground;
            let line: SharedString = match check {
                Check::Checking => t("models.key_checking").into(),
                Check::Ok => t("models.key_ok").into(),
                Check::Rejected => t("models.key_rejected").into(),
                Check::Offline => t("models.key_offline").into(),
                Check::Idle if has_key => t("models.key_saved").into(),
                Check::Idle => t("models.key_empty").into(),
            };
            h_flex()
                .w_full()
                .justify_between()
                .gap_3()
                .child(
                    v_flex().flex_1().min_w_0().child(Label::new(t("models.key_title")).text_sm()).child(
                        h_flex()
                            .items_start()
                            .gap_1p5()
                            .text_sm()
                            .text_color(muted)
                            .when(check == Check::Checking, |row| row.child(div().mt_0p5().child(Spinner::new().small())))
                            .when(check == Check::Ok, |row| {
                                row.child(Icon::new(AssetIcon::Check).small().mt_0p5().text_color(cx.theme().success))
                            })
                            .when(matches!(check, Check::Rejected | Check::Offline), |row| {
                                row.child(Icon::new(AssetIcon::TriangleAlert).small().mt_0p5().text_color(cx.theme().warning))
                            })
                            .child(div().flex_1().min_w_0().child(line)),
                    ),
                )
                .child(div().w(px(200.)).flex_shrink_0().child(Input::new(&key)))
        });

        let (main_write, small_write) = (workspace.clone(), workspace.clone());
        let (main_value, small_value) = (config.main.clone(), config.small.clone());
        let main_item = SettingItem::new(
            t("models.main_title"),
            SettingField::dropdown(
                model_options(),
                move |_| main_value.clone().into(),
                move |value, cx| edit_config(&main_write, cx, |config| config.main = value.to_string()),
            ),
        )
        .description(t("models.main_hint"));
        let small_item = SettingItem::new(
            t("models.small_title"),
            SettingField::dropdown(
                model_options(),
                move |_| small_value.clone().into(),
                move |value, cx| edit_config(&small_write, cx, |config| config.small = value.to_string()),
            ),
        )
        .description(t("models.small_hint"));

        // What can undo the choice: only worth saying when GLM is on or about to be.
        let relevant = provider == Provider::Glm || config.has_key();
        let shell_warning = relevant && self.models.shell_env;
        let pinned = models::pinned_claude_model(&paths).filter(|_| relevant);

        let mut details = SettingGroup::new().item(plan_item).item(key_item).item(main_item).item(small_item);
        if shell_warning {
            details = details.item(SettingItem::render(|_, _, _| Alert::warning("models-shell", t("models.warn_shell")).into_any_element()));
        }
        if let Some(model) = pinned {
            let text: SharedString = tr!("models.warn_model", model = model).into();
            details = details.item(SettingItem::render(move |_, _, _| Alert::warning("models-pinned", text.clone()).into_any_element()));
        }
        SettingPage::new(t("models.page")).icon(Icon::new(AssetIcon::Bot)).group(SettingGroup::new().item(provider_item)).group(details)
    }

    /// The key field was committed (Enter, leaving the field) or the plan changed: save the key and check it.
    /// `force` — check even if the key is the same and was already found valid.
    fn models_key_commit(&mut self, force: bool, cx: &mut Context<Self>) {
        let value = self.models.key.read(cx).value().trim().to_string();
        let paths = crate::sync::Paths::real();
        let mut config = models::load(&paths);
        if value != config.key {
            let key = value.clone();
            edit_config(&self.workspace, cx, move |config| config.key = key);
            config = models::load(&paths);
        } else if !force && matches!(self.models.check, Check::Ok | Check::Checking) {
            return;
        }
        if value.is_empty() {
            self.models.check = Check::Idle;
            cx.notify();
            return;
        }
        self.models.check = Check::Checking;
        cx.notify();
        let plan = config.plan;
        cx.spawn(async move |this, cx| {
            let key = value.clone();
            let result = cx.background_spawn(async move { models::check_key(plan, &key) }).await;
            let _ = this.update(cx, |this, cx| {
                // The key changed while it was being checked: the answer is about another one.
                if this.models.key.read(cx).value().trim() != value {
                    return;
                }
                this.models.check = match result {
                    models::KeyCheck::Valid => Check::Ok,
                    models::KeyCheck::Rejected => Check::Rejected,
                    models::KeyCheck::Offline => Check::Offline,
                };
                cx.notify();
            });
        })
        .detach();
    }
}

/// Server address without the scheme — as people type it.
fn bare_address(address: &str) -> String {
    address.trim_start_matches("https://").trim_start_matches("http://").to_string()
}

/// What failed and what to do about it — a line under the field it belongs to.
/// An icon next to the color: the error reads even without color.
fn problem(text: SharedString, cx: &App) -> impl IntoElement {
    let color = cx.theme().danger;
    h_flex()
        .items_start()
        .gap_1p5()
        .text_sm()
        .text_color(color)
        .child(Icon::new(AssetIcon::TriangleAlert).small().mt_0p5().text_color(color))
        .child(div().flex_1().min_w_0().child(text))
}

/// Action on the right of the server row: button label, whether it's disabled, what it does.
type RowRun = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;
type RowAction = (&'static str, bool, RowRun);

/// Server row: name, address and, if any, an action on the right; below it — what failed with this server.
fn server_row(name: SharedString, address: SharedString, action: Option<RowAction>, error: Option<SharedString>) -> SettingItem {
    SettingItem::render(move |_, _, cx| {
        v_flex()
            .w_full()
            .gap_2()
            .child(
                h_flex()
                    .gap_3()
                    .child(Icon::new(AssetIcon::Server).small().text_color(cx.theme().muted_foreground))
                    .child(
                        v_flex().flex_1().min_w_0().child(Label::new(name.clone()).text_sm()).child(
                            div()
                                .truncate()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(address.clone()),
                        ),
                    )
                    .when_some(action.clone(), |row, (label, disabled, run)| {
                        row.child(
                            Button::new("sync-server-action")
                                .outline()
                                .label(label)
                                .disabled(disabled)
                                .on_click(move |_, window, cx| run(window, cx)),
                        )
                    }),
            )
            .when_some(error.clone(), |this, error| this.child(problem(error, cx)))
    })
}

/// A "title — value" row. Text is the same size as a regular settings row, but the value is
/// always on the right: a regular row in a narrow window puts the field under the title, which short words don't need.
fn value_row(title: &'static str, value: SharedString) -> SettingItem {
    SettingItem::render(move |_, _, cx| {
        h_flex()
            .w_full()
            .justify_between()
            .gap_3()
            .child(Label::new(title).text_sm())
            .child(div().min_w_0().truncate().text_sm().text_color(cx.theme().muted_foreground).child(value.clone()))
    })
}

/// Which page to open the window on. For checking the layout: `PULT_SETTINGS_PAGE=2` opens Settings
/// on the third page right at launch.
fn start_page(requested: Option<usize>) -> SelectIndex {
    let page_ix = requested.or_else(|| std::env::var("PULT_SETTINGS_PAGE").ok().and_then(|page| page.parse().ok())).unwrap_or(0);
    SelectIndex { page_ix, group_ix: None }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pages = [self.general(cx), self.models_page(cx), self.folders(), self.sync_page(cx)];
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(TitleBar::new().child(div().flex_1().text_center().text_sm().font_medium().child(t("common.settings_2"))))
            .child(div().flex_1().min_h_0().child(
                Settings::new("settings")
                    .sidebar_width(px(200.))
                    // Search in Settings isn't needed, and Settings can't turn it off —
                    // so the sidebar header is hidden.
                    .header_style(&hidden())
                    .default_selected_index(self.start_page)
                    .pages(pages),
            ))
    }
}

/// Style that hides an element entirely.
fn hidden() -> gpui_kit::StyleRefinement {
    gpui_kit::StyleRefinement { display: Some(gpui_kit::Display::None), ..Default::default() }
}

/// Open the Settings window; returns it so a second ⌘, doesn't open another one.
pub fn open(workspace: WeakEntity<Workspace>, page: Option<usize>, cx: &mut App) -> Option<AnyWindowHandle> {
    // For checking the layout: `PULT_SETTINGS_SIZE=560x560` — a window of that size (the smallest is 560×560).
    let (width, height) = std::env::var("PULT_SETTINGS_SIZE")
        .ok()
        .and_then(|value| value.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
        .unwrap_or((760., 640.));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(width), px(height)), cx))),
        window_min_size: Some(size(px(560.), px(560.))),
        ..TitleBar::window_options()
    };
    gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| {
            let mut subscriptions = Vec::new();
            if let Some(entity) = workspace.upgrade() {
                subscriptions.push(cx.observe(&entity, |_, _, cx| cx.notify()));
            }
            if let Some(updater) = update::updater(cx) {
                subscriptions.push(cx.observe(&updater, |_, _, cx| cx.notify()));
            }
            // For checking the layout: `PULT_SYNC_ADDRESS` — the address is already entered and connecting has started.
            let preset = std::env::var("PULT_SYNC_ADDRESS").unwrap_or_default();
            let address = cx.new(|cx| {
                InputState::new(window, cx).placeholder("sync.example.com").default_value(preset.clone())
            });
            subscriptions.push(cx.subscribe_in(&address, window, |this: &mut SettingsView, _, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::PressEnter { .. } => this.sync_connect(window, cx),
                    InputEvent::Change => {
                        this.sync.error = None;
                        cx.notify();
                    }
                    _ => {}
                }
            }));
            let phase = if sync::config(&sync::Paths::real()).is_some() { Phase::On } else { Phase::Off };
            let saved_key = models::load(&crate::sync::Paths::real()).key;
            let key = cx.new(|cx| {
                let mut state = InputState::new(window, cx).placeholder(t("models.key_placeholder")).default_value(saved_key.clone());
                state.set_masked(true, window, cx);
                state
            });
            subscriptions.push(cx.subscribe_in(&key, window, |this: &mut SettingsView, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.models_key_commit(false, cx);
                }
            }));
            let mut view = SettingsView {
                workspace,
                sync: SyncUi { address, phase, busy: false, pushing: false, error: None, server: None, ticks: 0, sent_at: 0 },
                models: ModelsUi { key, check: Check::Idle, shell_env: false },
                start_page: start_page(page),
                _subscriptions: subscriptions,
            };
            // Does the login shell set its own `ANTHROPIC_*`? It starts a shell the first time — not on the window's thread.
            cx.spawn(async move |this, cx| {
                let found = cx.background_spawn(async { sessions::login_has_any(&models::SHELL_NAMES) }).await;
                let _ = this.update(cx, |this, cx| {
                    this.models.shell_env = found;
                    cx.notify();
                });
            })
            .detach();
            view.sync_refresh(cx);
            if !preset.is_empty() && matches!(view.sync.phase, Phase::Off) {
                view.sync_connect(window, cx);
            }
            // Sending state changes outside Pult (hooks, periodic pass): the window rereads it on its own.
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    let alive = this.update(cx, |this, cx| {
                        this.sync.ticks += 1;
                        // Something reached the server — refresh "On the server" right away, otherwise every half minute.
                        let sent_at = sync::status(&sync::Paths::real()).ok_at;
                        if sent_at != this.sync.sent_at || this.sync.ticks.is_multiple_of(15) {
                            this.sync.sent_at = sent_at;
                            this.sync_refresh(cx);
                        }
                        if matches!(this.sync.phase, Phase::On) {
                            cx.notify();
                        }
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            })
            .detach();
            view
        })
    })
    .ok()
    .map(|(handle, _)| handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(ok_at: u64, error: Option<&str>, error_at: u64, pending: u64) -> sync::Status {
        sync::Status { ok_at, error: error.map(str::to_string), error_at, pending, ..Default::default() }
    }

    #[test]
    fn sending_wins_over_everything() {
        assert!(matches!(sync_state(&status(10, Some("No connection."), 20, 1240), false), SyncState::Sending(text) if text == tr!("settings.pending_left_to_send", pending = files_label(1240)) && text.contains("1\u{202f}240")));
        assert!(matches!(sync_state(&status(10, None, 0, 0), true), SyncState::Sending(_)));
    }

    #[test]
    fn a_newer_error_is_shown_until_the_next_success() {
        assert!(matches!(sync_state(&status(10, Some("No connection."), 20, 0), false), SyncState::Failed(text) if text == "No connection."));
        assert!(matches!(sync_state(&status(30, Some("No connection."), 20, 0), false), SyncState::Idle(_)));
    }

    #[test]
    fn nothing_sent_yet() {
        assert!(matches!(sync_state(&status(0, None, 0, 0), false), SyncState::Idle(text) if text == t("settings.nothing_sent_yet")));
    }

    #[test]
    fn numbers_are_grouped() {
        assert_eq!(grouped(7), "7");
        assert_eq!(grouped(1240), "1\u{202f}240");
        assert_eq!(grouped(1234567), "1\u{202f}234\u{202f}567");
    }
}
