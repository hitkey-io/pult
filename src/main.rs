// Interface strings: locales/app.yml (see `i18n`).
rust_i18n::i18n!("locales", fallback = "en");

#[macro_use]
mod i18n;
mod claude;
mod code;
mod dialogs;
mod journal;
mod migrate;
mod models;
mod presence;
mod projects;
mod rename;
mod sessions;
mod settings;
mod switch;
mod sync;
mod terminal;
mod update;
mod workspace;

mod actions {
    gpui_kit::actions!(
        pult,
        [
            NewProject,
            AddFolders,
            OpenSettings,
            RenameProject,
            SplitProject,
            NewSubproject,
            TogglePause,
            PauseOthers,
            SwitchSession,
            UseClaude,
            UseGlm,
            OpenModels,
            OpenPalette,
            TerminalTab,
            TerminalBackTab,
            TerminalCopy,
            ToggleSidebar,
            ToggleInfo,
            ToggleTheme,
            GoBack,
            GoForward,
            CopyPath,
            RevealInFinder,
            ClearTerminal,
            ZoomIn,
            ZoomOut,
            ZoomReset,
            RemoveProject,
            About,
            CheckForUpdates,
            HideApp,
            HideOthers,
            ShowAll,
            Minimize,
            Zoom,
            ToggleFullscreen,
            CloseWindow,
            Quit,
        ]
    );
}

use actions::*;
use i18n::t;
use gpui_kit::component::{
    TitleBar, Theme, ThemeMode, WindowExt as _,
    input::{Copy, Cut, Paste, Redo, SelectAll, Undo},
};
// Icons beyond GPUI Kit's standard set (sections of the "Connected to Claude" panel).
gpui_kit::assets::icon_assets!(ExtraIcons, [
    Puzzle, Sparkles, ScrollText, FolderPlus, MessageSquare, Settings, Folder, SlidersHorizontal, Check, Circle, Minus, RefreshCw, Server, TriangleAlert, Download, Laptop
]);

/// Standard component icons plus ours.
struct AppAssets;

impl gpui_kit::AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<gpui_kit::SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

use gpui_kit::{
    App, AppContext as _, Bounds, Global, KeyBinding, Menu, MenuItem, OsAction, SharedString, SystemMenuType,
    WeakEntity, Window, WindowBounds, WindowOptions, point, px, size,
};

/// Pult's main window — for commands that come from the menu or the settings window.
struct MainWorkspace(WeakEntity<workspace::Workspace>);

impl Global for MainWorkspace {}

/// Main window frame from the previous launch, if it is still visible on one of the screens.
fn main_bounds(cx: &App) -> WindowBounds {
    if let Some([x, y, width, height]) = projects::saved_window_frame() {
        let bounds = Bounds::new(point(px(x), px(y)), size(px(width.max(720.)), px(height.max(480.))));
        if cx.displays().iter().any(|display| display.bounds().intersects(&bounds)) {
            return WindowBounds::Windowed(bounds);
        }
    }
    WindowBounds::Windowed(Bounds::centered(None, size(px(1200.), px(780.)), cx))
}

fn open_main(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(main_bounds(cx)),
        window_min_size: Some(size(px(720.), px(480.))),
        ..TitleBar::window_options()
    };
    let (_, workspace) = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| workspace::Workspace::new(window, cx))
    })
    .expect("failed to open the window");
    cx.set_global(MainWorkspace(workspace.downgrade()));
}

/// A "Window" menu command for the window currently in front (main or settings).
fn with_active_window(cx: &mut App, action: impl FnOnce(&mut Window, &mut App)) {
    if let Some(handle) = cx.active_window() {
        let _ = handle.update(cx, |_, window, cx| action(window, cx));
    }
}

/// Variables Claude Code uses to mark its child processes. Pult launched from a terminal running Claude Code
/// would inherit them, and Claude inside Pult would consider itself nested: it would not save the transcript
/// ("Transcript saving is off"). The user's own variables are re-read from their profile by zsh -l
/// anyway.
fn forget_parent_claude() {
    let inherited: Vec<String> = std::env::vars()
        .map(|(name, _)| name)
        // CLAUDE_JOB_DIR is the folder of the background session Pult was launched from: with it, Claude Code in
        // Pult's terminal takes that session's name.
        .filter(|name| {
            name == "CLAUDECODE"
                || name.starts_with("CLAUDE_CODE_")
                || ["CLAUDE_PID", "CLAUDE_EFFORT", "CLAUDE_JOB_DIR", "AI_AGENT"].contains(&name.as_str())
        })
        .collect();
    for name in inherited {
        // SAFETY: called first in main, before any threads start.
        unsafe { std::env::remove_var(name) };
    }
}

/// How to draw frames (`vendor/README.md`). In a virtual machine `CAMetalDisplayLink` gives ~12 frames per
/// second — a typed letter appears after ~80 ms; with `CVDisplayLink` after ~18 ms. On real hardware — as
/// GPUI intends. For testing — the `frame-source` file in Pult's data
/// folder: `metal`, `cvdisplaylink`, or a frame rate for `CAMetalDisplayLink`.
/// An app opened from the Finder gets a soft limit of 256 open files. Every project's terminal stays open
/// while Pult runs, sync keeps several connections, and `git` and `claude` add their pipes — near the limit
/// opening anything fails in odd places. Raised to what the system allows (macOS takes at most `OPEN_MAX`).
fn raise_open_file_limit() {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit/setrlimit only read and write the struct passed.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return;
        }
        // OPEN_MAX from <sys/syslimits.h>: setrlimit refuses more for the soft limit.
        let want = limit.rlim_max.min(10_240);
        if limit.rlim_cur < want {
            limit.rlim_cur = want;
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
}

fn choose_frame_source() {
    let forced = dirs::data_dir()
        .and_then(|dir| std::fs::read_to_string(dir.join("pult").join("frame-source")).ok())
        .map(|text| text.trim().to_string());
    let Some(choice) = forced.or_else(|| in_virtual_machine().then(|| "cvdisplaylink".to_string())) else {
        return;
    };
    // SAFETY: called at the start of main, before any threads start.
    unsafe {
        if choice == "cvdisplaylink" {
            std::env::set_var("GPUI_FRAME_SOURCE", "cvdisplaylink");
        } else if choice.parse::<f32>().is_ok() {
            std::env::set_var("GPUI_FRAME_RATE", &choice);
        }
    }
}

/// macOS reports that it runs under a hypervisor (`kern.hv_vmm_present` = 1).
fn in_virtual_machine() -> bool {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is a NUL-terminated string; the buffer and its size are the variables above.
    let found = unsafe {
        libc::sysctlbyname(c"kern.hv_vmm_present".as_ptr(), (&raw mut value).cast(), &mut size, std::ptr::null_mut(), 0)
    };
    found == 0 && value == 1
}

fn main() {
    // `pult sync …` — work without a window: called by Claude Code hooks and by the user from a terminal.
    let args: Vec<String> = std::env::args().collect();
    // The language first: command-line output and errors saved by the sync hook use it too.
    i18n::apply(projects::saved_language());
    if args.get(1).map(String::as_str) == Some("sync") {
        std::process::exit(sync::cli(&args[2..]));
    }
    // `pult journal hook` — called by Claude Code hooks of the sessions of split projects.
    if args.get(1).map(String::as_str) == Some("journal") {
        std::process::exit(journal::cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("code") {
        std::process::exit(code::cli(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("update") {
        std::process::exit(update::cli(&args[2..]));
    }
    forget_parent_claude();
    raise_open_file_limit();
    choose_frame_source();
    // Claude Code deletes transcripts older than 30 days on its own — Pult turns that off (`sync::KEEP_DAYS`).
    let _ = sync::keep_transcripts(&sync::Paths::real());
    sync::start_periodic();
    let app = gpui_kit::application().with_assets(AppAssets);
    // The main window was closed (⌘W, red button) but Pult keeps running — clicking the Dock icon
    // opens it again; background Claude sessions live on their own all this time.
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            open_main(cx);
        }
    });
    app.run(|cx: &mut App| {
            gpui_kit::init(cx);
            // Language — as in the system (Russian or English) or as chosen in settings; also for the labels
            // inside GPUI Kit components.
            i18n::apply(projects::saved_language());
            Theme::change(ThemeMode::Light, None, cx);

            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_action(|_: &HideApp, cx| cx.hide());
            cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
            cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
            cx.on_action(|_: &Minimize, cx| with_active_window(cx, |window, _| window.minimize_window()));
            cx.on_action(|_: &Zoom, cx| with_active_window(cx, |window, _| window.zoom_window()));
            cx.on_action(|_: &ToggleFullscreen, cx| with_active_window(cx, |window, _| window.toggle_fullscreen()));
            cx.on_action(|_: &CloseWindow, cx| with_active_window(cx, |window, _| window.remove_window()));
            cx.on_action(|_: &About, cx| {
                with_active_window(cx, |window, cx| {
                    let title = SharedString::from(tr!("app.pult_env", env = env!("CARGO_PKG_VERSION")));
                    window.open_alert_dialog(cx, move |alert, _, _| {
                        alert
                            .title(title.clone())
                            .description(t("app.pult_one_claude_code_session"))
                            .ok_text(t("app.done"))
                    });
                })
            });
            update::init(cx);
            cx.on_action(|_: &CheckForUpdates, cx| update::check_now(update::Origin::Menu, cx));
            // ⌘, and the "Pult → Settings…" menu also work from the settings window, which has no handler.
            cx.on_action(|_: &OpenSettings, cx| {
                let workspace = cx.try_global::<MainWorkspace>().and_then(|main| main.0.upgrade());
                match workspace {
                    Some(workspace) => workspace.update(cx, |workspace, cx| workspace.open_settings(cx)),
                    None => open_main(cx),
                }
            });
            cx.bind_keys([
                KeyBinding::new("cmd-n", NewProject, None),
                // Tab in the terminal goes to the program (completion), not to GPUI Kit's focus traversal (Root).
                KeyBinding::new("tab", TerminalTab, Some("Terminal")),
                KeyBinding::new("shift-tab", TerminalBackTab, Some("Terminal")),
                // ⌘C on Root (GPUI Kit) is intercepted before the terminal's `on_key_down`.
                KeyBinding::new("cmd-c", TerminalCopy, Some("Terminal")),
                KeyBinding::new("cmd-o", AddFolders, None),
                KeyBinding::new("cmd-,", OpenSettings, None),
                KeyBinding::new("shift-cmd-p", TogglePause, None),
                KeyBinding::new("cmd-b", ToggleSidebar, None),
                KeyBinding::new("alt-cmd-b", ToggleInfo, None),
                KeyBinding::new("shift-cmd-l", ToggleTheme, None),
                KeyBinding::new("cmd-[", GoBack, None),
                KeyBinding::new("cmd-]", GoForward, None),
                KeyBinding::new("cmd-k", OpenPalette, None),
                KeyBinding::new("cmd-=", ZoomIn, None),
                KeyBinding::new("cmd-+", ZoomIn, None),
                KeyBinding::new("cmd--", ZoomOut, None),
                KeyBinding::new("cmd-0", ZoomReset, None),
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("cmd-h", HideApp, None),
                KeyBinding::new("alt-cmd-h", HideOthers, None),
                KeyBinding::new("cmd-m", Minimize, None),
                KeyBinding::new("cmd-w", CloseWindow, None),
                KeyBinding::new("ctrl-cmd-f", ToggleFullscreen, None),
            ]);
            install_menus(cx);

            open_main(cx);
            // Check the settings look without clicks: see `settings::start_page`.
            if std::env::var_os("PULT_SETTINGS_PAGE").is_some()
                && let Some(workspace) = cx.try_global::<MainWorkspace>().and_then(|main| main.0.upgrade())
            {
                workspace.update(cx, |workspace, cx| workspace.open_settings(cx));
            }
            // Check the "New project" window look: `PULT_NEW_PROJECT=<name>`.
            if std::env::var_os("PULT_NEW_PROJECT").is_some()
                && let Some(window) = cx.windows().first().copied()
            {
                let _ = window.update(cx, |_, window, cx| window.dispatch_action(Box::new(NewProject), cx));
            }
            // Check subprojects without clicks: `PULT_DO` — see `Workspace::run_script`.
            if let Ok(script) = std::env::var("PULT_DO")
                && let Some(window) = cx.windows().first().copied()
                && let Some(workspace) = cx.try_global::<MainWorkspace>().and_then(|main| main.0.upgrade())
            {
                let _ = window.update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| workspace.run_script(&script, window, cx));
                });
            }
            // Check the subproject windows' look: `PULT_DIALOG=split|subproject|remove|remove-project|palette`
            // (with `PULT_SHOW=<project key>` to pick the project and `PULT_SUB_NAMES=A|B` to fill the fields).
            if let Ok(dialog) = std::env::var("PULT_DIALOG")
                && let Some(window) = cx.windows().first().copied()
            {
                let _ = window.update(cx, |_, window, cx| match dialog.as_str() {
                    "split" => window.dispatch_action(Box::new(SplitProject), cx),
                    "subproject" => window.dispatch_action(Box::new(NewSubproject), cx),
                    "remove" => window.dispatch_action(Box::new(RemoveProject), cx),
                    "palette" => window.dispatch_action(Box::new(OpenPalette), cx),
                    kind if kind.starts_with("switch-") => {
                        if let Some(workspace) = cx.try_global::<MainWorkspace>().and_then(|main| main.0.upgrade()) {
                            switch::demo(workspace.downgrade(), kind, window, cx);
                        }
                    }
                    "remove-project" => {
                        if let Some(workspace) = cx.try_global::<MainWorkspace>().and_then(|main| main.0.upgrade()) {
                            workspace.update(cx, |workspace, cx| workspace.confirm_remove_current_folder(window, cx));
                        }
                    }
                    _ => {}
                });
            }
            // Check the keyboard without typing: `PULT_KEYS=down,down,enter` presses those keys half a second
            // after launch.
            if let Ok(keys) = std::env::var("PULT_KEYS")
                && let Some(window) = cx.windows().first().copied()
            {
                let keys: &'static str = keys.leak();
                cx.spawn(async move |cx| {
                    cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                    for key in keys.split(',') {
                        let Ok(keystroke) = gpui_kit::Keystroke::parse(key.trim()) else {
                            continue;
                        };
                        let _ = window.update(cx, |_, window, cx| window.dispatch_keystroke(keystroke, cx));
                        cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
                    }
                })
                .detach();
            }
            cx.activate(true);
        });
}

/// The macOS menu; rebuilt when the language changes.
pub fn install_menus(cx: &mut App) {
    cx.set_menus([
        // As in any Mac app (Apple HIG: About, Settings, Services, Hide, Quit).
        Menu::new(t("common.pult")).items([
            MenuItem::action(t("app.about_pult"), About),
            MenuItem::action(t("app.check_for_updates"), CheckForUpdates),
            MenuItem::separator(),
            MenuItem::action(t("common.settings"), OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu(t("app.services"), SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(t("app.hide_pult"), HideApp),
            MenuItem::action(t("app.hide_others"), HideOthers),
            MenuItem::action(t("app.show_all"), ShowAll),
            MenuItem::separator(),
            MenuItem::action(t("app.quit_pult"), Quit),
        ]),
        Menu::new(t("app.file")).items([
            MenuItem::action(t("common.new_project"), NewProject),
            MenuItem::action(t("common.add_folders"), AddFolders),
            MenuItem::separator(),
            MenuItem::action(t("app.pause_continue"), TogglePause),
            MenuItem::action(t("common.pause_all_except_current"), PauseOthers),
            MenuItem::action(t("common.change_session"), SwitchSession),
            MenuItem::separator(),
            MenuItem::action(t("common.copy_path"), CopyPath),
            MenuItem::action(t("common.show_in_finder"), RevealInFinder),
            MenuItem::separator(),
            MenuItem::action(t("common.rename_project"), RenameProject),
            MenuItem::action(t("common.new_subproject"), NewSubproject),
            MenuItem::action(t("common.remove_from_pult"), RemoveProject),
            MenuItem::separator(),
            MenuItem::action(t("app.close_window"), CloseWindow),
        ]),
        // Edit — for input fields and the terminal (⌘C, ⌘V).
        Menu::new(t("app.edit")).items([
            MenuItem::os_action(t("app.undo"), Undo, OsAction::Undo),
            MenuItem::os_action(t("app.redo"), Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action(t("app.cut"), Cut, OsAction::Cut),
            MenuItem::os_action(t("app.copy"), Copy, OsAction::Copy),
            MenuItem::os_action(t("app.paste"), Paste, OsAction::Paste),
            MenuItem::os_action(t("app.select_all"), SelectAll, OsAction::SelectAll),
        ]),
        Menu::new(t("app.view")).items([
            MenuItem::action(t("app.sidebar"), ToggleSidebar),
            MenuItem::action(t("common.connected_to_claude"), ToggleInfo),
            MenuItem::action(t("app.switch_theme"), ToggleTheme),
            MenuItem::separator(),
            MenuItem::action(t("common.back"), GoBack),
            MenuItem::action(t("common.forward"), GoForward),
            MenuItem::separator(),
            MenuItem::action(t("app.zoom_in"), ZoomIn),
            MenuItem::action(t("app.zoom_out"), ZoomOut),
            MenuItem::action(t("app.actual_size"), ZoomReset),
            MenuItem::separator(),
            MenuItem::action(t("common.clear_terminal_screen"), ClearTerminal),
            MenuItem::separator(),
            MenuItem::action(t("common.search"), OpenPalette),
            MenuItem::separator(),
            MenuItem::action(t("app.enter_full_screen"), ToggleFullscreen),
        ]),
        Menu::new(t("app.window")).items([
            MenuItem::action(t("app.minimize"), Minimize),
            MenuItem::action(t("app.zoom"), Zoom),
        ]),
    ]);
}
