//! Syncing Claude Code sessions with a server (`server/` — `pult-syncd`).
//!
//! What goes to the server: everything a session consists of — the transcript `<id>.jsonl`, the session folder
//! (subagents, saved outputs and images, the title), the project memory (`memory/`) and file snapshots
//! (`~/.claude/file-history/<id>`). Claude Code settings and background-session records do not: they are
//! specific to each computer.
//!
//! A file's address on the server is a "place" plus a path inside it, not a path on disk. A place is the Claude Code
//! data folder (`~/.claude/projects/<path with "-">`) minus the prefix that differs between computers:
//! `root:EMBER` — a project in Pult's main folder, `home:Documents-x` — a folder in the home folder,
//! `abs:…` — everything else, `history:` — file snapshots. So the main folder on another computer
//! can live anywhere and have a different name.
//!
//! A transcript is only ever appended to, so only what appeared after the part the server accepted is sent
//! (zstd-compressed). The server accepts a chunk only as a continuation of its own end — a retry
//! never duplicates anything. If the file on disk was rewritten (the accepted end doesn't match), it is sent again
//! as a new "generation"; the server keeps the previous one. Other files are sent whole when they change.
//!
//! Triggers: Claude Code hooks (`pult sync hook`, see `install_hooks`) hand the work off to the background after
//! every turn; Pult walks everything every few minutes (`start_periodic`). Commands — `pult sync …` (`cli`).
//!
//! Receiving (`pull`) is the reverse: everything this computer lacks or that grew on the server
//! lands in this computer's data folder (`dir_name_for`). A transcript is appended to only if
//! it hasn't changed here since; if it diverged, it is left alone (`conflicts`).

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::i18n::t;
use crate::migrate::encode;

/// How many raw transcript bytes go in one request.
const CHUNK: u64 = 4 << 20;
/// End of the accepted part used to check that the file wasn't rewritten (same as the server).
const TAIL: u64 = 4096;
/// Larger files are not sent whole (the server's request body limit is 32 MB compressed).
pub(crate) const MAX_BLOB: u64 = 24 << 20;
const ZSTD_LEVEL: i32 = 9;
/// How many connections send at once: the server is far away, and a single connection is slow.
const STREAMS: usize = 6;
/// Claude Code events after which new data is sent.
const HOOK_EVENTS: [&str; 5] = ["UserPromptSubmit", "Stop", "SubagentStop", "PreCompact", "SessionEnd"];
const HOOK_MARK: &str = " sync hook";

/// Where things live. Temporary folders in tests.
#[derive(Clone, Debug)]
pub struct Paths {
    /// `~/.claude`
    pub claude: PathBuf,
    /// `~/Library/Application Support/pult`
    pub app: PathBuf,
    /// Pult's main folder.
    pub root: PathBuf,
    pub home: PathBuf,
}

impl Paths {
    pub fn real() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        let app = dirs::data_dir().unwrap_or_else(|| home.join("Library/Application Support")).join("pult");
        let root = fs::read_to_string(app.join("state.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|state| state.get("root").and_then(Value::as_str).map(PathBuf::from))
            .unwrap_or_else(|| home.join("Projects"));
        Self { claude: home.join(".claude"), app, root, home }
    }

    fn config(&self) -> PathBuf {
        self.app.join("sync.json")
    }

    fn state(&self) -> PathBuf {
        self.app.join("sync-state.json")
    }

    fn status(&self) -> PathBuf {
        self.app.join("sync-status.json")
    }

    fn log(&self) -> PathBuf {
        self.app.join("sync.log")
    }
}

/// Connection to the server. Stored in `sync.json` (mode 600): the device key is a secret.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// Address as the person typed it.
    pub address: String,
    /// Full API address (`https://…/sync/v1`).
    pub api: String,
    pub server_name: String,
    pub device: String,
    pub device_name: String,
    token: String,
}

/// What the client knows about what the server accepted — a cache; the server is the source of truth.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct Entry {
    /// Transcript (appended to) or a whole file.
    log: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    len: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    r#gen: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    tail: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    size: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    mtime: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    sha: String,
    /// The transcript's end was accepted from this computer. Only then does a mismatch mean "the file was
    /// rewritten here" and go out as a new generation; otherwise this is an old copy — receiving will extend it.
    #[serde(default, skip_serializing_if = "is_false")]
    own: bool,
}

impl Config {
    /// `Authorization` header with this computer's key.
    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// Which server the cache is for: server changed — cache is empty.
    #[serde(default)]
    api: String,
    #[serde(default)]
    files: BTreeMap<String, Entry>,
    /// Places the server has already been told about: place → path from the base.
    #[serde(default)]
    places: BTreeMap<String, String>,
    /// Server time at the last receive: the next one asks only for what changed since.
    #[serde(default)]
    pulled_at: u64,
    /// When the full list was last taken from the server (once an hour — in case something was missed).
    #[serde(default)]
    pulled_full_at: u64,
    /// Kinds of files this Pult receives (`RECEIVES`). An older Pult skipped files it didn't know but still moved
    /// `pulled_at` past them: after an update the full list is taken once.
    #[serde(default)]
    receives: u32,
}

/// 1 — code snapshots (`pult:code/…`, 0.8).
const RECEIVES: u32 = 1;

/// Summary for the Settings window and `pult sync status`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    /// When everything last got through.
    #[serde(default)]
    pub ok_at: u64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_at: u64,
    /// Raw bytes sent in total.
    #[serde(default)]
    pub sent_bytes: u64,
    /// Files that are not sent: too large.
    #[serde(default)]
    pub skipped: u64,
    /// A full pass is running: how many files are left.
    #[serde(default)]
    pub pending: u64,
    /// When receiving from the server last completed.
    #[serde(default)]
    pub pulled_at: u64,
    /// Raw bytes received in total.
    #[serde(default)]
    pub received_bytes: u64,
    /// Transcripts that changed differently both here and on the server: left alone.
    #[serde(default)]
    pub conflicts: u64,
}

/// A file that must be kept on the server.
#[derive(Clone, Debug)]
struct Item {
    place: String,
    file: String,
    path: PathBuf,
    log: bool,
}

impl Item {
    fn key(&self) -> String {
        format!("{}\n{}", self.place, self.file)
    }
}

/// What to send.
pub enum Scope {
    All,
    /// One session: its transcript, folder, project memory and file snapshots.
    Transcript(PathBuf),
    /// Only Pult's project snapshot (the list or a session changed).
    Projects,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|time| time.as_secs()).unwrap_or_default()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Write a whole file via a temporary one; `secret` — mode 600.
pub(crate) fn write_file(path: &Path, text: &[u8], secret: bool) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    if secret {
        options.mode(0o600);
    }
    options
        .open(&temp)
        .and_then(|mut file| file.write_all(text))
        .and_then(|()| fs::rename(&temp, path))
        .map_err(|error| tr!("sync.couldn_t_write_path_error", path = path.display(), error = error))
}

fn log_line(paths: &Path, text: &str) {
    // The log doesn't grow forever: over a megabyte — it starts over.
    if fs::metadata(paths).is_ok_and(|meta| meta.len() > 1 << 20) {
        let _ = fs::rename(paths, paths.with_extension("log.old"));
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(paths) {
        let _ = writeln!(file, "{} {text}", now());
    }
}

pub fn config(paths: &Paths) -> Option<Config> {
    read_json(&paths.config())
}

pub fn status(paths: &Paths) -> Status {
    read_json(&paths.status()).unwrap_or_default()
}

/// Sync is connected and the last attempt failed (the error is newer than the last success).
pub fn is_failing() -> bool {
    let paths = Paths::real();
    if config(&paths).is_none() {
        return false;
    }
    let status = status(&paths);
    status.error.is_some() && status.error_at > status.ok_at
}

// ───────────────────────────── places ─────────────────────────────

/// Place of a Claude Code data folder by its name: the prefix that differs between computers is dropped.
/// `cwd` — the working folder from the transcript, if known: a folder name is ambiguous ("/a-b" and
/// "/a/b" give the same one), and `~/CODE-OLD/x` must not end up in the main folder `~/CODE`.
fn place_of(name: &str, cwd: Option<&Path>, root: &Path, home: &Path) -> String {
    let under = |base: &Path| -> Option<String> {
        if let Some(cwd) = cwd {
            return strip_base(cwd, base).map(|rel| encode_rel(&rel));
        }
        let prefix = encode(base);
        let head = name.get(..prefix.len())?;
        if !head.eq_ignore_ascii_case(&prefix) {
            return None;
        }
        match &name[prefix.len()..] {
            "" => Some(String::new()),
            rest => rest.strip_prefix('-').map(str::to_string),
        }
    };
    if let Some(rest) = under(root) {
        format!("root:{rest}")
    } else if let Some(rest) = under(home) {
        format!("home:{rest}")
    } else {
        format!("abs:{name}")
    }
}

/// Path relative to the base. The base's letter case doesn't matter: old transcripts record the main folder
/// with its former spelling (`~/Code` before it was renamed to `~/CODE`).
fn strip_base(path: &Path, base: &Path) -> Option<PathBuf> {
    let (path, base) = (path.to_str()?, base.to_str()?);
    let head = path.get(..base.len())?;
    if !head.eq_ignore_ascii_case(base) {
        return None;
    }
    match &path[base.len()..] {
        "" => Some(PathBuf::new()),
        rest => rest.strip_prefix('/').map(PathBuf::from),
    }
}

fn encode_rel(rel: &Path) -> String {
    if rel.as_os_str().is_empty() { String::new() } else { encode(rel) }
}

/// Data folder name for a place on this computer (the inverse of `place_of`) — where to put a session
/// that came from another computer.
pub fn dir_name_for(place: &str, root: &Path, home: &Path) -> Option<String> {
    let (base, rest) = place.split_once(':')?;
    let join = |base: &Path| if rest.is_empty() { encode(base) } else { format!("{}-{rest}", encode(base)) };
    match base {
        "root" => Some(join(root)),
        "home" => Some(join(home)),
        "abs" => Some(rest.to_string()),
        _ => None,
    }
}

/// Working folder recorded in the transcript that yields exactly this data folder name. Only the beginning
/// of one file is read: transcripts can be half a gigabyte.
fn recorded_cwd(dir: &Path, name: &str) -> Option<PathBuf> {
    const KEY: &str = "\"cwd\":\"";
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    for path in files.iter().rev().take(3) {
        let mut head = Vec::new();
        if fs::File::open(path).and_then(|file| file.take(256 << 10).read_to_end(&mut head)).is_err() {
            continue;
        }
        let text = String::from_utf8_lossy(&head);
        let mut rest = text.as_ref();
        while let Some(start) = rest.find(KEY) {
            rest = &rest[start + KEY.len()..];
            let Some(end) = rest.find('"') else { break };
            let cwd = PathBuf::from(&rest[..end]);
            if encode(&cwd).eq_ignore_ascii_case(name) {
                return Some(cwd);
            }
            rest = &rest[end..];
        }
    }
    None
}

// ───────────────────────────── walk ─────────────────────────────

fn walk(dir: &Path, prefix: &str, place: &str, out: &mut Vec<Item>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".DS_Store" || name.ends_with(".tmp") {
            continue;
        }
        let file = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            walk(&entry.path(), &file, place, out);
        } else if kind.is_file() {
            out.push(Item { place: place.to_string(), log: name.ends_with(".jsonl"), file, path: entry.path() });
        }
    }
}

/// A data folder and its place.
fn place_of_dir(paths: &Paths, dir: &Path) -> Option<(String, Option<PathBuf>)> {
    let name = dir.file_name()?.to_string_lossy().into_owned();
    let cwd = recorded_cwd(dir, &name);
    Some((place_of(&name, cwd.as_deref(), &paths.root, &paths.home), cwd))
}

/// All files that must be on the server, and the places with their paths from the base.
fn scan(paths: &Paths, scope: &Scope) -> (Vec<Item>, BTreeMap<String, String>) {
    let mut items = Vec::new();
    let mut places = BTreeMap::new();
    let mut note_place = |place: &str, cwd: Option<&Path>| {
        let rel = cwd
            .and_then(|cwd| strip_base(cwd, &paths.root).or_else(|| strip_base(cwd, &paths.home)))
            .map(|rel| rel.to_string_lossy().into_owned())
            .unwrap_or_default();
        places.insert(place.to_string(), rel);
    };
    let history = paths.claude.join("file-history");
    match scope {
        Scope::All => {
            if let Ok(entries) = fs::read_dir(paths.claude.join("projects")) {
                for entry in entries.filter_map(Result::ok).filter(|entry| entry.path().is_dir()) {
                    if let Some((place, cwd)) = place_of_dir(paths, &entry.path()) {
                        note_place(&place, cwd.as_deref());
                        walk(&entry.path(), "", &place, &mut items);
                    }
                }
            }
            walk(&history, "", "history:", &mut items);
            if let Ok(entries) = fs::read_dir(crate::code::dir(paths)) {
                for entry in entries.filter_map(Result::ok) {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if crate::code::is_code_file(&name) {
                        items.push(Item { place: "pult:".into(), file: format!("code/{name}"), path: entry.path(), log: false });
                    }
                }
            }
        }
        Scope::Transcript(transcript) => {
            let (Some(dir), Some(session)) = (transcript.parent(), transcript.file_stem()) else {
                return (items, places);
            };
            let session = session.to_string_lossy().into_owned();
            if let Some((place, cwd)) = place_of_dir(paths, dir) {
                note_place(&place, cwd.as_deref());
                if transcript.is_file() {
                    items.push(Item { place: place.clone(), file: format!("{session}.jsonl"), path: transcript.clone(), log: true });
                }
                walk(&dir.join(&session), &session, &place, &mut items);
                walk(&dir.join("memory"), "memory", &place, &mut items);
            }
            walk(&history.join(&session), &session, "history:", &mut items);
            for (file, path) in crate::code::files(paths, &session) {
                items.push(Item { place: "pult:".into(), file, path, log: false });
            }
        }
        Scope::Projects => {}
    }
    if let Some(item) = shared_item(paths) {
        items.push(item);
    }
    (items, places)
}

/// Name of our own snapshot in the `shared` folder (`None` — sync isn't connected).
pub fn own_snapshot(paths: &Paths) -> Option<String> {
    Some(format!("projects-{}.json", config(paths)?.device))
}

/// Send a session's transcript and code now, in the background (it was stopped to move to another computer).
pub fn push_transcript_now(transcript: PathBuf) {
    std::thread::spawn(move || {
        let paths = Paths::real();
        if config(&paths).is_some() {
            if let Some(session) = transcript.file_stem().map(|stem| stem.to_string_lossy().into_owned()) {
                crate::code::snapshot(&paths, &session, Some(&transcript), None, true);
            }
            let _ = push(&paths, &Scope::Transcript(transcript));
        }
    });
}

/// Receive what's new from the server now, in the background (a session move is waiting for its transcript).
pub fn pull_now() {
    std::thread::spawn(|| {
        let paths = Paths::real();
        if config(&paths).is_some() {
            let _ = pull(&paths);
        }
    });
}

/// Folder of pinned-session snapshots: ours and other computers' (`projects-<computer>.json`).
pub fn shared_dir(paths: &Paths) -> PathBuf {
    paths.app.join("shared")
}

/// Our own pin snapshot to send. The file is rewritten only when the snapshot changed:
/// sending looks at the file's size and time.
fn shared_item(paths: &Paths) -> Option<Item> {
    let config = config(paths)?;
    let device = config.device;
    if device.is_empty() || !device.chars().all(|char| char.is_ascii_alphanumeric() || char == '-' || char == '_') {
        return None;
    }
    let mut pins = crate::projects::shared_projects(&paths.app.join("state.json"))?;
    // A session with no conversation yet won't start on another computer: the project goes out
    // without it, and the session follows once there is a conversation.
    for project in pins.projects.values_mut() {
        if project.session.as_deref().is_some_and(|session| !crate::sessions::session_has_conversation(session)) {
            project.session = None;
        }
    }
    pins.device_name = config.device_name;
    pins.presence = crate::presence::Presence::load();
    let text = serde_json::to_string_pretty(&pins).ok()? + "\n";
    let file = format!("projects-{device}.json");
    let path = shared_dir(paths).join(&file);
    if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        write_file(&path, text.as_bytes(), false).ok()?;
    }
    Some(Item { place: "pult:".into(), file, path, log: false })
}

// ───────────────────────────── server ─────────────────────────────

struct Client {
    agent: ureq::Agent,
    api: String,
    token: String,
}

/// Server response about a file.
#[derive(Debug, Default, Deserialize)]
struct Head {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    r#gen: u64,
    #[serde(default)]
    len: u64,
    #[serde(default)]
    tail: String,
    #[serde(default)]
    sha: String,
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .user_agent(concat!("pult/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// No connection. The detail (`error`) is for the log; it means nothing to a person.
fn network(_error: ureq::Error) -> String {
    t("sync.can_t_reach_server_pult").into()
}

fn refusal(status: u16, body: &str) -> String {
    let name = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    match (status, name.as_str()) {
        (401, _) => {
            t("sync.server_doesn_t_recognize_this").into()
        }
        (507, _) => t("sync.server_is_out_of_space").into(),
        (_, "wrong_code") => t("sync.that_code_didn_t_work").into(),
        (_, "pairing_expired") => t("sync.code_has_expired_click_cancel").into(),
        (_, "too_many_requests") => t("sync.too_many_attempts_wait_ten").into(),
        (_, "pairing_locked") => t("sync.too_many_wrong_codes_new")
        .into(),
        _ => tr!("sync.server_replied_status_name", name = name, status = status),
    }
}

impl Client {
    fn new(config: &Config) -> Self {
        Self { agent: agent(Duration::from_secs(120)), api: config.api.clone(), token: config.token.clone() }
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    fn post(&self, path: &str, body: &Value) -> Result<(u16, String), String> {
        let mut response = self
            .agent
            .post(format!("{}{path}", self.api))
            .header("Authorization", self.auth())
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(network)?;
        let text = response.body_mut().read_to_string().map_err(network)?;
        Ok((response.status().as_u16(), text))
    }

    fn put(&self, path: &str, query: &[(&str, &str)], body: &[u8]) -> Result<(u16, String), String> {
        let mut request = self.agent.put(format!("{}{path}", self.api)).header("Authorization", self.auth());
        for (key, value) in query {
            request = request.query(*key, *value);
        }
        let mut response = request.send(body).map_err(network)?;
        let text = response.body_mut().read_to_string().map_err(network)?;
        Ok((response.status().as_u16(), text))
    }

    fn get(&self, path: &str) -> Result<(u16, String), String> {
        let mut response =
            self.agent.get(format!("{}{path}", self.api)).header("Authorization", self.auth()).call().map_err(network)?;
        let text = response.body_mut().read_to_string().map_err(network)?;
        Ok((response.status().as_u16(), text))
    }

    /// What the server already stores for these files (`None` — nothing).
    fn heads(&self, items: &[&Item]) -> Result<Vec<Option<Head>>, String> {
        let list: Vec<Value> = items.iter().map(|item| json!({"place": item.place, "file": item.file})).collect();
        let (status, text) = self.post("/heads", &json!({ "items": list }))?;
        if status != 200 {
            return Err(refusal(status, &text));
        }
        #[derive(Deserialize)]
        struct Heads {
            items: Vec<Option<Head>>,
        }
        serde_json::from_str::<Heads>(&text).map(|heads| heads.items).map_err(|error| tr!("sync.couldn_t_read_server_s", error = error))
    }
}

// ───────────────────────────── sending ─────────────────────────────

/// Fingerprint of the last `TAIL` bytes before `len` — computed the same way as on the server.
fn tail_hash(path: &Path, len: u64) -> Option<String> {
    if len == 0 {
        return Some(String::new());
    }
    let start = len.saturating_sub(TAIL);
    let mut file = fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = vec![0; (len - start) as usize];
    file.read_exact(&mut bytes).ok()?;
    Some(sha_hex(&bytes))
}

fn read_range(path: &Path, offset: u64, len: u64) -> Result<Vec<u8>, String> {
    let read = || -> std::io::Result<Vec<u8>> {
        let mut file = fs::File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; len as usize];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    };
    read().map_err(|error| tr!("sync.couldn_t_read_path_error", path = path.display(), error = error))
}

fn mtime(meta: &fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map(|time| time.as_millis() as u64).unwrap_or_default()
}

/// Whether anything needs doing with a file — from the cache, without asking the server.
fn needs_work(item: &Item, entry: Option<&Entry>) -> bool {
    let Ok(meta) = fs::metadata(&item.path) else {
        return false;
    };
    match entry {
        None => true,
        Some(entry) if item.log => meta.len() != entry.len,
        Some(entry) => meta.len() != entry.size || mtime(&meta) != entry.mtime,
    }
}

/// One step of sending a file: a transcript chunk or a whole file. `Ok(true)` — the whole file is on the server.
/// Also returns the number of raw bytes sent.
fn step(client: &Client, item: &Item, entry: &mut Entry) -> Result<(bool, u64), String> {
    let Ok(meta) = fs::metadata(&item.path) else {
        return Ok((true, 0));
    };
    let size = meta.len();
    if !item.log {
        if size > MAX_BLOB {
            return Err(String::new());
        }
        let bytes = fs::read(&item.path).map_err(|error| tr!("sync.couldn_t_read_path_error", path = item.path.display(), error = error))?;
        let sha = sha_hex(&bytes);
        let mut sent = 0;
        if sha != entry.sha {
            let body = zstd::bulk::compress(&bytes, ZSTD_LEVEL).map_err(|error| error.to_string())?;
            let (status, text) = client.put("/blob", &[("place", &item.place), ("file", &item.file), ("sha256", &sha)], &body)?;
            if status != 200 {
                return Err(refusal(status, &text));
            }
            sent = bytes.len() as u64;
        }
        *entry = Entry { log: false, size, mtime: mtime(&meta), sha, ..Entry::default() };
        return Ok((true, sent));
    }
    entry.log = true;
    if entry.r#gen == 0 {
        entry.r#gen = 1;
    }
    // The accepted end doesn't match the file.
    if size < entry.len || tail_hash(&item.path, entry.len).as_deref() != Some(entry.tail.as_str()) {
        if !entry.own {
            // Another computer wrote the end: this is an old copy or a branch of its own. Don't push out
            // the server's version — receiving will extend the old one, a diverged one shows up in the log.
            return Ok((true, 0));
        }
        // The file was rewritten on this computer: start a new generation.
        entry.r#gen += 1;
        entry.len = 0;
        entry.tail = String::new();
    }
    if size == entry.len {
        return Ok((true, 0));
    }
    let len = (size - entry.len).min(CHUNK);
    let bytes = read_range(&item.path, entry.len, len)?;
    let body = zstd::bulk::compress(&bytes, ZSTD_LEVEL).map_err(|error| error.to_string())?;
    let (generation, offset) = (entry.r#gen.to_string(), entry.len.to_string());
    let query = [("place", item.place.as_str()), ("file", item.file.as_str()), ("gen", &generation), ("offset", &offset)];
    let (status, text) = client.put("/log", &query, &body)?;
    let head: Head = match status {
        200 | 409 => serde_json::from_str(&text).map_err(|error| tr!("sync.couldn_t_read_server_s", error = error))?,
        _ => return Err(refusal(status, &text)),
    };
    if status == 409 {
        // The server has a different end (stale cache or another computer wrote it): take it as the truth,
        // the next step checks it against the file and either continues or starts a new generation.
        *entry = Entry { log: true, len: head.len, r#gen: head.r#gen.max(1), tail: head.tail, own: false, ..Entry::default() };
        return Ok((false, 0));
    }
    if head.len != entry.len + len {
        return Err(t("sync.server_accepted_different_amount_than").into());
    }
    *entry = Entry { log: true, len: head.len, r#gen: head.r#gen, tail: head.tail, own: true, ..Entry::default() };
    Ok((head.len >= size, len))
}

struct Store<'a> {
    paths: &'a Paths,
    api: &'a str,
}

impl Store<'_> {
    fn load(&self) -> State {
        let state: State = read_json(&self.paths.state()).unwrap_or_default();
        if state.api == self.api { state } else { State { api: self.api.to_string(), ..State::default() } }
    }

    /// Change the cache under the lock: several processes send at once (hooks, Pult).
    fn locked<T>(&self, work: impl FnOnce(&mut State) -> T) -> Result<T, String> {
        fs::create_dir_all(&self.paths.app).map_err(|error| error.to_string())?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.paths.app.join("sync.lock"))
            .map_err(|error| error.to_string())?;
        lock.lock().map_err(|error| error.to_string())?;
        let mut state = self.load();
        let result = work(&mut state);
        let text = serde_json::to_string(&state).map_err(|error| error.to_string())?;
        write_file(&self.paths.state(), text.as_bytes(), false)?;
        Ok(result)
    }
}

/// Send everything new. Returns the number of raw bytes sent.
pub fn push(paths: &Paths, scope: &Scope) -> Result<u64, String> {
    let Some(config) = config(paths) else {
        return Err(t("sync.sync_isn_t_connected").into());
    };
    let result = push_with(paths, &config, scope);
    let mut status = status(paths);
    match &result {
        Ok((sent, skipped)) => {
            status.ok_at = now();
            status.error = None;
            status.sent_bytes += sent;
            if matches!(scope, Scope::All) {
                status.skipped = *skipped;
            }
        }
        Err(error) => {
            status.error = Some(error.clone());
            status.error_at = now();
            log_line(&paths.log(), &format!("error: {error}"));
        }
    }
    status.pending = 0;
    let _ = write_file(&paths.status(), serde_json::to_string(&status).unwrap_or_default().as_bytes(), false);
    result.map(|(sent, _)| sent)
}

/// What a worker thread reports about its work.
enum Progress {
    /// A step is done: the new cache entry, bytes sent, whether the file is finished.
    Step(String, Entry, u64, bool),
    Skipped,
    Failed(String),
}

fn push_with(paths: &Paths, config: &Config, scope: &Scope) -> Result<(u64, u64), String> {
    let client = Client::new(config);
    let store = Store { paths, api: &config.api };
    let (items, places) = scan(paths, scope);
    let snapshot = store.load();

    // Places the server doesn't know about yet.
    for (place, rel) in &places {
        if snapshot.places.get(place) == Some(rel) {
            continue;
        }
        let base = place.split_once(':').map(|(base, _)| base).unwrap_or_default();
        let body = json!({"place": place, "base": base, "rel": rel});
        let (status, text) = client.put("/place", &[], body.to_string().as_bytes())?;
        if status != 200 {
            return Err(refusal(status, &text));
        }
        store.locked(|state| state.places.insert(place.clone(), rel.clone()))?;
    }

    let mut todo: Vec<&Item> = items.iter().filter(|item| needs_work(item, snapshot.files.get(&item.key()))).collect();
    // What isn't in the cache may already be on the server (fresh install, another computer).
    let unknown: Vec<&Item> = todo.iter().copied().filter(|item| !snapshot.files.contains_key(&item.key())).collect();
    for batch in unknown.chunks(1000) {
        let heads = client.heads(batch)?;
        store.locked(|state| {
            for (item, head) in batch.iter().zip(heads) {
                if let Some(head) = head {
                    let entry = match head.kind.as_str() {
                        "log" => Entry { log: true, len: head.len, r#gen: head.r#gen, tail: head.tail, ..Entry::default() },
                        _ => Entry { log: false, sha: head.sha, ..Entry::default() },
                    };
                    state.files.entry(item.key()).or_insert(entry);
                }
            }
        })?;
    }
    if todo.is_empty() {
        return Ok((0, 0));
    }
    let known = store.load().files;
    // By size: some threads take from the small end (a fresh turn goes out at once), some from the
    // large end (a big transcript takes longest on one connection and must not start last).
    todo.sort_by_key(|item| fs::metadata(&item.path).map(|meta| meta.len()).unwrap_or_default());
    let total = todo.len();
    let queue = Mutex::new(VecDeque::from(todo));
    let stop = AtomicBool::new(false);
    let (report, progress) = mpsc::channel::<Progress>();

    let (mut sent, mut skipped, mut finished) = (0u64, 0u64, 0usize);
    let mut failure = None;
    std::thread::scope(|threads| {
        for worker in 0..STREAMS.min(total) {
            let (queue, stop, known, report) = (&queue, &stop, &known, report.clone());
            threads.spawn(move || {
                // Each thread has its own connection: speed is limited per connection, not per link.
                let client = Client::new(config);
                while !stop.load(Ordering::Relaxed) {
                    let item = {
                        let mut queue = queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        if worker % 2 == 0 { queue.pop_front() } else { queue.pop_back() }
                    };
                    let Some(item) = item else { break };
                    let mut entry = known.get(&item.key()).cloned().unwrap_or_default();
                    loop {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let message = match step(&client, item, &mut entry) {
                            Ok((done, bytes)) => Progress::Step(item.key(), entry.clone(), bytes, done),
                            // An empty error means the file is too large: skip it, the rest goes on.
                            Err(error) if error.is_empty() => Progress::Skipped,
                            Err(error) => Progress::Failed(error),
                        };
                        let last = !matches!(message, Progress::Step(_, _, _, false));
                        if matches!(message, Progress::Failed(_)) {
                            stop.store(true, Ordering::Relaxed);
                        }
                        if report.send(message).is_err() || last {
                            break;
                        }
                    }
                }
            });
        }
        drop(report);

        // This thread collects finished work and writes the cache every couple of seconds: the lock is held for an instant,
        // and another session's hook doesn't wait for the whole history to go out.
        let mut pending: BTreeMap<String, Entry> = BTreeMap::new();
        let mut flushed = Instant::now();
        let flush = |pending: &mut BTreeMap<String, Entry>, left: usize| {
            if !pending.is_empty() {
                let updates = std::mem::take(pending);
                let _ = store.locked(|state| state.files.extend(updates));
            }
            if matches!(scope, Scope::All) {
                let mut status = status(paths);
                status.pending = left as u64;
                let _ = write_file(&paths.status(), serde_json::to_string(&status).unwrap_or_default().as_bytes(), false);
            }
        };
        loop {
            match progress.recv_timeout(Duration::from_secs(1)) {
                Ok(Progress::Step(key, entry, bytes, done)) => {
                    sent += bytes;
                    finished += usize::from(done);
                    pending.insert(key, entry);
                }
                Ok(Progress::Skipped) => {
                    skipped += 1;
                    finished += 1;
                }
                Ok(Progress::Failed(error)) => {
                    failure.get_or_insert(error);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if flushed.elapsed() > Duration::from_secs(2) {
                flushed = Instant::now();
                flush(&mut pending, total - finished);
            }
        }
        flush(&mut pending, 0);
    });
    match failure {
        Some(error) => Err(error),
        None => Ok((sent, skipped)),
    }
}

// ───────────────────────────── connecting ─────────────────────────────

/// What the server says about itself at `/.well-known/pult-sync`.
#[derive(Debug, Deserialize)]
pub struct Discovery {
    pub service: String,
    pub protocol: u64,
    pub api: String,
    pub name: String,
}

/// Address as the person typed it → URL prefix.
fn origin(address: &str) -> String {
    let address = address.trim().trim_end_matches('/');
    let address = address.strip_suffix("/.well-known/pult-sync").unwrap_or(address);
    if address.contains("://") { address.to_string() } else { format!("https://{address}") }
}

/// Find a sync server by address: returns its description and the full API address.
pub fn discover(address: &str) -> Result<(Discovery, String), String> {
    let origin = origin(address);
    let mut response = agent(Duration::from_secs(15))
        .get(format!("{origin}/.well-known/pult-sync"))
        .call()
        .map_err(|_| t("sync.couldn_t_reach_server_check").to_string())?;
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|_| t("sync.couldn_t_reach_server_check").to_string())?;
    let found: Discovery = serde_json::from_str(&text)
        .ok()
        .filter(|found: &Discovery| found.service == "pult-sync")
        .ok_or_else(|| t("sync.there_s_no_sync_server").to_string())?;
    if found.protocol != 1 {
        return Err(t("sync.server_is_newer_than_this").into());
    }
    let api = if found.api.contains("://") { found.api.clone() } else { format!("{origin}{}", found.api) };
    Ok((found, api))
}

fn post_open(api: &str, path: &str, body: &Value) -> Result<Value, String> {
    let mut response = agent(Duration::from_secs(20))
        .post(format!("{api}{path}"))
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(network)?;
    let text = response.body_mut().read_to_string().map_err(network)?;
    if response.status().as_u16() != 200 {
        return Err(refusal(response.status().as_u16(), &text));
    }
    serde_json::from_str(&text).map_err(|error| tr!("sync.couldn_t_read_server_s", error = error))
}

/// This computer's name, as in System Settings.
pub fn computer_name() -> String {
    std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Mac".into())
}

/// A connection-code request the server accepted.
pub struct Pairing {
    pub id: String,
    /// How the code arrives: `telegram` or `console` (the server log).
    pub via: String,
    /// How many digits the code has.
    pub digits: usize,
}

/// Ask the server to send a connection code to its owner.
pub fn pair_start(api: &str, name: &str) -> Result<Pairing, String> {
    let reply = post_open(api, "/pair/start", &json!({"name": name, "platform": "macos"}))?;
    let field = |key: &str| reply.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let digits = reply.get("digits").and_then(Value::as_u64).unwrap_or(6) as usize;
    Ok(Pairing { id: field("pairing"), via: field("via"), digits })
}

/// Exchange the code for a device key and save the connection.
pub fn pair_finish(paths: &Paths, address: &str, api: &str, name: &str, pairing: &str, code: &str) -> Result<Config, String> {
    let reply = post_open(api, "/pair/finish", &json!({"pairing": pairing, "code": code}))?;
    let field = |key: &str| reply.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    save_config(paths, address, api, name, &field("server"), &field("device"), &field("token"))
}

/// Connect with a ready device key (printed by `pult-syncd pair` on the server).
pub fn connect_with_token(paths: &Paths, address: &str, api: &str, name: &str, token: &str) -> Result<Config, String> {
    let draft = Config {
        address: address.into(),
        api: api.into(),
        server_name: String::new(),
        device: String::new(),
        device_name: name.into(),
        token: token.into(),
    };
    let (status, text) = Client::new(&draft).get("/me")?;
    if status != 200 {
        return Err(refusal(status, &text));
    }
    let reply: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    let field = |key: &str| reply.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    save_config(paths, address, api, &field("device_name"), &field("server"), &field("device"), token)
}

fn save_config(paths: &Paths, address: &str, api: &str, name: &str, server: &str, device: &str, token: &str) -> Result<Config, String> {
    if token.is_empty() {
        return Err(t("sync.server_didn_t_issue_device").into());
    }
    let config = Config {
        address: address.trim().to_string(),
        api: api.to_string(),
        server_name: server.to_string(),
        device: device.to_string(),
        device_name: name.to_string(),
        token: token.to_string(),
    };
    let text = serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?;
    write_file(&paths.config(), text.as_bytes(), true)?;
    Ok(config)
}

/// Disconnect this computer: the server forgets its key, hooks are removed. Data on the server stays.
pub fn disconnect(paths: &Paths) -> Result<(), String> {
    if let Some(config) = config(paths) {
        let client = Client::new(&config);
        let _ = client.agent.delete(format!("{}/device", client.api)).header("Authorization", client.auth()).call();
    }
    uninstall_hooks(paths)?;
    for path in [paths.config(), paths.state(), paths.status()] {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

/// Server summary: how much it stores, how much is free.
pub fn server_status(paths: &Paths) -> Result<Value, String> {
    let config = config(paths).ok_or(t("sync.sync_isn_t_connected"))?;
    let (status, text) = Client::new(&config).get("/status")?;
    if status != 200 {
        return Err(refusal(status, &text));
    }
    serde_json::from_str(&text).map_err(|error| error.to_string())
}

// ───────────────────────────── Claude Code hooks ─────────────────────────────

fn hook_command(exe: &Path) -> String {
    format!("'{}'{HOOK_MARK}", exe.display())
}

/// Program the hooks call: the installed Pult, or this build during development.
pub fn hook_exe() -> PathBuf {
    let installed = PathBuf::from("/Applications/Pult.app/Contents/MacOS/pult");
    let current = std::env::current_exe().unwrap_or_else(|_| installed.clone());
    if current.starts_with("/Applications") || !installed.exists() { current } else { installed }
}

fn is_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| hooks.iter().any(|hook| hook.get("command").and_then(Value::as_str).is_some_and(|command| command.ends_with(HOOK_MARK))))
}

/// Write the sync hooks into Claude Code settings without touching others'. `false` — already there.
fn merge_hooks(settings: &mut Value, command: &str) -> bool {
    if !settings.is_object() {
        *settings = json!({});
    }
    let hooks = settings.as_object_mut().unwrap().entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let mut changed = false;
    for event in HOOK_EVENTS {
        let groups = hooks.as_object_mut().unwrap().entry(event).or_insert_with(|| json!([]));
        if !groups.is_array() {
            *groups = json!([]);
        }
        let groups = groups.as_array_mut().unwrap();
        let ours = json!({"hooks": [{"type": "command", "command": command, "timeout": 10}]});
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

pub(crate) fn edit_settings(paths: &Paths, edit: impl FnOnce(&mut Value) -> bool) -> Result<bool, String> {
    let path = paths.claude.join("settings.json");
    let mut settings: Value = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| tr!("sync.can_t_read_claude_settings", error = error))?,
        Err(_) => json!({}),
    };
    if !edit(&mut settings) {
        return Ok(false);
    }
    if path.exists() {
        let _ = fs::copy(&path, paths.app.join("claude-settings.bak-sync.json"));
    }
    let text = serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())?;
    write_file(&path, format!("{text}\n").as_bytes(), false)?;
    Ok(true)
}

/// How many days Claude Code keeps transcripts (`cleanupPeriodDays`). By default it deletes everything older than
/// 30 days — and sync would resurrect what was deleted on other computers. Claude Code doesn't accept zero
/// ("must be at least 1"), so "never delete" means a hundred years.
pub const KEEP_DAYS: u64 = 36_500;

/// Turn off automatic transcript deletion: set `cleanupPeriodDays` if it's missing or smaller.
/// A user value no smaller than ours is kept.
fn keep_transcripts_in(settings: &mut Value) -> bool {
    let Some(object) = settings.as_object_mut() else {
        return false;
    };
    if object.get("cleanupPeriodDays").and_then(Value::as_u64).is_some_and(|days| days >= KEEP_DAYS) {
        return false;
    }
    object.insert("cleanupPeriodDays".into(), json!(KEEP_DAYS));
    true
}

pub fn keep_transcripts(paths: &Paths) -> Result<bool, String> {
    edit_settings(paths, keep_transcripts_in)
}

pub fn install_hooks(paths: &Paths, exe: &Path) -> Result<bool, String> {
    let command = hook_command(exe);
    edit_settings(paths, |settings| {
        let hooks = merge_hooks(settings, &command);
        keep_transcripts_in(settings) || hooks
    })
}

pub fn uninstall_hooks(paths: &Paths) -> Result<bool, String> {
    edit_settings(paths, remove_hooks)
}

/// Claude Code hook: reads the event, hands sending off to the background and returns immediately.
fn hook(paths: &Paths) {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    if config(paths).is_none() {
        return;
    }
    let event = serde_json::from_str::<Value>(&input).ok();
    let Some(transcript) = event.as_ref().and_then(|event| event.get("transcript_path").and_then(Value::as_str)).map(str::to_string)
    else {
        return;
    };
    let cwd = event.as_ref().and_then(|event| event.get("cwd").and_then(Value::as_str)).unwrap_or_default().to_string();
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;
    // Own process group and empty streams: Claude Code doesn't wait for the background send.
    let _ = std::process::Command::new(exe)
        .args(["sync", "push", "--transcript", &transcript, "--cwd", &cwd])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
}

// ───────────────────────────── receiving ─────────────────────────────

/// A file on the server (`GET /files`).
#[derive(Clone, Debug, Deserialize)]
struct Remote {
    place: String,
    file: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    r#gen: u64,
    #[serde(default)]
    len: u64,
    #[serde(default)]
    tail: String,
    #[serde(default)]
    sha: String,
}

impl Remote {
    fn key(&self) -> String {
        format!("{}\n{}", self.place, self.file)
    }
}

/// Where a file from the server lives on this computer. `None` — unsafe name or unknown place.
fn local_path(paths: &Paths, place: &str, file: &str) -> Option<PathBuf> {
    let rel = Path::new(file);
    if file.is_empty() || !rel.components().all(|part| matches!(part, std::path::Component::Normal(_))) {
        return None;
    }
    if place == "history:" {
        return Some(paths.claude.join("file-history").join(rel));
    }
    if place == "pult:" {
        if let Some(name) = file.strip_prefix("code/") {
            return crate::code::is_code_file(name).then(|| crate::code::dir(paths).join(name));
        }
        let simple = !file.contains('/') && file.starts_with("projects-") && file.ends_with(".json");
        return simple.then(|| shared_dir(paths).join(file));
    }
    let dir = dir_name_for(place, &paths.root, &paths.home)?;
    if dir.is_empty() || dir.contains('/') || dir == "." || dir == ".." {
        return None;
    }
    Some(paths.claude.join("projects").join(dir).join(rel))
}

/// What to do with a file from the server.
#[derive(Debug, PartialEq)]
enum Fetch {
    /// Everything here matches the server — just remember it.
    Same(Entry),
    /// Append to the transcript from this offset (if the local beginning matches the server's).
    Append(u64),
    /// Take it whole: the file isn't here, or the file (not a transcript) wasn't touched here but changed there.
    Whole,
    /// Changed both here and there.
    Conflict,
    Nothing,
}

/// Decide from the cache and the disk without asking the server. An existing transcript is never replaced:
/// only appended to, and only when the local one is a prefix of the server's.
fn decide(remote: &Remote, path: &Path, cached: Option<&Entry>) -> Fetch {
    let meta = fs::metadata(path).ok();
    if remote.kind == "log" {
        let Some(meta) = meta else {
            // Not here: if it used to be, it was deliberately deleted on this computer — don't resurrect it.
            return if cached.is_some() { Fetch::Nothing } else { Fetch::Whole };
        };
        let size = meta.len();
        if cached.is_some_and(|cached| cached.log && cached.len == size && cached.len == remote.len && cached.tail == remote.tail) {
            return Fetch::Nothing;
        }
        return match size.cmp(&remote.len) {
            std::cmp::Ordering::Less => Fetch::Append(size),
            std::cmp::Ordering::Greater => Fetch::Nothing,
            std::cmp::Ordering::Equal if tail_hash(path, size).as_deref() == Some(remote.tail.as_str()) => {
                let own = cached.is_some_and(|cached| cached.own && cached.len == size);
                Fetch::Same(Entry { log: true, len: size, r#gen: remote.r#gen, tail: remote.tail.clone(), own, ..Entry::default() })
            }
            std::cmp::Ordering::Equal => Fetch::Conflict,
        };
    }
    let Some(meta) = meta else {
        return if cached.is_some() { Fetch::Nothing } else { Fetch::Whole };
    };
    if cached.is_some_and(|cached| cached.sha == remote.sha) {
        return Fetch::Nothing;
    }
    let same = fs::read(path).map(|bytes| sha_hex(&bytes) == remote.sha).unwrap_or(false);
    if same {
        return Fetch::Same(Entry { log: false, size: meta.len(), mtime: mtime(&meta), sha: remote.sha.clone(), ..Entry::default() });
    }
    match cached {
        Some(cached) if meta.len() == cached.size && mtime(&meta) == cached.mtime => Fetch::Whole,
        Some(_) => Fetch::Conflict,
        None => Fetch::Nothing,
    }
}

/// Connection for receiving: a transcript can be hundreds of megabytes, a global timeout won't do.
fn download_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(120)))
        .timeout_per_read(Some(Duration::from_secs(120)))
        .http_status_as_error(false)
        .user_agent(concat!("pult/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Receive one file. Returns the new cache entry and how many raw bytes arrived; `None` — the local
/// beginning of the transcript didn't match the server's (diverged), the file is left alone.
fn fetch(agent: &ureq::Agent, config: &Config, remote: &Remote, path: &Path, how: &Fetch) -> Result<Option<(Entry, u64)>, String> {
    let auth = format!("Bearer {}", config.token);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| tr!("sync.couldn_t_create_parent_error", parent = parent.display(), error = error))?;
    }
    if remote.kind != "log" {
        let mut response = agent
            .get(format!("{}/blob", config.api))
            .header("Authorization", &auth)
            .query("place", &remote.place)
            .query("file", &remote.file)
            .call()
            .map_err(network)?;
        let status = response.status().as_u16();
        if status != 200 {
            let text = response.body_mut().read_to_string().unwrap_or_default();
            return Err(refusal(status, &text));
        }
        let mut packed = Vec::new();
        response.body_mut().as_reader().read_to_end(&mut packed).map_err(|error| error.to_string())?;
        let bytes = zstd::stream::decode_all(packed.as_slice()).map_err(|error| error.to_string())?;
        if sha_hex(&bytes) != remote.sha {
            return Err(tr!("sync.file_what_arrived_doesn_t", file = remote.file));
        }
        write_file(path, &bytes, false)?;
        let meta = fs::metadata(path).map_err(|error| error.to_string())?;
        let entry = Entry { log: false, size: meta.len(), mtime: mtime(&meta), sha: remote.sha.clone(), ..Entry::default() };
        return Ok(Some((entry, bytes.len() as u64)));
    }
    let from = match how {
        Fetch::Append(from) => *from,
        _ => 0,
    };
    // Check: the last bytes of the local file must match the same bytes on the server —
    // so the request starts a little before the local end.
    let check = from.min(TAIL);
    let mine = if check > 0 { read_range(path, from - check, check)? } else { Vec::new() };
    let mut response = agent
        .get(format!("{}/log", config.api))
        .header("Authorization", &auth)
        .query("place", &remote.place)
        .query("file", &remote.file)
        .query("from", (from - check).to_string())
        .call()
        .map_err(network)?;
    let status = response.status().as_u16();
    let header = |name: &str| response.headers().get(name).and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<u64>().ok());
    let (server_len, server_gen) = (header("X-Pult-Len").unwrap_or(remote.len), header("X-Pult-Gen").unwrap_or(remote.r#gen));
    if status == 204 {
        // The server copy is not longer than the local one: nothing to append.
        return Ok(None);
    }
    if status != 200 {
        let text = response.body_mut().read_to_string().unwrap_or_default();
        return Err(refusal(status, &text));
    }
    let mut decoder = zstd::stream::read::Decoder::new(response.body_mut().as_reader()).map_err(|error| error.to_string())?;
    if check > 0 {
        let mut theirs = vec![0; check as usize];
        if decoder.read_exact(&mut theirs).is_err() || theirs != mine {
            return Ok(None);
        }
    }
    let received = if from > 0 {
        // Append only if the file here is still the same length.
        let mut file = fs::OpenOptions::new().append(true).open(path).map_err(|error| error.to_string())?;
        if file.metadata().map(|meta| meta.len()).unwrap_or_default() != from {
            return Err(String::new());
        }
        std::io::copy(&mut decoder, &mut file).map_err(|error| tr!("sync.couldn_t_append_to_path", path = path.display(), error = error))?
    } else {
        let mut temp = path.as_os_str().to_owned();
        temp.push(".tmp");
        let temp = PathBuf::from(temp);
        let mut file = fs::File::create(&temp).map_err(|error| error.to_string())?;
        let written = std::io::copy(&mut decoder, &mut file).map_err(|error| tr!("sync.couldn_t_write_path_error", path = path.display(), error = error))?;
        drop(file);
        fs::rename(&temp, path).map_err(|error| error.to_string())?;
        written
    };
    let len = from + received;
    if len != server_len {
        return Err(tr!("sync.file_received_len_of_server", file = remote.file, len = len, server_len = server_len));
    }
    let tail = tail_hash(path, len).unwrap_or_default();
    Ok(Some((Entry { log: true, len, r#gen: server_gen, tail, ..Entry::default() }, received)))
}

/// Receive result.
#[derive(Debug, Default)]
pub struct Pulled {
    pub files: u64,
    pub bytes: u64,
    pub conflicts: u64,
}

/// Fetch from the server everything that's missing here or that grew there.
pub fn pull(paths: &Paths) -> Result<Pulled, String> {
    let Some(config) = config(paths) else {
        return Err(t("sync.sync_isn_t_connected").into());
    };
    // One receive at a time: Pult and `pult sync pull` can come in at once.
    fs::create_dir_all(&paths.app).map_err(|error| error.to_string())?;
    let guard = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.app.join("pull.lock"))
        .map_err(|error| error.to_string())?;
    if guard.try_lock().is_err() {
        return Ok(Pulled::default());
    }
    let result = pull_with(paths, &config);
    let mut status = status(paths);
    match &result {
        Ok(pulled) => {
            status.pulled_at = now();
            status.received_bytes += pulled.bytes;
            status.conflicts = pulled.conflicts;
        }
        Err(error) => {
            status.error = Some(error.clone());
            status.error_at = now();
            log_line(&paths.log(), &format!("pull: {error}"));
        }
    }
    let _ = write_file(&paths.status(), serde_json::to_string(&status).unwrap_or_default().as_bytes(), false);
    result
}

fn pull_with(paths: &Paths, config: &Config) -> Result<Pulled, String> {
    let store = Store { paths, api: &config.api };
    let state = store.load();
    let full = now().saturating_sub(state.pulled_full_at) > 3600 || state.receives < RECEIVES;
    let since = if full { 0 } else { state.pulled_at.saturating_sub(120) };
    let (status, text) = Client::new(config).get(&format!("/files?since={since}"))?;
    if status != 200 {
        return Err(refusal(status, &text));
    }
    #[derive(Deserialize)]
    struct Listing {
        files: Vec<Remote>,
        now: u64,
    }
    let listing: Listing = serde_json::from_str(&text).map_err(|error| tr!("sync.couldn_t_read_server_s", error = error))?;

    let mut pulled = Pulled::default();
    let mut same = Vec::new();
    let mut jobs = Vec::new();
    for remote in listing.files {
        let Some(path) = local_path(paths, &remote.place, &remote.file) else { continue };
        match decide(&remote, &path, state.files.get(&remote.key())) {
            Fetch::Same(entry) => same.push((remote.key(), entry)),
            Fetch::Conflict => {
                pulled.conflicts += 1;
                log_line(&paths.log(), &format!("pull: diverged, left alone: {} {}", remote.place, remote.file));
            }
            Fetch::Nothing => {}
            how => jobs.push((remote, path, how)),
        }
    }
    // Small ones first: the session list fills quickly, and a big transcript doesn't hold up the rest.
    jobs.sort_by_key(|(remote, _, how)| match how {
        Fetch::Append(from) => remote.len.saturating_sub(*from),
        _ => remote.len,
    });

    let queue = Mutex::new(VecDeque::from(jobs));
    let (report, results) = mpsc::channel::<(String, Result<Option<(Entry, u64)>, String>)>();
    let mut done: Vec<(String, Entry)> = same;
    let mut first_error = None;
    std::thread::scope(|threads| {
        for _ in 0..STREAMS {
            let (queue, report) = (&queue, report.clone());
            threads.spawn(move || {
                let agent = download_agent();
                loop {
                    // The queue lock is released before the work, otherwise threads would go one at a time.
                    let next = queue.lock().ok().and_then(|mut queue| queue.pop_front());
                    let Some((remote, path, how)) = next else { break };
                    let result = fetch(&agent, config, &remote, &path, &how);
                    if report.send((remote.key(), result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(report);
        let mut flushed = Instant::now();
        for (key, result) in results {
            match result {
                Ok(Some((entry, bytes))) => {
                    pulled.files += 1;
                    pulled.bytes += bytes;
                    done.push((key, entry));
                }
                Ok(None) => {
                    pulled.conflicts += 1;
                    log_line(&paths.log(), &format!("pull: diverged, left alone: {}", key.replace('\n', " ")));
                }
                Err(error) if error.is_empty() => {}
                Err(error) => {
                    log_line(&paths.log(), &format!("pull: {error}"));
                    first_error.get_or_insert(error);
                }
            }
            if flushed.elapsed() > Duration::from_secs(2) && !done.is_empty() {
                let batch = std::mem::take(&mut done);
                let _ = store.locked(|state| state.files.extend(batch));
                flushed = Instant::now();
            }
        }
    });
    store.locked(|state| {
        state.files.extend(done);
        if first_error.is_none() {
            state.pulled_at = listing.now;
            if full {
                state.pulled_full_at = listing.now;
                state.receives = RECEIVES;
            }
        }
    })?;
    match first_error {
        Some(error) => Err(error),
        None => Ok(pulled),
    }
}

/// Every five minutes Pult sends what the hooks missed (and the whole history on first connect),
/// and fetches from the server what came from other computers.
pub fn start_periodic() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        // Everything — every 5 minutes; the project snapshot and receiving — every minute: a project added or
        // removed on one computer shows up on the others quickly.
        let mut round = 0u32;
        loop {
            let paths = Paths::real();
            if config(&paths).is_some() {
                let scope = if round.is_multiple_of(5) { Scope::All } else { Scope::Projects };
                let _ = push(&paths, &scope);
                let _ = pull(&paths);
            }
            round = round.wrapping_add(1);
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}

/// The project list or a pinned session changed — send the snapshot right away, in the background.
pub fn share_projects_now() {
    std::thread::spawn(|| {
        let paths = Paths::real();
        if config(&paths).is_some() {
            let _ = push(&paths, &Scope::Projects);
        }
    });
}

// ───────────────────────────── commands ─────────────────────────────

/// Size for people: "412 MB", "1.02 GB" (decimal comma and Russian units in Russian).
pub fn size_label(bytes: u64) -> String {
    let (value, decimals, unit) = match bytes {
        0..1024 => (bytes as f64, 0, t("sync.b")),
        1024..1_048_576 => (bytes as f64 / 1024., 0, t("sync.kb")),
        1_048_576..1_073_741_824 => (bytes as f64 / 1_048_576., 0, t("sync.mb")),
        _ => (bytes as f64 / 1_073_741_824., 2, t("sync.gb")),
    };
    let number = format!("{value:.decimals$}");
    let number = if crate::i18n::is_ru() { number.replace('.', ",") } else { number };
    format!("{number} {unit}")
}

fn connect_command(paths: &Paths, args: &[String]) -> Result<String, String> {
    let address = args
        .first()
        .ok_or(t("sync.specify_server_address_pult_sync"))?;
    let option = |name: &str| args.iter().position(|arg| arg == name).and_then(|index| args.get(index + 1)).cloned();
    let name = option("--name").unwrap_or_else(computer_name);
    let (found, api) = discover(address)?;
    let config = match option("--token") {
        Some(token) => connect_with_token(paths, address, &api, &name, &token)?,
        None => {
            let pairing = pair_start(&api, &name)?;
            let hint = if pairing.via == "telegram" {
                t("sync.code_was_sent_to_you")
            } else {
                t("sync.code_is_in_server_log")
            };
            eprint!("{}", tr!("sync.server_name_hint_code", name = found.name, hint = hint));
            let mut code = String::new();
            std::io::stdin().read_line(&mut code).map_err(|error| error.to_string())?;
            pair_finish(paths, address, &api, &name, &pairing.id, code.trim())?
        }
    };
    let exe = hook_exe();
    install_hooks(paths, &exe)?;
    Ok(tr!("sync.connected_to_server_name_as", server_name = config.server_name, device_name = config.device_name, exe = exe.display()))
}

fn status_command(paths: &Paths) -> Result<String, String> {
    let Some(config) = config(paths) else {
        return Ok(t("sync.sync_isn_t_connected").into());
    };
    let local = status(paths);
    let mut lines = vec![tr!("sync.server_server_name_address_this", server_name = config.server_name, address = config.address, device_name = config.device_name)];
    match local.ok_at {
        0 => lines.push(t("sync.nothing_sent_yet").into()),
        at => lines.push(tr!("sync.last_sent_at_s_ago", at = now().saturating_sub(at), sent_bytes = size_label(local.sent_bytes))),
    }
    if local.pending > 0 {
        lines.push(tr!("sync.sending_pending_files_left", pending = local.pending));
    }
    if local.skipped > 0 {
        lines.push(tr!("sync.not_sent_larger_than_size", size_label = size_label(MAX_BLOB), skipped = local.skipped));
    }
    if let Some(error) = &local.error {
        lines.push(tr!("sync.error_error", error = error));
    }
    match server_status(paths) {
        Ok(server) => {
            let number = |key: &str| server.get(key).and_then(Value::as_u64).unwrap_or_default();
            lines.push(tr!("sync.on_server_streams_files_raw", streams = number("streams"), raw_bytes = size_label(number("raw_bytes")), stored_bytes = size_label(number("stored_bytes")), free_bytes = size_label(number("free_bytes"))));
        }
        Err(error) => lines.push(error),
    }
    Ok(lines.join("\n"))
}

/// `pult sync …` — without a window. Returns the exit code.
pub fn cli(args: &[String]) -> i32 {
    let paths = Paths::real();
    let rest = args.get(1..).unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        Some("hook") => {
            hook(&paths);
            return 0;
        }
        Some("push") => {
            let value = |flag: &str| rest.iter().position(|arg| arg == flag).and_then(|index| rest.get(index + 1));
            let scope = match value("--transcript") {
                Some(path) => {
                    let transcript = PathBuf::from(path);
                    // The session's code first: it goes out with its conversation.
                    if let Some(session) = transcript.file_stem().map(|stem| stem.to_string_lossy().into_owned()) {
                        let cwd = value("--cwd").filter(|cwd| !cwd.is_empty()).map(PathBuf::from);
                        crate::code::snapshot(&paths, &session, Some(&transcript), cwd.as_deref(), false);
                    }
                    Scope::Transcript(transcript)
                }
                None => Scope::All,
            };
            push(&paths, &scope).map(|sent| tr!("sync.sent_sent", sent = size_label(sent)))
        }
        Some("pull") => pull(&paths).map(|pulled| {
            let mut text = tr!("sync.files_received_files_bytes", files = pulled.files, bytes = size_label(pulled.bytes));
            if pulled.conflicts > 0 {
                text.push_str(&tr!("sync.diverged_and_left_alone_conflicts", conflicts = pulled.conflicts));
            }
            text
        }),
        Some("connect") => connect_command(&paths, rest),
        Some("disconnect") => {
            disconnect(&paths).map(|()| t("sync.disconnected_data_on_server_is").to_string())
        }
        Some("status") => status_command(&paths),
        Some("install-hooks") => install_hooks(&paths, &hook_exe()).map(|changed| {
            if changed { t("sync.hooks_installed") } else { t("sync.hooks_are_already_installed") }.to_string()
        }),
        Some("uninstall-hooks") => uninstall_hooks(&paths).map(|changed| {
            if changed { t("sync.hooks_removed") } else { t("sync.there_were_no_hooks") }.to_string()
        }),
        _ => Err(t("sync.pult_sync_connect_address_name")
        .into()),
    };
    match result {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_drop_the_machine_specific_start() {
        let (root, home) = (Path::new("/home/user/CODE"), Path::new("/home/user"));
        assert_eq!(place_of("-home-user-CODE", None, root, home), "root:");
        assert_eq!(place_of("-home-user-CODE-EMBER", None, root, home), "root:EMBER");
        assert_eq!(place_of("-home-user-Documents-Craft", None, root, home), "home:Documents-Craft");
        assert_eq!(place_of("-private-tmp-x", None, root, home), "abs:-private-tmp-x");
        // Records from before the main folder's case change — the same place.
        assert_eq!(place_of("-home-user-Code-MAPLE", None, root, home), "root:MAPLE");
        // A folder next to the main one with a similar name is not inside it: the transcript path decides.
        let cwd = Path::new("/home/user/CODE-OLD/x");
        assert_eq!(place_of("-home-user-CODE-OLD-x", Some(cwd), root, home), "home:CODE-OLD-x");
        // Transcript started before the main folder's case change — same project.
        let cwd = Path::new("/home/user/Code/MAPLE/app");
        assert_eq!(place_of("-home-user-CODE-MAPLE-app", Some(cwd), root, home), "root:MAPLE-app");
        assert_eq!(strip_base(cwd, root), Some(PathBuf::from("MAPLE/app")));
        let cwd = Path::new("/home/user/CODE/EMBER/.claude/worktrees/TASK-93");
        assert_eq!(place_of(&encode(cwd), Some(cwd), root, home), "root:EMBER--claude-worktrees-TASK-93");
    }

    /// On another computer the main folder lives elsewhere — same place, its own data folder name.
    #[test]
    fn places_map_to_another_machine() {
        let (root, home) = (Path::new("/home/user/CODE"), Path::new("/home/user"));
        let (other_root, other_home) = (Path::new("/Volumes/Work/code"), Path::new("/home/user"));
        for name in ["-home-user-CODE", "-home-user-CODE-EMBER-api", "-home-user-Documents-x", "-private-tmp-x"] {
            let place = place_of(name, None, root, home);
            assert_eq!(dir_name_for(&place, root, home).as_deref(), Some(name));
            let there = dir_name_for(&place, other_root, other_home).unwrap();
            assert_eq!(place_of(&there, None, other_root, other_home), place);
        }
        assert_eq!(dir_name_for("root:EMBER-api", other_root, other_home).as_deref(), Some("-Volumes-Work-code-EMBER-api"));
        assert_eq!(dir_name_for("history:", root, home), None);
    }

    #[test]
    fn hooks_merge_keeps_foreign_hooks() {
        let mut settings = json!({
            "model": "opus",
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/x/cc-status"}]}]}
        });
        assert!(merge_hooks(&mut settings, "'/Applications/Pult.app/Contents/MacOS/pult' sync hook"));
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(settings["hooks"]["Stop"][0]["hooks"][0]["command"], "/x/cc-status");
        assert_eq!(settings["hooks"]["PreCompact"].as_array().unwrap().len(), 1);
        // A repeat changes nothing; a different path to Pult replaces our entry instead of adding a second one.
        assert!(!merge_hooks(&mut settings, "'/Applications/Pult.app/Contents/MacOS/pult' sync hook"));
        assert!(merge_hooks(&mut settings, "'/dev/pult' sync hook"));
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert!(remove_hooks(&mut settings));
        assert_eq!(settings, json!({
            "model": "opus",
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/x/cc-status"}]}]}
        }));
    }

    #[test]
    fn address_becomes_origin() {
        assert_eq!(origin("agent.example.com"), "https://agent.example.com");
        assert_eq!(origin(" https://agent.example.com/ "), "https://agent.example.com");
        assert_eq!(origin("http://127.0.0.1:8647/.well-known/pult-sync"), "http://127.0.0.1:8647");
    }

    #[test]
    fn transcripts_are_kept() {
        let mut settings = json!({"model": "x"});
        assert!(keep_transcripts_in(&mut settings));
        assert_eq!(settings["cleanupPeriodDays"], KEEP_DAYS);
        assert!(!keep_transcripts_in(&mut settings), "a second run changes nothing");
        let mut short = json!({"cleanupPeriodDays": 3650});
        assert!(keep_transcripts_in(&mut short));
        assert_eq!(short["cleanupPeriodDays"], KEEP_DAYS);
        let mut longer = json!({"cleanupPeriodDays": 99_999});
        assert!(!keep_transcripts_in(&mut longer), "a longer user value is kept");
    }

    #[test]
    fn local_paths_follow_this_computers_main_folder() {
        let paths = Paths { claude: "/u/.claude".into(), app: "/a".into(), root: "/u/Code".into(), home: "/u".into() };
        assert_eq!(local_path(&paths, "root:EMBER", "x.jsonl"), Some("/u/.claude/projects/-u-Code-EMBER/x.jsonl".into()));
        assert_eq!(local_path(&paths, "home:Documents", "memory/M.md"), Some("/u/.claude/projects/-u-Documents/memory/M.md".into()));
        assert_eq!(local_path(&paths, "history:", "s/abc@v1"), Some("/u/.claude/file-history/s/abc@v1".into()));
        assert_eq!(local_path(&paths, "root:X", "../escape"), None);
        assert_eq!(local_path(&paths, "root:X", "/etc/passwd"), None);
        assert_eq!(local_path(&paths, "abs:../x", "a"), None);
    }

    fn temp_paths(tag: &str) -> Paths {
        let base = std::env::temp_dir().join(format!("pult-sync-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let paths = Paths { claude: base.join("home/.claude"), app: base.join("app"), root: base.join("home/CODE"), home: base.join("home") };
        fs::create_dir_all(&paths.app).unwrap();
        paths
    }

    fn download(config: &Config, place: &str, file: &str, log: bool) -> Vec<u8> {
        let client = Client::new(config);
        let path = if log { "/log" } else { "/blob" };
        let mut response = client
            .agent
            .get(format!("{}{path}", client.api))
            .query("place", place)
            .query("file", file)
            .header("Authorization", client.auth())
            .call()
            .unwrap();
        assert_eq!(response.status().as_u16(), 200, "{place} {file}");
        let body = response.body_mut().with_config().limit(1 << 30).read_to_vec().unwrap();
        zstd::stream::decode_all(&body[..]).unwrap()
    }

    /// The whole path on a real server: `PULT_SYNCD=<path to pult-syncd> cargo test live_sync -- --ignored`.
    #[test]
    #[ignore]
    fn live_sync() {
        let server = std::env::var("PULT_SYNCD").expect("PULT_SYNCD");
        let paths = temp_paths("live");
        let data = paths.app.join("server-data");
        let port = 18647 + (std::process::id() % 1000) as u16;
        let address = format!("http://127.0.0.1:{port}");
        let run = |args: &[&str]| std::process::Command::new(&server).args(args).env("DATA_DIR", &data).env("LISTEN", format!("127.0.0.1:{port}")).env("MIN_FREE_MB", "1").output().unwrap();
        let token = String::from_utf8(run(&["pair", "Test Mac"]).stdout).unwrap().lines().find_map(|line| line.strip_prefix("token ").map(str::to_string)).unwrap();
        let mut child = std::process::Command::new(&server).env("DATA_DIR", &data).env("LISTEN", format!("127.0.0.1:{port}")).env("MIN_FREE_MB", "1").env("SERVER_NAME", "test").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(600));
        let result = std::panic::catch_unwind(|| {
            let (found, api) = discover(&address).unwrap();
            assert_eq!(found.name, "test");
            let config = connect_with_token(&paths, &address, &api, "x", &token).unwrap();
            assert_eq!(config.device_name, "Test Mac");

            let session = "11111111-2222-3333-4444-555555555555";
            let dir = paths.claude.join("projects").join(encode(&paths.root.join("DEMO")));
            fs::create_dir_all(dir.join(session).join("subagents")).unwrap();
            fs::create_dir_all(dir.join("memory")).unwrap();
            fs::create_dir_all(paths.claude.join("file-history").join(session)).unwrap();
            let transcript = dir.join(format!("{session}.jsonl"));
            let line = |n: usize| format!("{{\"type\":\"user\",\"cwd\":\"{}\",\"n\":{n},\"pad\":\"{}\"}}\n", paths.root.join("DEMO").display(), "é".repeat(n % 97));
            let mut text: String = (0..60_000).map(line).collect();
            fs::write(&transcript, &text).unwrap();
            fs::write(dir.join(session).join("subagents/agent-a.jsonl"), "{\"a\":1}\n").unwrap();
            fs::write(dir.join("memory/MEMORY.md"), "- note é\n").unwrap();
            fs::write(paths.claude.join("file-history").join(session).join("abc@v1"), b"\x00\x01binary").unwrap();
            assert!(text.len() as u64 > CHUNK, "the transcript must go in several chunks");

            let sent = push(&paths, &Scope::All).unwrap();
            assert!(sent >= text.len() as u64);
            assert_eq!(download(&config, "root:DEMO", &format!("{session}.jsonl"), true), text.as_bytes());
            assert_eq!(download(&config, "root:DEMO", "memory/MEMORY.md", false), "- note é\n".as_bytes());
            assert_eq!(download(&config, "history:", &format!("{session}/abc@v1"), false), b"\x00\x01binary");
            assert_eq!(push(&paths, &Scope::All).unwrap(), 0, "a repeat sends nothing");

            // A turn was appended — only the tail goes out.
            let more = line(60_001);
            text.push_str(&more);
            fs::OpenOptions::new().append(true).open(&transcript).unwrap().write_all(more.as_bytes()).unwrap();
            assert_eq!(push(&paths, &Scope::Transcript(transcript.clone())).unwrap(), more.len() as u64);
            assert_eq!(download(&config, "root:DEMO", &format!("{session}.jsonl"), true), text.as_bytes());

            // File rewritten — a new generation; the server holds exactly the new file.
            let rewritten: String = (0..50).map(line).collect();
            fs::write(&transcript, &rewritten).unwrap();
            assert_eq!(push(&paths, &Scope::All).unwrap(), rewritten.len() as u64);
            assert_eq!(download(&config, "root:DEMO", &format!("{session}.jsonl"), true), rewritten.as_bytes());

            // Cache lost (fresh install): the server remembers, nothing is sent again.
            fs::remove_file(paths.state()).unwrap();
            assert_eq!(push(&paths, &Scope::All).unwrap(), 0);

            // Without a cache a mismatch doesn't go out as a new generation: this may be an old copy.
            let stale: String = (0..10).map(line).collect();
            fs::write(&transcript, &stale).unwrap();
            assert_eq!(push(&paths, &Scope::All).unwrap(), 0);
            assert_eq!(download(&config, "root:DEMO", &format!("{session}.jsonl"), true), rewritten.as_bytes());
            fs::write(&transcript, &rewritten).unwrap();

            // Memory changed — the file is replaced.
            fs::write(dir.join("memory/MEMORY.md"), "- note é\n- more\n").unwrap();
            push(&paths, &Scope::Transcript(transcript.clone())).unwrap();
            assert_eq!(download(&config, "root:DEMO", "memory/MEMORY.md", false), "- note é\n- more\n".as_bytes());

            let server = server_status(&paths).unwrap();
            assert_eq!(server["device_name"], "Test Mac");
            let (status, text) = Client::new(&config).get("/sessions").unwrap();
            assert_eq!(status, 200);
            assert!(text.contains(session) && text.contains("\"rel\":\"DEMO\""), "{text}");

            // Second computer: the main folder is elsewhere and named differently — it fetches everything to itself.
            let other = Paths {
                claude: paths.app.parent().unwrap().join("other/.claude"),
                app: paths.app.parent().unwrap().join("other-app"),
                root: paths.app.parent().unwrap().join("other/Code"),
                home: paths.app.parent().unwrap().join("other"),
            };
            fs::create_dir_all(&other.app).unwrap();
            let token2 = String::from_utf8(run(&["pair", "Second Mac"]).stdout).unwrap().lines().find_map(|line| line.strip_prefix("token ").map(str::to_string)).unwrap();
            connect_with_token(&other, &address, &api, "y", &token2).unwrap();
            let there = other.claude.join("projects").join(encode(&other.root.join("DEMO")));
            let pulled = pull(&other).unwrap();
            assert!(pulled.files >= 4, "{pulled:?}");
            assert_eq!(pulled.conflicts, 0);
            assert_eq!(fs::read(there.join(format!("{session}.jsonl"))).unwrap(), rewritten.as_bytes());
            assert_eq!(fs::read_to_string(there.join("memory/MEMORY.md")).unwrap(), "- note é\n- more\n");
            assert_eq!(fs::read_to_string(there.join(session).join("subagents/agent-a.jsonl")).unwrap(), "{\"a\":1}\n");
            assert_eq!(fs::read(other.claude.join("file-history").join(session).join("abc@v1")).unwrap(), b"\x00\x01binary");
            assert_eq!(pull(&other).unwrap().files, 0, "a repeat fetches nothing");
            assert_eq!(push(&other, &Scope::All).unwrap(), 0, "what was received is not sent back");

            // A turn appended on the first — the second appends only the tail.
            let tail_line = line(70_000);
            fs::OpenOptions::new().append(true).open(&transcript).unwrap().write_all(tail_line.as_bytes()).unwrap();
            push(&paths, &Scope::Transcript(transcript.clone())).unwrap();
            let pulled = pull(&other).unwrap();
            assert_eq!((pulled.files, pulled.bytes), (1, tail_line.len() as u64));
            assert_eq!(fs::read(there.join(format!("{session}.jsonl"))).unwrap(), format!("{rewritten}{tail_line}").as_bytes());

            // Session continued on the second — the first receives its turns.
            let theirs = there.join(format!("{session}.jsonl"));
            let from_b = line(80_000);
            fs::OpenOptions::new().append(true).open(&theirs).unwrap().write_all(from_b.as_bytes()).unwrap();
            assert_eq!(push(&other, &Scope::Transcript(theirs.clone())).unwrap(), from_b.len() as u64);
            assert_eq!(pull(&paths).unwrap().bytes, from_b.len() as u64);
            assert_eq!(fs::read(&transcript).unwrap(), fs::read(&theirs).unwrap());
            let full = fs::read(&transcript).unwrap();

            // Second computer reinstalled over old files (a machine from an old snapshot):
            // the old copy doesn't push out the fresh one on the server, it is extended by receiving.
            fs::write(&theirs, &full[..full.len() / 3]).unwrap();
            fs::remove_file(other.state()).unwrap();
            assert_eq!(push(&other, &Scope::All).unwrap(), 0);
            assert_eq!(download(&config, "root:DEMO", &format!("{session}.jsonl"), true), full);
            pull(&other).unwrap();
            assert_eq!(fs::read(&theirs).unwrap(), full);

            // Appended differently there and here — the second leaves its transcript alone.
            fs::OpenOptions::new().append(true).open(there.join(format!("{session}.jsonl"))).unwrap().write_all(line(1).as_bytes()).unwrap();
            fs::OpenOptions::new().append(true).open(&transcript).unwrap().write_all(line(2).as_bytes()).unwrap();
            push(&paths, &Scope::Transcript(transcript.clone())).unwrap();
            let before = fs::read(there.join(format!("{session}.jsonl"))).unwrap();
            assert_eq!(pull(&other).unwrap().conflicts, 1);
            assert_eq!(fs::read(there.join(format!("{session}.jsonl"))).unwrap(), before);

            // What was deleted here doesn't come back.
            fs::remove_file(there.join("memory/MEMORY.md")).unwrap();
            pull(&other).unwrap();
            assert!(!there.join("memory/MEMORY.md").exists());

            // A foreign key is rejected.
            let mut stranger = config.clone();
            stranger.token = "0".repeat(64);
            assert_eq!(Client::new(&stranger).get("/status").unwrap().0, 401);
        });
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(paths.app.parent().unwrap());
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
