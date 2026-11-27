//! Windows over the workspace: search (⌘K), "New project" and "Add folders". GPUI Kit components only.

use std::{collections::BTreeSet, path::PathBuf};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    command::{Command, CommandGroup, CommandItem, CommandState},
    dialog::{DialogAction, DialogClose},
    scroll::ScrollableElement as _,
    h_flex,
    input::{Escape as InputEscape, Input, InputState},
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, WeakEntity, Window, div, prelude::FluentBuilder as _, px, rems,
};

use gpui_kit::component::Sizable as _;
use crate::i18n::t;
use crate::projects::{ThemePref, folder_name};
use crate::workspace::Workspace;

/// What a row of the search window does.
#[derive(Clone)]
pub enum PaletteAction {
    Project(String),
    NewProject,
    AddFolders,
    TogglePause,
    RestartSession,
    PauseOthers,
    Settings,
    RenameProject,
    SplitProject,
    NewSubproject,
    CloneSubproject,
    ToggleSidebar,
    ToggleInfo,
    Theme(ThemePref),
    /// Point Claude Code at Claude or at GLM.
    UseProvider(crate::models::Provider),
    /// Settings, the Models page.
    ModelsSettings,
}

/// Current project's commands in the search window: labels with its name.
pub struct CurrentCommands {
    /// "Pause “X”" or "Continue “X”".
    pub pause: Option<String>,
    /// "Restart “X”" when the session can be restarted.
    pub restart: Option<String>,
    /// "Rename “X”…".
    pub rename: Option<String>,
    /// "Split “X” into subprojects…" for an ordinary project, "New subproject in “X”…" for a subproject.
    pub subproject: Option<(String, PaletteAction)>,
    /// "Clone “X”…" for a subproject whose conversation can be copied.
    pub clone: Option<String>,
    /// Who Claude Code talks to now: the switch command offers the other one.
    pub provider: crate::models::Provider,
}

/// A search window row: label on the left, hint on the right (path or shortcut).
fn row(label: String, hint: String, cx: &App) -> impl IntoElement + use<> {
    h_flex()
        .w_full()
        .gap_2()
        .child(div().flex_1().truncate().child(label))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(hint),
        )
}

/// The search window: projects and commands. `projects` — (key, title, path to show).
pub fn open_palette(
    workspace: WeakEntity<Workspace>,
    projects: Vec<(String, String, String)>,
    current: CurrentCommands,
    sidebar_open: bool,
    info_open: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let state = cx.new(|cx| CommandState::new(window, cx));
    let mut project_actions = Vec::new();
    let mut project_items = Vec::new();
    for (id, name, path) in projects {
        project_actions.push(PaletteAction::Project(id));
        let (label, hint) = (name.clone(), path);
        project_items.push(CommandItem::new().label(name).child(move |_, cx| {
            row(label.clone(), hint.clone(), cx)
        }));
    }
    let pause_label: &'static str = current.pause.map(|label| &*label.leak()).unwrap_or("");
    let rename_label: &'static str = current.rename.map(|label| &*label.leak()).unwrap_or("");
    let mut commands = vec![
        (t("common.new_project"), "⌘N", PaletteAction::NewProject),
        (t("common.settings"), "⌘,", PaletteAction::Settings),
        match current.provider {
            crate::models::Provider::Glm => (t("models.cmd_to_claude"), "", PaletteAction::UseProvider(crate::models::Provider::Claude)),
            crate::models::Provider::Claude => (t("models.cmd_to_glm"), "", PaletteAction::UseProvider(crate::models::Provider::Glm)),
        },
        (t("models.cmd_settings"), "", PaletteAction::ModelsSettings),
    ];
    if !rename_label.is_empty() {
        commands.push((rename_label, "", PaletteAction::RenameProject));
    }
    commands.push((t("common.add_folders"), "⌘O", PaletteAction::AddFolders));
    if let Some((label, action)) = current.subproject {
        commands.push((&*label.leak(), "", action));
    }
    if let Some(label) = current.clone {
        commands.push((&*label.leak(), "", PaletteAction::CloneSubproject));
    }
    if !pause_label.is_empty() {
        commands.push((pause_label, "⇧⌘P", PaletteAction::TogglePause));
    }
    if let Some(label) = current.restart {
        commands.push((&*label.leak(), "", PaletteAction::RestartSession));
    }
    commands.push((t("common.pause_all_except_current"), "", PaletteAction::PauseOthers));
    commands.extend([
        (
            if sidebar_open {
                t("common.hide_sidebar")
            } else {
                t("common.show_sidebar")
            },
            "⌘B",
            PaletteAction::ToggleSidebar,
        ),
        (
            if info_open {
                t("dialogs.hide_connected_panel")
            } else {
                t("dialogs.show_connected_panel")
            },
            "⌥⌘B",
            PaletteAction::ToggleInfo,
        ),
        (t("dialogs.system_theme"), "", PaletteAction::Theme(ThemePref::System)),
        (t("common.light_theme"), "", PaletteAction::Theme(ThemePref::Light)),
        (t("common.dark_theme"), "", PaletteAction::Theme(ThemePref::Dark)),
    ]);
    let mut command_actions = Vec::new();
    let mut command_items = Vec::new();
    for (label, hint, action) in commands {
        command_actions.push(action);
        command_items.push(CommandItem::new().label(label).child(move |_, cx| {
            row(label.to_string(), hint.to_string(), cx)
        }));
    }
    // Groups go in the same order as in the window: projects first, then commands.
    let mut groups: Vec<Vec<PaletteAction>> = Vec::new();
    let mut has_projects = false;
    if !project_items.is_empty() {
        has_projects = true;
        groups.push(project_actions);
    }
    groups.push(command_actions);

    let focus_once = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        // The keyboard goes to the search field: typing filters, the arrows move through the list.
        if focus_once.replace(false) {
            state.update(cx, |state, cx| state.focus(window, cx));
        }
        let groups = groups.clone();
        let workspace = workspace.clone();
        let mut command = Command::new(&state)
            .placeholder(t("dialogs.project_or_command"))
            .bordered(false)
            // Without its own border the list is square; round it like the window around it, or its corners
            // stick out.
            .rounded(cx.theme().radius_lg)
            .max_h(rems(21.25))
            .empty(|_, _, cx| {
                div()
                    .py_6()
                    .text_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(t("dialogs.nothing_found"))
            })
            .on_confirm(move |path, window, cx| {
                let Some(action) = groups.get(path.section).and_then(|group| group.get(path.row)).cloned()
                else {
                    return;
                };
                window.close_dialog(cx);
                let _ = workspace.update(cx, |this, cx| this.run_palette(action, window, cx));
            });
        if has_projects {
            command = command.group(CommandGroup::new().label(t("common.projects")).items(project_items.clone()));
        }
        command = command.group(CommandGroup::new().label(t("dialogs.commands")).items(command_items.clone()));
        dialog
            .w(px(560.))
            .margin_top(px(88.))
            .p_0()
            .close_button(false)
            .child(command)
    });
}

/// The "~/Projects/…" caption under the name field: follows the input.
struct PathPreview {
    root: String,
    name: Entity<InputState>,
}

impl Render for PathPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let value = self.name.read(cx).value().to_string();
        let folder = folder_name(&value);
        let shown = if folder.is_empty() { "…".to_string() } else { folder };
        let issue = name_issue(&self.root, &value);
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(format!("{}/{shown}", self.root))),
            )
            // The error sits at the field, in words and with an icon, not as a notification after the click.
            .when_some(issue, |this, issue| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(gpui_kit::component::Icon::new(gpui_kit::component::IconName::CircleX).xsmall())
                        .child(SharedString::from(issue)),
                )
            })
    }
}

/// What is wrong with the new project's name while it is being typed; `root` — the main folder as shown.
fn name_issue(root: &str, value: &str) -> Option<String> {
    let name = folder_name(value);
    if name.is_empty() {
        return None;
    }
    if name.starts_with('.') || name.contains('/') || name.contains(':') {
        return Some(
            t("common.name_can_t_start_with")
                .into(),
        );
    }
    let base = match root.strip_prefix("~") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest.trim_start_matches('/')),
        None => PathBuf::from(root),
    };
    base.join(&name).exists().then(|| tr!("dialogs.folder_root_name_already_exists", root = root, name = name))
}

/// "Cancel" / "Create" buttons; "Create" is disabled while the name is empty.
struct NewProjectFooter {
    name: Entity<InputState>,
    root: String,
}

impl Render for NewProjectFooter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let value = self.name.read(cx).value().to_string();
        let empty = value.trim().is_empty() || name_issue(&self.root, &value).is_some();
        h_flex()
            .justify_end()
            .gap_2()
            .child(div().flex_none().child(
                DialogClose::new().child(Button::new("new-project-cancel").outline().label(t("common.cancel"))),
            ))
            .child(div().flex_none().child(
                DialogAction::new().child(
                    Button::new("new-project-create")
                        .primary()
                        .label(t("dialogs.create"))
                        .disabled(empty),
                ),
            ))
    }
}

impl Focusable for NewProjectFooter {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

/// The "New project" window: name, path caption, buttons.
pub fn open_new_project(
    workspace: WeakEntity<Workspace>,
    root: String,
    window: &mut Window,
    cx: &mut App,
) {
    let name = cx.new(|cx| {
        let mut state = InputState::new(window, cx);
        // For checking the look: `PULT_NEW_PROJECT=<name>` — the window opens with the name filled in.
        if let Ok(preset) = std::env::var("PULT_NEW_PROJECT") {
            state.set_value(preset, window, cx);
        }
        state
    });
    let preview = cx.new(|cx| {
        cx.observe(&name, |_, _, cx| cx.notify()).detach();
        PathPreview { root: root.clone(), name: name.clone() }
    });
    let footer = cx.new(|cx| {
        cx.observe(&name, |_, _, cx| cx.notify()).detach();
        NewProjectFooter { name: name.clone(), root: root.clone() }
    });
    let focus_once = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if focus_once.replace(false) {
            name.focus_handle(cx).focus(window, cx);
        }
        let name_for_ok = name.clone();
        let root_for_ok = root.clone();
        let workspace = workspace.clone();
        let header = v_flex()
            .gap_2()
            .child(div().text_base().font_semibold().child(t("dialogs.new_project")))
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(tr!("dialogs.folder_is_created_in_root", root = root))),
            );
        dialog
            .w(px(420.))
            .close_button(false)
            .child(
                v_flex()
                    .gap_4()
                    // The input passes an Escape it doesn't need on as its own action, not as the dialog's
                    // "Cancel" — without this, Escape doesn't close the window.
                    .on_action(|_: &InputEscape, window, cx| window.close_dialog(cx))
                    .child(header)
                    .child(
                        v_flex()
                            .gap_2()
                            .child(div().child(t("dialogs.name")))
                            .child(Input::new(&name))
                            .child(preview.clone()),
                    )
                    .child(footer.clone()),
            )
            .on_ok(move |_, window, cx| {
                let value = name_for_ok.read(cx).value().to_string();
                if value.trim().is_empty() || name_issue(&root_for_ok, &value).is_some() {
                    return false;
                }
                workspace
                    .update(cx, |this, cx| this.create_project(&value, window, cx))
                    .unwrap_or(false)
            })
    });
}

/// The session name a subproject will get, shown while its name is being typed.
fn session_hint(folder: &str, value: &str) -> String {
    if value.trim().is_empty() {
        let main = crate::sessions::main_name(folder);
        format!("{}-…", main.strip_suffix("-main").unwrap_or(&main))
    } else {
        crate::sessions::session_name(folder, Some(value.trim()))
    }
}

/// The name fields of the "Split into subprojects" (two) and "New subproject" (one) windows.
#[derive(Clone)]
struct SubprojectFields {
    folder: String,
    /// Subprojects the project already has.
    taken: Vec<String>,
    inputs: Vec<Entity<InputState>>,
    /// "What it does" under every name, same order. Optional.
    abouts: Vec<Entity<InputState>>,
}

impl SubprojectFields {
    fn values(&self, cx: &App) -> Vec<String> {
        self.inputs.iter().map(|input| input.read(cx).value().trim().to_string()).collect()
    }

    fn about_values(&self, cx: &App) -> Vec<String> {
        self.abouts.iter().map(|input| crate::projects::clean_about(input.read(cx).value().as_ref())).collect()
    }

    /// What is wrong with what is typed so far.
    fn issue(&self, cx: &App) -> Option<String> {
        let values = self.values(cx);
        if let [first, second] = values.as_slice()
            && !first.is_empty()
            && first.to_lowercase() == second.to_lowercase()
        {
            return Some(t("dialogs.subproject_names_must_differ").into());
        }
        values.iter().find_map(|value| crate::projects::sub_name_issue(&self.folder, &self.taken, value))
    }

    fn ready(&self, cx: &App) -> bool {
        self.values(cx).iter().all(|value| !value.is_empty()) && self.issue(cx).is_none()
    }

    fn watch<T: 'static>(&self, cx: &mut Context<T>) {
        for input in self.inputs.iter().chain(&self.abouts) {
            cx.observe(input, |_, _, cx| cx.notify()).detach();
        }
    }
}

/// Under the last field: the session names the subprojects will get, and what is wrong with the names.
struct SubprojectHint {
    fields: SubprojectFields,
}

impl Render for SubprojectHint {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let names: Vec<String> =
            self.fields.values(cx).iter().map(|value| session_hint(&self.fields.folder, value)).collect();
        let hint = match names.as_slice() {
            [first, second] => tr!("dialogs.sessions_first_second", first = first, second = second),
            _ => tr!("dialogs.session_name_hint", name = names.first().cloned().unwrap_or_default()),
        };
        v_flex()
            .gap_1()
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(SharedString::from(hint)))
            .when_some(self.fields.issue(cx), |this, issue| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(gpui_kit::component::Icon::new(gpui_kit::component::IconName::CircleX).xsmall())
                        .child(SharedString::from(issue)),
                )
            })
    }
}

/// "Cancel" and the main button; the main one is disabled until every name is filled in and fits.
struct SubprojectFooter {
    fields: SubprojectFields,
    ok: &'static str,
}

impl Render for SubprojectFooter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = self.fields.ready(cx);
        h_flex()
            .justify_end()
            .gap_2()
            .child(div().flex_none().child(
                DialogClose::new().child(Button::new("subproject-cancel").outline().label(t("common.cancel"))),
            ))
            .child(div().flex_none().child(
                DialogAction::new().child(Button::new("subproject-ok").primary().label(self.ok).disabled(!ready)),
            ))
    }
}

fn subproject_dialog(
    title: String,
    description: String,
    labels: Vec<&'static str>,
    fields: SubprojectFields,
    ok: &'static str,
    on_ok: impl Fn(Vec<String>, Vec<String>, &mut Window, &mut App) -> bool + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let hint = cx.new(|cx| {
        fields.watch(cx);
        SubprojectHint { fields: fields.clone() }
    });
    let footer = cx.new(|cx| {
        fields.watch(cx);
        SubprojectFooter { fields: fields.clone(), ok }
    });
    let focus_once = std::cell::Cell::new(true);
    let on_ok = std::rc::Rc::new(on_ok);
    window.open_dialog(cx, move |dialog, window, cx| {
        if focus_once.replace(false)
            && let Some(first) = fields.inputs.first()
        {
            first.focus_handle(cx).focus(window, cx);
        }
        let header = v_flex()
            .gap_2()
            .child(div().text_base().font_semibold().child(SharedString::from(title.clone())))
            .child(div().text_color(cx.theme().muted_foreground).child(SharedString::from(description.clone())));
        let last = fields.inputs.len().saturating_sub(1);
        let rows = fields.inputs.iter().zip(labels.iter()).enumerate().map(|(index, (input, label))| {
            v_flex()
                .gap_2()
                .child(div().child(*label))
                .child(Input::new(input))
                .when_some(fields.abouts.get(index), |this, about| this.child(Input::new(about)))
                .when(index == last, |this| this.child(hint.clone()))
        });
        let (fields, on_ok) = (fields.clone(), on_ok.clone());
        dialog
            .w(px(420.))
            .close_button(false)
            .child(
                v_flex()
                    .gap_4()
                    .on_action(|_: &InputEscape, window, cx| window.close_dialog(cx))
                    .child(header)
                    .children(rows)
                    .child(footer.clone()),
            )
            .on_ok(move |_, window, cx| {
                if !fields.ready(cx) {
                    return false;
                }
                on_ok(fields.values(cx), fields.about_values(cx), window, cx)
            })
    });
}

/// For checking the look: `PULT_SUB_NAMES=A|B` — the subproject windows open with the names filled in
/// (`PULT_SUB_ABOUT` — with the "what it does" lines).
fn preset_names(inputs: &[Entity<InputState>], window: &mut Window, cx: &mut App) {
    preset(inputs, "PULT_SUB_NAMES", window, cx);
}

fn preset_abouts(inputs: &[Entity<InputState>], window: &mut Window, cx: &mut App) {
    preset(inputs, "PULT_SUB_ABOUT", window, cx);
}

fn preset(inputs: &[Entity<InputState>], variable: &str, window: &mut Window, cx: &mut App) {
    let Ok(preset) = std::env::var(variable) else {
        return;
    };
    for (input, name) in inputs.iter().zip(preset.split('|')) {
        let name = name.to_string();
        input.update(cx, |state, cx| state.set_value(name, window, cx));
    }
}

/// The "Split into subprojects" window: the current conversation becomes the first subproject, the second
/// one starts new.
pub fn open_split(
    workspace: WeakEntity<Workspace>,
    id: String,
    folder: String,
    taken: Vec<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let first = cx.new(|cx| InputState::new(window, cx).placeholder(t("dialogs.example_first")));
    let second = cx.new(|cx| InputState::new(window, cx).placeholder(t("dialogs.example_second")));
    let about_first = cx.new(|cx| InputState::new(window, cx).placeholder(t("dialogs.about_example_first")));
    let about_second = cx.new(|cx| InputState::new(window, cx).placeholder(t("dialogs.about_example_second")));
    preset_names(&[first.clone(), second.clone()], window, cx);
    preset_abouts(&[about_first.clone(), about_second.clone()], window, cx);
    let fields =
        SubprojectFields { folder: folder.clone(), taken, inputs: vec![first, second], abouts: vec![about_first, about_second] };
    subproject_dialog(
        tr!("dialogs.split_name_into_subprojects", name = folder),
        t("dialogs.each_subproject_has_its_own").into(),
        vec![t("dialogs.current_conversation"), t("dialogs.new_subproject_field")],
        fields,
        t("dialogs.split"),
        move |values, abouts, window, cx| {
            workspace
                .update(cx, |this, cx| this.split_project(&id, (&values[0], &abouts[0]), (&values[1], &abouts[1]), window, cx))
                .unwrap_or(false)
        },
        window,
        cx,
    );
}

/// The "New subproject" window of a project that is already split.
pub fn open_new_subproject(
    workspace: WeakEntity<Workspace>,
    folder: String,
    path: String,
    taken: Vec<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let name = cx.new(|cx| InputState::new(window, cx));
    let about = cx.new(|cx| InputState::new(window, cx).placeholder(t("dialogs.about_example_new")));
    preset_names(std::slice::from_ref(&name), window, cx);
    preset_abouts(std::slice::from_ref(&about), window, cx);
    let fields = SubprojectFields { folder: folder.clone(), taken, inputs: vec![name], abouts: vec![about] };
    subproject_dialog(
        tr!("dialogs.new_subproject_in_name", name = folder),
        tr!("dialogs.own_conversation_in_same_folder", path = path),
        vec![t("dialogs.name")],
        fields,
        t("dialogs.create"),
        move |values, abouts, window, cx| {
            workspace
                .update(cx, |this, cx| this.create_subproject(&folder, &values[0], &abouts[0], window, cx))
                .unwrap_or(false)
        },
        window,
        cx,
    );
}

/// "Clone “X”…": a new subproject in the same project that starts from a copy of `source`'s conversation.
/// `busy` — Claude is answering in the source right now: the answer in progress stays only there.
#[allow(clippy::too_many_arguments)]
pub fn open_clone_subproject(
    workspace: WeakEntity<Workspace>,
    source: String,
    source_name: String,
    folder: String,
    taken: Vec<String>,
    busy: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let name = cx.new(|cx| InputState::new(window, cx));
    let about = cx.new(|cx| InputState::new(window, cx).placeholder(t("dialogs.about_example_new")));
    preset_names(std::slice::from_ref(&name), window, cx);
    preset_abouts(std::slice::from_ref(&about), window, cx);
    let fields = SubprojectFields { folder: folder.clone(), taken, inputs: vec![name], abouts: vec![about] };
    let mut description = tr!("dialogs.clone_description", folder = folder, name = source_name);
    if busy {
        description.push(' ');
        description.push_str(t("dialogs.clone_busy"));
    }
    subproject_dialog(
        tr!("dialogs.clone_title", name = source_name),
        description,
        vec![t("dialogs.name")],
        fields,
        t("dialogs.clone_ok"),
        move |values, abouts, window, cx| {
            workspace
                .update(cx, |this, cx| this.clone_subproject(&source, &values[0], &abouts[0], window, cx))
                .unwrap_or(false)
        },
        window,
        cx,
    );
}

/// The "What it does" window of a subproject: one line, read by the parallel sessions.
pub fn open_about(workspace: WeakEntity<Workspace>, id: String, title: String, current: String, window: &mut Window, cx: &mut App) {
    let input = cx.new(|cx| {
        let mut state = InputState::new(window, cx).placeholder(t("dialogs.about_example_new"));
        state.set_value(current, window, cx);
        state
    });
    let focus_once = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if focus_once.replace(false) {
            input.focus_handle(cx).focus(window, cx);
        }
        let input_for_ok = input.clone();
        let (workspace, id) = (workspace.clone(), id.clone());
        let header = v_flex()
            .gap_2()
            .child(div().text_base().font_semibold().child(SharedString::from(title.clone())))
            .child(div().text_color(cx.theme().muted_foreground).child(t("dialogs.about_description")));
        dialog
            .w(px(420.))
            .close_button(false)
            .child(
                v_flex()
                    .gap_4()
                    .on_action(|_: &InputEscape, window, cx| window.close_dialog(cx))
                    .child(header)
                    .child(v_flex().gap_2().child(div().child(t("dialogs.what_it_does"))).child(Input::new(&input)))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(div().flex_none().child(
                                DialogClose::new().child(Button::new("about-cancel").outline().label(t("common.cancel"))),
                            ))
                            .child(div().flex_none().child(
                                DialogAction::new().child(Button::new("about-save").primary().label(t("dialogs.save"))),
                            )),
                    ),
            )
            .on_ok(move |_, _, cx| {
                let text = input_for_ok.read(cx).value().to_string();
                let _ = workspace.update(cx, |this, cx| this.set_about(&id, &text, cx));
                true
            })
    });
}

/// The "Add folders" window: folders of the main folder that aren't in Pult yet, with checkboxes.
struct AddFoldersList {
    /// The folder and how many Claude sessions it has.
    candidates: Vec<(PathBuf, usize)>,
    selected: BTreeSet<usize>,
}

impl AddFoldersList {
    fn chosen(&self) -> Vec<PathBuf> {
        self.selected.iter().filter_map(|index| self.candidates.get(*index)).map(|(path, _)| path.clone()).collect()
    }
}

fn sessions_label(count: usize) -> String {
    if count == 0 {
        return t("dialogs.no_sessions").into();
    }
    crate::i18n::count(count as u64, "dialogs.count_session")
}

impl Render for AddFoldersList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let count = self.selected.len();
        let rows = self.candidates.iter().enumerate().map(|(index, (path, sessions))| {
            let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            let checked = self.selected.contains(&index);
            h_flex()
                .h_8()
                .px_2()
                .gap_2()
                .rounded(cx.theme().radius)
                .hover(|this| this.bg(cx.theme().accent))
                .child(
                    Checkbox::new(SharedString::from(format!("folder-{index}")))
                        .checked(checked)
                        .label(name)
                        .flex_1()
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            if *checked {
                                this.selected.insert(index);
                            } else {
                                this.selected.remove(&index);
                            }
                            cx.notify();
                        })),
                )
                .child(div().text_xs().text_color(muted).child(sessions_label(*sessions)))
        });
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .p_1()
                    .gap_0p5()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .max_h(rems(17.5))
                    .overflow_y_scrollbar()
                    .when(self.candidates.is_empty(), |this| {
                        this.child(
                            div()
                                .py_4()
                                .text_center()
                                .text_color(muted)
                                .child(t("dialogs.all_folders_are_already_in")),
                        )
                    })
                    .children(rows),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(div().flex_none().child(
                        DialogClose::new().child(Button::new("add-folders-cancel").outline().label(t("common.cancel"))),
                    ))
                    .child(div().flex_none().child(
                        DialogAction::new().child(
                            Button::new("add-folders-ok")
                                .primary()
                                .label(if count > 1 { tr!("dialogs.add_count", count = count) } else { t("dialogs.add").into() })
                                .disabled(count == 0),
                        ),
                    )),
            )
    }
}

pub fn open_add_folders(
    workspace: WeakEntity<Workspace>,
    root: String,
    candidates: Vec<(PathBuf, usize)>,
    window: &mut Window,
    cx: &mut App,
) {
    let list = cx.new(|_| AddFoldersList { candidates, selected: BTreeSet::new() });
    window.open_dialog(cx, move |dialog, _, cx| {
        let (list_for_ok, workspace) = (list.clone(), workspace.clone());
        dialog
            .w(px(440.))
            .close_button(false)
            .child(
                v_flex()
                    .gap_4()
                    .child(
                        v_flex()
                            .gap_2()
                            .child(div().text_base().font_semibold().child(t("dialogs.add_folders")))
                            .child(
                                div()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(SharedString::from(tr!("dialogs.folders_in_root_that_aren", root = root))),
                            ),
                    )
                    .child(list.clone()),
            )
            .on_ok(move |_, window, cx| {
                let paths = list_for_ok.read(cx).chosen();
                if paths.is_empty() {
                    return false;
                }
                let _ = workspace.update(cx, |this, cx| this.add_paths(paths, window, cx));
                true
            })
    });
}
