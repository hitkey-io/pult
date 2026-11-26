//! Moving a folder (the main one or a project's) together with everything Claude Code remembers about it by path.
//!
//! Claude Code stores absolute paths in many places, and each must be fixed, otherwise something quietly
//! breaks after the move:
//! - transcript and memory — `~/.claude/projects/<path with everything except letters and digits as "-">`;
//! - trust, approved MCP servers, the session's current git worktree (`activeWorktreeSession`) and
//!   repository paths — `~/.claude.json` (both keys and values);
//! - plugins enabled for the project — `~/.claude/plugins/installed_plugins.json` (`projectPath`);
//! - a background session — `~/.claude/jobs/<id>/state.json` (working folder, worktree, name, flags);
//! - the git worktree the session works in — the `worktree-state` line in the transcript: without the
//!   fix the session starts not in its own worktree but in the repository's main folder;
//! - the git worktrees themselves — absolute links `.git/…/worktrees/<name>/gitdir` ↔ the `.git` file.
//!
//! Steps: pause the affected background sessions → backup → the folder on disk → everything listed above
//! → the same sessions (`claude respawn`: same id, name, flags, worktree) now in the new folder.
//! `claude rm` is not used: for a session with a worktree it deletes the worktree itself.
//!
//! Steps run one at a time (`run_step`); between them the window shows progress. An error before the last
//! step — `rollback`: everything goes back as it was and the sessions continue in the old place.

use std::{
    fs,
    io::{BufRead as _, BufReader, ErrorKind, Write as _},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::i18n::t;
use crate::sessions::{self, Agent};

/// Move steps — labels for the window.
pub fn step_labels(move_folder: bool, many: bool) -> Vec<&'static str> {
    let mut steps = vec![
        if many {
            t("migrate.pausing_sessions")
        } else {
            t("migrate.pausing_session")
        },
        t("migrate.saving_backup"),
    ];
    if move_folder {
        steps.push(t("migrate.moving_folder_on_disk"));
    }
    steps.push(t("migrate.moving_claude_code_transcripts_memory"));
    steps.push(if many { t("migrate.continuing_sessions") } else { t("migrate.continuing_session") });
    steps
}

/// A background Claude Code session affected by the move.
#[derive(Clone, Debug)]
struct Job {
    short: String,
    /// Process before the move: if it was running, it is started again after the move; otherwise it stays as
    /// it was (paused, unloaded, crashed).
    pid: Option<u32>,
    state_path: PathBuf,
    /// The transcript (`<id>.jsonl`) before the move.
    transcript: Option<PathBuf>,
}

pub struct Move {
    from: PathBuf,
    to: PathBuf,
    /// Rename the folder on disk. `false` — the folder was already moved by hand ("Locate folder…").
    move_folder: bool,
    /// Session names before and after (`<project>-main`, `<project>-<subproject>`), if the project name
    /// changes.
    names: Vec<(String, String)>,
    /// The disk is case-insensitive: `~/CODE/x` and `~/Code/x` are the same, and old records
    /// in any case must be fixed too.
    fold: bool,
    jobs: Vec<Job>,
    backup: Option<PathBuf>,
    folder_moved: bool,
    /// Moved folders and files of `~/.claude/projects/…`: from where to where (for rollback).
    claude_dirs: Vec<(PathBuf, PathBuf)>,
    /// Top-level data folders: old name → new name (for transcript paths in `state.json`).
    data_names: Vec<(String, String)>,
    /// A file appended to the end of the same file at the new place (the exiting process managed to create it
    /// at the old path): source, destination, destination length before appending.
    merged: Vec<(PathBuf, PathBuf, u64)>,
    /// Original contents of rewritten files (for rollback).
    originals: Vec<(PathBuf, Vec<u8>)>,
    /// Transcripts that had a line appended: length before appending.
    appended: Vec<(PathBuf, u64)>,
}

/// Folder `to` is taken by another folder. On a Mac the disk is usually case-insensitive: `~/CODE` is
/// the same `~/Projects`, and changing only the case is an ordinary rename, not a taken place.
pub fn taken(from: &Path, to: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (fs::symlink_metadata(from), fs::symlink_metadata(to)) {
        (Ok(a), Ok(b)) => !(a.dev() == b.dev() && a.ino() == b.ino()),
        (_, Ok(_)) => true,
        _ => false,
    }
}

/// A path inside the folder (or the folder itself).
pub fn is_under(path: &Path, dir: &Path) -> bool {
    path.starts_with(dir)
}

/// How Claude Code names a project folder in `~/.claude/projects`.
pub fn encode(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|char| if char.is_ascii_alphanumeric() { char } else { '-' })
        .collect()
}

/// The disk the folder is on is case-insensitive.
fn folds_case(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let text = dir.to_string_lossy();
    let swapped: String = text
        .chars()
        .map(|char| if char.is_ascii_uppercase() { char.to_ascii_lowercase() } else { char.to_ascii_uppercase() })
        .collect();
    if swapped == text {
        return false;
    }
    match (fs::metadata(dir), fs::metadata(&swapped)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

fn move_data_error(error: &std::io::Error) -> String {
    tr!("migrate.couldn_t_move_claude_code", error = error)
}

fn read_data_error(error: &std::io::Error) -> String {
    tr!("migrate.couldn_t_read_claude_code", error = error)
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

fn claude_json() -> PathBuf {
    home().join(".claude.json")
}

fn data_root() -> PathBuf {
    home().join(".claude").join("code")
}

/// Claude Code sessions open not in the background (in another terminal or window) inside the folder. They
/// can't be stopped from outside: no moving while they are open. `ours` — pids of Pult's terminals.
pub fn blockers(agents: &[Agent], dir: &Path, ours: &[u32]) -> Vec<PathBuf> {
    agents
        .iter()
        .filter(|agent| !agent.is_background() && agent.is_running())
        .filter(|agent| agent.pid.is_none_or(|pid| !ours.contains(&pid)))
        .filter_map(|agent| agent.cwd.clone())
        .filter(|cwd| is_under(cwd, dir))
        .collect()
}

impl Move {
    pub fn new(from: PathBuf, to: PathBuf, move_folder: bool, names: Vec<(String, String)>) -> Self {
        let fold = folds_case(if from.exists() { &from } else { &to });
        Self {
            from,
            to,
            move_folder,
            names,
            fold,
            jobs: Vec::new(),
            backup: None,
            folder_moved: false,
            claude_dirs: Vec::new(),
            data_names: Vec::new(),
            merged: Vec::new(),
            originals: Vec::new(),
            appended: Vec::new(),
        }
    }

    pub fn steps(&self) -> usize {
        if self.move_folder { 5 } else { 4 }
    }

    /// The last step is continuing the sessions; its errors don't require a rollback.
    pub fn is_last(&self, step: usize) -> bool {
        step + 1 == self.steps()
    }

    /// The new spelling of a path, if it is inside the folder being moved and changes.
    fn remap(&self, value: &str) -> Option<String> {
        let (from, to) = (self.from.to_str()?, self.to.to_str()?);
        let head = value.get(..from.len())?;
        let rest = &value[from.len()..];
        let same = if self.fold { head.to_lowercase() == from.to_lowercase() } else { head == from };
        if !same || !(rest.is_empty() || rest.starts_with('/')) {
            return None;
        }
        let new = format!("{to}{rest}");
        (new != value).then_some(new)
    }

    #[cfg(test)]
    fn moved(&self, path: &Path) -> PathBuf {
        path.to_str().and_then(|text| self.remap(text)).map(PathBuf::from).unwrap_or_else(|| path.to_path_buf())
    }

    fn needs_move(&self, path: &Path) -> bool {
        path.to_str().is_some_and(|text| self.remap(text).is_some())
    }

    /// Fix all path strings inside JSON. `true` — something changed.
    fn remap_json(&self, value: &mut Value) -> bool {
        match value {
            Value::String(text) => match self.remap(text) {
                Some(new) => {
                    *text = new;
                    true
                }
                None => false,
            },
            Value::Array(items) => items.iter_mut().fold(false, |changed, item| self.remap_json(item) | changed),
            Value::Object(map) => map.values_mut().fold(false, |changed, item| self.remap_json(item) | changed),
            _ => false,
        }
    }

    /// Run a step. For the last step `Err` is the list of sessions that didn't start.
    pub fn run_step(&mut self, step: usize) -> Result<(), String> {
        let step = if !self.move_folder && step >= 2 { step + 1 } else { step };
        match step {
            0 => self.pause(),
            1 => self.save_backup(),
            2 => self.rename_folder(),
            3 => self.move_claude_data(),
            _ => self.resume(),
        }
    }

    /// Check there are no open sessions; stop the affected background ones (the transcript stays).
    fn pause(&mut self) -> Result<(), String> {
        if self.move_folder && taken(&self.from, &self.to) {
            return Err(tr!("common.folder_to_already_exists", to = crate::projects::display_path(&self.to)));
        }
        let open = |agents: &[Agent]| -> Option<PathBuf> {
            agents
                .iter()
                .filter(|agent| !agent.is_background() && agent.is_running())
                .filter_map(|agent| agent.cwd.clone())
                .find(|cwd| self.needs_move(cwd))
        };
        // Pult's terminals were just closed — the Claude processes in them don't exit right away.
        let mut agents = sessions::agents();
        for _ in 0..5 {
            if open(&agents).is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(600));
            agents = sessions::agents();
        }
        if let Some(cwd) = open(&agents) {
            return Err(tr!("migrate.claude_code_is_open_in", cwd = crate::projects::display_path(&cwd)));
        }
        for agent in agents.iter().filter(|agent| agent.is_background()) {
            let Some(short) = agent.id.clone() else {
                continue;
            };
            let state_path = sessions::job_state_path(&short);
            let Some(state) = read_json(&state_path) else {
                continue;
            };
            let transcript = transcript_of(&state, &agent.session_id);
            if !self.job_needs_fix(&state, transcript.as_deref()) {
                continue;
            }
            self.jobs.push(Job { short, pid: agent.pid, state_path, transcript });
        }
        for job in self.jobs.iter().filter(|job| job.pid.is_some()) {
            sessions::stop(&job.short);
        }
        // On exit, Claude appends service lines to the transcript at the old path. Move only after the
        // processes have finished, otherwise a new folder grows under the old name.
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.jobs.iter().filter_map(|job| job.pid).any(alive) {
            if Instant::now() > deadline {
                return Err(t("migrate.claude_didn_t_stop_within")
                .to_string());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Ok(())
    }

    /// The move affects the session: its paths, name or worktree in the transcript change.
    fn job_needs_fix(&self, state: &Value, transcript: Option<&Path>) -> bool {
        let mut copy = state.clone();
        if self.remap_json(&mut copy) {
            return true;
        }
        if let Some(link) = state.get("linkScanPath").and_then(Value::as_str)
            && self.moved_data_path(Path::new(link)) != Path::new(link)
        {
            return true;
        }
        transcript.and_then(|path| self.worktree_fix(path)).is_some()
    }

    fn save_backup(&mut self) -> Result<(), String> {
        let dir = dirs::data_dir()
            .ok_or(t("migrate.pult_s_data_folder_wasn"))?
            .join("pult")
            .join("backups");
        fs::create_dir_all(&dir)
            .map_err(|error| tr!("migrate.couldn_t_create_backup_folder", error = error))?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_secs())
            .unwrap_or_default();
        // The last ten copies are kept.
        if let Ok(entries) = fs::read_dir(&dir) {
            let mut old: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("claude-")))
                .collect();
            old.sort();
            let extra = old.len().saturating_sub(9);
            for path in old.into_iter().take(extra) {
                let _ = fs::remove_file(path);
            }
        }
        let target = dir.join(format!("claude-{stamp}.json"));
        if claude_json().exists() {
            fs::copy(claude_json(), &target)
                .map_err(|error| tr!("migrate.couldn_t_save_claude_json", error = error))?;
            self.backup = Some(target);
        }
        Ok(())
    }

    fn rename_folder(&mut self) -> Result<(), String> {
        if taken(&self.from, &self.to) {
            return Err(tr!("common.folder_to_already_exists", to = crate::projects::display_path(&self.to)));
        }
        if let Some(parent) = self.to.parent()
            && !parent.is_dir()
        {
            return Err(tr!("migrate.folder_parent_doesn_t_exist", parent = crate::projects::display_path(parent)));
        }
        fs::rename(&self.from, &self.to).map_err(|error| match error.kind() {
            ErrorKind::CrossesDevices => t("migrate.folder_can_t_be_moved").to_string(),
            _ => tr!("migrate.couldn_t_rename_folder_error", error = error),
        })?;
        self.folder_moved = true;
        Ok(())
    }

    /// Everything Claude Code and git remember about the folder by path (see the module description).
    fn move_claude_data(&mut self) -> Result<(), String> {
        self.fix_worktree_links()?;
        let moved_dirs = self.move_data_dirs()?;
        self.rewrite_claude_json()?;
        self.rewrite_file(&home().join(".claude").join("plugins").join("installed_plugins.json"))?;
        let jobs = self.jobs.clone();
        let mut transcripts: Vec<PathBuf> = Vec::new();
        for job in &jobs {
            self.rewrite_job(job)?;
            if let Some(path) = &job.transcript {
                transcripts.push(self.moved_data_path(path));
            }
        }
        // Transcripts no session holds (they are opened via `claude --resume`) — too.
        for dir in moved_dirs {
            if let Ok(entries) = fs::read_dir(&dir) {
                transcripts.extend(
                    entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl")),
                );
            }
        }
        transcripts.sort();
        transcripts.dedup();
        for path in transcripts {
            self.fix_worktree_state(&path)?;
        }
        Ok(())
    }

    /// Transcript and memory — `~/.claude/projects/<path>` folders. Returns their new places.
    fn move_data_dirs(&mut self) -> Result<Vec<PathBuf>, String> {
        let root = data_root();
        let (old_prefix, new_prefix) = (encode(&self.from), encode(&self.to));
        let Ok(entries) = fs::read_dir(&root) else {
            return Ok(Vec::new());
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .filter(|path| {
                let name = file_name(path);
                let head = name.get(..old_prefix.len()).unwrap_or("");
                let same = if self.fold { head.eq_ignore_ascii_case(&old_prefix) } else { head == old_prefix };
                same && (name.len() == old_prefix.len() || name[old_prefix.len()..].starts_with('-'))
            })
            .collect();
        dirs.sort();
        let mut moved = Vec::new();
        for dir in dirs {
            let name = file_name(&dir);
            let new_name = format!("{new_prefix}{}", &name[old_prefix.len()..]);
            if new_name == name {
                continue;
            }
            // The folder name is ambiguous ("/a-b" and "/a/b" give the same one): check the paths from the
            // transcript that give exactly this name. Paths from before earlier moves don't give the name —
            // if there are none, the name decides.
            let owners: Vec<PathBuf> = recorded_cwds(&dir).into_iter().filter(|cwd| encode(cwd) == name).collect();
            if !owners.is_empty() && !owners.iter().any(|cwd| self.needs_move(cwd)) {
                continue;
            }
            let target = root.join(&new_name);
            self.move_dir(&dir, &target)?;
            self.data_names.push((name, new_name));
            moved.push(target);
        }
        Ok(moved)
    }

    /// A path inside `~/.claude/projects` after moving the data folders.
    fn moved_data_path(&self, path: &Path) -> PathBuf {
        let root = data_root();
        let Ok(rest) = path.strip_prefix(&root) else {
            return path.to_path_buf();
        };
        let mut parts = rest.components();
        let Some(first) = parts.next() else {
            return path.to_path_buf();
        };
        let first = first.as_os_str().to_string_lossy();
        if let Some((_, new)) = self.data_names.iter().find(|(old, _)| *old == first) {
            return root.join(new).join(parts.as_path());
        }
        // Case-insensitive disk: the path is written in the old case, but the folder is already under the new one.
        let (old_prefix, new_prefix) = (encode(&self.from), encode(&self.to));
        if self.fold
            && first.get(..old_prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(&old_prefix))
            && (first.len() == old_prefix.len() || first[old_prefix.len()..].starts_with('-'))
        {
            let name = format!("{new_prefix}{}", &first[old_prefix.len()..]);
            if name != first && root.join(&name).is_dir() {
                return root.join(name).join(parts.as_path());
            }
        }
        path.to_path_buf()
    }

    /// Move a data folder; if there is already data at the new place, merge them.
    fn move_dir(&mut self, from: &Path, to: &Path) -> Result<(), String> {
        if !taken(from, to) {
            fs::rename(from, to).map_err(|error| move_data_error(&error))?;
            self.claude_dirs.push((from.to_path_buf(), to.to_path_buf()));
            return Ok(());
        }
        let entries = fs::read_dir(from).map_err(|error| read_data_error(&error))?;
        for entry in entries.filter_map(Result::ok) {
            let (source, target) = (entry.path(), to.join(entry.file_name()));
            if source.is_dir() && target.is_dir() {
                self.move_dir(&source, &target)?;
            } else if target.exists() && source.extension().is_some_and(|ext| ext == "jsonl") {
                // The exiting process appended a transcript tail at the old path — the tail goes to the end.
                let tail = fs::read(&source).map_err(|error| read_data_error(&error))?;
                let length = fs::metadata(&target).map(|meta| meta.len()).unwrap_or_default();
                fs::OpenOptions::new()
                    .append(true)
                    .open(&target)
                    .and_then(|mut file| file.write_all(&tail))
                    .map_err(|error| move_data_error(&error))?;
                let _ = fs::remove_file(&source);
                self.merged.push((source, target, length));
            } else if target.exists() {
                return Err(tr!("migrate.claude_code_already_has_target", target = target.display()));
            } else {
                fs::rename(&source, &target).map_err(|error| move_data_error(&error))?;
                self.claude_dirs.push((source, target));
            }
        }
        let _ = fs::remove_dir(from);
        Ok(())
    }

    /// Write a file, remembering the previous contents for rollback.
    fn write_file(&mut self, path: &Path, text: &[u8]) -> Result<(), String> {
        if !self.originals.iter().any(|(saved, _)| saved == path) {
            let old = fs::read(path)
                .map_err(|error| tr!("migrate.couldn_t_read_path_error", path = path.display(), error = error))?;
            self.originals.push((path.to_path_buf(), old));
        }
        let mut temp = path.as_os_str().to_owned();
        temp.push(".pult-tmp");
        fs::write(&temp, text)
            .map_err(|error| tr!("migrate.couldn_t_write_path_error", path = path.display(), error = error))?;
        fs::rename(&temp, path)
            .map_err(|error| tr!("migrate.couldn_t_write_path_error", path = path.display(), error = error))
    }

    /// A JSON file in which only path values change.
    fn rewrite_file(&mut self, path: &Path) -> Result<(), String> {
        let Some(mut value) = read_json(path) else {
            return Ok(());
        };
        if self.remap_json(&mut value) {
            let text = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
            self.write_file(path, text.as_bytes())?;
        }
        Ok(())
    }

    /// `~/.claude.json`: the `projects` keys (trust, MCP) and all path values.
    fn rewrite_claude_json(&mut self) -> Result<(), String> {
        let path = claude_json();
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(());
        };
        let mut config: Value =
            serde_json::from_str(&text).map_err(|error| tr!("migrate.couldn_t_read_claude_json", error = error))?;
        let mut changed = self.merge_project_keys(&mut config);
        changed |= self.remap_json(&mut config);
        if changed {
            let text = serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?;
            self.write_file(&path, text.as_bytes())?;
        }
        Ok(())
    }

    /// Rename the `projects` keys. If the key already exists at the new place (a session opened in the new
    /// folder managed to create it) — merge: new over old, trust stays if it was there.
    fn merge_project_keys(&self, config: &mut Value) -> bool {
        let Some(projects) = config.get_mut("projects").and_then(Value::as_object_mut) else {
            return false;
        };
        let keys: Vec<(String, String)> =
            projects.keys().filter_map(|key| Some((key.clone(), self.remap(key)?))).collect();
        for (key, new_key) in &keys {
            let Some(old) = projects.remove(key) else {
                continue;
            };
            let merged = match (old, projects.remove(new_key)) {
                (Value::Object(mut merged), Some(Value::Object(newer))) => {
                    let trusted = [&merged, &newer]
                        .iter()
                        .any(|map| map.get("hasTrustDialogAccepted").and_then(Value::as_bool) == Some(true));
                    merged.extend(newer);
                    if trusted {
                        merged.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
                    }
                    Value::Object(merged)
                }
                (old, _) => old,
            };
            projects.insert(new_key.clone(), merged);
        }
        !keys.is_empty()
    }

    /// A background session's `state.json`: paths, the transcript path, the main session's name.
    fn rewrite_job(&mut self, job: &Job) -> Result<(), String> {
        let Some(mut state) = read_json(&job.state_path) else {
            return Ok(());
        };
        self.remap_json(&mut state);
        if let Some(object) = state.as_object_mut() {
            if let Some(Value::String(path)) = object.get_mut("linkScanPath") {
                *path = self.moved_data_path(Path::new(path)).to_string_lossy().into_owned();
            }
            for (old, new) in &self.names {
                if object.get("name").and_then(Value::as_str) == Some(old.as_str()) {
                    object.insert("name".into(), Value::String(new.clone()));
                }
                if let Some(Value::Array(flags)) = object.get_mut("respawnFlags") {
                    for index in 1..flags.len() {
                        if matches!(flags[index - 1].as_str(), Some("-n" | "--name"))
                            && flags[index].as_str() == Some(old.as_str())
                        {
                            flags[index] = Value::String(new.clone());
                        }
                    }
                }
            }
        }
        let text = serde_json::to_string_pretty(&state).map_err(|error| error.to_string())?;
        self.write_file(&job.state_path, text.as_bytes())
    }

    /// The last worktree record in the transcript — with new paths, if they change.
    fn worktree_fix(&self, path: &Path) -> Option<String> {
        let mut last = last_worktree_state(path)?;
        if last.get("worktreeSession").is_none_or(Value::is_null) {
            return None;
        }
        if !self.remap_json(&mut last) {
            return None;
        }
        serde_json::to_string(&last).ok()
    }

    /// Append a worktree record with new paths to the transcript (Claude Code itself writes the same when
    /// entering a worktree): with it the session starts in its own worktree.
    fn fix_worktree_state(&mut self, path: &Path) -> Result<(), String> {
        let Some(line) = self.worktree_fix(path) else {
            return Ok(());
        };
        let length = fs::metadata(path).map(|meta| meta.len()).unwrap_or_default();
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(format!("{line}\n").as_bytes()))
            .map_err(|error| tr!("migrate.couldn_t_update_transcript_path", path = path.display(), error = error))?;
        self.appended.push((path.to_path_buf(), length));
        Ok(())
    }

    /// Git links between a repository and its worktrees are absolute paths: the worktree's `.git`
    /// (`gitdir: <repository>/.git/…/worktrees/<name>`) and `…/worktrees/<name>/gitdir` (the path to it).
    fn fix_worktree_links(&mut self) -> Result<(), String> {
        let mut found = Vec::new();
        find_git(&self.to, 0, &mut found);
        for git in found {
            if git.is_file() {
                // A worktree or a submodule: `.git` points to a service folder in the repository.
                let Ok(text) = fs::read_to_string(&git) else { continue };
                let Some(target) = text.trim().strip_prefix("gitdir: ").map(str::to_string) else { continue };
                let admin = match self.remap(&target) {
                    Some(new) => {
                        self.write_file(&git, format!("gitdir: {new}\n").as_bytes())?;
                        PathBuf::from(new)
                    }
                    None => git.parent().map(|dir| dir.join(&target)).unwrap_or_else(|| PathBuf::from(&target)),
                };
                self.fix_admin_link(&admin.join("gitdir"))?;
            } else {
                let mut admins = Vec::new();
                find_worktree_admins(&git, 0, &mut admins);
                for admin in admins {
                    self.fix_admin_link(&admin.join("gitdir"))?;
                    // A worktree outside the folder being moved points here — fix it too.
                    let Ok(text) = fs::read_to_string(admin.join("gitdir")) else { continue };
                    let back = PathBuf::from(text.trim());
                    if let Ok(link) = fs::read_to_string(&back)
                        && let Some(target) = link.trim().strip_prefix("gitdir: ")
                        && let Some(new) = self.remap(target)
                    {
                        self.write_file(&back, format!("gitdir: {new}\n").as_bytes())?;
                    }
                }
            }
        }
        Ok(())
    }

    /// `…/worktrees/<name>/gitdir` — the path to the worktree's `.git`.
    fn fix_admin_link(&mut self, gitdir: &Path) -> Result<(), String> {
        let Ok(text) = fs::read_to_string(gitdir) else {
            return Ok(());
        };
        match self.remap(text.trim()) {
            Some(new) => self.write_file(gitdir, format!("{new}\n").as_bytes()),
            None => Ok(()),
        }
    }

    /// Start the stopped sessions: `claude respawn` takes the working folder and flags from `state.json`.
    fn resume(&mut self) -> Result<(), String> {
        let failed = respawn(&self.jobs);
        // Exiting processes may have appended something at the old paths after the move — once more.
        let _ = self.move_data_dirs();
        let _ = self.rewrite_claude_json();
        if failed.is_empty() {
            Ok(())
        } else {
            Err(tr!("migrate.these_sessions_didn_t_start", join = failed.join(", ")))
        }
    }

    /// Put everything back as it was. Safe to call after any step.
    pub fn rollback(&mut self) {
        for (path, length) in self.appended.drain(..).rev() {
            if let Ok(file) = fs::OpenOptions::new().write(true).open(&path) {
                let _ = file.set_len(length);
            }
        }
        for (path, text) in self.originals.drain(..).rev() {
            let _ = fs::write(&path, text);
        }
        for (source, target, length) in self.merged.drain(..).rev() {
            if let Ok(bytes) = fs::read(&target) {
                let tail = bytes.get(length as usize..).unwrap_or_default();
                if let Some(parent) = source.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&source, tail);
                if let Ok(file) = fs::OpenOptions::new().write(true).open(&target) {
                    let _ = file.set_len(length);
                }
            }
        }
        for (from, to) in self.claude_dirs.drain(..).rev() {
            if let Some(parent) = from.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::rename(&to, &from);
        }
        self.data_names.clear();
        if self.folder_moved {
            let _ = fs::rename(&self.to, &self.from);
            self.folder_moved = false;
        }
        respawn(&self.jobs);
    }
}

/// Start the sessions that were running. Returns the ids of those that didn't start.
fn respawn(jobs: &[Job]) -> Vec<String> {
    jobs.iter()
        .filter(|job| job.pid.is_some())
        .filter(|job| sessions::respawn(&job.short).is_err())
        .map(|job| job.short.clone())
        .collect()
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string()])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// A background session's transcript: `linkScanPath` from `state.json` or a search for `<id>.jsonl`.
fn transcript_of(state: &Value, session_id: &str) -> Option<PathBuf> {
    if let Some(path) = state.get("linkScanPath").and_then(Value::as_str).map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    if session_id.is_empty() {
        return None;
    }
    fs::read_dir(data_root())
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(format!("{session_id}.jsonl")))
        .find(|path| path.is_file())
}

/// The last `{"type":"worktree-state",…}` line of the transcript.
pub(crate) fn last_worktree_state(path: &Path) -> Option<Value> {
    let finder = memchr::memmem::Finder::new(b"\"type\":\"worktree-state\"");
    let mut reader = BufReader::with_capacity(1 << 20, fs::File::open(path).ok()?);
    let mut line = Vec::new();
    let mut last = None;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if finder.find(&line).is_some()
            && let Ok(value) = serde_json::from_slice::<Value>(&line)
            && value.get("type").and_then(Value::as_str) == Some("worktree-state")
        {
            last = Some(value);
        }
    }
    last
}

/// Directories where we don't look for repositories: dependencies and build output.
const SKIP: &[&str] = &[
    "node_modules",
    "target",
    ".build",
    "DerivedData",
    "Pods",
    "vendor",
    ".venv",
    "venv",
    "dist",
    ".next",
    ".gradle",
    ".cache",
    "__pycache__",
];

/// All `.git` (repository folders and worktree/submodule files) inside the folder.
fn find_git(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > 12 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let Ok(kind) = entry.file_type() else { continue };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" {
            found.push(entry.path());
        } else if kind.is_dir() && !SKIP.contains(&name.as_ref()) && !name.starts_with("bazel-") {
            find_git(&entry.path(), depth + 1, found);
        }
    }
}

/// Worktree service folders in `.git` and in `.git/modules/…` of submodules.
fn find_worktree_admins(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > 8 {
        return;
    }
    if let Ok(entries) = fs::read_dir(dir.join("worktrees")) {
        found.extend(entries.filter_map(Result::ok).map(|entry| entry.path()).filter(|path| path.is_dir()));
    }
    if let Ok(entries) = fs::read_dir(dir.join("modules")) {
        for entry in entries.filter_map(Result::ok) {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                find_module_admins(&entry.path(), depth + 1, found);
            }
        }
    }
}

/// `modules/a/b` — a submodule can be nested deeper: its service folder is where `HEAD` is.
fn find_module_admins(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > 8 {
        return;
    }
    if dir.join("HEAD").is_file() {
        find_worktree_admins(dir, depth, found);
        return;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                find_module_admins(&entry.path(), depth + 1, found);
            }
        }
    }
}

/// Working folders recorded in the data folder's transcripts (`"cwd":"…"` in `.jsonl` lines).
fn recorded_cwds(dir: &Path) -> Vec<PathBuf> {
    const KEY: &str = "\"cwd\":\"";
    let mut found = std::collections::BTreeSet::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let mut rest = text.as_str();
        while let Some(start) = rest.find(KEY) {
            rest = &rest[start + KEY.len()..];
            if let Some(end) = rest.find('"') {
                found.insert(PathBuf::from(&rest[..end]));
                rest = &rest[end..];
            }
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_claude_code() {
        assert_eq!(encode(Path::new("/home/user/Code/DEMO_2")), "-home-user-Code-DEMO-2");
    }

    /// Changing only the case: on a Mac it is the same folder, the rename must go through.
    #[test]
    fn renames_case_only() {
        let base = std::env::temp_dir().join(format!("pult-case-{}", std::process::id()));
        let (from, to) = (base.join("code"), base.join("CODE"));
        fs::create_dir_all(from.join("data")).unwrap();
        fs::write(from.join("data").join("a.jsonl"), "{}").unwrap();
        // The disk is case-sensitive — nothing to check.
        if !to.exists() {
            fs::remove_dir_all(&base).unwrap();
            return;
        }
        assert!(!taken(&from, &to));
        let mut mv = Move::new(from.clone(), to.clone(), true, Vec::new());
        mv.rename_folder().unwrap();
        let names: Vec<String> =
            fs::read_dir(&base).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["CODE".to_string()]);
        // Claude's data folder — also by a single case change, without "already exists".
        let (data, data_upper) = (to.join("data"), to.join("DATA"));
        mv.move_dir(&data, &data_upper).unwrap();
        assert!(to.join("DATA").join("a.jsonl").exists());
        mv.rollback();
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn moves_paths_inside() {
        let mv = Move::new("/u/Code".into(), "/u/Work".into(), true, Vec::new());
        assert_eq!(mv.moved(Path::new("/u/Code")), PathBuf::from("/u/Work"));
        assert_eq!(mv.moved(Path::new("/u/Code/A/b")), PathBuf::from("/u/Work/A/b"));
        assert_eq!(mv.moved(Path::new("/u/CodeOld")), PathBuf::from("/u/CodeOld"));
        assert_eq!(mv.remap("/u/Work/A"), None);
    }

    /// Case-insensitive disk: old records in any case move too, already new ones don't;
    /// a path in the middle of text is not a path.
    #[test]
    fn remaps_any_case_when_disk_folds() {
        let mut mv = Move::new("/u/Code".into(), "/u/CODE".into(), false, Vec::new());
        mv.fold = true;
        assert_eq!(mv.remap("/u/code/A").as_deref(), Some("/u/CODE/A"));
        assert_eq!(mv.remap("/u/CODE/A"), None);
        let mut value =
            serde_json::json!({"cwd": "/u/Code/A", "text": "see /u/Code/A", "list": ["/u/Code"]});
        assert!(mv.remap_json(&mut value));
        assert_eq!(
            value,
            serde_json::json!({"cwd": "/u/CODE/A", "text": "see /u/Code/A", "list": ["/u/CODE"]})
        );
    }

    /// The key already exists at the new place — the data is merged, trust is not lost.
    #[test]
    fn merges_project_keys() {
        let mv = Move::new("/u/A".into(), "/u/B".into(), true, Vec::new());
        let mut config = serde_json::json!({"projects": {
            "/u/A/x": {"hasTrustDialogAccepted": true, "mcp": 1},
            "/u/B/x": {"hasTrustDialogAccepted": false, "lastCost": 2},
        }});
        assert!(mv.merge_project_keys(&mut config));
        assert_eq!(
            config,
            serde_json::json!({"projects": {"/u/B/x": {"hasTrustDialogAccepted": true, "mcp": 1, "lastCost": 2}}})
        );
    }

    /// A live move on real Claude Code data: `PULT_FROM`, `PULT_TO`, `PULT_MOVE_FOLDER=1`.
    #[test]
    #[ignore]
    fn live_move() {
        let from = PathBuf::from(std::env::var("PULT_FROM").unwrap());
        let to = PathBuf::from(std::env::var("PULT_TO").unwrap());
        let move_folder = std::env::var("PULT_MOVE_FOLDER").is_ok_and(|value| value == "1");
        let main_name = std::env::var("PULT_MAIN")
            .ok()
            .and_then(|names| names.split_once(':').map(|(old, new)| (old.to_string(), new.to_string())))
            .into_iter()
            .collect();
        let mut mv = Move::new(from, to, move_folder, main_name);
        eprintln!("fold: {}", mv.fold);
        for step in 0..mv.steps() {
            let result = mv.run_step(step);
            eprintln!("step {step}: {result:?}");
            if step == 0 {
                eprintln!("jobs: {:?}", mv.jobs.iter().map(|job| (&job.short, job.pid)).collect::<Vec<_>>());
            }
            if let Err(error) = result
                && !mv.is_last(step)
            {
                mv.rollback();
                panic!("{error}");
            }
            // `PULT_ROLLBACK=1` — roll everything back before the last step, as on an error.
            if mv.is_last(step + 1) && std::env::var("PULT_ROLLBACK").is_ok_and(|value| value == "1") {
                mv.rollback();
                eprintln!("rolled back");
                return;
            }
        }
        eprintln!("files: {:?}", mv.originals.iter().map(|(path, _)| path).collect::<Vec<_>>());
        eprintln!("appended: {:?}", mv.appended);
    }
}
