//! Project terminal: a PTY and the `alacritty_terminal` emulator (as in Zed).
//!
//! The project terminal runs Claude Code for its pinned session (`claude attach` or
//! `claude --resume`); Pult decides what to launch, the terminal only shows it. Switching
//! between projects does not interrupt processes. The screen is drawn as `StyledText` lines in the
//! look of an iTerm2 profile; mouse selection, ⌘C / ⌘V, scrollback with the wheel, zoom
//! ⌘+ / ⌘− / ⌘0 and pinch. `/clear`, `/reset`, `/new` are not sent: each project has
//! one pinned session, and `/compact` frees up context.

use std::{
    borrow::Cow,
    cell::Cell as StdCell,
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener, WindowSize},
    event_loop::{EventLoop, EventLoopSender, Msg},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{Config, Term, TermMode, cell::Flags, color::Colors},
    tty,
    vte::ansi::{Color, NamedColor, Processor},
};
use futures::{StreamExt as _, channel::mpsc};

use crate::i18n::t;
use gpui_kit::component::{
    ActiveTheme as _, StyledExt as _, button::Button, h_flex, v_flex,
};
use gpui_kit::{
    App, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable,
    FontStyle, FontWeight, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    ModifiersChangedEvent, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, PinchEvent, Pixels, Render, ScrollEvent,
    AnyElement, SharedString, StrikethroughStyle, Styled as _, StyledText, Task,
    TextRun, TouchPhase, UnderlineStyle, Window, canvas, div, font, prelude::FluentBuilder as _, px,
    rgb,
};

/// Terminal colours and font, as in an iTerm2 profile.
///
/// This is terminal content, not app chrome — the only
/// exception to the "theme tokens only" rule: the terminal looks like iTerm2
/// in any window theme. A cell is measured the way iTerm2 does: glyph width and line height
/// of the font (Monaco 22 → 13.2 × 30). Zoom — ⌘+ / ⌘− / ⌘0 and pinch.
pub(crate) mod profile {
    pub const FONT: &str = "Monaco";
    pub const FONT_SIZE: f32 = 22.;
    /// Zoom limits, as with iTerm2's ⌘+ / ⌘−.
    pub const MIN_FONT_SIZE: f32 = 9.;
    pub const MAX_FONT_SIZE: f32 = 72.;
    /// Monaco glyph width and line height per 1 pt (CoreText: 13.202 and 29.337 at 22 pt).
    pub const ADVANCE_PER_PT: f32 = 13.202 / 22.;
    pub const LINE_PER_PT: f32 = 29.337 / 22.;
    pub const SIDE_MARGIN: f32 = 5.;
    pub const TOP_BOTTOM_MARGIN: f32 = 2.;
    pub const BACKGROUND: u32 = 0x15191F;
    pub const FOREGROUND: u32 = 0xDCDCDC;
    pub const BOLD: u32 = 0xFFFFFF;
    pub const CURSOR: u32 = 0xFFFFFF;
    pub const CURSOR_TEXT: u32 = 0x000000;
    pub const SELECTION: u32 = 0xB3D7FF;
    pub const SELECTED_TEXT: u32 = 0x000000;
    /// ANSI 0–15 of the same profile.
    pub const ANSI: [u32; 16] = [
        0x14191E, 0xB43C2A, 0x00C200, 0xC7C400, 0x2744C7, 0xC040BE, 0x00C5C7, 0xC7C7C7,
        0x686868, 0xDD7975, 0x58E790, 0xECE100, 0xA7ABF2, 0xE17EE1, 0x60FDFF, 0xFFFFFF,
    ];
    pub const SCROLLBACK: usize = 10_000;
    /// Placeholders on the terminal background (no projects, no session chosen, paused).
    pub const EMPTY_TITLE: u32 = 0xFAFAFA;
    pub const EMPTY_HINT: u32 = 0xA3A3A3;
    pub const EMPTY_BUTTON_BORDER: u32 = 0x2F2F2F;
}

/// A link in the terminal text: its address and the cells it covers (may wrap to the next line).
#[derive(Clone, PartialEq)]
struct Link {
    url: String,
    cells: Vec<Point>,
}

/// Link under a cell: first OSC 8 (the program marked the link itself, as Claude Code does),
/// then an address in the text (`http://`, `https://`, `file://`), following long-line wraps.
fn link_at(term: &Term<Listener>, point: Point) -> Option<Link> {
    let grid = term.grid();
    let columns = term.columns();
    if point.line < grid.topmost_line() || point.line > grid.bottommost_line() || point.column.0 >= columns {
        return None;
    }
    if let Some(link) = grid[point.line][point.column].hyperlink() {
        let id = link.id().to_string();
        let same = |at: Point| grid[at.line][at.column].hyperlink().is_some_and(|other| other.id() == id);
        let mut cells = Vec::new();
        for line in (point.line.0 - 2).max(grid.topmost_line().0)..=(point.line.0 + 2).min(grid.bottommost_line().0) {
            for column in 0..columns {
                let at = Point::new(Line(line), Column(column));
                if same(at) {
                    cells.push(at);
                }
            }
        }
        return Some(Link { url: link.uri().to_string(), cells });
    }
    // Logical line: adjacent screen lines joined by wrapping.
    let mut first = point.line;
    while first > grid.topmost_line() && grid[first - 1usize][Column(columns - 1)].flags.contains(Flags::WRAPLINE) {
        first -= 1;
    }
    let mut cells = Vec::new();
    let mut text = String::new();
    let mut line = first;
    loop {
        for column in 0..columns {
            let cell = &grid[line][Column(column)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            cells.push(Point::new(line, Column(column)));
            text.push(if cell.c == '\0' { ' ' } else { cell.c });
        }
        if line >= grid.bottommost_line() || !grid[line][Column(columns - 1)].flags.contains(Flags::WRAPLINE) {
            break;
        }
        line += 1;
    }
    let chars: Vec<char> = text.chars().collect();
    let index = cells.iter().position(|cell| *cell == point)?;
    let stop = |char: char| char.is_whitespace() || "<>\"'`│|()[]{}«»".contains(char);
    let mut start = index;
    while start > 0 && !stop(chars[start - 1]) {
        start -= 1;
    }
    let mut end = index;
    while end < chars.len() && !stop(chars[end]) {
        end += 1;
    }
    let mut word: String = chars[start..end].iter().collect();
    let offset = ["https://", "http://", "file://"].iter().filter_map(|scheme| word.find(scheme)).min()?;
    let start = start + word[..offset].chars().count();
    word = word[offset..].to_string();
    while word.ends_with(['.', ',', ';', ':', '!', '?']) {
        word.pop();
        end -= 1;
    }
    (start <= index && index < end && word.len() > "https://".len())
        .then(|| Link { url: word, cells: cells[start..end].to_vec() })
}

/// Tells the window that the user changed the font size (so it can be remembered).
pub struct FontSizeChanged(pub f32);

impl EventEmitter<FontSizeChanged> for Terminal {}

/// Pult did not send a command that would replace the pinned session (`/clear` and synonyms).
pub struct ClearBlocked(pub String);

impl EventEmitter<ClearBlocked> for Terminal {}

/// Emulator and PTY events arrive on the reader thread — forward them to the UI through a channel.
#[derive(Clone)]
struct Listener(mpsc::UnboundedSender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.unbounded_send(event);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct GridSize {
    columns: usize,
    lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

struct Session {
    cwd: PathBuf,
    /// Arguments for `/bin/zsh` — used to restart the process on Enter after it exits.
    args: Vec<String>,
    pid: Option<u32>,
    term: Arc<FairMutex<Term<Listener>>>,
    pty: EventLoopSender,
    size: GridSize,
    exited: bool,
    /// When the process was started: a program that has just started is still drawing itself.
    opened: std::time::Instant,
    _events: Task<()>,
}

impl Session {
    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let _ = self.pty.send(Msg::Input(bytes.into()));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.pty.send(Msg::Shutdown);
    }
}

pub struct Terminal {
    sessions: HashMap<String, Session>,
    /// A project key that changed while its terminal was running (`rekey`): the process still reports
    /// events under the key it was started with.
    aliases: HashMap<String, String>,
    current: Option<String>,
    focus_handle: FocusHandle,
    font_size: f32,
    /// Terminal area bounds from the previous frame — used to compute the grid size.
    bounds: Rc<StdCell<Option<Bounds<Pixels>>>>,
    selecting: bool,
    /// Left button pressed and forwarded to the program (mouse mode): motion and release go to it too.
    /// Keeps the last cell so motion within one cell is not sent.
    reporting: Option<(usize, usize)>,
    /// Link under the pointer while ⌘ is held (as in iTerm2): the address and the cells to underline.
    hovered_link: Option<Link>,
    /// Last pointer position — to find a link when ⌘ is pressed without moving the mouse.
    last_mouse: Option<gpui_kit::Point<Pixels>>,
    scroll_remainder: f32,
    /// Input latency trace (when a `trace-input` file exists in Pult's data folder): when a key was pressed.
    trace_key: Option<std::time::Instant>,
    trace_frame: Option<std::time::Instant>,
}

/// Input latency log: `~/Library/Application Support/pult/trace-input.log`, enabled by a `trace-input`
/// file next to it. For hunting delays.
fn trace(line: &str) {
    use std::io::Write as _;
    static FILE: std::sync::OnceLock<Option<std::sync::Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    let file = FILE.get_or_init(|| {
        let dir = dirs::data_dir()?.join("pult");
        dir.join("trace-input").exists().then(|| {
            std::fs::OpenOptions::new().create(true).append(true).open(dir.join("trace-input.log")).ok().map(std::sync::Mutex::new)
        })?
    });
    if let Some(file) = file
        && let Ok(mut file) = file.lock()
    {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let _ = writeln!(file, "{}.{:03} {line}", now.as_secs(), now.subsec_millis());
    }
}

impl Terminal {
    pub fn new(font_size: Option<f32>, cx: &mut Context<Self>) -> Self {
        Self {
            sessions: HashMap::new(),
            aliases: HashMap::new(),
            current: None,
            focus_handle: cx.focus_handle().tab_stop(true),
            font_size: font_size
                .unwrap_or(profile::FONT_SIZE)
                .clamp(profile::MIN_FONT_SIZE, profile::MAX_FONT_SIZE),
            bounds: Rc::new(StdCell::new(None)),
            selecting: false,
            reporting: None,
            hovered_link: None,
            last_mouse: None,
            scroll_remainder: 0.,
            trace_key: None,
            trace_frame: None,
        }
    }

    /// Which project is shown; `None` — no terminal (a Pult placeholder takes its place).
    pub fn show(&mut self, id: Option<&str>, cx: &mut Context<Self>) {
        self.current = id.map(str::to_string);
        cx.notify();
    }

    /// Run `/bin/zsh <args>` in the project terminal; if the same process is already running, keep it.
    pub fn open(&mut self, id: &str, cwd: PathBuf, args: Vec<String>, cx: &mut Context<Self>) {
        if self
            .sessions
            .get(id)
            .is_some_and(|session| session.args == args && !session.exited)
        {
            return;
        }
        self.sessions.remove(id);
        self.spawn(id.to_string(), cwd, args, cx);
        cx.notify();
    }

    /// Close the project terminal's process (the background Claude session keeps living).
    pub fn close(&mut self, id: &str) {
        self.sessions.remove(id);
    }

    /// The project's key changed (split into subprojects, or merged back): its terminal keeps running.
    pub fn rekey(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        self.aliases.remove(new);
        if let Some(session) = self.sessions.remove(old) {
            self.sessions.insert(new.to_string(), session);
            self.aliases.insert(old.to_string(), new.to_string());
        }
        if self.current.as_deref() == Some(old) {
            self.current = Some(new.to_string());
        }
    }

    pub fn is_open(&self, id: &str) -> bool {
        self.sessions.get(id).is_some_and(|session| !session.exited)
    }

    /// Send a Claude Code command to the project terminal, only if the input line is empty —
    /// so it doesn't mix with what the user is typing. `true` if sent.
    pub fn send_command(&mut self, id: &str, command: &str) -> bool {
        let Some(session) = self.sessions.get(id).filter(|session| !session.exited) else {
            return false;
        };
        if !input_is_empty(&session.term.lock()) {
            return false;
        }
        session.write(format!("{command}\r").into_bytes());
        true
    }

    /// How long the project's terminal process has been running.
    pub fn age(&self, id: &str) -> Option<std::time::Duration> {
        self.sessions.get(id).filter(|session| !session.exited).map(|session| session.opened.elapsed())
    }

    /// A command Pult typed was not run and is still sitting in the input line: take it out again, so nothing of
    /// Pult's is left in the user's input. `true` if it was there.
    pub fn erase_typed(&mut self, id: &str, command: &str) -> bool {
        let Some(session) = self.sessions.get(id).filter(|session| !session.exited) else {
            return false;
        };
        let count = typed_left(&session.term.lock(), command);
        if count == 0 {
            return false;
        }
        session.write(vec![0x7f; count]);
        true
    }

    /// The project terminal's process — Pult uses it to find a session started right in the terminal.
    pub fn pid(&self, id: &str) -> Option<u32> {
        self.sessions.get(id).filter(|session| !session.exited)?.pid
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
    }

    /// Erase scrollback and the screen; the program redraws itself on ^L.
    pub fn clear_current(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.current_session() {
            {
                let mut term = session.term.lock();
                term.selection = None;
                term.grid_mut().clear_history();
            }
            session.write(&b"\x0c"[..]);
            cx.notify();
        }
    }

    /// ⌘+ / ⌘− — 1 pt step, ⌘0 — the size from the iTerm2 profile.
    pub fn zoom(&mut self, step: f32, cx: &mut Context<Self>) {
        self.set_font_size((self.font_size + step).round(), cx);
    }

    pub fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.set_font_size(profile::FONT_SIZE, cx);
    }

    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    pub fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        let size = size.clamp(profile::MIN_FONT_SIZE, profile::MAX_FONT_SIZE);
        if size != self.font_size {
            self.font_size = size;
            cx.emit(FontSizeChanged(size));
            cx.notify();
        }
    }

    fn cell(&self) -> (f32, f32) {
        (
            self.font_size * profile::ADVANCE_PER_PT,
            (self.font_size * profile::LINE_PER_PT).ceil(),
        )
    }

    /// How many columns and rows fit in the terminal area.
    fn grid_size(&self) -> GridSize {
        let (cell_width, line_height) = self.cell();
        let Some(bounds) = self.bounds.get() else {
            return GridSize { columns: 80, lines: 24 };
        };
        let width = f32::from(bounds.size.width) - 2. * profile::SIDE_MARGIN;
        let height = f32::from(bounds.size.height) - 2. * profile::TOP_BOTTOM_MARGIN;
        GridSize {
            columns: ((width / cell_width).floor() as usize).max(2),
            lines: ((height / line_height).floor() as usize).max(1),
        }
    }

    fn window_size(&self, size: GridSize) -> WindowSize {
        let (cell_width, line_height) = self.cell();
        WindowSize {
            num_lines: size.lines as u16,
            num_cols: size.columns as u16,
            cell_width: cell_width.round() as u16,
            cell_height: line_height as u16,
        }
    }

    fn spawn(&mut self, id: String, cwd: PathBuf, args: Vec<String>, cx: &mut Context<Self>) {
        self.aliases.remove(&id);
        let size = self.grid_size();
        let (sender, mut events) = mpsc::unbounded();
        let listener = Listener(sender);
        let config = Config {
            scrolling_history: profile::SCROLLBACK,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, listener.clone())));

        let mut env = HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());
        if std::env::var_os("LANG").is_none() {
            // An app launched from Finder has no LANG — without it zsh breaks Cyrillic.
            env.insert("LANG".to_string(), "en_US.UTF-8".to_string());
        }
        let options = tty::Options {
            shell: Some(tty::Shell::new("/bin/zsh".into(), args.clone())),
            working_directory: Some(cwd.clone()),
            drain_on_exit: false,
            env,
        };
        let pty = match tty::new(&options, self.window_size(size), 0) {
            Ok(pty) => pty,
            Err(error) => {
                write_note(&term, &tr!("terminal.couldn_t_start_terminal_error", error = error));
                return;
            }
        };
        let pid = Some(pty.child().id());
        let event_loop = match EventLoop::new(term.clone(), listener, pty, false, false) {
            Ok(event_loop) => event_loop,
            Err(error) => {
                write_note(&term, &tr!("terminal.couldn_t_start_terminal_error", error = error));
                return;
            }
        };
        let pty = event_loop.channel();
        event_loop.spawn();

        let session_id = id.clone();
        let events = cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let alive = this
                    .update(cx, |this, cx| this.on_term_event(&session_id, event, cx))
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        self.sessions.insert(
            id,
            Session {
                cwd,
                args,
                pid,
                term,
                pty,
                size,
                exited: false,
                opened: std::time::Instant::now(),
                _events: events,
            },
        );
    }

    fn on_term_event(&mut self, id: &str, event: TermEvent, cx: &mut Context<Self>) {
        // The key the process was started with may have changed since (`rekey`).
        let mut id = id.to_string();
        for _ in 0..4 {
            match self.aliases.get(&id) {
                Some(next) if !self.sessions.contains_key(&id) => id = next.clone(),
                _ => break,
            }
        }
        let id = id.as_str();
        let reported_size = self
            .sessions
            .get(id)
            .map(|session| self.window_size(session.size));
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        match event {
            TermEvent::Wakeup => {
                if let Some(key) = self.trace_key {
                    trace(&format!("  pty output +{} ms", key.elapsed().as_millis()));
                }
            }
            TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => {}
            TermEvent::PtyWrite(text) => session.write(text.into_bytes()),
            TermEvent::ClipboardStore(_, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text))
            }
            // A program asking to read the clipboard (OSC 52) is never answered: anything printed in the
            // terminal — a file shown with `cat`, a log — could ask, and the clipboard may hold a password.
            // Copying (`ClipboardStore`) still works.
            TermEvent::ClipboardLoad(..) => return,
            TermEvent::TextAreaSizeRequest(format) => {
                if let Some(size) = reported_size {
                    session.write(format(size).into_bytes());
                }
            }
            TermEvent::ColorRequest(index, format) => {
                let color = session.term.lock().colors()[index].unwrap_or_else(|| {
                    let value = default_color(index);
                    alacritty_terminal::vte::ansi::Rgb {
                        r: (value >> 16) as u8,
                        g: (value >> 8) as u8,
                        b: value as u8,
                    }
                });
                session.write(format(color).into_bytes());
            }
            TermEvent::ChildExit(_) | TermEvent::Exit => {
                if !session.exited {
                    session.exited = true;
                    write_note(
                        &session.term,
                        t("terminal.disconnected_from_claude_session_press"),
                    );
                }
            }
            TermEvent::Title(_) | TermEvent::ResetTitle | TermEvent::Bell => return,
        }
        if self.current.as_deref() == Some(id) {
            cx.notify();
        }
    }

    fn current_session(&self) -> Option<&Session> {
        self.sessions.get(self.current.as_ref()?)
    }

    /// Fit the grid and PTY to the terminal area at once, on every change (a window resized, a divider
    /// dragged, a panel shown or hidden, zoom) — as iTerm2 does. Holding the size back during a drag
    /// would make the terminal lag behind the window.
    fn sync_size(&mut self) {
        let size = self.grid_size();
        let window_size = self.window_size(size);
        let Some(session) = self.current.as_ref().and_then(|id| self.sessions.get_mut(id)) else {
            return;
        };
        if session.size != size {
            session.size = size;
            session.term.lock().resize(size);
            let _ = session.pty.send(Msg::Resize(window_size));
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        trace(&format!("key {:?}", event.keystroke.key));
        self.trace_key = Some(now);
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let Some(id) = self.current.clone() else {
            return;
        };

        if modifiers.platform {
            match keystroke.key.as_str() {
                "c" => {
                    let text = self
                        .sessions
                        .get(&id)
                        .and_then(|session| session.term.lock().selection_to_string());
                    if let Some(text) = text.filter(|text| !text.is_empty()) {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        cx.stop_propagation();
                    }
                }
                "v" if self.paste(cx) => cx.stop_propagation(),
                _ => {}
            }
            return;
        }

        if self.sessions.get(&id).is_some_and(|session| session.exited) {
            if keystroke.key == "enter" {
                if let Some(session) = self.sessions.remove(&id) {
                    let (cwd, args) = (session.cwd.clone(), session.args.clone());
                    drop(session);
                    self.spawn(id, cwd, args, cx);
                    cx.notify();
                }
                cx.stop_propagation();
            }
            return;
        }

        let Some(session) = self.sessions.get(&id) else {
            return;
        };
        if keystroke.key == "enter" && !modifiers.shift && !modifiers.alt && !modifiers.control
            && let Some(command) = session_switch_command(&session.term.lock())
        {
            cx.emit(ClearBlocked(command));
            cx.stop_propagation();
            return;
        }
        let mode = *session.term.lock().mode();
        let Some(bytes) = key_bytes(event, mode) else {
            return;
        };
        session.write(bytes);
        self.after_input(&id, cx);
        cx.stop_propagation();
    }

    /// A key that GPUI intercepts with a binding (Tab) — straight to the program in the terminal.
    fn send_key(&mut self, bytes: &'static [u8], cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else {
            return;
        };
        if let Some(session) = self.sessions.get(&id).filter(|session| !session.exited) {
            session.write(bytes);
            self.after_input(&id, cx);
        }
    }

    /// Input scrolls back to the bottom and clears the selection — as in iTerm2.
    fn after_input(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(session) = self.sessions.get(id) {
            let mut term = session.term.lock();
            term.selection = None;
            term.scroll_display(Scroll::Bottom);
        }
        cx.notify();
    }

    /// Grid cell under the pointer.
    fn point_at(&self, position: gpui_kit::Point<Pixels>, display_offset: usize) -> (Point, Side) {
        let (cell_width, line_height) = self.cell();
        let origin = self.bounds.get().map(|bounds| bounds.origin).unwrap_or_default();
        let size = self.grid_size();
        let x = (f32::from(position.x) - f32::from(origin.x) - profile::SIDE_MARGIN).max(0.);
        let y = (f32::from(position.y) - f32::from(origin.y) - profile::TOP_BOTTOM_MARGIN).max(0.);
        let column = ((x / cell_width) as usize).min(size.columns - 1);
        let row = ((y / line_height) as usize).min(size.lines - 1);
        let side = if x % cell_width > cell_width / 2. {
            Side::Right
        } else {
            Side::Left
        };
        (
            Point::new(Line(row as i32 - display_offset as i32), Column(column)),
            side,
        )
    }

    /// Screen cell (column, row from 1) under the pointer — for mouse reports to the program.
    fn report_cell(&self, position: gpui_kit::Point<Pixels>, display_offset: usize) -> (usize, usize) {
        let (point, _) = self.point_at(position, display_offset);
        (point.column.0 + 1, (point.line.0 + display_offset as i32).max(0) as usize + 1)
    }

    /// Mouse report in the format the program enabled (SGR or legacy X10).
    fn mouse_report(mode: TermMode, button: usize, (column, row): (usize, usize), release: bool) -> Vec<u8> {
        if mode.contains(TermMode::SGR_MOUSE) {
            format!("\x1b[<{button};{column};{row}{}", if release { 'm' } else { 'M' }).into_bytes()
        } else {
            let encode = |value: usize| (32 + value).min(255) as u8;
            let button = if release { 3 } else { button };
            vec![0x1b, b'[', b'M', encode(button), encode(column), encode(row)]
        }
    }

    fn modifier_bits(modifiers: gpui_kit::Modifiers) -> usize {
        4 * usize::from(modifiers.shift) + 8 * usize::from(modifiers.alt) + 16 * usize::from(modifiers.control)
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus(window, cx);
        // ⌘+click on a link opens it in the browser, as in iTerm2. Such a click is not passed to the program.
        if event.modifiers.platform {
            self.update_hovered_link(event.position, true, cx);
            if let Some(link) = self.hovered_link.clone() {
                cx.open_url(&link.url);
                return;
            }
        }
        let Some(session) = self.current_session() else {
            return;
        };
        // The program wants the mouse (Claude Code in full-screen mode) — as in iTerm2: the press goes to it,
        // and it does selection and copying itself (with a "copied to clipboard" notice).
        // With ⌥ — the terminal's own selection, as Claude Code hints ("hold ⌥ … for native copy").
        let mode = *session.term.lock().mode();
        if mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.alt {
            let display_offset = session.term.lock().grid().display_offset();
            let cell = self.report_cell(event.position, display_offset);
            let button = Self::modifier_bits(event.modifiers);
            session.write(Self::mouse_report(mode, button, cell, false));
            if let Some(session) = self.current_session() {
                session.term.lock().selection = None;
            }
            self.reporting = Some(cell);
            cx.notify();
            return;
        }
        let mut term = session.term.lock();
        let (point, side) = self.point_at(event.position, term.grid().display_offset());
        let kind = match event.click_count {
            2 => SelectionType::Semantic,
            3.. => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        term.selection = Some(Selection::new(kind, point, side));
        drop(term);
        self.selecting = true;
        cx.notify();
    }

    /// Find the link under the pointer (only with ⌘ held) and redraw the underline.
    fn update_hovered_link(&mut self, position: gpui_kit::Point<Pixels>, command: bool, cx: &mut Context<Self>) {
        self.last_mouse = Some(position);
        let link = command
            .then(|| {
                let session = self.current_session()?;
                let term = session.term.lock();
                let (point, _) = self.point_at(position, term.grid().display_offset());
                link_at(&term, point)
            })
            .flatten();
        if link != self.hovered_link {
            self.hovered_link = link;
            cx.notify();
        }
    }

    fn on_modifiers_changed(&mut self, event: &ModifiersChangedEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(position) = self.last_mouse {
            self.update_hovered_link(position, event.modifiers.platform, cx);
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.update_hovered_link(event.position, event.modifiers.platform, cx);
        if let Some(last) = self.reporting {
            if event.pressed_button != Some(MouseButton::Left) {
                return;
            }
            let Some(session) = self.current_session() else {
                return;
            };
            let mode = *session.term.lock().mode();
            if !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
                return;
            }
            let display_offset = session.term.lock().grid().display_offset();
            let cell = self.report_cell(event.position, display_offset);
            if cell != last {
                session.write(Self::mouse_report(mode, 32 + Self::modifier_bits(event.modifiers), cell, false));
                self.reporting = Some(cell);
            }
            return;
        }
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        let Some(session) = self.current_session() else {
            return;
        };
        let mut term = session.term.lock();
        let (point, side) = self.point_at(event.position, term.grid().display_offset());
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, side);
        }
        drop(term);
        cx.notify();
    }

    /// As in iTerm2 ("Copy to pasteboard on selection", on by default): once the button is released,
    /// the selection is already on the clipboard. A click without dragging (empty selection) leaves the clipboard alone.
    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.reporting.take().is_some() {
            if let Some(session) = self.current_session() {
                let mode = *session.term.lock().mode();
                let display_offset = session.term.lock().grid().display_offset();
                let cell = self.report_cell(event.position, display_offset);
                session.write(Self::mouse_report(mode, Self::modifier_bits(event.modifiers), cell, true));
            }
            return;
        }
        if !std::mem::take(&mut self.selecting) {
            return;
        }
        self.copy_selection(cx);
    }

    /// Copy the selected text to the clipboard. `true` if there was something to copy.
    /// Paste text from the clipboard (⌘V and Edit → Paste): with bracketed paste if the program asks for it.
    fn paste(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.current.clone() else {
            return false;
        };
        let text = cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default();
        let Some(session) = self.sessions.get(&id) else {
            return false;
        };
        let bracketed = session.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bytes = if bracketed { format!("\x1b[200~{text}\x1b[201~") } else { text };
        session.write(bytes.into_bytes());
        self.after_input(&id, cx);
        true
    }

    /// Files dropped: their paths, shell-escaped and space-separated, as a paste (iTerm2 does
    /// the same). Claude Code turns an image path into an "[Image #1]" attachment.
    fn drop_paths(&mut self, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else {
            return;
        };
        let Some(session) = self.sessions.get(&id).filter(|session| !session.exited) else {
            return;
        };
        let text = paths.iter().map(|path| shell_escape(&path.to_string_lossy())).collect::<Vec<_>>().join(" ") + " ";
        let bracketed = session.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        let bytes = if bracketed { format!("\x1b[200~{text}\x1b[201~") } else { text };
        session.write(bytes.into_bytes());
        self.after_input(&id, cx);
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let text = self
            .current_session()
            .and_then(|session| session.term.lock().selection_to_string())
            .filter(|text| !text.trim().is_empty());
        match text {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                true
            }
            None => false,
        }
    }

    /// The wheel scrolls history; in full-screen programs (vim, less) — via arrow keys, as in iTerm2.
    fn on_scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (_, line_height) = self.cell();
        let delta = f32::from(event.delta.pixel_delta(px(line_height)).y) / line_height;
        self.scroll_remainder += delta;
        let lines = self.scroll_remainder.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_remainder -= lines as f32;
        let Some(session) = self.current_session() else {
            return;
        };
        let mode = *session.term.lock().mode();
        if mode.intersects(TermMode::MOUSE_MODE) {
            // The program wants mouse events (Claude Code, vim with mouse=a): the wheel is buttons 64/65,
            // as iTerm2 sends. Arrows stay with the keyboard: in Claude Code they scroll the input history.
            let display_offset = session.term.lock().grid().display_offset();
            let cell = self.report_cell(event.position, display_offset);
            let button = if lines > 0 { 64 } else { 65 } + Self::modifier_bits(event.modifiers);
            session.write(Self::mouse_report(mode, button, cell, false).repeat(lines.unsigned_abs() as usize));
        } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            let key: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            session.write(key.repeat(lines.unsigned_abs() as usize));
        } else {
            session.term.lock().scroll_display(Scroll::Delta(lines));
        }
        cx.notify();
    }

    /// Trackpad pinch zooms smoothly; the rounded size is remembered when the gesture ends.
    fn on_pinch(&mut self, event: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.phase {
            TouchPhase::Ended | TouchPhase::Cancelled => {
                self.set_font_size(self.font_size.round(), cx);
                cx.emit(FontSizeChanged(self.font_size));
            }
            _ => {
                self.font_size = (self.font_size * (1. + event.delta))
                    .clamp(profile::MIN_FONT_SIZE, profile::MAX_FONT_SIZE);
                cx.notify();
            }
        }
    }
}

/// A path for the shell, as iTerm2 pastes it on drag and drop: special characters escaped with `\`.
fn shell_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for char in path.chars() {
        if char.is_whitespace() || "\\'\"`$&|;<>()[]{}*?!#~^".contains(char) {
            out.push('\\');
        }
        out.push(char);
    }
    out
}

/// Byte sequence for a key, as iTerm2 sends it (xterm).
fn key_bytes(event: &KeyDownEvent, mode: TermMode) -> Option<Vec<u8>> {
    let keystroke = &event.keystroke;
    let modifiers = keystroke.modifiers;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    // xterm modifier: 1 + Shift + 2·Option + 4·Control.
    let code = 1
        + u8::from(modifiers.shift)
        + 2 * u8::from(modifiers.alt)
        + 4 * u8::from(modifiers.control);
    let cursor = |letter: char| -> Vec<u8> {
        if code > 1 {
            format!("\x1b[1;{code}{letter}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let tilde = |number: u8| -> Vec<u8> {
        if code > 1 {
            format!("\x1b[{number};{code}~").into_bytes()
        } else {
            format!("\x1b[{number}~").into_bytes()
        }
    };
    let bytes = match keystroke.key.as_str() {
        // Shift+Enter is a new line in Claude Code, as in iTerm2: to a program that enabled the kitty keyboard
        // protocol — CSI 13;<mod>u, otherwise ESC CR (Option+Enter sends that too; Claude Code reads it as a
        // line break). Plain Enter is CR.
        "enter" if code > 1 && mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) => format!("\x1b[13;{code}u").into_bytes(),
        "enter" if modifiers.shift || modifiers.alt => b"\x1b\r".to_vec(),
        "enter" => b"\r".to_vec(),
        "backspace" if modifiers.alt => b"\x1b\x7f".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "tab" if modifiers.shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "up" => cursor('A'),
        "down" => cursor('B'),
        "right" => cursor('C'),
        "left" => cursor('D'),
        "home" => cursor('H'),
        "end" => cursor('F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        "space" if modifiers.control => vec![0],
        key if modifiers.control && key.chars().count() == 1 => {
            let character = key.chars().next()?.to_ascii_lowercase();
            match character {
                'a'..='z' => vec![character as u8 & 0x1f],
                '[' => vec![0x1b],
                '\\' => vec![0x1c],
                ']' => vec![0x1d],
                '/' | '-' => vec![0x1f],
                _ => return None,
            }
        }
        _ => {
            let text = keystroke.key_char.as_ref()?;
            if text.chars().any(char::is_control) {
                return None;
            }
            text.as_bytes().to_vec()
        }
    };
    Some(bytes)
}

/// Text in the Claude Code input line (the line under the cursor without ">" and the frame).
fn input_line(term: &Term<Listener>) -> String {
    let cursor = term.grid().cursor.point;
    let row = &term.grid()[cursor.line];
    let line: String = (0..term.columns()).map(|column| row[Column(column)].c).collect();
    line.trim_matches(|char: char| char.is_whitespace() || char == '│' || char == '\0')
        .trim_start_matches(['>', '❯', '›'])
        .trim()
        .to_string()
}

/// The input line is empty: nothing under the cursor, and no text on the line above it either (a command whose
/// Enter became a line break leaves the cursor on an empty second line).
fn input_is_empty(term: &Term<Listener>) -> bool {
    input_line(term).is_empty() && prompt_text_above(term).is_none()
}

/// Text of the input row right above the cursor ("❯ /rename x"), if that row is an input row.
fn prompt_text_above(term: &Term<Listener>) -> Option<String> {
    let cursor = term.grid().cursor.point;
    if cursor.line.0 <= 0 {
        return None;
    }
    let above = cursor.line - 1i32;
    let row = &term.grid()[above];
    let line: String = (0..term.columns()).map(|column| row[Column(column)].c).collect();
    let line = line.trim_matches(|char: char| char.is_whitespace() || char == '│' || char == '\0');
    let text = line.strip_prefix(['>', '❯', '›'])?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// How many Backspaces take `command` out of the input: its length when it is under the cursor, one more when its
/// Enter turned into a line break and the cursor sits on the empty line below it. 0 — it isn't there.
fn typed_left(term: &Term<Listener>, command: &str) -> usize {
    let length = command.chars().count();
    let here = input_line(term);
    if here == command {
        return length;
    }
    if here.is_empty() && prompt_text_above(term).as_deref() == Some(command) {
        return length + 1;
    }
    0
}

/// A command typed in the input line that would start a new session instead of the pinned one.
/// The Claude Code input line is the line under the cursor: "> /clear" (sometimes framed with "│").
fn session_switch_command(term: &Term<Listener>) -> Option<String> {
    let input = input_line(term);
    let command = input.split_whitespace().next()?;
    matches!(command, "/clear" | "/reset" | "/new").then(|| command.to_string())
}

/// A Pult service line written straight to the terminal screen (not to the shell).
fn write_note(term: &Arc<FairMutex<Term<Listener>>>, text: &str) {
    Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new()
        .advance(&mut *term.lock(), format!("\r\n{text}\r\n").as_bytes());
}

fn default_color(index: usize) -> u32 {
    match index {
        0..=15 => profile::ANSI[index],
        16..=231 => {
            let levels = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
            let value = index - 16;
            (levels[value / 36] << 16) | (levels[(value / 6) % 6] << 8) | levels[value % 6]
        }
        232..=255 => {
            let level = (8 + 10 * (index - 232)) as u32;
            (level << 16) | (level << 8) | level
        }
        _ if index == NamedColor::Background as usize => profile::BACKGROUND,
        _ if index == NamedColor::Cursor as usize => profile::CURSOR,
        _ if index == NamedColor::BrightForeground as usize => profile::BOLD,
        _ => profile::FOREGROUND,
    }
}

/// Cell color: first what the program set (OSC 4), then the iTerm2 profile.
fn resolve(color: Color, colors: &Colors, bold: bool) -> u32 {
    let index = match color {
        Color::Spec(rgb) => return (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b),
        Color::Indexed(index) => usize::from(index),
        Color::Named(named) => {
            let index = named as usize;
            match named {
                // "Use Bright Bold" in the profile: bold text in normal colors is drawn bright.
                _ if bold && index < 8 => index + 8,
                NamedColor::Foreground if bold => NamedColor::BrightForeground as usize,
                NamedColor::DimForeground => NamedColor::Foreground as usize,
                _ if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize)
                    .contains(&index) =>
                {
                    index - NamedColor::DimBlack as usize
                }
                _ => index,
            }
        }
    };
    colors[index]
        .map(|rgb| (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b))
        .unwrap_or_else(|| default_color(index))
}

fn dim(color: u32) -> u32 {
    let channel = |shift: u32| (((color >> shift) & 0xff) * 2 / 3) << shift;
    channel(16) | channel(8) | channel(0)
}

#[derive(Clone, Copy, PartialEq)]
struct CellStyle {
    fg: u32,
    bg: Option<u32>,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

impl Focusable for Terminal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Terminal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = std::time::Instant::now();
        if let Some(key) = self.trace_key {
            let gap = self.trace_frame.map(|frame| now.duration_since(frame).as_millis()).unwrap_or(0);
            trace(&format!("  render +{} ms (previous frame {} ms ago)", key.elapsed().as_millis(), gap));
        }
        self.trace_frame = Some(now);
        self.sync_size();
        let focused = self.focus_handle.is_focused(window);
        let (cell_width, line_height) = self.cell();
        let font_size = self.font_size;
        let base_font = font(profile::FONT);

        // Screen lines: text + style runs, and the cursor for the unfocused outline.
        let mut rows: Vec<(String, Vec<TextRun>)> = Vec::new();
        let mut hollow_cursor = None;
        let hovered_link = self.hovered_link.as_ref();
        let pointer = hovered_link.is_some();
        if let Some(session) = self.current_session() {
            let term = session.term.lock();
            let content = term.renderable_content();
            let selection = content.selection;
            let cursor = content.cursor;
            let show_cursor = content.mode.contains(TermMode::SHOW_CURSOR);
            let display_offset = content.display_offset as i32;
            let colors = content.colors;
            let mut current_line = None;
            let mut runs_style: Vec<CellStyle> = Vec::new();
            let mut run_lengths: Vec<usize> = Vec::new();
            let mut text = String::new();
            let flush = |text: &mut String,
                             styles: &mut Vec<CellStyle>,
                             lengths: &mut Vec<usize>,
                             rows: &mut Vec<(String, Vec<TextRun>)>| {
                let runs = styles
                    .drain(..)
                    .zip(lengths.drain(..))
                    .map(|(style, len)| TextRun {
                        len,
                        font: gpui_kit::Font {
                            weight: if style.bold { FontWeight::BOLD } else { FontWeight::NORMAL },
                            style: if style.italic { FontStyle::Italic } else { FontStyle::Normal },
                            ..base_font.clone()
                        },
                        color: Hsla::from(rgb(style.fg)),
                        background_color: style.bg.map(|bg| Hsla::from(rgb(bg))),
                        underline: style.underline.then(|| UnderlineStyle {
                            thickness: px(1.),
                            color: Some(Hsla::from(rgb(style.fg))),
                            wavy: false,
                        }),
                        strikethrough: style.strike.then(|| StrikethroughStyle {
                            thickness: px(1.),
                            color: Some(Hsla::from(rgb(style.fg))),
                        }),
                    })
                    .collect();
                rows.push((std::mem::take(text), runs));
            };
            for indexed in content.display_iter {
                if current_line != Some(indexed.point.line) {
                    if current_line.is_some() {
                        flush(&mut text, &mut runs_style, &mut run_lengths, &mut rows);
                    }
                    current_line = Some(indexed.point.line);
                }
                let cell = indexed.cell;
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let bold = cell.flags.contains(Flags::BOLD);
                let mut fg = resolve(cell.fg, colors, bold);
                let mut bg = match cell.bg {
                    Color::Named(NamedColor::Background) => None,
                    other => Some(resolve(other, colors, false)),
                };
                if cell.flags.contains(Flags::DIM) {
                    fg = dim(fg);
                }
                if cell.flags.contains(Flags::INVERSE) {
                    let back = bg.unwrap_or(profile::BACKGROUND);
                    bg = Some(fg);
                    fg = back;
                }
                if cell.flags.contains(Flags::HIDDEN) {
                    fg = bg.unwrap_or(profile::BACKGROUND);
                }
                let point = indexed.point;
                let in_link = hovered_link.is_some_and(|link| link.cells.contains(&point));
                if selection.is_some_and(|range| range.contains(point)) {
                    fg = profile::SELECTED_TEXT;
                    bg = Some(profile::SELECTION);
                }
                let at_cursor = show_cursor && point == cursor.point;
                if at_cursor {
                    if focused {
                        fg = profile::CURSOR_TEXT;
                        bg = Some(profile::CURSOR);
                    } else {
                        let row = point.line.0 + display_offset;
                        hollow_cursor = Some((point.column.0, row));
                    }
                }
                let style = CellStyle {
                    fg,
                    bg,
                    bold,
                    italic: cell.flags.contains(Flags::ITALIC),
                    underline: cell.flags.intersects(Flags::ALL_UNDERLINES) || in_link,
                    strike: cell.flags.contains(Flags::STRIKEOUT),
                };
                let before = text.len();
                text.push(if cell.c == '\0' { ' ' } else { cell.c });
                if let Some(extra) = cell.zerowidth() {
                    text.extend(extra.iter());
                }
                let len = text.len() - before;
                match runs_style.last() {
                    Some(last) if *last == style => *run_lengths.last_mut().unwrap() += len,
                    _ => {
                        runs_style.push(style);
                        run_lengths.push(len);
                    }
                }
            }
            if current_line.is_some() {
                flush(&mut text, &mut runs_style, &mut run_lengths, &mut rows);
            }
        }

        let bounds = self.bounds.clone();
        let cursor_color = Hsla::from(rgb(profile::CURSOR));
        div()
            .id("terminal")
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(|this, _: &crate::actions::TerminalTab, _, cx| this.send_key(b"\t", cx)))
            .on_action(cx.listener(|this, _: &crate::actions::TerminalCopy, _, cx| {
                this.copy_selection(cx);
            }))
            // Edit → Copy / Paste from the menu.
            .on_action(cx.listener(|this, _: &gpui_kit::component::input::Copy, _, cx| {
                this.copy_selection(cx);
            }))
            .on_action(cx.listener(|this, _: &gpui_kit::component::input::Paste, _, cx| {
                this.paste(cx);
            }))
            .on_action(cx.listener(|this, _: &crate::actions::TerminalBackTab, _, cx| this.send_key(b"\x1b[Z", cx)))
            // Files from Finder: paths into the input line, as in iTerm2 (Claude Code recognizes images in them).
            .on_drop(cx.listener(|this, paths: &gpui_kit::ExternalPaths, window, cx| {
                this.drop_paths(paths.paths(), cx);
                this.focus_handle.focus(window, cx);
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .when(pointer, |this| this.cursor_pointer())
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll(cx.listener(Self::on_scroll))
            .on_pinch(cx.listener(Self::on_pinch))
            .relative()
            .size_full()
            .overflow_hidden()
            .px(px(profile::SIDE_MARGIN))
            .py(px(profile::TOP_BOTTOM_MARGIN))
            .bg(Hsla::from(rgb(profile::BACKGROUND)))
            .text_color(Hsla::from(rgb(profile::FOREGROUND)))
            .font_family(profile::FONT)
            .text_size(px(font_size))
            .line_height(px(line_height))
            .child(
                canvas(
                    move |area, window, _| {
                        if bounds.get() != Some(area) {
                            bounds.set(Some(area));
                            window.refresh();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .children(rows.into_iter().map(move |(text, runs)| {
                div()
                    .h(px(line_height))
                    .whitespace_nowrap()
                    .child(StyledText::new(SharedString::from(text)).with_runs(runs))
            }))
            .when_some(hollow_cursor, |this, (column, row)| {
                this.child(
                    div()
                        .absolute()
                        .left(px(profile::SIDE_MARGIN + column as f32 * cell_width))
                        .top(px(profile::TOP_BOTTOM_MARGIN + row as f32 * line_height))
                        .w(px(cell_width))
                        .h(px(line_height))
                        .border_1()
                        .border_color(cursor_color),
                )
            })
    }
}

/// Placeholder in place of the terminal: a title, an explanation and up to two buttons (the last is the main
/// action). Colors come from the terminal profile: the placeholder sits on its background in any window theme.
pub fn placeholder(
    title: impl Into<SharedString>,
    text: impl Into<SharedString>,
    buttons: Vec<Button>,
    cx: &App,
) -> AnyElement {
    placeholder_with(title, text, buttons, false, cx)
}

/// "Claude Code not found" placeholder: an install link (an external resource — `Link`) and check again.
pub fn placeholder_no_claude(check: Button, cx: &App) -> AnyElement {
    v_flex()
        .id("terminal-placeholder")
        .size_full()
        .items_center()
        .justify_center()
        .gap_3()
        .px_6()
        .bg(Hsla::from(rgb(profile::BACKGROUND)))
        .font_family(cx.theme().font_family.clone())
        .text_size(px(14.))
        .line_height(px(20.))
        .child(div().font_medium().text_color(Hsla::from(rgb(profile::EMPTY_TITLE))).child(t("terminal.claude_code_not_found")))
        .child(
            div()
                .max_w(px(420.))
                .text_center()
                .text_color(Hsla::from(rgb(profile::EMPTY_HINT)))
                .child(t("terminal.pult_runs_sessions_through_claude")),
        )
        .child(
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    gpui_kit::component::link::Link::new("claude-install")
                        .href("https://docs.claude.com/en/docs/claude-code/setup")
                        // The theme's link color is unreadable on the dark terminal background — use our own, underline too.
                        .text_color(Hsla::from(rgb(profile::EMPTY_TITLE)))
                        .text_decoration_1()
                        .text_decoration_color(Hsla::from(rgb(profile::EMPTY_HINT)))
                        .child(t("terminal.how_to_install")),
                )
                .child(
                    check
                        .outline()
                        .bg(gpui_kit::transparent_black())
                        .border_color(Hsla::from(rgb(profile::EMPTY_BUTTON_BORDER)))
                        .text_color(Hsla::from(rgb(profile::EMPTY_TITLE))),
                ),
        )
        .into_any_element()
}

/// Placeholder; `busy` — work in progress (starting, moving): a spinner above the title instead of an ellipsis.
pub fn placeholder_with(
    title: impl Into<SharedString>,
    text: impl Into<SharedString>,
    buttons: Vec<Button>,
    busy: bool,
    cx: &App,
) -> AnyElement {
    v_flex()
        .id("terminal-placeholder")
        .size_full()
        .items_center()
        .justify_center()
        .gap_3()
        .px_6()
        .bg(Hsla::from(rgb(profile::BACKGROUND)))
        .font_family(cx.theme().font_family.clone())
        .text_size(px(14.))
        .line_height(px(20.))
        .when(busy, |this| {
            this.child(gpui_kit::component::spinner::Spinner::new().color(Hsla::from(rgb(profile::EMPTY_HINT))))
        })
        .child(
            div()
                .font_medium()
                .text_color(Hsla::from(rgb(profile::EMPTY_TITLE)))
                .child(title.into()),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_center()
                .text_color(Hsla::from(rgb(profile::EMPTY_HINT)))
                .child(text.into()),
        )
        .when(!buttons.is_empty(), |this| {
            this.child(h_flex().gap_2().children(buttons.into_iter().map(|button| {
                button
                    .outline()
                    .bg(gpui_kit::transparent_black())
                    .border_color(Hsla::from(rgb(profile::EMPTY_BUTTON_BORDER)))
                    .text_color(Hsla::from(rgb(profile::EMPTY_TITLE)))
            })))
        })
        .into_any_element()
}

/// A step of a session move shown in its placeholder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Done,
    Now,
    Wait,
    /// Not needed this time (the label says why).
    Skip,
}

/// Placeholder with the steps of a long operation (a session moving from another computer).
pub fn placeholder_steps(
    title: impl Into<SharedString>,
    text: Option<SharedString>,
    steps: Vec<(SharedString, Step)>,
    buttons: Vec<Button>,
    cx: &App,
) -> AnyElement {
    use gpui_kit::component::Sizable as _;
    let bright = Hsla::from(rgb(profile::EMPTY_TITLE));
    let dim = Hsla::from(rgb(profile::EMPTY_HINT));
    v_flex()
        .id("terminal-placeholder")
        .size_full()
        .items_center()
        .justify_center()
        .gap_3()
        .px_6()
        .bg(Hsla::from(rgb(profile::BACKGROUND)))
        .font_family(cx.theme().font_family.clone())
        .text_size(px(14.))
        .line_height(px(20.))
        .child(div().font_medium().text_color(bright).child(title.into()))
        .when_some(text, |this, text| this.child(div().max_w(px(420.)).text_center().text_color(dim).child(text)))
        .child(
            v_flex().max_w(px(420.)).gap_2().children(steps.into_iter().map(|(label, step)| {
                let icon = match step {
                    Step::Done => gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Check).small().text_color(bright).into_any_element(),
                    Step::Now => gpui_kit::component::spinner::Spinner::new().small().color(bright).into_any_element(),
                    Step::Wait => gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Circle).small().text_color(dim).into_any_element(),
                    Step::Skip => gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Minus).small().text_color(dim).into_any_element(),
                };
                let color = if matches!(step, Step::Done | Step::Now) { bright } else { dim };
                h_flex()
                    .items_start()
                    .gap_2()
                    .text_color(color)
                    .child(div().mt_0p5().flex_shrink_0().child(icon))
                    .child(div().min_w_0().child(label))
            })),
        )
        .when(!buttons.is_empty(), |this| {
            this.child(h_flex().gap_2().children(buttons.into_iter().map(|button| {
                button
                    .outline()
                    .bg(gpui_kit::transparent_black())
                    .border_color(Hsla::from(rgb(profile::EMPTY_BUTTON_BORDER)))
                    .text_color(bright)
            })))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_paths_are_escaped_like_iterm2() {
        assert_eq!(shell_escape("/tmp/Écran partagé (2).png"), "/tmp/Écran\\ partagé\\ \\(2\\).png");
        assert_eq!(shell_escape("/tmp/plain.jpg"), "/tmp/plain.jpg");
        assert_eq!(shell_escape("/tmp/it's & $x"), "/tmp/it\\'s\\ \\&\\ \\$x");
    }

    fn term_with(text: &str, columns: usize) -> Term<Listener> {
        let (sender, _) = mpsc::unbounded();
        let size = GridSize { columns, lines: 4 };
        let mut term = Term::new(Config::default(), &size, Listener(sender));
        Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut term, text.as_bytes());
        term
    }

    #[test]
    fn a_command_left_in_the_input_is_found() {
        // Under the cursor: as typed.
        let term = term_with("─── name ─\r\n❯ /rename a-b", 40);
        assert!(!input_is_empty(&term));
        assert_eq!(typed_left(&term, "/rename a-b"), 11);
        // Enter became a line break: the cursor is on an empty line under the command.
        let term = term_with("─── name ─\r\n❯ /rename a-b\r\n", 40);
        assert!(!input_is_empty(&term));
        assert_eq!(typed_left(&term, "/rename a-b"), 12);
        // Someone else's text is never erased.
        assert_eq!(typed_left(&term, "/rename other"), 0);
        // An empty input under the frame line.
        let term = term_with("─── name ─\r\n❯ ", 40);
        assert!(input_is_empty(&term));
        assert_eq!(typed_left(&term, "/rename a-b"), 0);
    }

    #[test]
    fn finds_plain_url() {
        let term = term_with("see https://github.com/x/y. ok", 60);
        let link = link_at(&term, Point::new(Line(0), Column(10))).unwrap();
        assert_eq!(link.url, "https://github.com/x/y");
        assert!(link_at(&term, Point::new(Line(0), Column(1))).is_none());
    }

    #[test]
    fn finds_wrapped_url() {
        let term = term_with("go https://example.com/very/long/path", 20);
        let link = link_at(&term, Point::new(Line(1), Column(3))).unwrap();
        assert_eq!(link.url, "https://example.com/very/long/path");
    }

    #[test]
    fn finds_osc8_link() {
        let term = term_with("\x1b]8;;https://claude.ai/x\x1b\\label\x1b]8;;\x1b\\ tail", 40);
        let link = link_at(&term, Point::new(Line(0), Column(2))).unwrap();
        assert_eq!(link.url, "https://claude.ai/x");
        assert_eq!(link.cells.len(), 5);
    }
}
