//! The "Main folder" and "Rename project" window: the new name (and, for the main folder, the location),
//! what the move will affect, then the move's progress step by step. The move itself is `migrate::Move`.

use std::path::PathBuf;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, WindowExt as _,
    alert::Alert,
    button::{Button, ButtonVariants as _},
    dialog::DialogClose,
    h_flex,
    input::{Escape as InputEscape, Input, InputEvent, InputState},
    scroll::ScrollableElement as _,
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, Styled as _, Subscription, WeakEntity, Window, div, prelude::FluentBuilder as _, px, rems,
};
use gpui_kit::assets::IconName as AssetIcon;

use crate::i18n::t;
use crate::migrate;
use crate::projects::display_path;
use crate::workspace::{Affected, Workspace};

/// What is being moved.
#[derive(Clone, PartialEq, Eq)]
pub enum Target {
    /// The main folder with all projects.
    Root,
    /// One project's folder (id = folder name).
    Project(String),
}

pub struct RenameView {
    workspace: WeakEntity<Workspace>,
    target: Target,
    /// The folder now.
    from: PathBuf,
    /// Where to put it (for a project — always the main folder).
    parent: PathBuf,
    name: Entity<InputState>,
    /// The move was started from this window: show its progress and error; the window closes when it is done.
    started: bool,
    _subscriptions: Vec<Subscription>,
}

impl RenameView {
    fn current_name(&self) -> String {
        self.from.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
    }

    fn to(&self, cx: &App) -> PathBuf {
        self.parent.join(self.name.read(cx).value().trim())
    }

    /// What is wrong with the new name; `None` — the move can go ahead.
    fn issue(&self, cx: &App) -> Option<String> {
        let name = self.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return Some(t("rename.enter_name").into());
        }
        if name.starts_with('.') || name.contains('/') || name.contains(':') {
            return Some(t("rename.name_can_t_start_with").into());
        }
        let to = self.to(cx);
        if to == self.from {
            return Some(t("rename.this_is_current_name").into());
        }
        if to.starts_with(&self.from) {
            return Some(t("rename.folder_can_t_be_moved").into());
        }
        if migrate::taken(&self.from, &to) {
            return Some(tr!("common.folder_to_already_exists", to = display_path(&to)));
        }
        None
    }

    fn pick_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                let _ = this.update(cx, |this, cx| {
                    this.parent = path;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running(cx) || self.issue(cx).is_some() {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        if !workspace.read(cx).blockers(&self.from, cx).is_empty() {
            return;
        }
        let (target, from, to) = (self.target.clone(), self.from.clone(), self.to(cx));
        self.started = true;
        workspace.update(cx, |workspace, cx| workspace.start_move(target, from, to, true, window, cx));
        cx.notify();
    }

    fn running(&self, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).move_state().map(|state| state.step.is_some()))
            .unwrap_or(false)
    }
}

impl Render for RenameView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger, warning, success, mono) =
            (theme.muted_foreground, theme.danger, theme.warning, theme.success, theme.mono_font_family.clone());
        let project = matches!(self.target, Target::Project(_));
        let state = self.workspace.upgrade().and_then(|workspace| {
            let workspace = workspace.read(cx);
            workspace.move_state().map(|state| (state.labels.clone(), state.step, state.error.clone()))
        });
        let (labels, step, error) = match state.filter(|_| self.started) {
            Some((labels, step, error)) => (labels, step, error),
            None => (Vec::new(), None, None),
        };
        let running = step.is_some();
        let to = self.to(cx);
        let moving = self.parent.as_path() != self.from.parent().unwrap_or(self.parent.as_path());
        let title: SharedString = match (&self.target, running) {
            (Target::Root, true) => t("rename.moving_main_folder").into(),
            (Target::Project(_), true) => t("rename.moving_project").into(),
            (Target::Root, false) => t("common.main_folder").into(),
            (Target::Project(_), false) => tr!("rename.rename_current_name", current_name = self.current_name()).into(),
        };
        let description: SharedString = if running {
            format!("{} → {}", display_path(&self.from), display_path(&to)).into()
        } else if project {
            t("rename.project_folder_will_be_renamed")
            .into()
        } else {
            t("rename.rename_or_move_folder_with")
            .into()
        };
        let header = v_flex()
            .gap_2()
            .child(div().text_base().font_semibold().child(title))
            .child(div().text_color(muted).child(description));

        if let Some(current) = step {
            let steps = labels.iter().enumerate().map(|(index, label)| {
                let icon = if index < current {
                    Icon::new(AssetIcon::Check).small().text_color(success).into_any_element()
                } else if index == current {
                    Spinner::new().small().into_any_element()
                } else {
                    Icon::new(AssetIcon::Circle).small().text_color(muted).into_any_element()
                };
                h_flex()
                    .gap_2()
                    .when(index > current, |this| this.text_color(muted))
                    .child(icon)
                    .child(*label)
            });
            return v_flex()
                .gap_4()
                .child(header)
                .child(v_flex().gap_2().children(steps))
                .child(div().text_xs().text_color(muted).child(t("rename.if_something_goes_wrong_everything")))
                .into_any_element();
        }

        let workspace = self.workspace.upgrade();
        let affected: Vec<Affected> =
            workspace.as_ref().map(|workspace| workspace.read(cx).affected(&self.from)).unwrap_or_default();
        let blockers: Vec<PathBuf> =
            workspace.as_ref().map(|workspace| workspace.read(cx).blockers(&self.from, cx)).unwrap_or_default();
        let issue = self.issue(cx);
        // An untouched field isn't an error: just the path caption.
        let untouched = self.name.read(cx).value().trim() == self.current_name() && !moving;
        let shown_issue = issue.clone().filter(|_| !untouched);
        let count_label = if project {
            t("rename.project_session").to_string()
        } else {
            let folders: std::collections::HashSet<&str> = affected.iter().map(|item| item.folder.as_str()).collect();
            tr!("rename.affects_affected", affected = crate::settings::projects_label(folders.len()))
        };
        let foreground = cx.theme().foreground;
        let rows = affected.iter().map(|item| {
            // Yellow text on a light background is unreadable: the warning is an icon, the text keeps the normal color.
            let warn = matches!(item.tone, crate::workspace::Tone::Warning);
            h_flex()
                .min_h_7()
                .px_2()
                .gap_2()
                .child(div().flex_1().min_w_0().truncate().child(item.name.clone()))
                .child(
                    h_flex()
                        .gap_1()
                        .text_xs()
                        .text_color(if warn { foreground } else { muted })
                        .when(warn, |this| this.child(Icon::new(gpui_kit::component::IconName::TriangleAlert).xsmall().text_color(warning)))
                        .child(item.state),
                )
        });
        let blocker_rows = blockers.iter().map(|cwd| {
            h_flex()
                .min_h_7()
                .px_2()
                .gap_2()
                .child(div().flex_1().min_w_0().truncate().child(display_path(cwd)))
                .child(div().text_xs().text_color(danger).child(t("rename.claude_code_is_open_in")))
        });
        let has_blockers = !blockers.is_empty();
        let ok_label = if moving { t("rename.move") } else { t("rename.rename") };

        v_flex()
            .gap_4()
            // Escape from the name field: the field passes it on as its own action, not as the dialog's "Cancel".
            .on_action(|_: &InputEscape, window, cx| window.close_dialog(cx))
            .child(header)
            .when(!project, |this| {
                this.child(
                    v_flex()
                        .gap_2()
                        .child(t("rename.location"))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(mono.clone())
                                        .text_sm()
                                        .child(display_path(&self.parent)),
                                )
                                .child(
                                    Button::new("rename-pick-parent")
                                        .outline()
                                        .label(t("rename.choose"))
                                        .on_click(cx.listener(|this, _, window, cx| this.pick_parent(window, cx))),
                                ),
                        ),
                )
            })
            .child(
                v_flex()
                    .gap_2()
                    .child(if project { t("rename.new_name") } else { t("rename.name") })
                    .child(Input::new(&self.name))
                    .child(match &shown_issue {
                        Some(message) => div().text_xs().text_color(danger).child(message.clone()),
                        None => div().text_xs().font_family(mono.clone()).text_color(muted).child(display_path(&to)),
                    }),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(muted).child(count_label))
                    .child(
                        v_flex()
                            .p_1()
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded(cx.theme().radius)
                            .max_h(rems(8.75))
                            .overflow_y_scrollbar()
                            .children(blocker_rows)
                            .children(rows)
                            .when(affected.is_empty() && !has_blockers, |this| {
                                this.child(div().py_2().px_2().text_color(muted).child(t("rename.no_projects_inside")))
                            }),
                    )
                    .when(has_blockers, |this| {
                        this.child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .text_xs()
                                        .text_color(danger)
                                        .child(t("rename.close_these_sessions_there_session")),
                                )
                                .child(
                                    Button::new("rename-recheck")
                                        .ghost()
                                        .xsmall()
                                        .label(t("common.check_again"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(workspace) = this.workspace.upgrade() {
                                                workspace.update(cx, |workspace, cx| workspace.refresh_agents(cx));
                                            }
                                        })),
                                ),
                        )
                    }),
            )
            .when_some(error, |this, error| this.child(Alert::error("rename-error", error)))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(div().flex_none().child(
                        DialogClose::new().child(Button::new("rename-cancel").outline().label(t("common.cancel"))),
                    ))
                    .child(
                        Button::new("rename-ok")
                            .primary()
                            .label(ok_label)
                            .disabled(issue.is_some() || has_blockers)
                            .on_click(cx.listener(|this, _, window, cx| this.start(window, cx))),
                    ),
            )
            .into_any_element()
    }
}

/// Open the window inside `window`. `from` — the folder now.
pub fn open(workspace: WeakEntity<Workspace>, target: Target, from: PathBuf, window: &mut Window, cx: &mut App) {
    let current = from.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let parent = from.parent().map(PathBuf::from).unwrap_or_default();
    let view = cx.new(|cx| {
        let name = cx.new(|cx| InputState::new(window, cx).default_value(current.clone()));
        let subscriptions = vec![
            cx.subscribe(&name, |_: &mut RenameView, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            // The move finished successfully — the window closes by itself.
            cx.observe_in(
                &workspace.upgrade().expect("the Pult window is open"),
                window,
                |this: &mut RenameView, workspace, window, cx| {
                    if this.started && workspace.read(cx).move_state().is_some_and(|state| state.done) {
                        this.started = false;
                        window.close_dialog(cx);
                    }
                    cx.notify();
                },
            ),
        ];
        RenameView {
            workspace: workspace.clone(),
            target,
            from,
            parent,
            name,
            started: false,
            _subscriptions: subscriptions,
        }
    });
    let focus_once = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if focus_once.replace(false) {
            let name = view.read(cx).name.clone();
            name.focus_handle(cx).focus(window, cx);
        }
        let running = view.read(cx).running(cx);
        let confirm = view.clone();
        dialog
            .w(px(460.))
            .close_button(false)
            .keyboard(!running)
            .overlay_closable(!running)
            // Enter in the name field is the same "Rename"; the window doesn't close by itself.
            .on_ok(move |_, window, cx| {
                confirm.update(cx, |view, cx| view.start(window, cx));
                false
            })
            .child(view.clone())
    });
}
