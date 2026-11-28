//! Moving every project between Claude and GLM (Settings → Models): the question when a turn is running and the
//! restart of the running sessions with their new `--settings`, with its progress. One project at a time is
//! `Workspace::switch_project_provider`; the settings themselves are `models`.
//!
//! Running sessions always restart: a Claude Code process reads its settings when it starts. `claude stop` + the
//! new flag + `respawn` keep the session id, name and conversation; a turn that was running is cut off, which is
//! why the window asks first.

use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogFooter,
    h_flex,
    progress::Progress,
    spinner::Spinner,
    v_flex,
};
use gpui_kit::assets::IconName as AssetIcon;
use gpui_kit::{
    App, AppContext as _, Context, FontWeight, IntoElement, ParentElement as _, Render, Styled as _, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px,
};

use crate::i18n::t;
use crate::models::{self, Provider};
use crate::sessions;
use crate::sync::Paths;
use crate::workspace::Workspace;

/// A running background session of a project: what the restart needs to know about it.
#[derive(Clone, Debug)]
pub struct Target {
    /// The project's id.
    pub id: String,
    pub name: String,
    /// Claude Code's short id of the background session.
    pub short: String,
    pub pid: Option<u32>,
    /// A turn is going on (or waits for a decision): a restart cuts it off.
    pub working: bool,
}

pub fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => t("models.claude"),
        Provider::Glm => t("models.glm"),
    }
}

enum Phase {
    /// Turns are running: asking whether to cut them off.
    Ask,
    Run { done: usize },
    /// Some sessions didn't come back: their names.
    Failed(Vec<String>),
}

pub struct SwitchView {
    workspace: WeakEntity<Workspace>,
    to: Provider,
    /// The `--settings` value the sessions come back with.
    flag: String,
    targets: Vec<Target>,
    phase: Phase,
    /// `go` has been called: the work is under way (or done).
    started: bool,
}

impl SwitchView {
    fn running(&self) -> bool {
        matches!(self.phase, Phase::Run { .. })
    }

    fn go(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Not "is the phase Run": with no turns running the dialog opens in Run at once, and `go` still has to start
        // the work (otherwise it would hang at "0 of N").
        if std::mem::replace(&mut self.started, true) {
            return;
        }
        self.phase = Phase::Run { done: 0 };
        let (targets, workspace, flag) = (self.targets.clone(), self.workspace.clone(), self.flag.clone());
        let ids: Vec<String> = targets.iter().map(|target| target.id.clone()).collect();
        let _ = workspace.update(cx, |this, cx| this.models_restarting(&ids, cx));
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            // All at once: the sessions are independent, and the switch should take effect now.
            let tasks: Vec<_> = targets
                .iter()
                .cloned()
                .map(|target| {
                    let flag = flag.clone();
                    cx.background_spawn(async move { (target.name.clone(), sessions::switch_settings(&target.short, target.pid, &flag)) })
                })
                .collect();
            let mut failed = Vec::new();
            for task in tasks {
                let (name, result) = task.await;
                if result.is_err() {
                    failed.push(name);
                }
                let _ = this.update(cx, |this, cx| {
                    if let Phase::Run { done } = &mut this.phase {
                        *done += 1;
                    }
                    cx.notify();
                });
            }
            let _ = this.update_in(cx, |this, window, cx| this.finish(failed, window, cx));
        })
        .detach();
    }

    fn finish(&mut self, failed: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.targets.iter().map(|target| target.id.clone()).collect();
        let _ = self.workspace.update(cx, |ws, cx| ws.models_switched(&ids, cx));
        if failed.is_empty() {
            window.close_dialog(cx);
            window.push_notification(
                tr!("models.done_restarted", to = provider_name(self.to), n = self.targets.len()),
                cx,
            );
        } else {
            self.phase = Phase::Failed(failed);
            cx.notify();
        }
    }
}

impl Render for SwitchView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let to = provider_name(self.to);
        match &self.phase {
            Phase::Ask => {
                let turns = self.targets.iter().filter(|target| target.working).count();
                v_flex()
                    .gap_2()
                    .text_sm()
                    .text_color(muted)
                    .child(tr!("models.ask_turns", n = turns, to = to))
                    .child(tr!("models.ask_restart", n = self.targets.len()))
                    .into_any_element()
            }
            Phase::Run { done } => {
                let total = self.targets.len();
                let percent = if total == 0 { 100. } else { *done as f32 * 100. / total as f32 };
                v_flex()
                    .gap_2()
                    .child(Progress::new("models-progress").value(percent))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_sm()
                            .text_color(muted)
                            .child(Spinner::new().small())
                            .child(tr!("models.run_line", done = done, total = total)),
                    )
                    .into_any_element()
            }
            Phase::Failed(names) => {
                let danger = cx.theme().danger;
                h_flex()
                    .items_start()
                    .gap_1p5()
                    .text_sm()
                    .child(Icon::new(AssetIcon::TriangleAlert).small().mt_0p5().text_color(cx.theme().warning))
                    .child(div().flex_1().min_w_0().child(tr!("models.failed_body", names = names.join(", "), to = to)))
                    .text_color(if names.is_empty() { danger } else { cx.theme().foreground })
                    .into_any_element()
            }
        }
    }
}

/// The default becomes `to` (new projects get it) and the `--settings` value of `to` is returned: for GLM its
/// session file is written first. Claude Code's own `settings.json` is not touched.
pub fn prepare(paths: &Paths, to: Provider) -> Result<String, String> {
    let flag = models::session_flag(paths, to)?;
    let mut config = models::load(paths);
    config.default_glm = to == Provider::Glm;
    models::save(paths, &config)?;
    Ok(flag)
}

/// Restart the running sessions of all projects on `to` (`flag` — from `prepare`). The caller has already saved the
/// choice and checked that there are sessions to restart.
pub fn start(workspace: WeakEntity<Workspace>, to: Provider, flag: String, targets: Vec<Target>, window: &mut Window, cx: &mut App) {
    let turns = targets.iter().any(|target| target.working);
    let view = cx.new(|_| SwitchView { workspace, to, flag, targets, phase: if turns { Phase::Ask } else { Phase::Run { done: 0 } }, started: false });
    show(view.clone(), window, cx);
    if !turns {
        // Not right now: the caller is the Workspace in the middle of its own update, and `go` updates it.
        window.defer(cx, move |window, cx| view.update(cx, |view, cx| view.go(window, cx)));
    }
}

/// For checking the look without sessions: `PULT_DIALOG=switch-ask|switch-run|switch-failed`.
pub fn demo(workspace: WeakEntity<Workspace>, kind: &str, window: &mut Window, cx: &mut App) {
    let target = |name: &str, working: bool| Target { id: name.into(), name: name.into(), short: String::new(), pid: None, working };
    let targets = vec![target("WILLOW", true), target("EMBER", true), target("MAPLE", false), target("LOTUS", false)];
    let phase = match kind {
        "switch-run" => Phase::Run { done: 1 },
        // As when no turn is running: the dialog opens already in Run and `go` still has to start the work.
        "switch-go" => Phase::Run { done: 0 },
        "switch-failed" => Phase::Failed(vec!["EMBER".into(), "MAPLE".into()]),
        _ => Phase::Ask,
    };
    let view = cx.new(|_| SwitchView { workspace, to: Provider::Glm, flag: String::new(), targets, phase, started: false });
    show(view.clone(), window, cx);
    // `switch-go`: the real run on made-up sessions, as if "Switch and restart" was pressed.
    if kind == "switch-go" {
        window.defer(cx, move |window, cx| view.update(cx, |view, cx| view.go(window, cx)));
    }
}

fn show(view: gpui_kit::Entity<SwitchView>, window: &mut Window, cx: &mut App) {
    let shown = view;
    window.open_dialog(cx, move |dialog, _, cx| {
        let state = shown.read(cx);
        let running = state.running();
        let title = match &state.phase {
            Phase::Ask if state.to == Provider::Glm => t("models.ask_title_glm"),
            Phase::Ask => t("models.ask_title_claude"),
            Phase::Run { .. } => t("models.run_title"),
            Phase::Failed(_) => t("models.failed_title"),
        };
        let footer = match &state.phase {
            Phase::Ask => {
                let go = shown.clone();
                let label = if state.to == Provider::Glm { t("models.go_glm") } else { t("models.go_claude") };
                Some(
                    DialogFooter::new()
                        .child(Button::new("models-cancel").outline().label(t("common.cancel")).on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("models-go").primary().label(label).on_click(move |_, window, cx| {
                            go.update(cx, |view, cx| view.go(window, cx));
                        })),
                )
            }
            Phase::Run { .. } => None,
            Phase::Failed(_) => Some(
                DialogFooter::new()
                    .child(Button::new("models-ok").primary().label(t("models.failed_ok")).on_click(|_, window, cx| window.close_dialog(cx))),
            ),
        };
        dialog
            .w(px(440.))
            .close_button(false)
            .keyboard(!running)
            .overlay_closable(!running)
            .title(div().text_base().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(shown.clone())
            .when_some(footer, |dialog, footer| dialog.footer(footer))
    });
}
