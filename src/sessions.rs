//! Claude Code sessions: each Pult project has one pinned to it.
//!
//! Pult doesn't store transcripts itself and doesn't parse their format to work — everything goes through
//! the Claude Code CLI:
//! - `claude agents --json --all` — which sessions are running now and what they do (background ones and
//!   those open in other windows); `claude --bg -n <project>` — a new background session,
//!   `claude --resume <id> --bg` — start the pinned one from the same place, `claude stop <id>` — pause
//!   (the transcript stays on disk); `claude attach <id>` in Pult's terminal — attach to a background session.
//!
//! Background sessions are held by the Claude Code supervisor, not by Pult: close Pult and Claude keeps working.
//! Pult starts the supervisor itself, as a launchd job (`ensure_daemon`): otherwise macOS attributes it to
//! Pult, and after Pult quits a ghost of the app stays in the Dock.
//! The list of a folder's transcripts comes from the file names `~/.claude/projects/<folder>/<id>.jsonl`
//! (this location is documented); the title from the contents is best effort.

use std::{
    collections::HashMap,
    fs,
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant, SystemTime},
};

use serde::Deserialize;

use crate::i18n::t;

/// What the session is doing now — for the icon in the project list and the "Session" section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Claude is answering or running a command.
    Working,
    /// Claude asked a question or is waiting for permission.
    Waiting,
    /// The turn is over; the session is waiting for the next message.
    Ready,
    /// No process: paused, unloaded when idle, or after a reboot. The transcript is on disk.
    Stopped,
}

/// A line of `claude agents --json`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Agent {
    /// The background session's short id — used by `attach` and `stop`. Sessions open in a window have none.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "sessionId", default)]
    pub session_id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub status: Option<String>,
    /// State at the supervisor: `stopped` — stopped (`claude stop`, pause); anything else without `pid` —
    /// unloaded when idle but alive: `attach` will start it.
    #[serde(default)]
    pub state: Option<String>,
    /// Session name (`-n` / `/rename`).
    #[serde(default)]
    pub name: Option<String>,
    /// The session's working folder.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// What the session waits for when its status is `waiting` (`input needed`…), from the live registry.
    #[serde(skip)]
    pub waiting_for: Option<String>,
}

impl Agent {
    pub fn is_background(&self) -> bool {
        self.kind == "background"
    }

    pub fn is_running(&self) -> bool {
        self.pid.is_some()
    }

    /// Stopped by a pause (not just unloaded when idle), or ended (`done`): `attach` doesn't bring an ended one
    /// back, Continue restarts it (`resume` → `respawn`).
    pub fn is_stopped(&self) -> bool {
        !self.is_running() && matches!(self.state.as_deref(), Some("stopped" | "done"))
    }

    /// Take what the process says about itself in the live registry (see `Live`), fresher than the supervisor's
    /// list. Returns whether anything shown changed.
    pub fn apply_live(&mut self, live: &Live) -> bool {
        let before = (self.status.clone(), self.state.clone(), self.waiting_for.clone(), self.name.clone());
        let known = matches!(live.status.as_deref(), Some("busy" | "waiting" | "idle" | "shell"));
        if known && self.status != live.status {
            self.status = live.status.clone();
            // The supervisor's `state` was read with the previous status and would contradict the new one: a turn
            // that started is `working`; between turns (`idle`, or `shell` — only a background command still
            // runs) it is `done`. `waiting` is read from the status alone.
            match live.status.as_deref() {
                Some("busy") => self.state = Some("working".into()),
                Some("idle" | "shell") => self.state = Some("done".into()),
                _ => {}
            }
        }
        self.waiting_for = live.waiting_for.clone().filter(|_| self.status.as_deref() == Some("waiting"));
        if live.name.is_some() {
            self.name = live.name.clone();
        }
        before != (self.status.clone(), self.state.clone(), self.waiting_for.clone(), self.name.clone())
    }

    pub fn activity(&self) -> Activity {
        if self.is_stopped() {
            return Activity::Stopped;
        }
        if !self.is_running() {
            return Activity::Ready;
        }
        // A finished turn (`state: done`) stays "busy" while background shells it started keep running (a dev
        // server, a test backend): Claude itself is waiting for the user, so that is Ready, not Working.
        if self.state.as_deref() == Some("done") && self.status.as_deref() != Some("waiting") {
            return Activity::Ready;
        }
        match self.status.as_deref() {
            Some("busy" | "shell") => Activity::Working,
            Some("waiting") => Activity::Waiting,
            _ => Activity::Ready,
        }
    }
}

/// What a running `claude` process writes about itself to `~/.claude/sessions/<pid>.json`: read from the file,
/// without starting a process (`claude agents` takes one every time and is slow when the Mac is short of memory).
/// An internal format, read defensively like `jobs/<id>/state.json`; a record that is missing or unreadable
/// just means the supervisor's list is used alone.
#[derive(Clone, Debug, PartialEq)]
pub struct Live {
    pub session_id: String,
    /// `busy` — working on a turn, `waiting` — something waits on the person (`waiting_for` says what), `idle` —
    /// between turns, `shell` — between turns with a background command still running.
    pub status: Option<String>,
    pub waiting_for: Option<String>,
    pub name: Option<String>,
    /// `procStart`: tells this process from another that got the same pid later.
    pub proc_start: String,
}

fn parse_live(bytes: &[u8]) -> Option<(u32, Live)> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let pid = u32::try_from(value.get("pid")?.as_u64()?).ok()?;
    let text = |key: &str| value.get(key).and_then(serde_json::Value::as_str).filter(|text| !text.is_empty()).map(str::to_string);
    Some((
        pid,
        Live {
            session_id: text("sessionId")?,
            status: text("status"),
            waiting_for: text("waitingFor"),
            name: text("name"),
            proc_start: text("procStart").unwrap_or_default(),
        },
    ))
}

/// The live registry: running processes by pid. Dead ones (a file left behind by a crash) are left out.
pub fn live_sessions() -> HashMap<u32, Live> {
    let Some(dir) = dirs::home_dir().map(|home| home.join(".claude").join("sessions")) else {
        return HashMap::new();
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return HashMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "json"))
        .filter_map(|entry| parse_live(&fs::read(entry.path()).ok()?))
        .filter(|(pid, _)| alive(*pid))
        .collect()
}

/// A saved transcript of the folder.
#[derive(Clone, Debug)]
pub struct Transcript {
    pub id: String,
    pub title: String,
    pub modified: SystemTime,
    pub size: u64,
}

#[derive(Debug)]
pub enum StartError {
    /// The folder wasn't trusted: Claude Code will ask about it itself if run in the terminal.
    NotTrusted,
    Failed(String),
}

/// Every session Pult starts runs without permission prompts.
/// `attach` takes no flags: an attached session keeps the mode it was started in.
pub const SKIP_PERMISSIONS: &str = "--dangerously-skip-permissions";

/// Sessions of a project work in its folder together, not each in a worktree of its own: what one changes,
/// the others see at once: nothing is isolated from the neighbours. Claude Code
/// otherwise moves a background session in a repository into `.claude/worktrees/<task>` before its first
/// edit (`worktree.bgIsolation`, on by default). The flag is kept in the session's respawn flags, so it holds
/// across restarts; the `CLAUDE_BG_ISOLATION` variable is not carried into a background session (checked
/// on Claude Code 2.1.330).
pub const SHARED_CHECKOUT: [&str; 2] = ["--settings", r#"{"worktree":{"bgIsolation":"none"}}"#];

/// The project's main session name: the folder in lowercase, underscores and spaces as hyphens, plus `-main`
/// (`EMBER` → `ember-main`, `CEDAR_GARDEN` → `cedar-garden-main`). Like git's main branch:
/// branches via `/branch` are `<project>-<topic>`. The suffix makes the name unambiguous for
/// `claude attach` (which also accepts a part of the name).
pub fn main_name(folder: &str) -> String {
    let base: String = folder
        .to_lowercase()
        .chars()
        .map(|char| if char.is_alphanumeric() { char } else { '-' })
        .collect();
    let base = base.split('-').filter(|part| !part.is_empty()).collect::<Vec<_>>().join("-");
    format!("{base}-main")
}

/// The pinned session's name: `<project>-main` for an ordinary project, `<project>-<subproject>` for a
/// subproject, with the subproject name in Latin letters: a Cyrillic name is transliterated (`maple-finansy`).
pub fn session_name(folder: &str, sub: Option<&str>) -> String {
    let main = main_name(folder);
    let Some(sub) = sub else {
        return main;
    };
    let base = main.strip_suffix("-main").unwrap_or(&main);
    let slug = latin_slug(sub);
    format!("{base}-{}", if slug.is_empty() { "sub".to_string() } else { slug })
}

/// A name as a lowercase Latin slug: Cyrillic transliterated, everything else that isn't a letter or a
/// digit becomes a hyphen.
pub(crate) fn latin_slug(name: &str) -> String {
    let mut out = String::new();
    for char in name.to_lowercase().chars() {
        let latin = match char {
            'а' => "a", 'б' => "b", 'в' => "v", 'г' => "g", 'д' => "d", 'е' | 'ё' | 'э' => "e", 'ж' => "zh",
            'з' => "z", 'и' => "i", 'й' | 'ы' => "y", 'к' => "k", 'л' => "l", 'м' => "m", 'н' => "n",
            'о' => "o", 'п' => "p", 'р' => "r", 'с' => "s", 'т' => "t", 'у' => "u", 'ф' => "f", 'х' => "h",
            'ц' => "c", 'ч' => "ch", 'ш' => "sh", 'щ' => "sch", 'ъ' | 'ь' => "", 'ю' => "yu", 'я' => "ya",
            other if other.is_ascii_alphanumeric() => {
                out.push(other);
                continue;
            }
            _ => "-",
        };
        out.push_str(latin);
    }
    out.split('-').filter(|part| !part.is_empty()).collect::<Vec<_>>().join("-")
}

/// Running `claude` in `zsh -lc`: the command from the login PATH, or else `~/.local/bin/claude`,
/// where the Claude Code installer puts it (it adds the path to `.zshrc`, which `-l` doesn't read:
/// on a clean Mac Pult couldn't find `claude` at all).
const EXEC_CLAUDE: &str = "exec \"${commands[claude]:-$HOME/.local/bin/claude}\" \"$@\"";

/// The user's login environment and where `claude` is, read from one login shell and kept for an hour.
#[derive(Clone)]
struct LoginEnv {
    claude: PathBuf,
    vars: Vec<(String, String)>,
}

/// Pult opened from Finder has no user PATH and LANG, and a background session inherits the environment —
/// without the login one its commands won't find brew, cargo and the rest. It is read once, not on every call:
/// `zsh -l` runs `~/.zprofile`, and Pult calls `claude` every few seconds: a profile line that starts a heavy
/// background process would pile such processes up until memory runs out.
fn login_env() -> Option<LoginEnv> {
    static CACHE: std::sync::Mutex<Option<(Instant, LoginEnv)>> = std::sync::Mutex::new(None);
    if let Ok(cache) = CACHE.lock()
        && let Some((at, env)) = cache.as_ref()
        && at.elapsed() < Duration::from_secs(3600)
    {
        return Some(env.clone());
    }
    // Login *and* interactive, like a new terminal tab: most PATH lines live in `~/.zshrc` (Homebrew, Go,
    // Bun…), and without them the sessions' tools — language servers like `gopls` among them — aren't found.
    // `.zshrc` may print (shell integration escapes), so the answer starts after a marker.
    let mut command = Command::new("/bin/zsh");
    command
        .arg("-lic")
        .arg(format!("printf '\\n{ENV_MARKER}\\0%s\\0' \"${{commands[claude]:-$HOME/.local/bin/claude}}\"; env -0"))
        .stdin(Stdio::null());
    let output = output_within(&mut command, Duration::from_secs(20)).ok()?;
    let env = parse_login_env(&output.stdout)?;
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((Instant::now(), env.clone()));
    }
    Some(env)
}

const ENV_MARKER: &str = "@@PULT-ENV@@";

fn parse_login_env(stdout: &[u8]) -> Option<LoginEnv> {
    let marker = format!("{ENV_MARKER}\0");
    let start = memchr::memmem::find(stdout, marker.as_bytes())? + marker.len();
    let mut parts = stdout[start..].split(|byte| *byte == 0);
    let claude = PathBuf::from(String::from_utf8_lossy(parts.next()?).into_owned());
    let vars = parts
        .filter_map(|part| {
            let text = String::from_utf8_lossy(part);
            let (key, value) = text.split_once('=')?;
            (!key.is_empty() && !matches!(key, "PWD" | "OLDPWD" | "SHLVL" | "_")).then(|| (key.to_string(), value.to_string()))
        })
        .collect();
    (!claude.as_os_str().is_empty()).then_some(LoginEnv { claude, vars })
}

/// A shell command with the user's login environment (their PATH: `go`, `npm`, `rustup`…).
pub fn login_command(script: &str) -> Command {
    let mut command = Command::new("/bin/zsh");
    match login_env() {
        Some(env) => {
            command.arg("-c").arg(script).envs(env.vars);
        }
        None => {
            command.arg("-lc").arg(script);
        }
    }
    command.stdin(Stdio::null());
    command
}

/// `claude` with the user's login environment.
pub fn claude(cwd: &Path, args: &[&str]) -> Command {
    ensure_daemon();
    let mut command = match login_env() {
        Some(env) => {
            let mut command = Command::new(&env.claude);
            command.args(args).envs(env.vars);
            command
        }
        // The login shell didn't answer: as before, through it.
        None => {
            let mut command = Command::new("/bin/zsh");
            command.arg("-lc").arg(EXEC_CLAUDE).arg("claude").args(args);
            command
        }
    };
    command.current_dir(cwd);
    if std::env::var_os("LANG").is_none() {
        command.env("LANG", "en_US.UTF-8");
    }
    command
}

/// How hard macOS is pressed for memory: 1 — normal, 2 — warning, 4 — critical (0 — unknown).
pub fn memory_pressure() -> u32 {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is a NUL-terminated string; the buffer and its size are the variables above.
    let found = unsafe {
        libc::sysctlbyname(
            c"kern.memorystatus_vm_pressure_level".as_ptr(),
            (&raw mut value).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if found == 0 { value.max(0) as u32 } else { 0 }
}

/// Why a background session failed: the text of its last timeline entry, when that entry is `failed`.
pub fn last_failure(short_id: &str) -> Option<String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let path = dirs::home_dir()?.join(".claude/jobs").join(short_id).join("timeline.jsonl");
    let mut file = fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(256 << 10))).ok()?;
    let mut tail = String::new();
    file.read_to_string(&mut tail).ok()?;
    let last: serde_json::Value = tail.lines().rev().find_map(|line| serde_json::from_str(line).ok())?;
    if last.get("state").and_then(serde_json::Value::as_str) != Some("failed") {
        return None;
    }
    let text = ["text", "detail"]
        .iter()
        .filter_map(|key| last.get(*key).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .find(|text| !text.is_empty())?;
    let mut text: String = text.chars().take(600).collect();
    if text.chars().count() == 600 {
        text.push('…');
    }
    Some(text)
}

/// A call with a time limit: a hung `claude` doesn't block Pult. If it doesn't fit, the process is killed
/// and the result is a `TimedOut` error.
pub fn output_within(command: &mut Command, limit: Duration) -> std::io::Result<Output> {
    use std::io::Read as _;
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(pipe) = stdout.as_mut() {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    });
    let err = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "claude didn't respond in time"));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(Output { status, stdout: out.join().unwrap_or_default(), stderr: err.join().unwrap_or_default() })
}

/// The shell couldn't find `claude`.
pub fn not_installed_message() -> &'static str {
    t("sessions.claude_code_not_found_install")
}

/// How long to wait for a session to start or resume.
const START_LIMIT: Duration = Duration::from_secs(60);

/// The start error text for a person: what happened and what to do.
fn start_failure(error: &std::io::Error) -> StartError {
    if error.kind() == std::io::ErrorKind::TimedOut {
        StartError::Failed(
            t("sessions.claude_code_didn_t_respond")
                .to_string(),
        )
    } else {
        StartError::Failed(tr!("sessions.couldn_t_start_claude_code", error = error))
    }
}

/// A `claude` response showing it's missing: the shell couldn't find the command.
fn not_installed(text: &str) -> bool {
    text.contains("no such file or directory") || text.contains("command not found")
}

/// Whether Claude Code is installed: the user's shell finds `claude` (or it lies where the installer puts it).
pub fn installed() -> bool {
    if let Some(env) = login_env() {
        return env.claude.is_file();
    }
    let mut command = Command::new("/bin/zsh");
    command.arg("-lc").arg("command -v claude >/dev/null || test -x \"$HOME/.local/bin/claude\"");
    output_within(&mut command, Duration::from_secs(15)).is_ok_and(|output| output.status.success())
}

/// The same arguments for running in Pult's terminal.
pub fn shell_args(args: &[&str]) -> Vec<String> {
    ensure_daemon();
    let mut all = vec!["-lc".to_string(), EXEC_CLAUDE.to_string(), "claude".to_string()];
    all.extend(args.iter().map(|arg| arg.to_string()));
    all
}

/// The label of the launchd job Pult uses to start the Claude Code supervisor.
const DAEMON_LABEL: &str = "io.hitkey.pult.claude-daemon";

/// Claude Code starts the background-session supervisor (`claude daemon`) itself — on the first `claude` call
/// that needs it. Then macOS attributes it to the caller: to Pult. Pult quits or updates — and its ghost
/// hangs in the Dock ("exited-with-subordinates") while the supervisor and its sessions live. So before any
/// `claude` call Pult starts the supervisor itself, as a launchd job: that way it belongs to the system.
/// The mode is the same as self-start (`--origin transient`): with no clients and sessions the supervisor
/// exits by itself.
pub fn ensure_daemon() {
    static STARTING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    if daemon_alive() {
        return;
    }
    let Ok(_starting) = STARTING.lock() else {
        return;
    };
    if daemon_alive() || start_daemon().is_err() {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !daemon_alive() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Whether the supervisor is alive: its pid is in `~/.claude/daemon.lock`.
fn daemon_alive() -> bool {
    let Some(home) = dirs::home_dir() else {
        return true;
    };
    fs::read_to_string(home.join(".claude/daemon.lock"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|lock| lock.get("pid").and_then(serde_json::Value::as_u64))
        .is_some_and(|pid| alive(pid as u32))
}

fn start_daemon() -> std::io::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| std::io::Error::other("no home folder"))?;
    let dir = dirs::data_dir().unwrap_or_else(|| home.join("Library/Application Support")).join("pult");
    fs::create_dir_all(&dir)?;
    let plist = dir.join("claude-daemon.plist");
    // The job carries the user's whole shell environment, tokens included — only the user may read it.
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt, os::unix::fs::PermissionsExt};
        let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&plist)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(daemon_plist(&home).as_bytes())?;
    }
    let uid = std::os::unix::fs::MetadataExt::uid(&fs::metadata(&home)?);
    // The job is already loaded (the supervisor ran and exited) — just start it again. Unloading and
    // reloading is not possible: `bootout` doesn't take effect at once, and the next `bootstrap` fails with error 5.
    let job = format!("gui/{uid}/{DAEMON_LABEL}");
    let loaded = Command::new("/bin/launchctl").args(["print", &job]).output().is_ok_and(|output| output.status.success());
    if loaded
        && Command::new("/bin/launchctl").args(["kickstart", &job]).output().is_ok_and(|output| output.status.success())
    {
        return Ok(());
    }
    let output = Command::new("/bin/launchctl")
        .args(["bootstrap", &format!("gui/{uid}")])
        .arg(&plist)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(String::from_utf8_lossy(&output.stderr).into_owned()))
    }
}

fn daemon_plist(home: &Path) -> String {
    let escape = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    // The supervisor starts every session process, and they inherit its environment: it gets the user's full
    // shell environment, otherwise sessions miss whatever `~/.zshrc` adds to PATH.
    let (program, mut vars) = match login_env() {
        Some(env) => {
            let claude = escape(&env.claude.to_string_lossy());
            (format!("<string>{claude}</string><string>daemon</string><string>run</string><string>--origin</string><string>transient</string>"), env.vars)
        }
        None => {
            let script = escape(&format!("{} daemon run --origin transient", EXEC_CLAUDE.trim_end_matches(" \"$@\"")));
            (format!("<string>/bin/zsh</string><string>-lc</string><string>{script}</string>"), Vec::new())
        }
    };
    if !vars.iter().any(|(key, _)| key == "LANG") {
        vars.push(("LANG".into(), "en_US.UTF-8".into()));
    }
    let environment: String = vars
        .iter()
        .map(|(key, value)| format!("<key>{}</key><string>{}</string>", escape(key), escape(value)))
        .collect();
    let home = escape(&home.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>{DAEMON_LABEL}</string>
	<key>ProgramArguments</key>
	<array>{program}</array>
	<key>WorkingDirectory</key><string>{home}</string>
	<key>EnvironmentVariables</key><dict>{environment}</dict>
	<key>RunAtLoad</key><true/>
	<key>KeepAlive</key><false/>
	<key>AbandonProcessGroup</key><true/>
	<key>StandardOutPath</key><string>/dev/null</string>
	<key>StandardErrorPath</key><string>/dev/null</string>
</dict>
</plist>
"#
    )
}

/// All sessions Claude Code knows: background (including stopped) and open in windows.
pub fn agents() -> Vec<Agent> {
    try_agents().unwrap_or_default()
}

/// The sessions Claude Code knows; `None` — it didn't answer (slow, busy, failed): that is not "no sessions".
/// One entry Pult can't read (a newer Claude Code changed a field) doesn't hide the others.
pub fn try_agents() -> Option<Vec<Agent>> {
    let home = dirs::home_dir().unwrap_or_default();
    let output = output_within(&mut claude(&home, &["agents", "--json", "--all"]), Duration::from_secs(20)).ok()?;
    if !output.status.success() {
        return None;
    }
    parse_agents(&output.stdout)
}

fn parse_agents(stdout: &[u8]) -> Option<Vec<Agent>> {
    let text = String::from_utf8_lossy(stdout);
    let start = text.find('[')?;
    let entries: Vec<serde_json::Value> = serde_json::from_str(&text[start..]).ok()?;
    Some(entries.into_iter().filter_map(|entry| serde_json::from_value(entry).ok()).collect())
}

/// A new background session named after the project. Returns its full id.
/// `settings` — the `--settings` value for the project's models (`models::session_flag`).
pub fn start_new(cwd: &Path, name: &str, settings: &str) -> Result<String, StartError> {
    let short = run_background(cwd, &["--bg", "-n", name, SKIP_PERMISSIONS, SHARED_CHECKOUT[0], settings])?;
    agents()
        .into_iter()
        .find(|agent| agent.id.as_deref() == Some(short.as_str()))
        .map(|agent| agent.session_id)
        .ok_or_else(|| StartError::Failed(t("sessions.claude_code_didn_t_return").into()))
}

/// A copy of `source` under a new id, started in the background as `name` in `cwd`: the new session remembers
/// everything said so far and goes on separately. Returns the new full id.
///
/// The transcript is copied by Pult rather than with `--fork-session`: a forked background session writes its
/// transcript only after its first answer, so a fork paused before that would lose the conversation. A line
/// still being written (the source is answering) is left out.
pub fn clone_session(cwd: &Path, source: &str, name: &str, settings: &str) -> Result<String, StartError> {
    let from = transcript_path(cwd, source).ok_or_else(|| StartError::Failed(t("sessions.no_conversation_to_copy").into()))?;
    let id = new_session_id().map_err(|error| StartError::Failed(tr!("sessions.couldn_t_copy_conversation", error = error)))?;
    copy_transcript(&from, &transcripts_dir(cwd), source, &id)
        .map_err(|error| StartError::Failed(tr!("sessions.couldn_t_copy_conversation", error = error)))?;
    let short = run_background(cwd, &["--resume", &id, "--bg", "-n", name, SKIP_PERMISSIONS, SHARED_CHECKOUT[0], settings])?;
    for _ in 0..10 {
        if let Some(agents) = try_agents()
            && let Some(agent) = agents.iter().find(|agent| agent.id.as_deref() == Some(short.as_str()))
        {
            return Ok(agent.session_id.clone());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(StartError::Failed(t("sessions.started_but_not_listed").into()))
}

/// Write `from` (the conversation of `source`) into `dir` as `<id>.jsonl`, the session id in each record
/// replaced, and copy the session's own folder (subagents, tool results) beside it.
fn copy_transcript(from: &Path, dir: &Path, source: &str, id: &str) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let text = fs::read_to_string(from)?;
    let mut out = String::with_capacity(text.len());
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(mut record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if record.get("sessionId").and_then(|value| value.as_str()) == Some(source) {
            record["sessionId"] = serde_json::Value::String(id.to_string());
        }
        out.push_str(&record.to_string());
        out.push('\n');
    }
    let target = dir.join(format!("{id}.jsonl"));
    fs::write(&target, out)?;
    if let Some(folder) = from.parent().map(|parent| parent.join(source))
        && folder.is_dir()
        && let Err(error) = copy_dir(&folder, &dir.join(id))
    {
        let _ = fs::remove_file(&target);
        return Err(error);
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &to.join(entry.file_name()))?;
        } else if kind.is_file() {
            fs::copy(entry.path(), to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// A random (version 4) UUID, the form Claude Code uses for session ids.
fn new_session_id() -> std::io::Result<String> {
    use std::io::Read as _;
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]))
}

/// Start the pinned session in the background from the same place.
///
/// Claude Code still keeping a record of the session (crashed — `failed`, finished — `done`, unloaded) counts it
/// as running and `--resume --bg` starts a *copy* under a new id: every Continue made another copy while the
/// pinned one stayed down. Such a session is restarted with
/// `respawn` — same id. Returns the session to pin when Claude Code still started a copy (it carries the same
/// conversation).
pub fn resume(cwd: &Path, session_id: &str, settings: &str) -> Result<Option<String>, StartError> {
    let listed = |agents: &[Agent], session: &str| {
        agents.iter().any(|agent| agent.session_id == session && agent.pid.is_some() && agent.state.as_deref() != Some("failed"))
    };
    let known = try_agents()
        .unwrap_or_default()
        .into_iter()
        .filter(|agent| agent.session_id == session_id && agent.pid.is_none() && agent.state.as_deref() != Some("stopped"))
        .find_map(|agent| agent.id);
    if let Some(short) = known
        && {
            set_settings_flag(&short, settings);
            respawn(&short).is_ok()
        }
    {
        for _ in 0..10 {
            if try_agents().is_some_and(|agents| listed(&agents, session_id)) {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    let short = run_background(cwd, &["--resume", session_id, "--bg", SKIP_PERMISSIONS, SHARED_CHECKOUT[0], settings])?;
    // Under which session Claude Code started it: the same, or a copy.
    for _ in 0..10 {
        if let Some(agents) = try_agents()
            && let Some(agent) = agents.iter().find(|agent| agent.id.as_deref() == Some(short.as_str()))
        {
            return Ok((agent.session_id != session_id).then(|| agent.session_id.clone()));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(StartError::Failed(t("sessions.started_but_not_listed").into()))
}

/// Whether a background session runs without permission prompts. Claude Code keeps the launch flags in
/// `~/.claude/jobs/<short id>/state.json` (`respawnFlags`) — an internal file, hence
/// `None` if it couldn't be read.
pub fn skips_permissions(short_id: &str) -> Option<bool> {
    let flags = respawn_flags(short_id)?;
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    Some(
        flags.contains(&SKIP_PERMISSIONS)
            || flags.windows(2).any(|pair| pair == ["--permission-mode", "bypassPermissions"]),
    )
}

/// `~/.claude/jobs/<short id>/state.json` — how Claude Code restarts a background session: working
/// folder, git worktree, name, flags (`respawnFlags`). An internal file — best effort.
pub fn job_state_path(short_id: &str) -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude").join("jobs").join(short_id).join("state.json")
}

/// The flags Claude Code restarts a background session with (`respawnFlags`).
pub fn respawn_flags(short_id: &str) -> Option<Vec<String>> {
    let state: serde_json::Value = serde_json::from_str(&fs::read_to_string(job_state_path(short_id)).ok()?).ok()?;
    Some(
        state
            .get("respawnFlags")?
            .as_array()?
            .iter()
            .filter_map(|flag| flag.as_str().map(str::to_string))
            .collect(),
    )
}

/// Make a session Pult restarts work in the project's folder with its neighbours (`SHARED_CHECKOUT`): the
/// flag is added to the session's `respawnFlags` if it was started without it. A session that already moved
/// into a worktree stays there; the next ones don't. Best effort — an internal file.
fn share_checkout(short_id: &str) {
    let path = job_state_path(short_id);
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut state) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(flags) = state.get_mut("respawnFlags").and_then(serde_json::Value::as_array_mut) else {
        return;
    };
    if flags.iter().any(|flag| flag.as_str() == Some(SHARED_CHECKOUT[1])) {
        return;
    }
    // A different `--settings` of the user's own is left alone.
    if flags.iter().any(|flag| flag.as_str() == Some(SHARED_CHECKOUT[0])) {
        return;
    }
    flags.push(serde_json::Value::String(SHARED_CHECKOUT[0].into()));
    flags.push(serde_json::Value::String(SHARED_CHECKOUT[1].into()));
    if let Ok(text) = serde_json::to_string_pretty(&state) {
        let _ = fs::write(&path, text);
    }
}

/// Whether a `--settings` value is one Pult puts there: the shared-checkout JSON or the GLM session file.
fn ours(value: &str) -> bool {
    value == SHARED_CHECKOUT[1] || crate::models::is_glm_flag(value)
}

/// Point the `--settings` flag Pult gave a job (`respawnFlags` in its `state.json`) at `value`; with `add`, also
/// give it one if it has none. A `--settings` of the user's own is left alone. `true` — the file changed.
pub fn replace_settings_flag(state_path: &Path, value: &str, add: bool) -> bool {
    let Ok(text) = fs::read_to_string(state_path) else {
        return false;
    };
    let Ok(mut state) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let Some(flags) = state.get_mut("respawnFlags").and_then(serde_json::Value::as_array_mut) else {
        return false;
    };
    let at = flags.windows(2).position(|pair| pair[0].as_str() == Some(SHARED_CHECKOUT[0]) && pair[1].as_str().is_some_and(ours));
    match at {
        Some(index) if flags[index + 1].as_str() == Some(value) => return false,
        Some(index) => flags[index + 1] = serde_json::Value::String(value.into()),
        None if add && !flags.iter().any(|flag| flag.as_str() == Some(SHARED_CHECKOUT[0])) => {
            flags.push(serde_json::Value::String(SHARED_CHECKOUT[0].into()));
            flags.push(serde_json::Value::String(value.into()));
        }
        None => return false,
    }
    serde_json::to_string_pretty(&state).is_ok_and(|text| fs::write(state_path, text).is_ok())
}

/// The models flag of a background session, for its next restart.
pub fn set_settings_flag(short_id: &str, value: &str) {
    replace_settings_flag(&job_state_path(short_id), value, true);
}

/// Move a running background session to other models: stop it, wait for the process, point its flags at `value`,
/// `respawn` — the same id, name and conversation.
pub fn switch_settings(short_id: &str, pid: Option<u32>, value: &str) -> Result<(), StartError> {
    stop(short_id);
    if let Some(pid) = pid {
        let deadline = Instant::now() + Duration::from_secs(15);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    set_settings_flag(short_id, value);
    respawn(short_id)
}

/// The one entry of a job's `providerEnv` that isn't a provider setting.
const JOB_CREDENTIAL: &str = "CLAUDE_BG_SESSION_CREDENTIAL";

/// A background job remembers the provider variables it was started with (`providerEnv` in its `state.json`,
/// the model names among them) and `respawn` applies them again. After a switch between Claude and GLM the saved
/// GLM model names hold the restarted session on GLM, though the settings are Claude's again (a manual stop and
/// start makes a new job and cures it). The saved variables are brought
/// to what a fresh start would take now: the value in the settings' `env`, else in the login shell, else the variable
/// goes. Returns whether anything changed.
fn reconcile_provider_env(
    saved: &mut serde_json::Map<String, serde_json::Value>,
    settings_env: &serde_json::Map<String, serde_json::Value>,
    login: &[(String, String)],
) -> bool {
    let mut changed = false;
    let keys: Vec<String> = saved.keys().filter(|key| key.as_str() != JOB_CREDENTIAL).cloned().collect();
    for key in keys {
        let wanted = settings_env
            .get(&key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| login.iter().find(|(name, _)| *name == key).map(|(_, value)| value.clone()))
            .filter(|value| !value.is_empty());
        match wanted {
            Some(value) if saved.get(&key).and_then(serde_json::Value::as_str) == Some(value.as_str()) => {}
            Some(value) => {
                saved.insert(key, serde_json::Value::String(value));
                changed = true;
            }
            None => {
                saved.shift_remove(&key);
                changed = true;
            }
        }
    }
    changed
}

/// Bring a job's saved provider variables up to date before it is restarted (`reconcile_provider_env`). Best
/// effort — an internal file.
fn refresh_provider_env(short_id: &str) {
    let path = job_state_path(short_id);
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut state) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(saved) = state.get_mut("providerEnv").and_then(serde_json::Value::as_object_mut) else {
        return;
    };
    let settings_env = dirs::home_dir()
        .and_then(|home| fs::read_to_string(home.join(".claude").join("settings.json")).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|settings| settings.get("env").and_then(serde_json::Value::as_object).cloned())
        .unwrap_or_default();
    let login = login_env().map(|env| env.vars).unwrap_or_default();
    if reconcile_provider_env(saved, &settings_env, &login)
        && let Ok(text) = serde_json::to_string_pretty(&state)
    {
        let _ = fs::write(&path, text);
    }
}

/// Restart a background session from its `state.json`: same id, name, flags and git worktree.
pub fn respawn(short_id: &str) -> Result<(), StartError> {
    share_checkout(short_id);
    refresh_provider_env(short_id);
    let home = dirs::home_dir().unwrap_or_default();
    let output = output_within(&mut claude(&home, &["respawn", short_id]), START_LIMIT).map_err(|error| start_failure(&error))?;
    let text = format!("{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    if text.contains("Workspace not trusted") {
        return Err(StartError::NotTrusted);
    }
    if text.lines().any(|line| line.trim().starts_with("restarted ")) {
        return Ok(());
    }
    if not_installed(&text) {
        return Err(StartError::Failed(not_installed_message().to_string()));
    }
    let first = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    Err(StartError::Failed(tr!("sessions.claude_code_didn_t_restart", first = first)))
}

/// Restart a background session without permission prompts. A background session's flags are fixed
/// once and for all (`--resume` with new flags makes a copy with another id), so: stop it, fix the flags
/// in `state.json` and restart the same session. `claude rm` won't do: for a session with a git worktree
/// it deletes the worktree itself.
pub fn relaunch_skipping_permissions(short_id: &str, pid: Option<u32>) -> Result<(), StartError> {
    stop(short_id);
    if let Some(pid) = pid {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    let path = job_state_path(short_id);
    let failed = |error: String| {
        StartError::Failed(tr!("sessions.couldn_t_update_session_s", error = error))
    };
    let mut state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).map_err(|error| failed(error.to_string()))?)
            .map_err(|error| failed(error.to_string()))?;
    let flags = state
        .get_mut("respawnFlags")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| failed(t("sessions.no_respawnflags").into()))?;
    let mut kept = Vec::new();
    let mut iter = std::mem::take(flags).into_iter();
    while let Some(flag) = iter.next() {
        if flag.as_str() == Some("--permission-mode") {
            iter.next();
            continue;
        }
        if flag.as_str() != Some(SKIP_PERMISSIONS) {
            kept.push(flag);
        }
    }
    kept.push(serde_json::Value::String(SKIP_PERMISSIONS.into()));
    *flags = kept;
    let text = serde_json::to_string_pretty(&state).map_err(|error| failed(error.to_string()))?;
    fs::write(&path, text).map_err(|error| failed(error.to_string()))?;
    respawn(short_id)
}

/// Restart a running background session so it picks up new settings (`~/.claude/settings.json` is read when a
/// Claude Code process starts): stop it, wait for the process to exit, `respawn` — the same id, name, flags and
/// git worktree, the conversation goes on from where it was. A turn that was running is cut off.
pub fn restart(short_id: &str, pid: Option<u32>) -> Result<(), StartError> {
    stop(short_id);
    if let Some(pid) = pid {
        let deadline = Instant::now() + Duration::from_secs(15);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    respawn(short_id)
}

/// Whether the login shell's environment sets any of these variables. Starts a shell the first time in an hour —
/// run it in the background.
pub fn login_has_any(names: &[&str]) -> bool {
    login_env().is_some_and(|env| env.vars.iter().any(|(name, value)| !value.is_empty() && names.contains(&name.as_str())))
}

/// Whether a process is alive. Without running `ps`: the window asks this on every terminal check, and
/// starting a process on the main thread slowed down input in a virtual machine.
pub fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 sends nothing, it only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

/// Pause: the process stops, the transcript stays.
pub fn stop(short_id: &str) {
    let home = dirs::home_dir().unwrap_or_default();
    let _ = output_within(&mut claude(&home, &["stop", short_id]), Duration::from_secs(30));
}

/// Launch with `--bg`; the short id is taken from the answer "backgrounded · <id> · <name>".
pub fn run_background(cwd: &Path, args: &[&str]) -> Result<String, StartError> {
    let output = output_within(&mut claude(cwd, args), START_LIMIT).map_err(|error| start_failure(&error))?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if text.contains("Workspace not trusted") {
        return Err(StartError::NotTrusted);
    }
    text.lines()
        .find_map(|line| {
            let rest = line.trim().strip_prefix("backgrounded · ")?;
            rest.split(" · ").next().map(str::to_string)
        })
        .ok_or_else(|| {
            if not_installed(&text) {
                return StartError::Failed(not_installed_message().to_string());
            }
            let first = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
            StartError::Failed(tr!("sessions.claude_code_didn_t_start", first = first))
        })
}

/// How much memory the process takes, in megabytes.
pub fn memory_mb(pid: u32) -> Option<u64> {
    let output = Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let kb: u64 = String::from_utf8_lossy(&output.stdout).trim().parse().ok()?;
    Some(kb / 1024)
}

/// The folder where Claude Code keeps a project's transcripts: the path with everything except letters and digits as "-".
fn transcripts_dir(project: &Path) -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude").join("projects").join(crate::migrate::encode(project))
}

/// Whether this computer has the session's transcript (for example, it came from another computer).
/// Searched in all data folders: the session could run in a git worktree inside the project
/// (`<project>/…/.claude/worktrees/<task>`), with its transcript in that worktree's data folder. Claude Code
/// restarts such a session from the project root as well and appends to the same file.
///
/// It must be an actual conversation: a just-started session with nothing written yet has the file, but
/// it holds only a name (`custom-title`, `agent-name`). Claude Code can't restart such a session ("source
/// session … not found").
pub fn has_transcript(project: &Path, session: &str) -> bool {
    // A conversation, once it appears, doesn't disappear — checked ones are remembered: the window asks every 3 s.
    static KNOWN: std::sync::Mutex<Option<std::collections::HashSet<String>>> = std::sync::Mutex::new(None);
    if KNOWN.lock().is_ok_and(|known| known.as_ref().is_some_and(|known| known.contains(session))) {
        return true;
    }
    let found = transcript_path(project, session).is_some_and(|path| has_conversation(&path));
    if found && let Ok(mut known) = KNOWN.lock() {
        known.get_or_insert_with(Default::default).insert(session.to_string());
    }
    found
}

/// Length of the session's transcript on this computer (0 — it isn't here).
pub fn transcript_len(project: &Path, session: &str) -> u64 {
    transcript_path(project, session).and_then(|path| fs::metadata(path).ok()).map_or(0, |meta| meta.len())
}

pub fn transcript_path(project: &Path, session: &str) -> Option<PathBuf> {
    let file = format!("{session}.jsonl");
    let own = transcripts_dir(project).join(&file);
    if own.is_file() {
        return Some(own);
    }
    let all = dirs::home_dir().unwrap_or_default().join(".claude").join("projects");
    fs::read_dir(all)
        .ok()?
        .filter_map(Result::ok)
        .map(|dir| dir.path().join(&file))
        .find(|path| path.is_file())
}

/// The transcript has at least one message (only the beginning is read: transcripts can be half a gigabyte).
pub fn has_conversation(transcript: &Path) -> bool {
    use std::io::Read as _;
    let mut head = Vec::new();
    let read = fs::File::open(transcript).and_then(|file| file.take(4 << 20).read_to_end(&mut head));
    read.is_ok() && {
        let text = String::from_utf8_lossy(&head);
        text.contains("\"type\":\"user\"") || text.contains("\"type\":\"assistant\"")
    }
}

/// Whether the session with this id has a conversation — in any data folder (for the shared projects snapshot).
pub fn session_has_conversation(session: &str) -> bool {
    has_transcript(Path::new("/"), session)
}

/// How many transcripts the folder has — for the "Add folders" window.
pub fn transcript_count(project: &Path) -> usize {
    fs::read_dir(transcripts_dir(project))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
                .count()
        })
        .unwrap_or(0)
}

/// The folder's transcripts, newest first.
pub fn transcripts(project: &Path) -> Vec<Transcript> {
    let Ok(entries) = fs::read_dir(transcripts_dir(project)) else {
        return Vec::new();
    };
    let mut list: Vec<Transcript> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "jsonl" {
                return None;
            }
            let id = path.file_stem()?.to_string_lossy().into_owned();
            let meta = entry.metadata().ok()?;
            if meta.len() == 0 {
                return None;
            }
            Some(Transcript {
                title: title_of(&path).unwrap_or_else(|| short_id(&id).to_string()),
                id,
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                size: meta.len(),
            })
        })
        .collect();
    list.sort_by_key(|item| std::cmp::Reverse(item.modified));
    list
}

/// A transcript's title: the name given to the session, otherwise the first request. The line format
/// is internal and may change — on failure the short id is shown.
fn title_of(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut first_prompt = None;
    for line in BufReader::new(file).lines().take(400).map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match value.get("type").and_then(|kind| kind.as_str()) {
            Some("custom-title") => {
                if let Some(title) = value.get("customTitle").and_then(|title| title.as_str()) {
                    return Some(clean(title));
                }
            }
            Some("summary") if first_prompt.is_none() => {
                if let Some(summary) = value.get("summary").and_then(|summary| summary.as_str()) {
                    first_prompt = Some(clean(summary));
                }
            }
            Some("user") if first_prompt.is_none() => {
                let content = value.get("message").and_then(|message| message.get("content"));
                let text = match content {
                    Some(serde_json::Value::String(text)) => Some(text.as_str()),
                    Some(serde_json::Value::Array(parts)) => parts
                        .iter()
                        .find_map(|part| part.get("text").and_then(|text| text.as_str())),
                    _ => None,
                };
                if let Some(text) = text.filter(|text| !text.starts_with('<')) {
                    first_prompt = Some(clean(text));
                }
            }
            _ => {}
        }
    }
    first_prompt
}

fn clean(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 80 {
        format!("{}…", line.chars().take(80).collect::<String>())
    } else {
        line
    }
}

pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// "just now", "12 min ago", "yesterday", then the date ("Mar 4" in English).
pub fn ago(time: SystemTime) -> String {
    let seconds = SystemTime::now()
        .duration_since(time)
        .map(|age| age.as_secs())
        .unwrap_or(0);
    match seconds {
        0..60 => t("sessions.just_now").into(),
        60..3600 => tr!("sessions.seconds_min_ago", seconds = seconds / 60),
        3600..86_400 => tr!("sessions.seconds_h_ago", seconds = seconds / 3600),
        86_400..172_800 => t("sessions.yesterday").into(),
        _ => {
            let format = if crate::i18n::is_ru() { "+%d.%m" } else { "+%b %e" };
            let output = Command::new("/bin/date")
                .env("LC_TIME", if crate::i18n::is_ru() { "ru_RU.UTF-8" } else { "en_US.UTF-8" })
                .args(["-r", &time.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0).to_string(), format])
                .output()
                .ok();
            output
                .map(|output| String::from_utf8_lossy(&output.stdout).split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_default()
        }
    }
}

/// "3.1 MB" (Russian uses a decimal comma).
pub fn size_label(bytes: u64) -> String {
    let mb = bytes as f64 / 1_048_576.;
    if mb < 0.1 {
        return tr!("sessions.max_kb", max = (bytes / 1024).max(1));
    }
    let number = format!("{mb:.1}");
    let number = if crate::i18n::is_ru() { number.replace('.', ",") } else { number };
    tr!("sessions.number_mb", number = number)
}

/// When the session last wrote to its transcript.
pub fn last_activity(project: &Path, session_id: &str) -> Option<SystemTime> {
    fs::metadata(transcripts_dir(project).join(format!("{session_id}.jsonl")))
        .and_then(|meta| meta.modified())
        .ok()
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn subproject_sessions_are_named_in_latin() {
        assert_eq!(session_name("EMBER", None), "ember-main");
        assert_eq!(session_name("MAPLE", Some("Отчёты")), "maple-otchety");
        assert_eq!(session_name("CEDAR_GARDEN", Some("Backend API")), "cedar-garden-backend-api");
        assert_eq!(session_name("MAPLE", Some("Щётки и ёлки")), "maple-schetki-i-elki");
        assert_eq!(session_name("MAPLE", Some("!!!")), "maple-sub");
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;

    #[test]
    fn login_env_is_read_from_one_shell() {
        let out = b"\x1b]1337;CurrentDir=/x\x07\n@@PULT-ENV@@\0/home/user/.local/bin/claude\0PATH=/opt/homebrew/bin:/usr/bin\0PWD=/x\0LANG=ru_RU.UTF-8\0EMPTY=\0SHLVL=1\0";
        let env = parse_login_env(out).unwrap();
        assert_eq!(env.claude, PathBuf::from("/home/user/.local/bin/claude"));
        let keys: Vec<&str> = env.vars.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["PATH", "LANG", "EMPTY"], "the shell's own variables stay out");
        assert!(parse_login_env(b"").is_none());
        assert!(parse_login_env(b"/x/claude\0PATH=/bin\0").is_none(), "no marker — the shell said nothing useful");
    }

    /// The real shell of this Mac: `cargo test live_login_env -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_login_env() {
        let env = login_env().expect("the shell answered");
        let path = env.vars.iter().find(|(key, _)| key == "PATH").map(|(_, value)| value.clone()).unwrap_or_default();
        println!("claude: {}\nPATH: {path}", env.claude.display());
        assert!(env.claude.exists());
        let plist = daemon_plist(&dirs::home_dir().unwrap());
        assert!(plist.contains("<key>PATH</key>") && plist.contains("<string>daemon</string>"));
    }

    fn object(json: &str) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_restarted_job_drops_the_model_names_of_the_provider_it_left() {
        // GLM → Claude: the settings no longer set the model variables, the job still remembers them.
        let mut saved = object(
            r#"{"ANTHROPIC_DEFAULT_SONNET_MODEL":"glm-5.4[1m]","ANTHROPIC_DEFAULT_HAIKU_MODEL":"glm-5.3-flash[1m]","CLAUDE_BG_SESSION_CREDENTIAL":"env-login"}"#,
        );
        assert!(reconcile_provider_env(&mut saved, &object("{}"), &[]));
        assert_eq!(serde_json::Value::Object(saved), serde_json::json!({"CLAUDE_BG_SESSION_CREDENTIAL": "env-login"}));
    }

    #[test]
    fn a_restarted_job_takes_the_current_value_from_the_settings_then_from_the_shell() {
        let mut saved = object(
            r#"{"ANTHROPIC_DEFAULT_SONNET_MODEL":"glm-5.2[1m]","ANTHROPIC_DEFAULT_OPUS_MODEL":"old","ANTHROPIC_DEFAULT_HAIKU_MODEL":"same","CLAUDE_BG_SESSION_CREDENTIAL":"env-login"}"#,
        );
        let settings = object(r#"{"ANTHROPIC_DEFAULT_SONNET_MODEL":"glm-5.4[1m]","ANTHROPIC_DEFAULT_HAIKU_MODEL":"same"}"#);
        let login = [("ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(), "from-shell".to_string())];
        assert!(reconcile_provider_env(&mut saved, &settings, &login));
        assert_eq!(saved["ANTHROPIC_DEFAULT_SONNET_MODEL"], "glm-5.4[1m]", "the settings win");
        assert_eq!(saved["ANTHROPIC_DEFAULT_OPUS_MODEL"], "from-shell");
        assert_eq!(saved["CLAUDE_BG_SESSION_CREDENTIAL"], "env-login", "not a provider setting");
        // Nothing left to change: the file isn't rewritten.
        assert!(!reconcile_provider_env(&mut saved, &settings, &login));
    }

    #[test]
    fn a_job_without_saved_provider_variables_is_left_alone() {
        let mut saved = object(r#"{"CLAUDE_BG_SESSION_CREDENTIAL":"env-login"}"#);
        assert!(!reconcile_provider_env(&mut saved, &object(r#"{"ANTHROPIC_DEFAULT_SONNET_MODEL":"glm-5.4[1m]"}"#), &[]));
        assert_eq!(saved.len(), 1, "variables the job never had are not added: the settings reach it at start");
    }

    #[test]
    fn a_live_record_is_read_defensively() {
        let json = br#"{"pid":12358,"sessionId":"0920f257","status":"waiting","waitingFor":"input needed","name":"x","procStart":"Tue Nov 24","future":{"a":1}}"#;
        let (pid, live) = parse_live(json).unwrap();
        assert_eq!(pid, 12358);
        assert_eq!(live.status.as_deref(), Some("waiting"));
        assert_eq!(live.waiting_for.as_deref(), Some("input needed"));
        assert!(parse_live(br#"{"pid":1}"#).is_none(), "no session id — not a session");
        assert!(parse_live(b"not json").is_none());
        assert!(parse_live(br#"{"pid":-4,"sessionId":"s"}"#).is_none());
    }

    #[test]
    fn a_copied_conversation_carries_the_new_id_and_the_session_folder() {
        let base = std::env::temp_dir().join(format!("pult-clone-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (from_dir, to_dir) = (base.join("from"), base.join("to"));
        fs::create_dir_all(from_dir.join("old/subagents")).unwrap();
        fs::write(from_dir.join("old/subagents/a.jsonl"), "{}\n").unwrap();
        let from = from_dir.join("old.jsonl");
        fs::write(
            &from,
            "{\"type\":\"user\",\"sessionId\":\"old\",\"text\":\"hi\"}\n\n{\"type\":\"summary\"}\n{\"type\":\"assistant\",\"sessionId\":\"old\",\"te",
        )
        .unwrap();
        let id = new_session_id().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        copy_transcript(&from, &to_dir, "old", &id).unwrap();
        let copy = fs::read_to_string(to_dir.join(format!("{id}.jsonl"))).unwrap();
        let lines: Vec<&str> = copy.lines().collect();
        assert_eq!(lines.len(), 2, "the line still being written is left out");
        assert!(lines[0].contains(&id) && !lines[0].contains("\"old\""));
        assert_eq!(lines[1], "{\"type\":\"summary\"}");
        assert!(to_dir.join(&id).join("subagents/a.jsonl").is_file());
        assert!(fs::read_to_string(&from).unwrap().contains("\"sessionId\":\"old\""), "the original is untouched");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn the_models_flag_of_a_job_is_swapped_and_the_users_own_settings_left_alone() {
        let dir = std::env::temp_dir().join(format!("pult-flags-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let glm = "/u/Library/Application Support/pult/glm-session.json";
        let write = |flags: serde_json::Value| fs::write(&path, serde_json::json!({"cwd": "/x", "respawnFlags": flags}).to_string()).unwrap();
        let flags = || -> Vec<String> {
            let state: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            state["respawnFlags"].as_array().unwrap().iter().map(|flag| flag.as_str().unwrap().to_string()).collect()
        };
        // Pult's shared-checkout flag becomes the GLM file, and back.
        write(serde_json::json!([SKIP_PERMISSIONS, "--settings", SHARED_CHECKOUT[1]]));
        assert!(replace_settings_flag(&path, glm, true));
        assert_eq!(flags(), vec![SKIP_PERMISSIONS, "--settings", glm]);
        assert!(!replace_settings_flag(&path, glm, true), "nothing to change");
        assert!(replace_settings_flag(&path, SHARED_CHECKOUT[1], false));
        assert_eq!(flags(), vec![SKIP_PERMISSIONS, "--settings", SHARED_CHECKOUT[1]]);
        // A job without the flag gets one only when asked to.
        write(serde_json::json!([SKIP_PERMISSIONS]));
        assert!(!replace_settings_flag(&path, glm, false));
        assert!(replace_settings_flag(&path, glm, true));
        assert_eq!(flags(), vec![SKIP_PERMISSIONS, "--settings", glm]);
        // The user's own --settings is never replaced.
        write(serde_json::json!(["--settings", "/mine.json"]));
        assert!(!replace_settings_flag(&path, glm, true));
        assert_eq!(flags(), vec!["--settings", "/mine.json"]);
    }

    /// A real clone of an existing session: `PULT_CLONE_DIR=<trusted folder> PULT_CLONE_SOURCE=<session id>
    /// cargo test live_clone -- --ignored --nocapture`. Starts the copy in the background and stops it again.
    #[test]
    #[ignore]
    fn live_clone() {
        let dir = PathBuf::from(std::env::var("PULT_CLONE_DIR").unwrap());
        let source = std::env::var("PULT_CLONE_SOURCE").unwrap();
        let copy = clone_session(&dir, &source, "pult-clone-check", SHARED_CHECKOUT[1]).unwrap();
        println!("copy: {copy}");
        assert_ne!(copy, source);
        assert!(has_transcript(&dir, &copy));
        let agent = agents().into_iter().find(|agent| agent.session_id == copy).expect("listed");
        assert_eq!(agent.name.as_deref(), Some("pult-clone-check"));
        stop(agent.id.as_deref().unwrap());
    }

    #[test]
    fn the_registry_is_fresher_than_the_list() {
        let live = |status: &str| Live {
            session_id: "s".into(),
            status: Some(status.into()),
            waiting_for: Some("input needed".into()),
            name: Some("fresh".into()),
            proc_start: String::new(),
        };
        // The list still says the turn is done; the process says it began another.
        let mut agent = Agent { pid: Some(5), status: Some("idle".into()), state: Some("done".into()), ..Agent::default() };
        assert_eq!(agent.activity(), Activity::Ready);
        assert!(agent.apply_live(&live("busy")));
        assert_eq!(agent.activity(), Activity::Working, "a stale `done` must not hide a new turn");
        assert_eq!(agent.name.as_deref(), Some("fresh"));
        assert_eq!(agent.waiting_for, None, "the reason is shown only while waiting");
        assert!(agent.apply_live(&live("waiting")));
        assert_eq!(agent.activity(), Activity::Waiting);
        assert_eq!(agent.waiting_for.as_deref(), Some("input needed"));
        assert!(agent.apply_live(&live("shell")));
        assert_eq!(agent.activity(), Activity::Ready, "a background command alone is not work");
        assert!(!agent.apply_live(&live("shell")), "nothing changed, nothing to redraw");
        // A status Pult doesn't know is left alone.
        assert!(!agent.apply_live(&Live { status: Some("dreaming".into()), ..live("shell") }));
        assert_eq!(agent.status.as_deref(), Some("shell"));
    }

    /// This Mac's real registry against the real list: `cargo test live_registry -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_registry() {
        let live = live_sessions();
        let agents = try_agents().expect("the supervisor answered");
        for agent in agents.iter().filter(|agent| agent.pid.is_some()) {
            let record = live.get(&agent.pid.unwrap()).expect("every listed process has a registry file");
            assert_eq!(record.session_id, agent.session_id, "the same session");
            let mut copy = agent.clone();
            copy.apply_live(record);
            println!("{:?} list {:?}/{:?} → live {:?}, activity {:?}", agent.name, agent.status, agent.state, copy.status, copy.activity());
            assert_eq!(copy.activity(), agent.activity(), "at rest the registry agrees with the list");
        }
        let started = std::time::Instant::now();
        for _ in 0..100 {
            let _ = live_sessions();
        }
        println!("100 registry reads: {} ms", started.elapsed().as_millis());
    }

    #[test]
    fn an_ended_session_counts_as_stopped() {
        let agent = |state: &str, pid: Option<u32>| Agent { state: Some(state.into()), pid, kind: "background".into(), ..Agent::default() };
        assert!(agent("done", None).is_stopped());
        assert!(agent("stopped", None).is_stopped());
        assert!(!agent("blocked", None).is_stopped(), "unloaded when idle is alive");
        assert!(!agent("done", Some(5)).is_stopped());
    }

    #[test]
    fn agents_survive_an_unreadable_entry() {
        let out = br#"[{"id":"a1","sessionId":"s1","pid":12,"state":"running"},{"id":"a2","sessionId":"s2","pid":"oops"}]"#;
        let agents = parse_agents(out).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].session_id, "s1");
        assert!(parse_agents(b"Error: supervisor not running").is_none(), "no list is not an empty list");
        assert_eq!(parse_agents(b"[]").unwrap().len(), 0);
    }

    fn agent(status: &str, state: &str) -> Agent {
        serde_json::from_value(serde_json::json!({
            "pid": 1, "id": "x", "sessionId": "s", "kind": "background", "status": status, "state": state
        }))
        .expect("agent")
    }

    #[test]
    fn finished_turn_with_background_shells_is_ready() {
        assert_eq!(agent("busy", "done").activity(), Activity::Ready);
        assert_eq!(agent("busy", "working").activity(), Activity::Working);
        assert_eq!(agent("waiting", "done").activity(), Activity::Waiting);
        assert_eq!(agent("idle", "done").activity(), Activity::Ready);
    }
}
