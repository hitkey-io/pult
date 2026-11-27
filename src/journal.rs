//! Journal of parallel sessions: the sessions of one split project work side by side, and each of them is told
//! about the others.
//!
//! The rule: nothing is blocked and sessions don't talk to each other. A session only gets
//! facts — who else works in this project, what they do, which files they changed lately — and tells its user
//! what the risks are and what to do about them; the user decides.
//!
//! Claude Code hooks call `pult journal hook` (see `install_hooks`):
//! - `SessionStart` and `UserPromptSubmit` — the session gets a summary of its neighbours (at the prompt only
//!   when it has changed since the last one);
//! - `PostToolUse` on file edits — the edit is recorded, and if a neighbour changed the same file lately the
//!   session is told so;
//! - `Stop` and `SessionEnd` — recorded, so the others can tell who is working.
//!
//! Pult writes the records itself, so the model can't forget to. Files, all in Pult's data folder:
//! `journal.jsonl` (records, kept for three hours), `journal-seen.json` (what each session was already told),
//! `journal-status.json` (states of the sessions as the running Pult sees them: working, waiting, paused).
//! Only the subproject, the session, the file and the time are recorded — never the text of a prompt.

use std::{
    collections::BTreeMap,
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::i18n::{count, t};
use crate::projects::{Member, clean_about, members, one_line};

/// A neighbour's edit counts as "just now" within this many seconds.
const OVERLAP_WINDOW: u64 = 20 * 60;
/// Files a neighbour changed within this long are listed in the summary.
const RECENT_WINDOW: u64 = 2 * 60 * 60;
/// A session without news for this long isn't considered working.
const WORKING_WINDOW: u64 = 30 * 60;
/// The same overlap is not announced again to the same session sooner than this.
const REANNOUNCE: u64 = 5 * 60;
/// Records older than this are dropped when the file is trimmed.
const KEEP: u64 = 3 * 60 * 60;
/// The file is trimmed when it grows past this.
const MAX_BYTES: u64 = 1_000_000;
/// The running Pult's states are trusted for this long.
const STATUS_FRESH: u64 = 90;
/// Files listed per neighbour.
const FILES_SHOWN: usize = 5;

const HOOK_MARK: &str = " journal hook";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Start,
    Prompt,
    Edit,
    Stop,
    End,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    t: u64,
    key: String,
    session: String,
    kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Seen {
    /// Hash of the last summary each session was given.
    #[serde(default)]
    summary: BTreeMap<String, u64>,
    /// When a session was last told about an overlap: `session|neighbour|file` → time.
    #[serde(default)]
    warned: BTreeMap<String, u64>,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|time| time.as_secs()).unwrap_or(0)
}

fn journal_path(dir: &Path) -> PathBuf {
    dir.join("journal.jsonl")
}

fn seen_path(dir: &Path) -> PathBuf {
    dir.join("journal-seen.json")
}

fn status_path(dir: &Path) -> PathBuf {
    dir.join("journal-status.json")
}

// ───────────────────────────── paths ─────────────────────────────

/// The same file seen from the session's own worktree (`<repo>/.claude/worktrees/<name>/src/x.rs`) and from the
/// main checkout (`<repo>/src/x.rs`) is one file.
fn normalize(path: &str) -> String {
    const MARK: &str = "/.claude/worktrees/";
    if let Some(index) = path.find(MARK) {
        let rest = &path[index + MARK.len()..];
        if let Some(slash) = rest.find('/') {
            return format!("{}{}", &path[..index], &rest[slash..]);
        }
    }
    path.to_string()
}

/// The disk of a Mac ignores case: `~/Code/x` and `~/CODE/x` are one folder, so paths are compared that way.
fn same_path(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// A file for the summary: relative to the project folder, a line of plain text.
fn shown(file: &str, folder: &Path) -> String {
    let folder = folder.display().to_string();
    let folder = folder.trim_end_matches('/');
    let relative = match file.strip_prefix(folder) {
        Some(rest) => rest.to_string(),
        // The same folder written in another case (`/CODE/` and `/Code/`).
        None if file.to_lowercase().starts_with(&folder.to_lowercase()) => file.chars().skip(folder.chars().count()).collect(),
        None => file.to_string(),
    };
    let relative = if relative.len() < file.len() { relative.trim_start_matches('/').to_string() } else { relative };
    one_line(&relative).chars().take(120).collect()
}

fn plain(text: &str) -> String {
    one_line(text)
}

fn hash(text: &str) -> u64 {
    // FNV-1a: the same in every process and every version.
    let mut value = 0xcbf29ce484222325u64;
    for byte in text.bytes() {
        value ^= u64::from(byte);
        value = value.wrapping_mul(0x100000001b3);
    }
    value
}

// ───────────────────────────── files ─────────────────────────────

fn read_records(dir: &Path) -> Vec<Record> {
    fs::read_to_string(journal_path(dir))
        .map(|text| text.lines().filter_map(|line| serde_json::from_str::<Record>(line).ok()).collect())
        .unwrap_or_default()
}

fn append(dir: &Path, record: &Record) {
    let path = journal_path(dir);
    let _ = fs::create_dir_all(dir);
    if fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_BYTES) {
        let keep: Vec<String> = read_records(dir)
            .into_iter()
            .filter(|old| record.t.saturating_sub(old.t) <= KEEP)
            .filter_map(|old| serde_json::to_string(&old).ok())
            .collect();
        // Newest records only, and never more than half of the limit: the file must not stay big.
        let mut size = 0;
        let mut kept: Vec<&String> = keep
            .iter()
            .rev()
            .take_while(|line| {
                size += line.len() as u64 + 1;
                size <= MAX_BYTES / 2
            })
            .collect();
        kept.reverse();
        let text: String = kept.iter().map(|line| format!("{line}\n")).collect();
        write_atomic(&path, &text);
    }
    let Ok(line) = serde_json::to_string(record) else {
        return;
    };
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(format!("{line}\n").as_bytes());
    }
}

fn load_seen(dir: &Path) -> Seen {
    fs::read_to_string(seen_path(dir)).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

/// Write a file so that nobody ever reads half of it: a temp file of this process, then a rename.
fn write_atomic(path: &Path, text: &str) {
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    if fs::write(&temp, text).is_ok() && fs::rename(&temp, path).is_err() {
        let _ = fs::remove_file(&temp);
    }
}

fn save_seen(dir: &Path, seen: &mut Seen, at: u64) {
    seen.warned.retain(|_, when| at.saturating_sub(*when) <= KEEP);
    if let Ok(text) = serde_json::to_string(seen) {
        write_atomic(&seen_path(dir), &text);
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Status {
    at: u64,
    states: BTreeMap<String, String>,
}

/// The running Pult tells the hooks how it sees the sessions (it asks Claude Code; a hook can't).
/// `states` — project key → `working`, `waiting`, `idle`, `paused` or `remote`.
pub fn write_status(dir: &Path, states: &BTreeMap<String, &'static str>) {
    let status = Status { at: now(), states: states.iter().map(|(key, state)| (key.clone(), (*state).to_string())).collect() };
    let Ok(text) = serde_json::to_string(&status) else {
        return;
    };
    // The time is what changes every time: write only when the states changed or the file is getting stale.
    if let Some(old) = fs::read_to_string(status_path(dir)).ok().and_then(|text| serde_json::from_str::<Status>(&text).ok())
        && old.states == status.states
        && status.at.saturating_sub(old.at) < STATUS_FRESH / 3
    {
        return;
    }
    let _ = fs::create_dir_all(dir);
    write_atomic(&status_path(dir), &text);
}

fn fresh_status(dir: &Path, at: u64) -> Option<Status> {
    let status = fs::read_to_string(status_path(dir)).ok().and_then(|text| serde_json::from_str::<Status>(&text).ok())?;
    (at.saturating_sub(status.at) <= STATUS_FRESH).then_some(status)
}

// ───────────────────────────── what a session is told ─────────────────────────────

/// How a neighbour is doing: from Pult's states while it is running, otherwise from the records.
fn state_of(member: &Member, status: &Option<Status>, records: &[Record], at: u64) -> &'static str {
    if let Some(state) = status.as_ref().and_then(|status| status.states.get(&member.key)) {
        return match state.as_str() {
            "working" => "working",
            "waiting" => "waiting",
            "paused" => "paused",
            "remote" => "remote",
            _ => "idle",
        };
    }
    let last = records.iter().rev().find(|record| record.key == member.key && Some(&record.session) == member.session.as_ref());
    match last {
        Some(record) if matches!(record.kind, Kind::Prompt | Kind::Edit) && at.saturating_sub(record.t) <= WORKING_WINDOW => "working",
        _ => "idle",
    }
}

fn state_word(state: &str) -> &'static str {
    match state {
        "working" => t("journal.state_working"),
        "waiting" => t("journal.state_waiting"),
        "paused" => t("journal.state_paused"),
        "remote" => t("journal.state_remote"),
        _ => t("journal.state_idle"),
    }
}

fn ago(seconds: u64) -> String {
    let minutes = seconds / 60;
    if minutes == 0 { t("journal.just_now").into() } else { count(minutes, "journal.minutes_ago") }
}

/// The subprojects that run next to `me`: same folder, a session pinned.
fn neighbours<'a>(all: &'a [Member], me: &Member) -> Vec<&'a Member> {
    all.iter()
        .filter(|other| other.folder == me.folder && other.key != me.key && other.sub.is_some() && other.session.is_some() && other.session != me.session)
        .collect()
}

fn footer() -> String {
    t("journal.footer").into()
}

/// The summary of the neighbours: who works, on what, which files they changed in the last two hours.
/// `stable` leaves out what flips all the time (the states), the files in sorted order, and the rules: this form
/// is what decides whether the session has to be told again.
fn summary_text(dir: &Path, me: &Member, all: &[Member], at: u64, stable: bool) -> String {
    let near = neighbours(all, me);
    if near.is_empty() {
        return String::new();
    }
    let records = read_records(dir);
    let status = fresh_status(dir, at);
    let mut lines = vec![t("journal.header").to_string()];
    for other in near {
        let state = state_of(other, &status, &records, at);
        let name = plain(other.sub.as_deref().unwrap_or_default());
        let about = clean_about(&other.about);
        let mut line = if stable { format!("• «{name}»") } else { format!("• «{name}» ({})", state_word(state)) };
        if !about.is_empty() {
            line.push_str(&format!(" — {about}"));
        }
        lines.push(line);
        let mut files: Vec<String> = Vec::new();
        for record in records.iter().rev().filter(|record| record.kind == Kind::Edit && record.key == other.key) {
            if at.saturating_sub(record.t) > RECENT_WINDOW {
                break;
            }
            let file = shown(record.file.as_deref().unwrap_or_default(), &other.path);
            if !files.contains(&file) {
                files.push(file);
            }
            if files.len() == FILES_SHOWN {
                break;
            }
        }
        if stable {
            files.sort();
        }
        if !files.is_empty() {
            lines.push(format!("  {}", tr!("journal.edited_in_two_hours", files = files.join(", "))));
        }
    }
    if !stable {
        lines.push(footer());
    }
    lines.join("\n")
}

/// What the key's session knows about its neighbours — also shown in the session panel. Empty: not a
/// subproject, or no neighbours.
pub fn summary(dir: &Path, key: &str) -> String {
    let Some(all) = members(&dir.join("state.json")) else {
        return String::new();
    };
    let Some(me) = all.iter().find(|member| member.key == key && member.sub.is_some()) else {
        return String::new();
    };
    summary_text(dir, me, &all, now(), false)
}

/// The neighbours that changed `file` lately: (name, state, seconds ago, what it does).
fn overlaps(dir: &Path, me: &Member, all: &[Member], file: &str, at: u64) -> Vec<(Member, &'static str, u64)> {
    let records = read_records(dir);
    let status = fresh_status(dir, at);
    neighbours(all, me)
        .into_iter()
        .filter_map(|other| {
            let last = records
                .iter()
                .rev()
                .find(|record| record.kind == Kind::Edit && record.key == other.key && record.file.as_deref().is_some_and(|other| same_path(other, file)))?;
            let age = at.saturating_sub(last.t);
            (age <= OVERLAP_WINDOW).then(|| {
                let copy = Member {
                    key: other.key.clone(),
                    folder: other.folder.clone(),
                    sub: other.sub.clone(),
                    session: other.session.clone(),
                    about: other.about.clone(),
                    path: other.path.clone(),
                };
                (copy, state_of(other, &status, &records, at), age)
            })
        })
        .collect()
}

// ───────────────────────────── the hook ─────────────────────────────

/// The text the session gets for the event, if any. `dir` is Pult's data folder.
fn handle(dir: &Path, event: &Value, at: u64) -> Option<(String, String)> {
    let name = event.get("hook_event_name").and_then(Value::as_str)?;
    let session = event.get("session_id").and_then(Value::as_str)?;
    let all = members(&dir.join("state.json"))?;
    let me = all.iter().find(|member| member.sub.is_some() && member.session.as_deref() == Some(session))?;
    let record = |kind: Kind, file: Option<String>| Record { t: at, key: me.key.clone(), session: session.to_string(), kind, file };
    let mut seen = load_seen(dir);
    let context = match name {
        "SessionStart" => {
            append(dir, &record(Kind::Start, None));
            let text = summary_text(dir, me, &all, at, false);
            seen.summary.insert(session.to_string(), hash(&summary_text(dir, me, &all, at, true)));
            save_seen(dir, &mut seen, at);
            text
        }
        "UserPromptSubmit" => {
            append(dir, &record(Kind::Prompt, None));
            let key = hash(&summary_text(dir, me, &all, at, true));
            let new = seen.summary.get(session) != Some(&key);
            seen.summary.insert(session.to_string(), key);
            save_seen(dir, &mut seen, at);
            if new { summary_text(dir, me, &all, at, false) } else { String::new() }
        }
        "PostToolUse" => {
            let tool = event.get("tool_name").and_then(Value::as_str).unwrap_or_default();
            if !matches!(tool, "Edit" | "Write" | "MultiEdit") {
                return None;
            }
            let file = normalize(event.get("tool_input").and_then(|input| input.get("file_path")).and_then(Value::as_str)?);
            let found = overlaps(dir, me, &all, &file, at);
            append(dir, &record(Kind::Edit, Some(file.clone())));
            let mut lines = Vec::new();
            for (other, state, age) in found {
                let key = format!("{session}|{}|{file}", other.key);
                if seen.warned.get(&key).is_some_and(|when| at.saturating_sub(*when) < REANNOUNCE) {
                    continue;
                }
                seen.warned.insert(key, at);
                let about = clean_about(&other.about);
                lines.push(tr!(
                    "journal.overlap",
                    file = shown(&file, &me.path),
                    ago = ago(age),
                    name = plain(other.sub.as_deref().unwrap_or_default()),
                    state = state_word(state),
                    about = if about.is_empty() { String::new() } else { format!(" — {about}") }
                ));
            }
            if lines.is_empty() {
                return None;
            }
            save_seen(dir, &mut seen, at);
            lines.push(footer());
            lines.join("\n")
        }
        "Stop" | "SubagentStop" => {
            if name == "Stop" {
                append(dir, &record(Kind::Stop, None));
            }
            return None;
        }
        "SessionEnd" => {
            append(dir, &record(Kind::End, None));
            seen.summary.remove(session);
            save_seen(dir, &mut seen, at);
            return None;
        }
        _ => return None,
    };
    (!context.is_empty()).then(|| (name.to_string(), context))
}

/// What Claude Code expects on stdout: text added to the model's context.
fn output(name: &str, context: &str) -> String {
    json!({"hookSpecificOutput": {"hookEventName": name, "additionalContext": context}}).to_string()
}

/// `pult journal hook`: reads the event from stdin, prints the text for the session (if any). Never fails and
/// never blocks: whatever goes wrong, the session just gets nothing.
pub fn hook(dir: &Path) {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let Ok(event) = serde_json::from_str::<Value>(&input) else {
        return;
    };
    if let Some((name, context)) = handle(dir, &event, now()) {
        println!("{}", output(&name, &context));
    }
}

/// Records hold project keys and absolute paths; after a rename or a move of a folder they would point nowhere, and
/// the advisory window is short anyway — start the journal afresh.
pub fn reset(dir: &Path) {
    let _ = fs::remove_file(journal_path(dir));
    let _ = fs::remove_file(seen_path(dir));
}

/// Pult's data folder: `~/Library/Application Support/pult`.
pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_default().join("pult")
}

/// `pult journal …` — without a window. Returns the exit code.
pub fn cli(args: &[String]) -> i32 {
    let dir = data_dir();
    match args.first().map(String::as_str) {
        Some("hook") => hook(&dir),
        Some("install-hooks") => {
            let paths = crate::sync::Paths::real();
            let exe = crate::sync::hook_exe();
            if !supports_commands(&exe) {
                eprintln!("{} is too old for the journal hooks", exe.display());
                return 1;
            }
            match install_hooks(&paths, &exe) {
                Ok(changed) => println!("{}", if changed { "hooks installed" } else { "hooks are already installed" }),
                Err(error) => {
                    eprintln!("{error}");
                    return 1;
                }
            }
        }
        Some("uninstall-hooks") => match uninstall_hooks(&crate::sync::Paths::real()) {
            Ok(changed) => println!("{}", if changed { "hooks removed" } else { "there were no hooks" }),
            Err(error) => {
                eprintln!("{error}");
                return 1;
            }
        },
        Some("status") => {
            let all = members(&dir.join("state.json")).unwrap_or_default();
            for member in all.iter().filter(|member| member.sub.is_some()) {
                println!("{}\n{}\n", member.key, summary(&dir, &member.key));
            }
        }
        _ => {
            eprintln!("pult journal hook | status | install-hooks | uninstall-hooks");
            return 2;
        }
    }
    0
}

// ───────────────────────────── installing the hooks ─────────────────────────────

/// (event, tool matcher) pairs the journal listens to.
const EVENTS: [(&str, Option<&str>); 5] = [
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    ("PostToolUse", Some("Edit|Write|MultiEdit")),
    ("Stop", None),
    ("SessionEnd", None),
];

fn is_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| hooks.iter().any(|hook| hook.get("command").and_then(Value::as_str).is_some_and(|command| command.ends_with(HOOK_MARK))))
}

/// Write the journal hooks into Claude Code settings next to others' (and next to the sync hooks).
/// `false` — already there.
fn merge_hooks(settings: &mut Value, command: &str) -> bool {
    if !settings.is_object() {
        *settings = json!({});
    }
    let hooks = settings.as_object_mut().unwrap().entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let mut changed = false;
    for (event, matcher) in EVENTS {
        let groups = hooks.as_object_mut().unwrap().entry(event).or_insert_with(|| json!([]));
        if !groups.is_array() {
            *groups = json!([]);
        }
        let groups = groups.as_array_mut().unwrap();
        let mut ours = json!({"hooks": [{"type": "command", "command": command, "timeout": 5}]});
        if let Some(matcher) = matcher {
            ours["matcher"] = json!(matcher);
        }
        match groups.iter().position(is_ours) {
            Some(index) if groups[index] == ours => {}
            Some(index) => {
                groups[index] = ours;
                changed = true;
            }
            None => {
                groups.push(ours);
                changed = true;
            }
        }
    }
    changed
}

fn remove_hooks(settings: &mut Value) -> bool {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for groups in hooks.values_mut().filter_map(Value::as_array_mut) {
        let before = groups.len();
        groups.retain(|group| !is_ours(group));
        changed |= groups.len() != before;
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|groups| !groups.is_empty()));
    changed
}

pub fn install_hooks(paths: &crate::sync::Paths, exe: &Path) -> Result<bool, String> {
    let command = format!("'{}'{HOOK_MARK}", exe.display());
    crate::sync::edit_settings(paths, |settings| merge_hooks(settings, &command))
}

pub fn uninstall_hooks(paths: &crate::sync::Paths) -> Result<bool, String> {
    crate::sync::edit_settings(paths, remove_hooks)
}

/// Whether the Pult at `exe` knows `pult journal hook`: this very program, or an installed app that is new enough.
/// A Pult without the command opens a window when a hook calls it, so the hooks must never point at one. The app
/// is not run to find out (that is the very thing to avoid): its version is read from `Info.plist`.
pub(crate) fn supports_commands(exe: &Path) -> bool {
    if std::env::current_exe().is_ok_and(|current| current == exe) {
        return true;
    }
    let Some(plist) = exe.parent().and_then(Path::parent).map(|contents| contents.join("Info.plist")) else {
        return false;
    };
    let Ok(text) = fs::read_to_string(plist) else {
        return false;
    };
    let Some(after) = text.split("<key>CFBundleShortVersionString</key>").nth(1) else {
        return false;
    };
    let version = after.split("<string>").nth(1).and_then(|rest| rest.split("</string>").next()).unwrap_or_default().trim().to_string();
    !version.is_empty() && !crate::update::newer(env!("CARGO_PKG_VERSION"), &version)
}

/// Pult puts the hooks in as soon as a project is split into subprojects (no server needed), if the Pult they
/// would call can handle them; otherwise the next launch (after an update) does it.
pub fn ensure_hooks() {
    let exe = crate::sync::hook_exe();
    if supports_commands(&exe) {
        let _ = install_hooks(&crate::sync::Paths::real(), &exe);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pult-journal-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// state.json with MAPLE split into API / Web / Docs (the last one without a session).
    fn project(dir: &Path) -> PathBuf {
        let folder = dir.join("MAPLE");
        fs::create_dir_all(&folder).unwrap();
        let state = json!({"root": dir, "projects": [
            {"path": folder, "sub": "API", "session": "s-api", "about": "Screens and forms"},
            {"path": folder, "sub": "Web", "session": "s-web", "about": "API and data"},
            {"path": folder, "sub": "Docs"},
            {"path": dir.join("SOLO"), "session": "s-solo"}
        ]});
        fs::write(dir.join("state.json"), state.to_string()).unwrap();
        folder
    }

    fn event(name: &str, session: &str, file: Option<&str>) -> Value {
        let mut event = json!({"hook_event_name": name, "session_id": session});
        if let Some(file) = file {
            event["tool_name"] = json!("Edit");
            event["tool_input"] = json!({"file_path": file});
        }
        event
    }

    #[test]
    fn worktree_paths_are_the_same_file() {
        assert_eq!(normalize("/p/MAPLE/repo/.claude/worktrees/job-1/src/a.rs"), "/p/MAPLE/repo/src/a.rs");
        assert_eq!(normalize("/p/MAPLE/repo/src/a.rs"), "/p/MAPLE/repo/src/a.rs");
        assert_eq!(normalize("/p/.claude/worktrees/x"), "/p/.claude/worktrees/x");
        assert_eq!(shown("/p/MAPLE/src/a.rs", Path::new("/p/MAPLE")), "src/a.rs");
        // The same folder in another case, as on a Mac disk that ignores case.
        assert_eq!(shown("/home/user/CODE/MAPLE/src/a.rs", Path::new("/home/user/Code/MAPLE")), "src/a.rs");
        assert_eq!(shown("/elsewhere/a.rs", Path::new("/p/MAPLE")), "/elsewhere/a.rs");
        assert!(same_path("/home/user/CODE/MAPLE/a.rs", "/home/user/Code/MAPLE/a.rs"));
        assert!(!same_path("/p/a.rs", "/p/b.rs"));
    }

    #[test]
    fn a_session_of_an_ordinary_project_gets_nothing() {
        let dir = temp("ordinary");
        project(&dir);
        assert!(handle(&dir, &event("SessionStart", "s-solo", None), 1000).is_none());
        assert!(handle(&dir, &event("SessionStart", "stranger", None), 1000).is_none());
        assert!(!journal_path(&dir).exists());
    }

    #[test]
    fn a_session_is_told_about_its_neighbours_once() {
        let dir = temp("summary");
        let folder = project(&dir);
        let file = folder.join("src/report.rs").display().to_string();
        // API worked on report.rs ten minutes ago.
        handle(&dir, &event("SessionStart", "s-api", None), 1000);
        handle(&dir, &event("PostToolUse", "s-api", Some(&file)), 1100);
        // Web starts: it is told about API (working, what it does, the file), not about Docs (no session).
        let (name, text) = handle(&dir, &event("SessionStart", "s-web", None), 1700).unwrap();
        assert_eq!(name, "SessionStart");
        assert!(text.contains("«API»") && text.contains("Screens and forms"), "{text}");
        assert!(text.contains("src/report.rs"), "{text}");
        assert!(!text.contains("Docs"), "{text}");
        assert!(text.contains(crate::i18n::t("journal.state_working")), "{text}");
        // The prompt right after: nothing changed, so nothing is repeated.
        assert!(handle(&dir, &event("UserPromptSubmit", "s-web", None), 1710).is_none());
        // API edits another file: the summary changed, the next prompt carries it.
        handle(&dir, &event("PostToolUse", "s-api", Some(&folder.join("src/stock.rs").display().to_string())), 1800);
        let (_, text) = handle(&dir, &event("UserPromptSubmit", "s-web", None), 1900).unwrap();
        assert!(text.contains("src/stock.rs"), "{text}");
        // What the prompt said is never recorded.
        let journal = fs::read_to_string(journal_path(&dir)).unwrap();
        assert!(!journal.contains("prompt\":"), "{journal}");
    }

    #[test]
    fn an_overlap_is_announced_then_not_repeated_for_a_while() {
        let dir = temp("overlap");
        let folder = project(&dir);
        let file = folder.join("repo/src/report.rs").display().to_string();
        let worktree = folder.join("repo/.claude/worktrees/job-9/src/report.rs").display().to_string();
        handle(&dir, &event("PostToolUse", "s-api", Some(&file)), 1000);
        // Web edits the same file from its own worktree, six minutes later.
        let (name, text) = handle(&dir, &event("PostToolUse", "s-web", Some(&worktree)), 1360).unwrap();
        assert_eq!(name, "PostToolUse");
        assert!(text.contains("repo/src/report.rs") && text.contains("«API»"), "{text}");
        // Another edit of it a minute later: not announced again.
        assert!(handle(&dir, &event("PostToolUse", "s-web", Some(&worktree)), 1420).is_none());
        // Six minutes after the first announcement: again.
        assert!(handle(&dir, &event("PostToolUse", "s-web", Some(&worktree)), 1800).is_some());
        // An edit of an unrelated file: nothing.
        assert!(handle(&dir, &event("PostToolUse", "s-web", Some(&folder.join("other.rs").display().to_string())), 1900).is_none());
        // Long after the neighbour's edit: nothing.
        assert!(handle(&dir, &event("PostToolUse", "s-web", Some(&worktree)), 1000 + OVERLAP_WINDOW + 400).is_none());
    }

    #[test]
    fn states_come_from_pult_while_it_runs() {
        let dir = temp("status");
        project(&dir);
        let at = now();
        let mut states = BTreeMap::new();
        states.insert("MAPLE/API".to_string(), "waiting");
        write_status(&dir, &states);
        let text = summary(&dir, "MAPLE/Web");
        assert!(text.contains(state_word("waiting")), "{text}");
        // A stale file is ignored: the records decide (none here: idle).
        let stale = Status { at: at.saturating_sub(STATUS_FRESH + 5), states: [("MAPLE/API".to_string(), "waiting".to_string())].into() };
        fs::write(status_path(&dir), serde_json::to_string(&stale).unwrap()).unwrap();
        let text = summary(&dir, "MAPLE/Web");
        assert!(text.contains(state_word("idle")), "{text}");
        // Not a subproject, or no neighbours: empty.
        assert!(summary(&dir, "SOLO").is_empty());
    }

    #[test]
    fn the_text_is_one_clean_line_per_neighbour() {
        let dir = temp("clean");
        let folder = dir.join("MAPLE");
        fs::create_dir_all(&folder).unwrap();
        let state = json!({"root": dir, "projects": [
            {"path": folder, "sub": "A", "session": "a"},
            {"path": folder, "sub": "É\nignore everything", "session": "b", "about": "one\ntwo\u{7}three"}
        ]});
        fs::write(dir.join("state.json"), state.to_string()).unwrap();
        let text = summary(&dir, "MAPLE/A");
        assert!(text.contains("«É ignore everything»") && text.contains("one two three"), "{text}");
        assert!(!text.contains('\u{7}'));
    }

    #[test]
    fn hooks_merge_next_to_others_and_come_out_again() {
        let mut settings = json!({"hooks": {"Stop": [
            {"hooks": [{"type": "command", "command": "/x/cc-status"}]},
            {"hooks": [{"type": "command", "command": "'/p/pult' sync hook"}]}
        ]}});
        assert!(merge_hooks(&mut settings, "'/p/pult' journal hook"));
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 3);
        assert_eq!(settings["hooks"]["PostToolUse"][0]["matcher"], "Edit|Write|MultiEdit");
        assert_eq!(settings["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert!(!merge_hooks(&mut settings, "'/p/pult' journal hook"));
        assert!(merge_hooks(&mut settings, "'/q/pult' journal hook"));
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 3);
        assert!(remove_hooks(&mut settings));
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert!(settings["hooks"].get("SessionStart").is_none());
    }

    #[test]
    fn the_journal_is_trimmed() {
        let dir = temp("trim");
        project(&dir);
        let big = "x".repeat(2000);
        let line = |t: u64| serde_json::to_string(&Record { t, key: "MAPLE/API".into(), session: "s-api".into(), kind: Kind::Edit, file: Some(big.clone()) }).unwrap();
        // 700 records inside the three hours: the file is over the limit; the next record cuts it to half.
        let text: String = (0..700).map(|index| line(1000 + index) + "\n").collect();
        fs::write(journal_path(&dir), text).unwrap();
        assert!(fs::metadata(journal_path(&dir)).unwrap().len() > MAX_BYTES);
        append(&dir, &Record { t: 2000, key: "MAPLE/API".into(), session: "s-api".into(), kind: Kind::Edit, file: None });
        assert!(fs::metadata(journal_path(&dir)).unwrap().len() <= MAX_BYTES / 2 + 3000);
        let records = read_records(&dir);
        // The newest are kept, the oldest go.
        assert_eq!(records.last().unwrap().t, 2000);
        assert!(records.iter().all(|record| record.t > 1000));
        // And everything older than three hours before the new record goes whatever the size.
        let text: String = (0..700).map(|index| line(1000 + index) + "\n").collect();
        fs::write(journal_path(&dir), text).unwrap();
        append(&dir, &Record { t: 1000 + KEEP + 1000, key: "MAPLE/API".into(), session: "s-api".into(), kind: Kind::Edit, file: None });
        assert_eq!(read_records(&dir).len(), 1);
    }

    #[test]
    fn a_state_flip_does_not_repeat_the_summary() {
        let dir = temp("stable");
        let folder = project(&dir);
        let file = folder.join("src/a.rs").display().to_string();
        handle(&dir, &event("PostToolUse", "s-api", Some(&file)), 1000);
        assert!(handle(&dir, &event("SessionStart", "s-web", None), 1100).is_some());
        // The neighbour starts, stops, starts again: its state flips, the facts are the same.
        handle(&dir, &event("UserPromptSubmit", "s-api", None), 1200);
        assert!(handle(&dir, &event("UserPromptSubmit", "s-web", None), 1210).is_none());
        handle(&dir, &event("Stop", "s-api", None), 1300);
        assert!(handle(&dir, &event("UserPromptSubmit", "s-web", None), 1310).is_none());
        // A bare session start is not work.
        handle(&dir, &event("SessionEnd", "s-api", None), 1320);
        handle(&dir, &event("SessionStart", "s-api", None), 1330);
        let records = read_records(&dir);
        let all = members(&dir.join("state.json")).unwrap();
        let front = all.iter().find(|member| member.key == "MAPLE/API").unwrap();
        assert_eq!(state_of(front, &None, &records, 1340), "idle");
    }

    #[test]
    fn hidden_characters_do_not_travel() {
        assert_eq!(one_line("ok\u{202E}\u{200B}\u{E0049}x\u{2028}y"), "okx y");
        let dir = temp("hidden");
        let folder = dir.join("MAPLE");
        fs::create_dir_all(&folder).unwrap();
        let state = json!({"root": dir, "projects": [
            {"path": folder, "sub": "A", "session": "a"},
            {"path": folder, "sub": "B", "session": "b", "about": "ok\u{202E}\u{E0049}\u{200B}x"}
        ]});
        fs::write(dir.join("state.json"), state.to_string()).unwrap();
        assert!(summary(&dir, "MAPLE/A").contains("— okx"));
    }

    #[test]
    fn a_session_pinned_to_two_subprojects_is_not_its_own_neighbour() {
        let dir = temp("twice");
        let folder = dir.join("MAPLE");
        fs::create_dir_all(&folder).unwrap();
        let state = json!({"root": dir, "projects": [
            {"path": folder, "sub": "A", "session": "same"},
            {"path": folder, "sub": "B", "session": "same"}
        ]});
        fs::write(dir.join("state.json"), state.to_string()).unwrap();
        assert!(summary(&dir, "MAPLE/A").is_empty());
    }

    #[test]
    fn hooks_never_point_at_a_pult_without_the_command() {
        let dir = temp("support");
        let app = dir.join("Pult.app/Contents");
        fs::create_dir_all(app.join("MacOS")).unwrap();
        let exe = app.join("MacOS/pult");
        let plist = |version: &str| format!("<dict><key>CFBundleShortVersionString</key>\n<string>{version}</string></dict>");
        fs::write(app.join("Info.plist"), plist("0.0.9")).unwrap();
        assert!(!supports_commands(&exe));
        fs::write(app.join("Info.plist"), plist(env!("CARGO_PKG_VERSION"))).unwrap();
        assert!(supports_commands(&exe));
        assert!(!supports_commands(&dir.join("nowhere/MacOS/pult")));
    }
}
