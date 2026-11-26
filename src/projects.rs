//! Pult's projects — only folders that were added explicitly, each with one pinned Claude session.
//!
//! All projects live in one main folder (by default `~/Projects`); there is one main folder, and it can be
//! renamed or moved in settings.
//!
//! Stored in `~/Library/Application Support/pult/state.json`: the main folder, the project folder path,
//! the pinned session id, previous sessions (they can be restored), the terminal font size.
//! New folders in the main folder don't get into Pult by themselves. Pult never deletes anything from disk.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::i18n::t;

/// A session that was replaced: it stays in the project's history and can be restored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviousSession {
    pub id: String,
    /// When it was replaced, "DD.MM".
    pub until: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    previous: Vec<PreviousSession>,
    /// When the session was pinned (Unix seconds). Decides whose session is newer if computers pinned
    /// different ones. Missing — an old record, older than any new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pinned_at: Option<u64>,
    /// When the project was added to Pult (Unix seconds): when an add and a removal on different computers
    /// conflict, the later one wins. Missing — an old record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    added_at: Option<u64>,
    /// Subproject name. A project split into subprojects is several entries with the same folder and
    /// different names: each has its own session, the folder is shared. `None` — an ordinary project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sub: Option<String>,
    /// What the subproject does, one line: parallel sessions read it to understand what the others are
    /// working on. Written by the user, shared between computers (the later change wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    about: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    about_at: Option<u64>,
}

/// The key of an entry in Pult and in the shared snapshot: the folder name, or `folder/subproject`. A folder
/// name can't contain `/`, so the two never collide.
fn key_of(entry: &Entry) -> String {
    key_for(&folder_of(&entry.path), entry.sub.as_deref())
}

pub fn key_for(folder: &str, sub: Option<&str>) -> String {
    match sub {
        Some(sub) => format!("{folder}/{sub}"),
        None => folder.to_string(),
    }
}

/// A key back into the folder and the subproject name.
pub fn split_key(key: &str) -> (&str, Option<&str>) {
    match key.split_once('/') {
        Some((folder, sub)) => (folder, Some(sub)),
        None => (key, None),
    }
}

/// A project in the shared snapshot: how other computers see it. There may be no session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedProject {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default)]
    pub pinned_at: u64,
    #[serde(default)]
    pub previous: Vec<PreviousSession>,
    #[serde(default)]
    pub added_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    #[serde(default)]
    pub about_at: u64,
}

/// This computer's shared projects snapshot: project folder name → project and its session, plus removed
/// projects with their removal time. Projects live in the main folder, and the folder name is the same on
/// all computers — the main folder itself can be anywhere.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SharedProjects {
    #[serde(default)]
    pub projects: BTreeMap<String, SharedProject>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub removed: BTreeMap<String, u64>,
    /// The computer's name — shown by the others ("The session is open on “…”").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device_name: String,
    /// Where sessions are running (`presence.rs`).
    #[serde(default)]
    pub presence: crate::presence::Presence,
}

/// The projects snapshot from `state.json` (also read by windowless sync).
pub fn shared_projects(state_file: &Path) -> Option<SharedProjects> {
    let saved = serde_json::from_str::<SavedState>(&fs::read_to_string(state_file).ok()?).ok()?;
    let root = saved.root?;
    let projects = saved
        .projects?
        .into_iter()
        .filter(|entry| entry.path.parent() == Some(root.as_path()))
        .map(|entry| {
            let key = key_of(&entry);
            let shared = SharedProject {
                session: entry.session,
                pinned_at: entry.pinned_at.unwrap_or(0),
                previous: entry.previous,
                added_at: entry.added_at.unwrap_or(0),
                about: entry.about,
                about_at: entry.about_at.unwrap_or(0),
            };
            (key, shared)
        })
        .collect();
    Some(SharedProjects { projects, removed: saved.removed, ..Default::default() })
}

/// What `adopt_shared` changed.
#[derive(Debug, Default)]
pub struct Adopted {
    pub changed: bool,
    /// Projects removed on another computer.
    pub removed: Vec<String>,
}

/// A folder name is fit for a project from another computer: one folder directly in the main folder.
fn plain_folder(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains('/') && !name.contains(':')
}

/// What is wrong with a subproject name next to the `taken` ones. `None` — fine, or still empty.
pub fn sub_name_issue(folder: &str, taken: &[String], name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if name.starts_with('.') || name.contains('/') || name.contains(':') {
        return Some(t("common.name_can_t_start_with").into());
    }
    let lower = name.to_lowercase();
    taken
        .iter()
        .any(|other| other.trim().to_lowercase() == lower)
        .then(|| tr!("projects.subproject_already_exists", folder = folder, name = name))
}

/// The longest "what it does" line, in characters.
pub const ABOUT_MAX: usize = 200;

/// Characters that are invisible or reorder text (zero-width, bidi controls, the tag block): they are dropped, so
/// nothing hidden can travel in a line that another session reads.
fn is_hidden(char: char) -> bool {
    matches!(char, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}' | '\u{E0000}'..='\u{E007F}')
}

/// One line of plain text: line breaks, control and invisible characters are dropped or become spaces.
pub fn one_line(text: &str) -> String {
    let line: String = text
        .chars()
        .filter(|char| !is_hidden(*char))
        .map(|char| if char.is_control() { ' ' } else { char })
        .collect();
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The "what it does" line: one clean line, limited in length.
pub fn clean_about(text: &str) -> String {
    one_line(text).chars().take(ABOUT_MAX).collect()
}

/// A project entry as the journal of parallel sessions sees it.
pub struct Member {
    pub key: String,
    pub folder: String,
    pub sub: Option<String>,
    pub session: Option<String>,
    pub about: String,
    pub path: PathBuf,
}

/// Every project and subproject from `state.json`; the windowless journal hook reads this.
pub fn members(state_file: &Path) -> Option<Vec<Member>> {
    let saved = serde_json::from_str::<SavedState>(&fs::read_to_string(state_file).ok()?).ok()?;
    Some(
        saved
            .projects?
            .into_iter()
            .map(|entry| Member {
                key: key_of(&entry),
                folder: folder_of(&entry.path),
                sub: entry.sub.clone(),
                session: entry.session.clone(),
                about: entry.about.clone().unwrap_or_default(),
                path: entry.path.clone(),
            })
            .collect(),
    )
}

/// A snapshot key is fit: a plain folder, optionally with a subproject name.
fn plain_key(key: &str) -> bool {
    let (folder, sub) = split_key(key);
    plain_folder(folder) && sub.is_none_or(|sub| !sub.trim().is_empty() && !sub.contains('/'))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|time| time.as_secs()).unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    id: String,
    folder: String,
    sub: Option<String>,
    path: PathBuf,
    session: Option<String>,
    previous: Vec<PreviousSession>,
    about: String,
}

impl Project {
    /// What the subproject does, as the user wrote it; empty if not written.
    pub fn about(&self) -> &str {
        &self.about
    }

    /// The key in Pult: the folder name, or `folder/subproject` for a subproject.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// What the row is called: the subproject name, or the folder name for an ordinary project.
    pub fn name(&self) -> &str {
        self.sub.as_deref().unwrap_or(&self.folder)
    }

    /// The project folder name.
    pub fn folder(&self) -> &str {
        &self.folder
    }

    /// The subproject name, if the project is split.
    pub fn sub(&self) -> Option<&str> {
        self.sub.as_deref()
    }

    /// The full title: `MAPLE › Finance` for a subproject, the folder name otherwise.
    pub fn title(&self) -> String {
        match &self.sub {
            Some(sub) => format!("{} › {sub}", self.folder),
            None => self.folder.clone(),
        }
    }

    /// The name of the pinned Claude Code session (`maple-main`, `maple-finansy`).
    pub fn session_name(&self) -> String {
        crate::sessions::session_name(&self.folder, self.sub.as_deref())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The pinned Claude Code session.
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    pub fn previous(&self) -> &[PreviousSession] {
        &self.previous
    }

    pub fn exists(&self) -> bool {
        self.path.is_dir()
    }
}

pub fn display_path(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(&home).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Window appearance: "System" (the default, as is customary on a Mac) or chosen manually.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    #[default]
    System,
    Light,
    Dark,
}

/// Where the main window was: x, y, width, height (logical points).
pub type WindowFrame = [f32; 4];

#[derive(Default, Serialize, Deserialize)]
struct SavedState {
    #[serde(default)]
    theme: ThemePref,
    #[serde(default)]
    language: crate::i18n::LangPref,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window: Option<WindowFrame>,
    /// The main folder; missing — `~/Projects`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<PathBuf>,
    /// Pult's folders. `None` — a file from the earlier version, where all `~/Projects` folders were projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    projects: Option<Vec<Entry>>,
    /// Earlier version: hidden folders. Read once during the migration.
    #[serde(default, skip_serializing)]
    hidden: BTreeSet<String>,
    #[serde(default)]
    terminal_font_size: Option<f32>,
    /// The project that was open when Pult quit (its key); this computer only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_selected: Option<String>,
    /// Projects removed from Pult: folder name → when (Unix seconds). Shared with other computers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    removed: BTreeMap<String, u64>,
    /// Projects whose subprojects are folded in the sidebar (this computer only).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    collapsed: BTreeSet<String>,
}

/// Folder name from the project name: spaces become "_", as the "New project" window shows.
pub fn folder_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join("_")
}

fn folder_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn today() -> String {
    std::process::Command::new("/bin/date")
        .arg("+%d.%m")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

pub struct ProjectStore {
    root: PathBuf,
    /// The main folder was chosen by the person (or projects already live in it). Until then `root` is only a
    /// default, nothing is created in it, and nothing from other computers is placed there.
    root_chosen: bool,
    state_file: Option<PathBuf>,
    entries: Vec<Entry>,
    removed: BTreeMap<String, u64>,
    collapsed: BTreeSet<String>,
    last_selected: Option<String>,
    terminal_font_size: Option<f32>,
    theme: ThemePref,
    language: crate::i18n::LangPref,
    window: Option<WindowFrame>,
}

/// Language from the previous launch — before the first window opens.
pub fn saved_language() -> crate::i18n::LangPref {
    dirs::data_dir()
        .and_then(|dir| fs::read_to_string(dir.join("pult").join("state.json")).ok())
        .and_then(|text| serde_json::from_str::<SavedState>(&text).ok())
        .map(|saved| saved.language)
        .unwrap_or_default()
}

/// Main window frame from the previous launch — before the window opens.
pub fn saved_window_frame() -> Option<WindowFrame> {
    let file = dirs::data_dir()?.join("pult").join("state.json");
    serde_json::from_str::<SavedState>(&fs::read_to_string(file).ok()?).ok()?.window
}

impl ProjectStore {
    pub fn load() -> Self {
        let state_file = dirs::data_dir().map(|dir| dir.join("pult").join("state.json"));
        let text = state_file.as_ref().and_then(|file| fs::read_to_string(file).ok());
        let fresh = text.is_none();
        let saved = text.and_then(|text| serde_json::from_str::<SavedState>(&text).ok()).unwrap_or_default();
        let root = saved.root.clone().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Projects"));
        let migrate = saved.projects.is_none();
        let entries = match saved.projects {
            Some(entries) => entries,
            // A fresh install: no projects until they are added.
            None if fresh => Vec::new(),
            // Migration from the earlier version (the list was "all folders except hidden"): what was visible stays.
            None => folders_in(&root)
                .into_iter()
                .filter(|path| !saved.hidden.contains(&folder_of(path)))
                .map(|path| Entry { path, session: None, previous: Vec::new(), pinned_at: None, added_at: None, sub: None, about: None, about_at: None })
                .collect(),
        };
        let root_chosen = saved.root.is_some() || !entries.is_empty();
        let store = Self {
            root,
            root_chosen,
            state_file,
            entries,
            removed: saved.removed,
            collapsed: saved.collapsed,
            last_selected: saved.last_selected,
            terminal_font_size: saved.terminal_font_size,
            theme: saved.theme,
            language: saved.language,
            window: saved.window,
        };
        if migrate {
            store.save();
        }
        store
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder lies directly in the main folder — only such folders can be projects.
    pub fn in_root(&self, path: &Path) -> bool {
        path.parent() == Some(self.root.as_path())
    }

    /// The main folder hasn't been chosen yet (a first launch): Pult asks for it before creating or adding projects.
    pub fn needs_root(&self) -> bool {
        !self.root_chosen
    }

    /// Make `path` the main folder without moving anything. Only while Pult has no projects: projects live in
    /// the main folder, so with projects the folder is moved instead (`move_root`).
    pub fn choose_root(&mut self, path: PathBuf) -> Result<(), String> {
        if !self.entries.is_empty() {
            return Err(t("projects.main_folder_has_projects").into());
        }
        if !path.is_dir() {
            return Err(tr!("projects.folder_not_found_path", path = display_path(&path)));
        }
        self.root = path;
        self.root_chosen = true;
        self.save();
        Ok(())
    }

    /// The main folder was renamed or moved: the project paths inside follow it.
    pub fn move_root(&mut self, to: PathBuf) {
        self.root_chosen = true;
        let from = std::mem::replace(&mut self.root, to.clone());
        for entry in &mut self.entries {
            if let Ok(rest) = entry.path.strip_prefix(&from) {
                entry.path = to.join(rest);
            }
        }
        self.save();
    }

    /// Pult's projects alphabetically, case-insensitive.
    pub fn list(&self) -> Vec<Project> {
        let mut projects: Vec<Project> = self
            .entries
            .iter()
            .map(|entry| Project {
                id: key_of(entry),
                folder: folder_of(&entry.path),
                sub: entry.sub.clone(),
                path: entry.path.clone(),
                session: entry.session.clone(),
                previous: entry.previous.clone(),
                about: entry.about.clone().unwrap_or_default(),
            })
            .collect();
        projects.sort_by_key(|project| (project.folder.to_lowercase(), project.folder.clone(), project.sub.is_some()));
        projects
    }

    /// Folders of the main folder that aren't in Pult yet.
    pub fn candidates(&self) -> Vec<PathBuf> {
        folders_in(&self.root)
            .into_iter()
            .filter(|path| !self.has_folder(&folder_of(path)))
            .collect()
    }

    fn has_folder(&self, name: &str) -> bool {
        self.entries.iter().any(|entry| folder_of(&entry.path) == name)
    }

    fn entry_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| key_of(entry) == id)
    }

    /// Subproject names of a folder, in order. Empty — an ordinary project.
    pub fn subs(&self, folder: &str) -> Vec<String> {
        self.entries.iter().filter(|entry| folder_of(&entry.path) == folder).filter_map(|entry| entry.sub.clone()).collect()
    }

    /// What is wrong with a subproject name; `also` — another name being entered next to it (the split
    /// window has two fields). `None` — fine, or still empty.
    pub fn sub_issue(&self, folder: &str, name: &str, also: Option<&str>) -> Option<String> {
        let mut taken = self.subs(folder);
        taken.extend(also.map(|also| also.trim().to_string()));
        sub_name_issue(folder, &taken, name)
    }

    /// Write what the subproject does (one line, up to `ABOUT_MAX` characters). Other computers take the later one.
    pub fn set_about(&mut self, id: &str, about: &str) {
        let about = clean_about(about);
        let Some(entry) = self.entry_mut(id) else {
            return;
        };
        if entry.about.clone().unwrap_or_default() == about {
            return;
        }
        entry.about = (!about.is_empty()).then_some(about);
        entry.about_at = Some(unix_now());
        self.save();
    }

    /// Whether the project's subprojects are folded in the sidebar.
    pub fn is_collapsed(&self, folder: &str) -> bool {
        self.collapsed.contains(folder)
    }

    pub fn set_collapsed(&mut self, folder: &str, collapsed: bool) {
        let changed = if collapsed { self.collapsed.insert(folder.to_string()) } else { self.collapsed.remove(folder) };
        if changed {
            self.save();
        }
    }

    /// Split an ordinary project into two subprojects: its conversation becomes the first one, the second
    /// starts empty. Returns both keys. Other computers see "the plain project removed, two subprojects
    /// added" — the same way a rename travels.
    pub fn split(&mut self, id: &str, first: &str, second: &str) -> Result<(String, String), String> {
        let (first, second) = (first.trim().to_string(), second.trim().to_string());
        let (folder, sub) = split_key(id);
        let folder = folder.to_string();
        if sub.is_some() || first.is_empty() || second.is_empty() {
            return Err(t("projects.project_not_found").into());
        }
        if let Some(issue) = self.sub_issue(&folder, &first, None).or_else(|| self.sub_issue(&folder, &second, Some(&first))) {
            return Err(issue);
        }
        let now = unix_now();
        let Some(index) = self.entries.iter().position(|entry| key_of(entry) == id) else {
            return Err(t("projects.project_not_found").into());
        };
        let path = self.entries[index].path.clone();
        self.entries[index].sub = Some(first.clone());
        self.entries[index].added_at = Some(now);
        self.entries.insert(index + 1, Entry { path, session: None, previous: Vec::new(), pinned_at: None, added_at: Some(now), sub: Some(second.clone()), about: None, about_at: None });
        self.removed.insert(folder.clone(), now);
        let kept = key_for(&folder, Some(&first));
        self.removed.remove(&kept);
        let new = key_for(&folder, Some(&second));
        self.removed.remove(&new);
        self.save();
        Ok((kept, new))
    }

    /// Add a subproject to a project that is already split. Returns its key.
    pub fn add_sub(&mut self, folder: &str, name: &str) -> Result<String, String> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(t("projects.enter_name").into());
        }
        if let Some(issue) = self.sub_issue(folder, &name, None) {
            return Err(issue);
        }
        let Some(last) = self.entries.iter().rposition(|entry| folder_of(&entry.path) == folder && entry.sub.is_some()) else {
            return Err(t("projects.project_not_found").into());
        };
        let path = self.entries[last].path.clone();
        self.entries.insert(last + 1, Entry { path, session: None, previous: Vec::new(), pinned_at: None, added_at: Some(unix_now()), sub: Some(name.clone()), about: None, about_at: None });
        let key = key_for(folder, Some(&name));
        self.removed.remove(&key);
        self.save();
        Ok(key)
    }

    /// A split project left with one subproject becomes an ordinary project again, with that conversation.
    /// Returns the name of that subproject.
    fn merge_single(&mut self, folder: &str) -> Option<String> {
        let mut of_folder = self.entries.iter_mut().filter(|entry| folder_of(&entry.path) == folder);
        let only = of_folder.next()?;
        if of_folder.next().is_some() {
            return None;
        }
        let sub = only.sub.take()?;
        let now = unix_now();
        only.added_at = Some(now);
        self.removed.insert(key_for(folder, Some(&sub)), now);
        self.removed.remove(folder);
        Some(sub)
    }

    /// Remove a whole project with all its subprojects from Pult.
    pub fn remove_folder(&mut self, folder: &str) {
        let now = unix_now();
        let keys: Vec<String> = self.entries.iter().filter(|entry| folder_of(&entry.path) == folder).map(key_of).collect();
        if keys.is_empty() {
            return;
        }
        self.entries.retain(|entry| folder_of(&entry.path) != folder);
        for key in keys {
            self.removed.insert(key, now);
        }
        self.collapsed.remove(folder);
        self.save();
    }

    /// Add folders. A folder with the same name as one already in Pult is skipped.
    pub fn add(&mut self, paths: &[PathBuf]) -> Vec<String> {
        let mut added = Vec::new();
        for path in paths {
            let name = folder_of(path);
            if path.is_dir() && self.in_root(path) && !self.has_folder(&name) {
                let now = unix_now();
                self.entries.push(Entry { path: path.clone(), session: None, previous: Vec::new(), pinned_at: None, added_at: Some(now), sub: None, about: None, about_at: None });
                self.removed.remove(&name);
                added.push(name);
            }
        }
        if !added.is_empty() {
            self.root_chosen = true;
            self.save();
        }
        added
    }

    pub fn create(&mut self, name: &str) -> Result<Project, String> {
        let name = folder_name(name);
        let name = name.as_str();
        if name.is_empty() {
            return Err(t("projects.enter_name").into());
        }
        if name.starts_with('.') || name.contains('/') || name.contains(':') {
            return Err(t("common.name_can_t_start_with").into());
        }
        let path = self.root.join(name);
        if path.exists() {
            return Err(tr!("common.folder_to_already_exists", path = display_path(&path)));
        }
        if self.has_folder(name) {
            return Err(tr!("projects.project_name_is_already_in", name = name));
        }
        fs::create_dir_all(&path).map_err(|error| tr!("projects.couldn_t_create_folder_error", error = error))?;
        self.add(std::slice::from_ref(&path));
        self.list()
            .into_iter()
            .find(|project| project.id == name)
            .ok_or_else(|| t("projects.couldn_t_add_project").into())
    }

    /// Remove from Pult: the folder and transcripts stay on disk.
    /// Remove the project from Pult — on all computers (the folder is not deleted from disk anywhere).
    /// For a subproject: only that one; if one subproject is left after it, the project turns ordinary again —
    /// then the name of that last subproject is returned.
    pub fn remove(&mut self, id: &str) -> Option<String> {
        let before = self.entries.len();
        self.entries.retain(|entry| key_of(entry) != id);
        if self.entries.len() == before {
            return None;
        }
        self.removed.insert(id.to_string(), unix_now());
        let (folder, sub) = split_key(id);
        let merged = if sub.is_some() { self.merge_single(folder) } else { None };
        if !self.entries.iter().any(|entry| folder_of(&entry.path) == folder && entry.sub.is_some()) {
            self.collapsed.remove(folder);
        }
        self.save();
        merged
    }

    /// Unpin a session that doesn't exist: Claude Code doesn't know it, and it has no conversation (started and
    /// closed without writing anything). It can't be started; the project shows "no session chosen".
    pub fn forget_session(&mut self, id: &str, session: &str) {
        let Some(entry) = self.entry_mut(id) else {
            return;
        };
        if entry.session.as_deref() != Some(session) {
            return;
        }
        entry.session = None;
        entry.pinned_at = None;
        self.save();
    }

    /// Pin a session to the project; the previous one goes to history.
    pub fn bind(&mut self, id: &str, session: &str) {
        let Some(entry) = self.entry_mut(id) else {
            return;
        };
        if entry.session.as_deref() == Some(session) {
            return;
        }
        if let Some(old) = entry.session.replace(session.to_string()) {
            entry.previous.retain(|previous| previous.id != old);
            entry.previous.insert(0, PreviousSession { id: old, until: today() });
        }
        entry.previous.retain(|previous| previous.id != session);
        entry.pinned_at = Some(unix_now());
        // One conversation belongs to one subproject: it leaves the history of its neighbours.
        let folder = split_key(id).0.to_string();
        for other in self.entries.iter_mut().filter(|other| folder_of(&other.path) == folder && key_of(other) != id) {
            other.previous.retain(|previous| previous.id != session);
        }
        self.save();
    }

    /// Project folders missing on this computer (the project and its session came from another one) are
    /// created by Pult itself, empty: there is no need to pick the folder by hand. The code
    /// is pulled in later (`git clone` into an empty folder works). Only folders directly in the main one.
    pub fn ensure_folders(&self) -> Vec<String> {
        if !self.root_chosen {
            return Vec::new();
        }
        self.entries
            .iter()
            .filter(|entry| entry.path.parent() == Some(self.root.as_path()) && !entry.path.exists())
            .filter(|entry| fs::create_dir_all(&entry.path).is_ok())
            .map(|entry| folder_of(&entry.path))
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect()
    }

    /// Projects from other computers (`projects-<computer>.json` snapshots in `dir`). A project added
    /// anywhere appears here too (`ensure_folders` creates its folder), one removed anywhere is removed;
    /// when an add and a removal conflict, the later one wins. Session: a project without a session takes
    /// the other's; if they differ, the one pinned later stays; the previous one goes to history. Returns what
    /// changed: projects removed here — so the window closes their terminals.
    /// `usable(session)` — whether the session has a conversation on this computer: an empty one (nothing
    /// written yet) or one that hasn't arrived yet can't be started by Claude Code; Pult skips it while so.
    pub fn adopt_shared(&mut self, dir: &Path, usable: impl Fn(&str) -> bool) -> Adopted {
        let mut adopted = Adopted::default();
        // Before the main folder is chosen there is nowhere to put projects from other computers.
        if !self.root_chosen {
            return adopted;
        }
        let Ok(files) = fs::read_dir(dir) else {
            return adopted;
        };
        let mut added: BTreeMap<String, u64> = BTreeMap::new();
        let mut removed: BTreeMap<String, u64> = BTreeMap::new();
        let mut pins: BTreeMap<String, SharedProject> = BTreeMap::new();
        let mut abouts: BTreeMap<String, (u64, String)> = BTreeMap::new();
        for file in files.filter_map(Result::ok) {
            let Some(snapshot) =
                fs::read_to_string(file.path()).ok().and_then(|text| serde_json::from_str::<SharedProjects>(&text).ok())
            else {
                continue;
            };
            for (name, project) in snapshot.projects {
                if !plain_key(&name) {
                    continue;
                }
                let latest = added.entry(name.clone()).or_insert(project.added_at);
                *latest = (*latest).max(project.added_at);
                if project.about_at > 0 && abouts.get(&name).is_none_or(|(at, _)| project.about_at > *at) {
                    abouts.insert(name.clone(), (project.about_at, project.about.clone().unwrap_or_default()));
                }
                if project.session.is_some() && pins.get(&name).is_none_or(|current| project.pinned_at > current.pinned_at) {
                    pins.insert(name, project);
                }
            }
            for (name, at) in snapshot.removed {
                let latest = removed.entry(name).or_insert(at);
                *latest = (*latest).max(at);
            }
        }

        // Sessions and history of entries removed below: (folder, session, history). A split or a merge made
        // on another computer removes the entry here, and what was pinned to it only here must not vanish.
        let mut orphans: Vec<(String, Option<String>, Vec<PreviousSession>)> = Vec::new();
        // Add and remove: whichever is later — the add or the removal (here and anywhere).
        let local: BTreeMap<String, u64> =
            self.entries.iter().map(|entry| (key_of(entry), entry.added_at.unwrap_or(0))).collect();
        let names: BTreeSet<String> = added.keys().chain(removed.keys()).chain(local.keys()).cloned().collect();
        for name in names {
            let present = local.get(&name).copied().max(added.get(&name).copied());
            let gone = self.removed.get(&name).copied().max(removed.get(&name).copied());
            let keep = match (present, gone) {
                (Some(present), Some(gone)) => present > gone,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if keep && !local.contains_key(&name) {
                let (folder, sub) = split_key(&name);
                self.entries.push(Entry {
                    path: self.root.join(folder),
                    session: None,
                    previous: Vec::new(),
                    pinned_at: None,
                    added_at: present,
                    sub: sub.map(str::to_string),
                    about: None,
                    about_at: None,
                });
                self.removed.remove(&name);
                adopted.changed = true;
            } else if !keep && local.contains_key(&name) {
                for entry in self.entries.iter().filter(|entry| key_of(entry) == name) {
                    orphans.push((folder_of(&entry.path), entry.session.clone(), entry.previous.clone()));
                }
                self.entries.retain(|entry| key_of(entry) != name);
                self.removed.insert(name.clone(), gone.unwrap_or(0));
                adopted.removed.push(name);
                adopted.changed = true;
            } else if !keep && let Some(gone) = gone && self.removed.get(&name) != Some(&gone) {
                self.removed.insert(name, gone);
                adopted.changed = true;
            }
        }

        // A folder is either one ordinary project or subprojects, never both: conflicting changes on two
        // computers (one merged back while the other added a subproject, or a computer on an earlier version
        // added the folder again) can leave both. Subprojects win; the ordinary entry goes, with a removal as
        // old as itself so that every computer drops it and nothing bounces back.
        let split: BTreeSet<String> =
            self.entries.iter().filter(|entry| entry.sub.is_some()).map(|entry| folder_of(&entry.path)).collect();
        for folder in &split {
            let Some(index) = self.entries.iter().position(|entry| entry.sub.is_none() && folder_of(&entry.path) == *folder)
            else {
                continue;
            };
            let plain = self.entries.remove(index);
            let at = plain.added_at.unwrap_or(0).max(self.removed.get(folder).copied().unwrap_or(0));
            self.removed.insert(folder.clone(), at);
            orphans.push((folder.clone(), plain.session, plain.previous));
            // What another computer pinned to the ordinary project is kept the same way.
            if let Some(pin) = pins.get(folder) {
                orphans.push((folder.clone(), pin.session.clone(), pin.previous.clone()));
            }
            adopted.removed.push(folder.clone());
            adopted.changed = true;
        }
        // The fold state of projects that are not split any more.
        let folded = self.collapsed.len();
        self.collapsed.retain(|folder| split.contains(folder));
        adopted.changed |= self.collapsed.len() != folded;

        // Sessions.
        for entry in &mut self.entries {
            let Some(pin) = pins.get(&key_of(entry)) else {
                continue;
            };
            let Some(session) = pin.session.as_deref() else {
                continue;
            };
            if entry.session.as_deref() == Some(session) {
                continue;
            }
            if entry.session.is_some() && pin.pinned_at <= entry.pinned_at.unwrap_or(0) {
                continue;
            }
            if !usable(session) {
                continue;
            }
            if let Some(old) = entry.session.replace(session.to_string()) {
                entry.previous.retain(|previous| previous.id != old);
                entry.previous.insert(0, PreviousSession { id: old, until: today() });
            }
            for previous in &pin.previous {
                if !entry.previous.iter().any(|known| known.id == previous.id) {
                    entry.previous.push(previous.clone());
                }
            }
            entry.previous.retain(|previous| previous.id != session);
            entry.pinned_at = Some(pin.pinned_at);
            adopted.changed = true;
        }

        // What the subprojects do: the later written line wins.
        for entry in &mut self.entries {
            let Some((at, about)) = abouts.get(&key_of(entry)) else {
                continue;
            };
            if *at > entry.about_at.unwrap_or(0) {
                let about = clean_about(about);
                entry.about = (!about.is_empty()).then_some(about);
                entry.about_at = Some(*at);
                adopted.changed = true;
            }
        }

        // What the removed entries had goes to the history of the first project left in their folder, unless
        // some project of the folder already has it.
        for (folder, session, previous) in orphans {
            let mut lost: Vec<PreviousSession> = session.into_iter().map(|id| PreviousSession { id, until: today() }).collect();
            lost.extend(previous);
            let known: BTreeSet<String> = self
                .entries
                .iter()
                .filter(|entry| folder_of(&entry.path) == folder)
                .flat_map(|entry| entry.session.iter().cloned().chain(entry.previous.iter().map(|previous| previous.id.clone())))
                .collect();
            let Some(heir) = self.entries.iter_mut().find(|entry| folder_of(&entry.path) == folder) else {
                continue;
            };
            for item in lost {
                if !known.contains(&item.id) && !heir.previous.iter().any(|previous| previous.id == item.id) {
                    heir.previous.push(item);
                    adopted.changed = true;
                }
            }
        }
        if adopted.changed {
            self.save();
        }
        adopted
    }

    /// The project folder moved or was renamed: the session stays the same.
    /// For a split project the folder is shared, so all its subprojects move together; the returned key is
    /// the new key of `id`.
    pub fn relocate(&mut self, id: &str, path: PathBuf) -> Result<String, String> {
        let name = folder_of(&path);
        let (old, sub) = split_key(id);
        let (old, sub) = (old.to_string(), sub.map(str::to_string));
        if name != old && self.has_folder(&name) {
            return Err(tr!("projects.project_name_is_already_in", name = name));
        }
        if !self.has_folder(&old) {
            return Err(t("projects.project_not_found").into());
        }
        let now = unix_now();
        for index in 0..self.entries.len() {
            if folder_of(&self.entries[index].path) != old {
                continue;
            }
            let before = key_of(&self.entries[index]);
            self.entries[index].path = path.clone();
            if name != old {
                // Other computers see this as "old one removed, new one added" with the same session.
                self.entries[index].added_at = Some(now);
                let after = key_of(&self.entries[index]);
                self.removed.remove(&after);
                self.removed.insert(before, now);
            }
        }
        if name != old && self.collapsed.remove(&old) {
            self.collapsed.insert(name.clone());
        }
        self.save();
        Ok(key_for(&name, sub.as_deref()))
    }

    pub fn theme(&self) -> ThemePref {
        self.theme
    }

    pub fn language(&self) -> crate::i18n::LangPref {
        self.language
    }

    pub fn set_language(&mut self, language: crate::i18n::LangPref) {
        if self.language != language {
            self.language = language;
            self.save();
        }
    }

    pub fn set_theme(&mut self, theme: ThemePref) {
        if self.theme != theme {
            self.theme = theme;
            self.save();
        }
    }

    pub fn set_window_frame(&mut self, frame: WindowFrame) {
        if self.window != Some(frame) {
            self.window = Some(frame);
            self.save();
        }
    }

    /// The project that was open last time, if it still exists.
    pub fn last_selected(&self) -> Option<&str> {
        self.last_selected.as_deref().filter(|key| self.entries.iter().any(|entry| key_of(entry) == *key))
    }

    /// Remember the open project: Pult opens it again after a restart.
    pub fn set_last_selected(&mut self, key: &str) {
        if self.last_selected.as_deref() != Some(key) {
            self.last_selected = Some(key.to_string());
            self.save();
        }
    }

    pub fn terminal_font_size(&self) -> Option<f32> {
        self.terminal_font_size
    }

    pub fn set_terminal_font_size(&mut self, size: f32) {
        if self.terminal_font_size != Some(size) {
            self.terminal_font_size = Some(size);
            self.save();
        }
    }

    fn save(&self) {
        let Some(file) = &self.state_file else {
            return;
        };
        if let Some(dir) = file.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let state = SavedState {
            theme: self.theme,
            language: self.language,
            window: self.window,
            root: self.root_chosen.then(|| self.root.clone()),
            projects: Some(self.entries.clone()),
            hidden: BTreeSet::new(),
            terminal_font_size: self.terminal_font_size,
            removed: self.removed.clone(),
            collapsed: self.collapsed.clone(),
            last_selected: self.last_selected.clone(),
        };
        if let Ok(text) = serde_json::to_string_pretty(&state) {
            let _ = fs::write(file, text);
        }
        // Share with other computers right away (the snapshot is sent only if it changed).
        crate::sync::share_projects_now();
    }
}

/// Subfolders, except hidden ones (starting with a dot), alphabetically.
fn folders_in(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .collect();
    folders.sort_by_key(|path| folder_of(path).to_lowercase());
    folders
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &Path, session: Option<&str>, pinned_at: Option<u64>, added_at: Option<u64>) -> Entry {
        Entry { path: path.to_path_buf(), session: session.map(str::to_string), previous: Vec::new(), pinned_at, added_at, sub: None, about: None, about_at: None }
    }

    fn store(root: &Path, entries: Vec<Entry>, removed: &[(&str, u64)]) -> ProjectStore {
        ProjectStore {
            root: root.to_path_buf(),
            root_chosen: true,
            state_file: None,
            entries,
            removed: removed.iter().map(|(name, at)| (name.to_string(), *at)).collect(),
            collapsed: BTreeSet::new(),
            last_selected: None,
            terminal_font_size: None,
            theme: ThemePref::default(),
            language: crate::i18n::LangPref::default(),
            window: None,
        }
    }

    fn snapshots(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pult-shared-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (index, text) in files.iter().enumerate() {
            fs::write(dir.join(format!("projects-{index}.json")), text).unwrap();
        }
        dir
    }

    fn names(projects: &ProjectStore) -> Vec<String> {
        projects.list().iter().map(|project| project.id().to_string()).collect()
    }

    #[test]
    fn the_main_folder_is_asked_for_on_a_first_launch() {
        let dir = snapshots("first", &[r#"{"projects": {"MAPLE": {"session": "s-maple", "added_at": 5}}}"#]);
        let chosen = std::env::temp_dir().join(format!("pult-first-root-{}", std::process::id()));
        fs::create_dir_all(&chosen).unwrap();
        let mut projects = store(Path::new("/default/Projects"), Vec::new(), &[]);
        projects.root_chosen = false;
        assert!(projects.needs_root());
        assert!(!projects.adopt_shared(&dir, |_| true).changed, "nowhere to put projects from other computers yet");
        assert!(projects.ensure_folders().is_empty());
        assert!(projects.choose_root(chosen.join("missing")).is_err());
        projects.choose_root(chosen.clone()).unwrap();
        assert!(!projects.needs_root());
        assert_eq!(projects.root(), chosen.as_path());
        assert!(projects.adopt_shared(&dir, |_| true).changed, "once chosen, they come");
        assert_eq!(projects.list()[0].path(), chosen.join("MAPLE"));
        assert!(projects.choose_root(std::env::temp_dir()).is_err(), "with projects the folder is moved, not swapped");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&chosen);
    }

    #[test]
    fn sessions_pinned_elsewhere_are_adopted() {
        let dir = snapshots("pins", &[r#"{"projects": {
            "EMBER": {"session": "mac-ember", "pinned_at": 0},
            "MAPLE": {"session": "mac-maple", "pinned_at": 100},
            "JADE": {"session": "mac-jade", "pinned_at": 50, "previous": [{"id": "old-jade", "until": "01.11"}]},
            "HERON": {}
        }}"#]);
        let root = Path::new("/r");
        let mut projects = store(root, vec![
            entry(&root.join("EMBER"), None, None, None),
            entry(&root.join("MAPLE"), Some("here-maple"), Some(200), None),
            entry(&root.join("JADE"), Some("here-jade"), Some(10), None),
            entry(&root.join("HERON"), None, None, None),
        ], &[]);
        assert!(projects.adopt_shared(&dir, |_| true).changed);
        let list = projects.list();
        let get = |name: &str| list.iter().find(|project| project.id() == name).unwrap().clone();
        assert_eq!(get("EMBER").session(), Some("mac-ember"), "an empty project takes the other's session");
        assert_eq!(get("MAPLE").session(), Some("here-maple"), "own one pinned later stays");
        let jade = get("JADE");
        assert_eq!(jade.session(), Some("mac-jade"), "the other's is newer — taken");
        let previous: Vec<&str> = jade.previous().iter().map(|previous| previous.id.as_str()).collect();
        assert_eq!(previous, ["here-jade", "old-jade"], "own one goes to history, the other's history is added");
        assert_eq!(get("HERON").session(), None);
        assert!(!projects.adopt_shared(&dir, |_| true).changed, "the second time changes nothing");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn projects_added_and_removed_elsewhere_follow() {
        let dir = snapshots("list", &[
            r#"{"projects": {"NEW": {"added_at": 100, "session": "s-new", "pinned_at": 100},
                             "OLD": {"added_at": 0}, "../x": {"added_at": 9}},
                "removed": {"GONE": 300, "BACK": 50}}"#,
            r#"{"projects": {"BACK": {"added_at": 400}}, "removed": {"OLD": 10}}"#,
        ]);
        let root = Path::new("/r");
        let mut projects = store(root, vec![
            entry(&root.join("GONE"), Some("s-gone"), Some(1), Some(200)),
            entry(&root.join("KEEP"), None, None, Some(500)),
            entry(&root.join("OLD"), None, None, None),
            entry(&root.join("RECENT"), None, None, Some(900)),
        ], &[("BACK", 60)]);
        let adopted = projects.adopt_shared(&dir, |_| true);
        assert!(adopted.changed);
        let mut removed = adopted.removed.clone();
        removed.sort();
        assert_eq!(removed, ["GONE", "OLD"], "those removed after being added go away");
        assert_eq!(names(&projects), ["BACK", "KEEP", "NEW", "RECENT"], "added anywhere appear, foreign paths don't");
        let new = projects.list().into_iter().find(|project| project.id() == "NEW").unwrap();
        assert_eq!(new.session(), Some("s-new"), "a new project comes with its session");
        assert_eq!(new.path(), root.join("NEW"));
        assert_eq!(projects.removed.get("GONE"), Some(&300), "the removal is remembered and shared further");
        assert!(!projects.removed.contains_key("BACK"), "added after the removal");
        assert!(!projects.adopt_shared(&dir, |_| true).changed, "the second time changes nothing");

        // Removed here — after another's snapshot where the project still exists, it doesn't come back.
        projects.remove("KEEP");
        assert!(!names(&projects).contains(&"KEEP".to_string()));
        let again = snapshots("list2", &[r#"{"projects": {"KEEP": {"added_at": 500}}}"#]);
        projects.adopt_shared(&again, |_| true);
        assert!(!names(&projects).contains(&"KEEP".to_string()), "removal after the add");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&again);
    }

    /// The snapshot this store would share, as another computer reads it.
    fn snapshot_of(projects: &ProjectStore) -> String {
        let shared = SharedProjects {
            projects: projects
                .entries
                .iter()
                .map(|entry| {
                    let shared = SharedProject {
                        session: entry.session.clone(),
                        pinned_at: entry.pinned_at.unwrap_or(0),
                        previous: entry.previous.clone(),
                        added_at: entry.added_at.unwrap_or(0),
                        about: entry.about.clone(),
                        about_at: entry.about_at.unwrap_or(0),
                    };
                    (key_of(entry), shared)
                })
                .collect(),
            removed: projects.removed.clone(),
            ..Default::default()
        };
        serde_json::to_string(&shared).unwrap()
    }

    #[test]
    fn split_keeps_the_conversation_as_the_first_subproject() {
        let root = Path::new("/r");
        let mut projects = store(root, vec![entry(&root.join("MAPLE"), Some("s-maple"), Some(10), Some(5))], &[]);
        let (kept, new) = projects.split("MAPLE", " API ", "Web").unwrap();
        assert_eq!((kept.as_str(), new.as_str()), ("MAPLE/API", "MAPLE/Web"));
        let list = projects.list();
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].id(), list[0].name(), list[0].folder(), list[0].session()), ("MAPLE/API", "API", "MAPLE", Some("s-maple")));
        assert_eq!((list[1].id(), list[1].session(), list[1].path()), ("MAPLE/Web", None, root.join("MAPLE").as_path()));
        assert_eq!(list[0].title(), "MAPLE › API");
        assert_eq!(list[1].session_name(), "maple-web");
        // The plain project is gone for the other computers.
        assert!(projects.removed.contains_key("MAPLE"));
        // The same names again, or a subproject of a subproject, are refused.
        assert!(projects.split("MAPLE/API", "A", "B").is_err());
        assert!(projects.add_sub("MAPLE", "api").is_err());
        assert_eq!(projects.add_sub("MAPLE", "Docs").unwrap(), "MAPLE/Docs");
        assert_eq!(projects.subs("MAPLE"), ["API", "Web", "Docs"]);
    }

    #[test]
    fn split_names_must_differ_and_fit() {
        let root = Path::new("/r");
        let mut projects = store(root, vec![entry(&root.join("MAPLE"), Some("s"), None, None)], &[]);
        assert!(projects.split("MAPLE", "API", "api").is_err());
        assert!(projects.split("MAPLE", "a/b", "c").is_err());
        assert!(projects.split("MAPLE", "", "c").is_err());
        assert_eq!(names(&projects), ["MAPLE"]);
    }

    #[test]
    fn last_subproject_turns_the_project_ordinary_again() {
        let root = Path::new("/r");
        let mut projects = store(root, vec![entry(&root.join("MAPLE"), Some("s-maple"), Some(10), Some(5))], &[]);
        projects.split("MAPLE", "API", "Web").unwrap();
        projects.add_sub("MAPLE", "Docs").unwrap();
        assert_eq!(projects.remove("MAPLE/Docs"), None);
        assert_eq!(names(&projects), ["MAPLE/API", "MAPLE/Web"]);
        assert_eq!(projects.remove("MAPLE/API"), Some("Web".to_string()));
        assert_eq!(names(&projects), ["MAPLE"]);
        assert!(!projects.removed.contains_key("MAPLE"));
        assert!(projects.removed.contains_key("MAPLE/Web"));
    }

    #[test]
    fn a_split_travels_to_another_computer() {
        let root = Path::new("/r");
        // Both computers have the plain project with the same session.
        let mut here = store(root, vec![entry(&root.join("MAPLE"), Some("s-maple"), Some(10), Some(5))], &[]);
        let mut there = store(Path::new("/other"), vec![entry(Path::new("/other/MAPLE"), Some("s-maple"), Some(10), Some(5))], &[]);
        here.split("MAPLE", "API", "Web").unwrap();
        here.bind("MAPLE/Web", "s-web");
        let dir = snapshots("split", &[&snapshot_of(&here), &snapshot_of(&there)]);
        let adopted = there.adopt_shared(&dir, |_| true);
        assert_eq!(adopted.removed, ["MAPLE"]);
        let list = there.list();
        assert_eq!(names(&there), ["MAPLE/API", "MAPLE/Web"]);
        assert_eq!((list[0].session(), list[1].session()), (Some("s-maple"), Some("s-web")));
        assert_eq!(list[1].path(), Path::new("/other/MAPLE"));

        // Merged back here: the other computer follows.
        here.remove("MAPLE/API");
        assert_eq!(names(&here), ["MAPLE"]);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        here.entries[0].added_at = Some(unix_now());
        let dir = snapshots("merge", &[&snapshot_of(&here), &snapshot_of(&there)]);
        there.adopt_shared(&dir, |_| true);
        assert_eq!(names(&there), ["MAPLE"]);
        assert_eq!(there.list()[0].session(), Some("s-web"));
    }

    #[test]
    fn renaming_a_split_project_moves_all_its_subprojects() {
        let root = Path::new("/r");
        let mut projects = store(root, vec![entry(&root.join("MAPLE"), Some("s"), None, Some(1))], &[]);
        projects.split("MAPLE", "API", "Web").unwrap();
        projects.set_collapsed("MAPLE", true);
        assert_eq!(projects.relocate("MAPLE/Web", root.join("SPRUCE")).unwrap(), "SPRUCE/Web");
        assert_eq!(names(&projects), ["SPRUCE/API", "SPRUCE/Web"]);
        assert!(projects.is_collapsed("SPRUCE") && !projects.is_collapsed("MAPLE"));
        assert!(projects.removed.contains_key("MAPLE/API") && projects.removed.contains_key("MAPLE/Web"));
        projects.remove_folder("SPRUCE");
        assert!(names(&projects).is_empty());
    }

    #[test]
    fn a_folder_is_never_both_ordinary_and_split() {
        let root = Path::new("/r");
        // Here the project is split; a computer on an earlier version added the plain folder again, later.
        let mut here = store(root, vec![entry(&root.join("MAPLE"), Some("s-one"), Some(10), Some(5))], &[]);
        here.split("MAPLE", "One", "Two").unwrap();
        here.set_collapsed("MAPLE", true);
        let later = unix_now() + 100;
        let dir = snapshots("mixed", &[&format!(
            r#"{{"projects": {{"MAPLE": {{"session": "s-plain", "pinned_at": {later}, "added_at": {later}}}}}}}"#
        )]);
        let adopted = here.adopt_shared(&dir, |_| true);
        assert_eq!(names(&here), ["MAPLE/One", "MAPLE/Two"]);
        assert!(adopted.removed.contains(&"MAPLE".to_string()));
        // The plain entry is gone for everyone, and its conversation is kept in the history.
        assert!(here.removed.get("MAPLE").copied().unwrap_or(0) >= later);
        assert!(here.list()[0].previous().iter().any(|previous| previous.id == "s-plain"));
        assert!(here.is_collapsed("MAPLE"));
        // A second pass changes nothing.
        assert!(!here.adopt_shared(&dir, |_| true).changed);
    }

    #[test]
    fn a_session_pinned_only_here_survives_a_split_made_elsewhere() {
        let root = Path::new("/r");
        // Here a newer session was pinned to the plain project; elsewhere the project was split with the old one.
        let mut here = store(root, vec![entry(&root.join("MAPLE"), Some("s-new"), Some(50), Some(5))], &[]);
        here.set_collapsed("MAPLE", true);
        let at = unix_now() + 100;
        let dir = snapshots("orphan", &[&format!(
            r#"{{"projects": {{"MAPLE/One": {{"session": "s-old", "pinned_at": 10, "added_at": {at}}},
                                "MAPLE/Two": {{"added_at": {at}}}}},
                 "removed": {{"MAPLE": {at}}}}}"#
        )]);
        here.adopt_shared(&dir, |_| true);
        assert_eq!(names(&here), ["MAPLE/One", "MAPLE/Two"]);
        let one = &here.list()[0];
        assert_eq!(one.session(), Some("s-old"));
        assert!(one.previous().iter().any(|previous| previous.id == "s-new"));
    }

    #[test]
    fn fold_state_goes_when_the_project_is_not_split_any_more() {
        let root = Path::new("/r");
        let mut here = store(root, vec![entry(&root.join("MAPLE"), Some("s"), Some(10), Some(5))], &[]);
        here.collapsed.insert("MAPLE".into());
        let dir = snapshots("fold", &[r#"{"projects": {}}"#]);
        here.adopt_shared(&dir, |_| true);
        assert!(!here.is_collapsed("MAPLE"));
    }

    #[test]
    fn subprojects_stay_together_in_the_list_and_share_no_session() {
        let root = Path::new("/r");
        let mut projects = store(root, vec![
            entry(&root.join("MAPLE"), Some("s0"), None, None),
            entry(&root.join("ALPHA"), None, None, None),
        ], &[]);
        projects.split("MAPLE", "One", "Two").unwrap();
        projects.entries.push(entry(&root.join("ZETA"), None, None, None));
        projects.add_sub("MAPLE", "Three").unwrap();
        assert_eq!(names(&projects), ["ALPHA", "MAPLE/One", "MAPLE/Two", "MAPLE/Three", "ZETA"]);
        // One conversation — one subproject: pinning it elsewhere removes it from the neighbour's history.
        projects.bind("MAPLE/One", "s1");
        assert!(projects.list()[1].previous().iter().any(|previous| previous.id == "s0"));
        projects.bind("MAPLE/Two", "s0");
        assert!(projects.list()[1].previous().iter().all(|previous| previous.id != "s0"));
    }

    #[test]
    fn what_a_subproject_does_is_one_clean_line_and_travels() {
        let root = Path::new("/r");
        let mut here = store(root, vec![entry(&root.join("MAPLE"), Some("s"), None, Some(5))], &[]);
        let mut there = store(Path::new("/o"), vec![entry(Path::new("/o/MAPLE"), Some("s"), None, Some(5))], &[]);
        here.split("MAPLE", "One", "Two").unwrap();
        here.set_about("MAPLE/One", "  Screens\nand forms\u{7}  ");
        assert_eq!(here.list()[0].about(), "Screens and forms");
        assert_eq!(clean_about(&"é".repeat(500)).chars().count(), ABOUT_MAX);
        let dir = snapshots("about", &[&snapshot_of(&here)]);
        there.adopt_shared(&dir, |_| true);
        assert_eq!(there.list()[0].about(), "Screens and forms");
        // A later change on the other computer wins; an earlier one does not.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        there.set_about("MAPLE/One", "Forms only");
        let dir = snapshots("about2", &[&snapshot_of(&there)]);
        here.adopt_shared(&dir, |_| true);
        assert_eq!(here.list()[0].about(), "Forms only");
        // Cleared: the line goes empty everywhere.
        here.set_about("MAPLE/One", "");
        assert_eq!(here.list()[0].about(), "");
    }

    #[test]
    fn the_open_project_is_remembered() {
        let root = Path::new("/r");
        let mut projects = store(root, vec![entry(&root.join("MAPLE"), Some("s"), None, Some(5)), entry(&root.join("JADE"), None, None, None)], &[]);
        assert_eq!(projects.last_selected(), None);
        projects.split("MAPLE", "One", "Two").unwrap();
        projects.set_last_selected("MAPLE/Two");
        assert_eq!(projects.last_selected(), Some("MAPLE/Two"));
        // A project that is gone is not "last" any more (merged back, removed).
        projects.remove("MAPLE/One");
        assert_eq!(projects.last_selected(), None);
        projects.set_last_selected("JADE");
        assert_eq!(projects.last_selected(), Some("JADE"));
    }
}
