//! Code travels with a session.
//!
//! A session's conversation already goes to the sync server; this module adds the code it works on, so the session
//! continues on another computer from the same state instead of "Your worktree … no longer exists".
//!
//! Sending (after every answer and when the session is handed over): where the session works — its git worktree
//! (`.claude/worktrees/<name>`) or the project's own checkout — is read from the conversation. A snapshot commit is
//! made from the working state (every change not committed, new files included, `.gitignore`d ones not) without
//! touching the index or the files, and goes into a git bundle together with the commits the remotes don't have.
//! The description (`code/<session>.json`) and the bundle (`code/<session>.bundle`) are Pult's files on the sync
//! server.
//!
//! Receiving (before a session that came from another computer starts here): the same worktree is created on the
//! same branch from the same commit and the uncommitted changes are laid on top; the conversation gets a
//! `worktree-state` line with this computer's paths, so Claude Code continues inside it. Nothing of the person's is
//! overwritten: changes made here that Pult doesn't know stop it, and the incoming code is kept in a branch.

use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::sync::{self, Paths};

/// What happened to a session's code.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The snapshot is on the server.
    #[default]
    Ok,
    /// The folder isn't a git repository.
    Nogit,
    /// The changes are larger than the sync server takes.
    Big,
    /// Carrying code is turned off on the sending computer.
    Off,
}

/// The description of a session's code snapshot (`code/<session>.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub session: String,
    /// Computer that made the snapshot.
    pub device: String,
    pub device_name: String,
    pub at: u64,
    pub status: Status,
    /// The main repository, relative to the project folder ("" — the folder itself).
    #[serde(default)]
    pub repo: String,
    /// Where the session works, relative to the project folder: the repository or its worktree.
    #[serde(default)]
    pub top: String,
    #[serde(default)]
    pub worktree: bool,
    #[serde(default)]
    pub worktree_name: String,
    /// "" — a detached HEAD.
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub head: String,
    /// Commit holding the uncommitted changes on top of `head` ("" — there are none).
    #[serde(default)]
    pub snapshot: String,
    /// Tree of the working state.
    #[serde(default)]
    pub tree: String,
    #[serde(default)]
    pub files: u32,
    /// `origin` without user and password: a computer without the repository clones it from here.
    #[serde(default)]
    pub remote: String,
    /// sha256 of the bundle ("" — everything is in the remotes, no bundle).
    #[serde(default)]
    pub bundle: String,
    /// Size of a bundle too large to send.
    #[serde(default)]
    pub size: u64,
}

impl Manifest {
    /// The state this snapshot describes.
    fn mark(&self) -> Mark {
        Mark { head: self.head.clone(), tree: self.tree.clone() }
    }
}

/// What a session's code is on this computer: the head and the tree of the working state that this computer last
/// sent or laid down. A working copy still in that state can be replaced by newer code safely.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Mark {
    head: String,
    tree: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Local {
    /// Carrying code on this computer (off — the snapshot says only that).
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    marks: BTreeMap<String, Mark>,
    /// Snapshots the person chose to continue without (session → `head tree`).
    #[serde(default)]
    skipped: BTreeMap<String, String>,
    /// Snapshots laid down here (session → `head tree`).
    #[serde(default)]
    laid: BTreeMap<String, String>,
}

fn yes() -> bool {
    true
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|time| time.as_secs()).unwrap_or_default()
}

pub fn dir(paths: &Paths) -> PathBuf {
    paths.app.join("code")
}

fn manifest_path(paths: &Paths, session: &str) -> PathBuf {
    dir(paths).join(format!("{session}.json"))
}

fn bundle_path(paths: &Paths, session: &str) -> PathBuf {
    dir(paths).join(format!("{session}.bundle"))
}

fn local_file(paths: &Paths) -> PathBuf {
    paths.app.join("code.json")
}

/// A file name in `code/` that may come from the server.
pub fn is_code_file(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    matches!(ext, "json" | "bundle")
        && stem.len() == 36
        && stem.chars().all(|char| char.is_ascii_hexdigit() || char == '-')
}

fn load_local(paths: &Paths) -> Local {
    fs::read_to_string(local_file(paths))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Local { enabled: true, ..Local::default() })
}

/// Change `code.json` under a lock: hooks of several sessions write it at once.
fn with_local<T>(paths: &Paths, work: impl FnOnce(&mut Local) -> T) -> T {
    let _ = fs::create_dir_all(&paths.app);
    let lock = fs::OpenOptions::new().create(true).append(true).open(paths.app.join("code.lock"));
    let _guard = lock.as_ref().ok().map(|file| file.lock());
    let mut local = load_local(paths);
    let result = work(&mut local);
    if let Ok(text) = serde_json::to_string_pretty(&local) {
        let _ = sync::write_file(&local_file(paths), text.as_bytes(), false);
    }
    result
}

pub fn enabled() -> bool {
    load_local(&Paths::real()).enabled
}

pub fn set_enabled(on: bool) {
    with_local(&Paths::real(), |local| local.enabled = on);
}

pub fn manifest(paths: &Paths, session: &str) -> Option<Manifest> {
    serde_json::from_str(&fs::read_to_string(manifest_path(paths, session)).ok()?).ok()
}

// ───────────────────────────── git ─────────────────────────────

fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o ConnectTimeout=20");
    }
    command
}

fn run(mut command: Command) -> Result<String, String> {
    let output = command.output().map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    run(git_command(dir, args))
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git(dir, args).is_ok()
}

/// The working state as a tree: everything `git add -A` would take, through a copy of the index, so the person's
/// index and files stay as they are.
fn working_tree(top: &Path, scratch: &Path) -> Result<String, String> {
    let index = git(top, &["rev-parse", "--path-format=absolute", "--git-path", "index"])?;
    let _ = fs::remove_file(scratch);
    if Path::new(&index).is_file() {
        fs::copy(&index, scratch).map_err(|error| error.to_string())?;
    }
    let with_index = |args: &[&str]| {
        let mut command = git_command(top, args);
        command.env("GIT_INDEX_FILE", scratch);
        run(command)
    };
    let result = with_index(&["add", "-A", "--", "."]).and_then(|_| with_index(&["write-tree"]));
    let _ = fs::remove_file(scratch);
    result
}

/// Commit holding the working state on top of `head`, with a fixed author: works without the person's git identity.
fn commit_tree(top: &Path, tree: &str, head: &str) -> Result<String, String> {
    let mut command = git_command(top, &["commit-tree", tree, "-p", head, "-m", "Pult: uncommitted changes"]);
    for (key, value) in [
        ("GIT_AUTHOR_NAME", "Pult"),
        ("GIT_AUTHOR_EMAIL", "pult@localhost"),
        ("GIT_COMMITTER_NAME", "Pult"),
        ("GIT_COMMITTER_EMAIL", "pult@localhost"),
    ] {
        command.env(key, value);
    }
    run(command)
}

/// `origin` without the user and password some people keep in the address.
fn clean_remote(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((host, path)) => {
                let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
                format!("{scheme}://{host}/{path}")
            }
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

/// Addresses to try for a repository: as recorded, and over SSH for an HTTPS one (`https://host/a/b.git` →
/// `git@host:a/b.git`) — a computer often has an SSH key for GitHub but no HTTPS password.
fn remote_addresses(remote: &str) -> Vec<String> {
    let mut addresses = vec![remote.to_string()];
    if let Some(rest) = remote.strip_prefix("https://")
        && let Some((host, path)) = rest.split_once('/')
        && !path.is_empty()
    {
        addresses.push(format!("git@{host}:{path}"));
    }
    addresses
}

fn sha256_file(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 << 10];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The path with symbolic links resolved, as git reports it (`/var` → `/private/var`); as is when it doesn't exist.
fn real(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `path` relative to `base`, comparing without regard to case (macOS disks): `None` — not inside.
fn relative(path: &Path, base: &Path) -> Option<String> {
    let path = path.to_string_lossy().to_string();
    let base = base.to_string_lossy().trim_end_matches('/').to_string();
    if path.eq_ignore_ascii_case(&base) || path.to_lowercase() == base.to_lowercase() {
        return Some(String::new());
    }
    let prefix = format!("{}/", base.to_lowercase());
    path.to_lowercase().starts_with(&prefix).then(|| path[prefix.len()..].to_string())
}

// ───────────────────────────── sending ─────────────────────────────

/// The project folder of a session pinned in Pult (`state.json`); sessions outside Pult carry no code.
fn pinned_folder(paths: &Paths, session: &str) -> Option<PathBuf> {
    let state: Value = serde_json::from_str(&fs::read_to_string(paths.app.join("state.json")).ok()?).ok()?;
    state.get("projects")?.as_array()?.iter().find_map(|entry| {
        if entry.get("session")?.as_str()? != session {
            return None;
        }
        Some(PathBuf::from(entry.get("path")?.as_str()?))
    })
}

/// The folder the session is in now: the last `"cwd"` of its conversation (inside a worktree — the worktree).
fn last_cwd(transcript: &Path) -> Option<PathBuf> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = fs::File::open(transcript).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(512 << 10);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let at = memchr::memmem::rfind(&tail, b"\"cwd\":\"")?;
    let rest = &tail[at + 6..];
    let value: String = serde_json::Deserializer::from_slice(rest).into_iter::<String>().next()?.ok()?;
    let path = PathBuf::from(value);
    path.is_dir().then_some(path)
}

/// Make (or refresh) this computer's snapshot of a session's code. `force` — write it again even if the code
/// didn't change: the session is being handed over, and the other computer waits for a snapshot made after that.
/// Returns whether there is anything to send.
pub fn snapshot(paths: &Paths, session: &str, transcript: Option<&Path>, cwd: Option<&Path>, force: bool) -> bool {
    let Some(config) = sync::config(paths) else {
        return false;
    };
    let Some(folder) = pinned_folder(paths, session) else {
        return false;
    };
    let _ = fs::create_dir_all(dir(paths));
    // One snapshot of a session at a time (hooks of consecutive answers).
    let lock = fs::OpenOptions::new().create(true).append(true).open(dir(paths).join(format!("{session}.lock")));
    let _guard = lock.as_ref().ok().map(|file| file.lock());

    let previous = manifest(paths, session);
    let mut manifest = Manifest {
        session: session.to_string(),
        device: config.device.clone(),
        device_name: config.device_name.clone(),
        at: now(),
        ..Manifest::default()
    };
    let enabled = load_local(paths).enabled;
    let state = if enabled { describe_code(paths, session, &folder, transcript, cwd, &mut manifest) } else { Err(Status::Off) };
    if let Err(status) = state {
        manifest.status = status;
        let _ = fs::remove_file(bundle_path(paths, session));
    }
    let unchanged = previous.as_ref().is_some_and(|previous| {
        let mut same = previous.clone();
        same.at = manifest.at;
        same == manifest
    });
    if unchanged && !force {
        return true;
    }
    if manifest.status == Status::Ok {
        with_local(paths, |local| {
            local.marks.insert(session.to_string(), manifest.mark());
        });
    }
    let Ok(text) = serde_json::to_string_pretty(&manifest) else {
        return false;
    };
    sync::write_file(&manifest_path(paths, session), text.as_bytes(), false).is_ok()
}

/// Fill the manifest from the session's working copy; the bundle goes next to it.
fn describe_code(
    paths: &Paths,
    session: &str,
    folder: &Path,
    transcript: Option<&Path>,
    cwd: Option<&Path>,
    manifest: &mut Manifest,
) -> Result<(), Status> {
    let place = cwd
        .filter(|path| path.is_dir())
        .map(Path::to_path_buf)
        .or_else(|| transcript.and_then(last_cwd))
        .unwrap_or_else(|| folder.to_path_buf());
    let top = PathBuf::from(git(&place, &["rev-parse", "--show-toplevel"]).map_err(|_| Status::Nogit)?);
    let common = PathBuf::from(
        git(&top, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).map_err(|_| Status::Nogit)?,
    );
    let main = if common.file_name().is_some_and(|name| name == ".git") {
        common.parent().map(Path::to_path_buf).unwrap_or_else(|| top.clone())
    } else {
        top.clone()
    };
    let folder = real(folder);
    manifest.repo = relative(&main, &folder).ok_or(Status::Nogit)?;
    manifest.top = relative(&top, &folder).ok_or(Status::Nogit)?;
    manifest.worktree = manifest.top != manifest.repo;
    if manifest.worktree {
        manifest.worktree_name = top.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    }
    manifest.head = git(&top, &["rev-parse", "HEAD"]).map_err(|_| Status::Nogit)?;
    manifest.branch = git(&top, &["symbolic-ref", "-q", "--short", "HEAD"]).unwrap_or_default();
    manifest.remote = git(&top, &["config", "--get", "remote.origin.url"]).map(|url| clean_remote(&url)).unwrap_or_default();

    let scratch = dir(paths).join(format!("{session}.index"));
    manifest.tree = working_tree(&top, &scratch).map_err(|_| Status::Nogit)?;
    let head_tree = git(&top, &["rev-parse", "HEAD^{tree}"]).map_err(|_| Status::Nogit)?;
    if manifest.tree != head_tree {
        manifest.snapshot = commit_tree(&top, &manifest.tree, &manifest.head).map_err(|_| Status::Nogit)?;
        manifest.files = git(&top, &["diff", "--name-only", &manifest.head, &manifest.snapshot])
            .map(|names| names.lines().count() as u32)
            .unwrap_or_default();
    }

    // The bundle: the tip (snapshot or head) with every commit the remotes don't have.
    let tip = if manifest.snapshot.is_empty() { manifest.head.clone() } else { manifest.snapshot.clone() };
    let reference = format!("refs/pult/snapshot/{session}");
    let bundle = bundle_path(paths, session);
    let part = dir(paths).join(format!("{session}.bundle.tmp"));
    let _ = fs::remove_file(&part);
    let made = git(&top, &["update-ref", &reference, &tip]).and_then(|_| {
        git(&top, &["bundle", "create", &part.to_string_lossy(), &reference, "--not", "--remotes"])
    });
    let _ = git(&top, &["update-ref", "-d", &reference]);
    match made {
        Ok(_) => {
            let size = fs::metadata(&part).map(|meta| meta.len()).unwrap_or_default();
            if size > sync::MAX_BLOB {
                let _ = fs::remove_file(&part);
                let _ = fs::remove_file(&bundle);
                manifest.size = size;
                return Err(Status::Big);
            }
            manifest.bundle = sha256_file(&part).unwrap_or_default();
            if sha256_file(&bundle).as_deref() != Some(manifest.bundle.as_str()) {
                fs::rename(&part, &bundle).map_err(|_| Status::Nogit)?;
            } else {
                let _ = fs::remove_file(&part);
            }
        }
        // Everything is already in the remotes ("Refusing to create empty bundle").
        Err(_) => {
            let _ = fs::remove_file(&part);
            let _ = fs::remove_file(&bundle);
        }
    }
    Ok(())
}

/// Files of a session's snapshot to send.
pub fn files(paths: &Paths, session: &str) -> Vec<(String, PathBuf)> {
    [manifest_path(paths, session), bundle_path(paths, session)]
        .into_iter()
        .filter(|path| path.is_file())
        .filter_map(|path| Some((format!("code/{}", path.file_name()?.to_string_lossy()), path)))
        .collect()
}

// ───────────────────────────── receiving ─────────────────────────────

/// Why the code from another computer didn't go into place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stuck {
    /// The working copy here has its own changes (number of files); the incoming code is kept in `branch`.
    Dirty { files: u32, branch: String },
    /// Commits the session worked from aren't here and couldn't be fetched.
    Fetch,
    /// The repository isn't here and couldn't be cloned (no access).
    Clone { remote: String },
    /// The changes there are larger than the sync server takes (MB).
    Big { megabytes: u64 },
    /// The snapshot's bundle hasn't come from the sync server.
    NotArrived,
    /// The folder here isn't a git repository and there is nowhere to clone it from.
    NoRepo,
    /// Something else went wrong (git's message).
    Failed(String),
}

/// The code is in place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Restored {
    /// The branch was different here: the code went into another one (wanted, used).
    pub renamed: Option<(String, String)>,
}

/// A snapshot from another computer that isn't laid down here yet.
pub fn pending(paths: &Paths, session: &str) -> Option<Manifest> {
    let own = sync::config(paths)?.device;
    let manifest = manifest(paths, session)?;
    if manifest.device == own || manifest.status == Status::Off || manifest.status == Status::Nogit {
        return None;
    }
    let local = load_local(paths);
    if !local.enabled {
        return None;
    }
    let key = format!("{} {}", manifest.head, manifest.tree);
    if local.skipped.get(session) == Some(&key) || local.laid.get(session) == Some(&key) {
        return None;
    }
    (local.marks.get(session) != Some(&manifest.mark())).then_some(manifest)
}

/// The bundle the description names has fully arrived.
pub fn ready(paths: &Paths, manifest: &Manifest) -> bool {
    manifest.bundle.is_empty() || sha256_file(&bundle_path(paths, &manifest.session)).as_deref() == Some(manifest.bundle.as_str())
}

/// Continue without this snapshot: it won't be asked about again.
pub fn skip(paths: &Paths, manifest: &Manifest) {
    with_local(paths, |local| {
        local.skipped.insert(manifest.session.clone(), format!("{} {}", manifest.head, manifest.tree));
    });
}

fn device_slug(name: &str) -> String {
    let slug = crate::sessions::latin_slug(name);
    let short: String = slug.chars().take(24).collect();
    let short = short.trim_end_matches('-').to_string();
    if short.is_empty() { "other".into() } else { short }
}

/// A branch name not taken yet: `base`, `base-2`, …
fn free_branch(repo: &Path, base: &str) -> String {
    (1..100)
        .map(|n| if n == 1 { base.to_string() } else { format!("{base}-{n}") })
        .find(|name| !git_ok(repo, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{name}")]))
        .unwrap_or_else(|| format!("{base}-{}", now()))
}

/// Worktrees where a branch is checked out, except `except`.
fn checked_out_elsewhere(repo: &Path, branch: &str, except: &Path) -> bool {
    let Ok(list) = git(repo, &["worktree", "list", "--porcelain"]) else {
        return false;
    };
    let mut path = String::new();
    for line in list.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            path = rest.to_string();
        } else if line.strip_prefix("branch refs/heads/") == Some(branch) && relative(Path::new(&path), &real(except)) != Some(String::new()) {
            return true;
        }
    }
    false
}

/// The branch the code goes into: the same name if it doesn't exist here, or can be moved forward to `head`
/// without losing anything; otherwise a new one with the other computer's name.
fn choose_branch(repo: &Path, manifest: &Manifest, target: &Path) -> Option<(String, bool)> {
    let wanted = manifest.branch.as_str();
    if wanted.is_empty() {
        return None;
    }
    let reference = format!("refs/heads/{wanted}");
    if !git_ok(repo, &["show-ref", "--verify", "--quiet", &reference]) {
        return Some((wanted.to_string(), false));
    }
    let movable = git_ok(repo, &["merge-base", "--is-ancestor", &reference, &manifest.head]);
    if movable && !checked_out_elsewhere(repo, wanted, target) {
        return Some((wanted.to_string(), false));
    }
    Some((free_branch(repo, &format!("{wanted}-{}", device_slug(&manifest.device_name))), true))
}

/// Get `tip` (and its history) into the repository: from the bundle, otherwise from `origin`.
fn fetch(paths: &Paths, repo: &Path, manifest: &Manifest) -> Result<String, Stuck> {
    let tip = if manifest.snapshot.is_empty() { manifest.head.clone() } else { manifest.snapshot.clone() };
    let have = |repo: &Path| git_ok(repo, &["cat-file", "-e", &format!("{tip}^{{commit}}")]);
    if have(repo) {
        return Ok(tip);
    }
    let from_bundle = || {
        let bundle = bundle_path(paths, &manifest.session);
        git(
            repo,
            &[
                "fetch",
                "-q",
                "--no-write-fetch-head",
                &bundle.to_string_lossy(),
                &format!("+refs/pult/snapshot/{0}:refs/pult/incoming/{0}", manifest.session),
            ],
        )
    };
    if !manifest.bundle.is_empty() && from_bundle().is_ok() && have(repo) {
        return Ok(tip);
    }
    // The bundle needs commits that are only in `origin`, or there is no bundle at all.
    let mut fetched = git(repo, &["fetch", "-q", "origin"]).is_ok();
    if !fetched || !have(repo) {
        for address in remote_addresses(&manifest.remote).iter().skip(1) {
            if git(repo, &["fetch", "-q", address, "+refs/heads/*:refs/remotes/origin/*"]).is_ok() {
                fetched = true;
                break;
            }
        }
    }
    if fetched {
        if !manifest.bundle.is_empty() {
            let _ = from_bundle();
        }
        if have(repo) {
            return Ok(tip);
        }
    }
    Err(Stuck::Fetch)
}

/// The project's repository here: cloned from `origin` if the folder is empty or absent.
fn repository(folder: &Path, manifest: &Manifest) -> Result<PathBuf, Stuck> {
    let repo = if manifest.repo.is_empty() { folder.to_path_buf() } else { folder.join(&manifest.repo) };
    if git_ok(&repo, &["rev-parse", "--git-dir"]) && git(&repo, &["rev-parse", "--show-toplevel"]).is_ok_and(|top| relative(Path::new(&top), &real(&repo)) == Some(String::new())) {
        return Ok(repo);
    }
    let empty = fs::read_dir(&repo).map_or(true, |mut entries| {
        entries.all(|entry| entry.is_ok_and(|entry| entry.file_name() == ".DS_Store"))
    });
    if !empty {
        return Err(Stuck::NoRepo);
    }
    if manifest.remote.is_empty() {
        return Err(Stuck::NoRepo);
    }
    let parent = repo.parent().ok_or(Stuck::NoRepo)?;
    fs::create_dir_all(parent).map_err(|error| Stuck::Failed(error.to_string()))?;
    let _ = fs::remove_file(repo.join(".DS_Store"));
    let cloned = remote_addresses(&manifest.remote)
        .iter()
        .any(|address| git(parent, &["clone", "-q", address, &repo.to_string_lossy()]).is_ok());
    if !cloned {
        return Err(Stuck::Clone { remote: manifest.remote.clone() });
    }
    Ok(repo)
}

/// Lay a session's code from another computer down here.
pub fn restore(paths: &Paths, folder: &Path, manifest: &Manifest, transcript: Option<&Path>) -> Result<Restored, Stuck> {
    if manifest.status == Status::Big {
        return Err(Stuck::Big { megabytes: manifest.size.div_ceil(1 << 20) });
    }
    let repo = repository(folder, manifest)?;
    let tip = fetch(paths, &repo, manifest)?;
    let target = if manifest.top.is_empty() { folder.to_path_buf() } else { folder.join(&manifest.top) };
    let mark = load_local(paths).marks.get(&manifest.session).cloned();
    let scratch = dir(paths).join(format!("{}.index", manifest.session));
    let mut restored = Restored::default();

    if target.exists() {
        // The working copy is here already: replace it only if nothing in it is unknown to Pult.
        let head = git(&target, &["rev-parse", "HEAD"]).map_err(Stuck::Failed)?;
        let tree = working_tree(&target, &scratch).map_err(Stuck::Failed)?;
        let head_tree = git(&target, &["rev-parse", "HEAD^{tree}"]).map_err(Stuck::Failed)?;
        let known = mark.as_ref().is_some_and(|mark| mark.head == head && mark.tree == tree);
        let changed = tree != head_tree;
        let behind = head == manifest.head || git_ok(&target, &["merge-base", "--is-ancestor", &head, &manifest.head]);
        if !known && (changed || !behind) {
            // Files changed here beyond what Pult knows: against the state it left, otherwise against the commit.
            let known_tree = mark.as_ref().filter(|mark| mark.head == head).map_or(head_tree.as_str(), |mark| mark.tree.as_str());
            let files = if changed {
                git(&target, &["diff", "--name-only", known_tree, &tree]).map(|names| names.lines().count() as u32).unwrap_or(1).max(1)
            } else {
                0
            };
            let branch = free_branch(&repo, &format!("pult/{}-{}", &manifest.session[..8], device_slug(&manifest.device_name)));
            let _ = git(&repo, &["branch", &branch, &tip]);
            if files == 0 {
                // Commits here that the other computer doesn't have: the code goes into its own branch instead.
                return lay_down(&repo, &target, manifest, &tip, Some((manifest.branch.clone(), branch)), transcript, paths);
            }
            return Err(Stuck::Dirty { files, branch });
        }
        if changed {
            git(&target, &["reset", "-q", "--hard", "HEAD"]).map_err(Stuck::Failed)?;
            git(&target, &["clean", "-q", "-fd"]).map_err(Stuck::Failed)?;
        }
        let branch = choose_branch(&repo, manifest, &target);
        match &branch {
            Some((name, _)) => git(&target, &["checkout", "-q", "-B", name, &manifest.head]).map_err(Stuck::Failed)?,
            None => git(&target, &["checkout", "-q", "--detach", &manifest.head]).map_err(Stuck::Failed)?,
        };
        if let Some((name, true)) = branch {
            restored.renamed = Some((manifest.branch.clone(), name));
        }
    } else {
        let branch = choose_branch(&repo, manifest, &target);
        let target_text = target.to_string_lossy().into_owned();
        match &branch {
            Some((name, _)) if git_ok(&repo, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{name}")]) => {
                git(&repo, &["branch", "-f", name, &manifest.head]).map_err(Stuck::Failed)?;
                git(&repo, &["worktree", "add", "-q", &target_text, name]).map_err(Stuck::Failed)?;
            }
            Some((name, _)) => {
                git(&repo, &["worktree", "add", "-q", "-b", name, &target_text, &manifest.head]).map_err(Stuck::Failed)?;
            }
            None => {
                git(&repo, &["worktree", "add", "-q", "--detach", &target_text, &manifest.head]).map_err(Stuck::Failed)?;
            }
        }
        if let Some((name, true)) = branch {
            restored.renamed = Some((manifest.branch.clone(), name));
        }
    }
    finish(paths, &repo, &target, manifest, &tip, transcript)?;
    Ok(restored)
}

/// Commits here that the other computer doesn't have and no uncommitted changes: the incoming code goes into its
/// own branch, checked out in the working copy; the local branch stays as it was.
fn lay_down(
    repo: &Path,
    target: &Path,
    manifest: &Manifest,
    tip: &str,
    renamed: Option<(String, String)>,
    transcript: Option<&Path>,
    paths: &Paths,
) -> Result<Restored, Stuck> {
    let name = renamed.as_ref().map(|(_, name)| name.clone()).unwrap_or_default();
    git(target, &["checkout", "-q", "-B", &name, &manifest.head]).map_err(Stuck::Failed)?;
    finish(paths, repo, target, manifest, tip, transcript)?;
    Ok(Restored { renamed })
}

/// Lay the uncommitted changes on top of `head`, remember the state and point the conversation to this computer's
/// worktree.
fn finish(paths: &Paths, repo: &Path, target: &Path, manifest: &Manifest, tip: &str, transcript: Option<&Path>) -> Result<(), Stuck> {
    if !manifest.snapshot.is_empty() {
        // The files become those of the snapshot; the index stays at `head`, so the changes are uncommitted again.
        git(target, &["read-tree", "-u", "--reset", tip]).map_err(Stuck::Failed)?;
        git(target, &["reset", "-q", &manifest.head]).map_err(Stuck::Failed)?;
    }
    let _ = git(repo, &["update-ref", "-d", &format!("refs/pult/incoming/{}", manifest.session)]);
    let scratch = dir(paths).join(format!("{}.index", manifest.session));
    let tree = working_tree(target, &scratch).unwrap_or_else(|_| manifest.tree.clone());
    let head = git(target, &["rev-parse", "HEAD"]).unwrap_or_else(|_| manifest.head.clone());
    with_local(paths, |local| {
        local.marks.insert(manifest.session.clone(), Mark { head, tree });
        local.laid.insert(manifest.session.clone(), format!("{} {}", manifest.head, manifest.tree));
    });
    if let Some(transcript) = transcript {
        let _ = point_conversation(transcript, repo, target, manifest);
    }
    Ok(())
}

/// The conversation remembers the worktree with the other computer's path; a `worktree-state` line with this
/// computer's paths makes Claude Code continue inside it (Claude Code writes the same line when entering one).
fn point_conversation(transcript: &Path, repo: &Path, target: &Path, manifest: &Manifest) -> std::io::Result<()> {
    let line = if manifest.worktree {
        let branch = git(target, &["symbolic-ref", "-q", "--short", "HEAD"]).unwrap_or_default();
        let original = git(repo, &["symbolic-ref", "-q", "--short", "HEAD"]).unwrap_or_else(|_| "HEAD".into());
        let original_head = git(repo, &["rev-parse", "HEAD"]).unwrap_or_default();
        serde_json::json!({
            "type": "worktree-state",
            "worktreeSession": {
                "originalCwd": repo.to_string_lossy(),
                "worktreePath": target.to_string_lossy(),
                "worktreeName": manifest.worktree_name,
                "worktreeBranch": branch,
                "originalBranch": original,
                "originalHeadCommit": original_head,
                "sessionId": manifest.session,
            },
            "sessionId": manifest.session,
        })
    } else {
        // Working in the checkout itself: a worktree the conversation remembers would be one that isn't here.
        match crate::migrate::last_worktree_state(transcript) {
            Some(last) if !last.get("worktreeSession").is_none_or(Value::is_null) => serde_json::json!({
                "type": "worktree-state",
                "worktreeSession": null,
                "sessionId": manifest.session,
            }),
            _ => return Ok(()),
        }
    };
    let mut file = fs::OpenOptions::new().append(true).open(transcript)?;
    file.write_all(format!("{line}\n").as_bytes())
}

// ───────────────────────────── for the window ─────────────────────────────

/// The "Code" row of the session panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Summary {
    /// Sync isn't connected.
    NoSync,
    Off,
    Nogit,
    Big { megabytes: u64 },
    /// Carried: in a worktree (name) or the checkout, changed files, when the snapshot went to the server.
    Carried { worktree: Option<String>, files: u32, at: u64 },
    /// No snapshot yet (the session hasn't answered since it was pinned).
    None,
}

pub fn summary(paths: &Paths, session: &str) -> Summary {
    if sync::config(paths).is_none() {
        return Summary::NoSync;
    }
    if !load_local(paths).enabled {
        return Summary::Off;
    }
    match manifest(paths, session) {
        None => Summary::None,
        Some(manifest) => match manifest.status {
            Status::Off => Summary::Off,
            Status::Nogit => Summary::Nogit,
            Status::Big => Summary::Big { megabytes: manifest.size.div_ceil(1 << 20) },
            Status::Ok => Summary::Carried {
                worktree: manifest.worktree.then(|| manifest.worktree_name.clone()),
                files: manifest.files,
                at: manifest.at,
            },
        },
    }
}

/// `pult code status | snapshot <session> | restore <session>` — for checking without the window.
pub fn cli(args: &[String]) -> i32 {
    let paths = Paths::real();
    let session = args.get(1).map(String::as_str).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("status") => {
            let mut names: Vec<_> = fs::read_dir(dir(&paths))
                .map(|entries| entries.filter_map(Result::ok).map(|entry| entry.file_name().to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            names.sort();
            for name in names.iter().filter(|name| name.ends_with(".json") && is_code_file(name)) {
                let session = name.trim_end_matches(".json");
                if let Some(manifest) = manifest(&paths, session) {
                    let pending = if pending(&paths, session).is_some() { " pending" } else { "" };
                    println!(
                        "{} {:?} {} top={} branch={} files={} bundle={} from {}{pending}",
                        &session[..8],
                        manifest.status,
                        if manifest.worktree { "worktree" } else { "checkout" },
                        manifest.top,
                        manifest.branch,
                        manifest.files,
                        !manifest.bundle.is_empty(),
                        manifest.device_name
                    );
                }
            }
            0
        }
        Some("snapshot") => {
            let transcript = pinned_folder(&paths, session).and_then(|folder| crate::sessions::transcript_path(&folder, session));
            let ok = snapshot(&paths, session, transcript.as_deref(), None, true);
            println!("{}", if ok { "ok" } else { "nothing (not pinned or sync not connected)" });
            i32::from(!ok)
        }
        Some("restore") => {
            let (Some(folder), Some(manifest)) = (pinned_folder(&paths, session), pending(&paths, session)) else {
                eprintln!("nothing to restore");
                return 1;
            };
            match restore(&paths, &folder, &manifest, None) {
                Ok(restored) => {
                    println!("ok {restored:?}");
                    0
                }
                Err(stuck) => {
                    eprintln!("{stuck:?}");
                    1
                }
            }
        }
        _ => {
            eprintln!("pult code status | snapshot <session> | restore <session>");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_loses_its_password() {
        assert_eq!(clean_remote("https://user:token@github.com/a/b.git"), "https://github.com/a/b.git");
        assert_eq!(clean_remote("git@github.com:a/b.git"), "git@github.com:a/b.git");
        assert_eq!(clean_remote("https://github.com/a/b.git"), "https://github.com/a/b.git");
    }

    #[test]
    fn https_remote_is_tried_over_ssh_too() {
        assert_eq!(
            remote_addresses("https://github.com/hitkey-io/pult.git"),
            vec!["https://github.com/hitkey-io/pult.git".to_string(), "git@github.com:hitkey-io/pult.git".to_string()]
        );
        assert_eq!(remote_addresses("git@github.com:a/b.git"), vec!["git@github.com:a/b.git".to_string()]);
    }

    #[test]
    fn code_files_are_recognized() {
        assert!(is_code_file("0920f257-58bc-4f9e-a8c7-9345002aff75.json"));
        assert!(is_code_file("0920f257-58bc-4f9e-a8c7-9345002aff75.bundle"));
        assert!(!is_code_file("../x.json"));
        assert!(!is_code_file("0920f257-58bc-4f9e-a8c7-9345002aff75.index"));
    }

    #[test]
    fn relative_paths_ignore_case() {
        assert_eq!(relative(Path::new("/home/user/CODE/X/.claude/worktrees/w"), Path::new("/home/user/Code/X")).as_deref(), Some(".claude/worktrees/w"));
        assert_eq!(relative(Path::new("/home/user/Code/X"), Path::new("/home/user/code/x")).as_deref(), Some(""));
        assert_eq!(relative(Path::new("/home/user/Other"), Path::new("/home/user/Code")), None);
    }

    /// Two computers with different main folders: a worktree session with a new file, a changed file and a local
    /// commit goes from one to the other and back.
    #[test]
    fn code_goes_to_another_computer_and_back() {
        let base = std::env::temp_dir().join(format!("pult-code-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let sh = |dir: &Path, script: &str| {
            let status = Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "{script}");
        };
        // "origin" with one commit, a project on computer A cloned from it, a worktree with work in it.
        let origin = base.join("origin.git");
        fs::create_dir_all(&base).unwrap();
        sh(&base, "git init -q --bare -b main origin.git && git init -q -b main seed && cd seed && echo one > a.txt && git add . && git commit -qm one && git push -q ../origin.git main");
        let session = "0920f257-58bc-4f9e-a8c7-9345002aff75";
        let computer = |name: &str| {
            let home = base.join(name);
            let paths = Paths { claude: home.join(".claude"), app: home.join("app"), root: home.join("Projects"), home: home.clone() };
            fs::create_dir_all(&paths.app).unwrap();
            fs::write(
                paths.app.join("sync.json"),
                format!(r#"{{"address":"x","api":"x","server_name":"x","device":"{name}","device_name":"Computer {name}","token":"t"}}"#),
            )
            .unwrap();
            let folder = paths.root.join("PROJ");
            fs::write(
                paths.app.join("state.json"),
                serde_json::json!({"projects": [{"path": folder.to_string_lossy(), "session": session}]}).to_string(),
            )
            .unwrap();
            (paths, folder)
        };
        let (a, folder_a) = computer("a");
        fs::create_dir_all(&a.root).unwrap();
        sh(&a.root, &format!("git clone -q {} PROJ", origin.display()));
        sh(&folder_a, "git worktree add -q -b work .claude/worktrees/w && cd .claude/worktrees/w && echo two > b.txt && git add . && git commit -qm two && echo changed > a.txt && echo new > c.txt");
        let worktree_a = folder_a.join(".claude/worktrees/w");
        assert!(snapshot(&a, session, None, Some(&worktree_a), false));
        let sent = manifest(&a, session).unwrap();
        assert_eq!((sent.status, sent.worktree, sent.branch.as_str(), sent.files), (Status::Ok, true, "work", 2));
        assert!(!sent.bundle.is_empty(), "the local commit goes in a bundle");

        // Computer B: the project folder is empty (Pult created it); the snapshot files came from the server.
        let (b, folder_b) = computer("b");
        fs::create_dir_all(&folder_b).unwrap();
        fs::create_dir_all(dir(&b)).unwrap();
        for (_, path) in files(&a, session) {
            fs::copy(&path, dir(&b).join(path.file_name().unwrap())).unwrap();
        }
        let incoming = pending(&b, session).expect("a snapshot from another computer is pending");
        assert!(ready(&b, &incoming));
        restore(&b, &folder_b, &incoming, None).unwrap();
        let worktree_b = folder_b.join(".claude/worktrees/w");
        assert_eq!(fs::read_to_string(worktree_b.join("a.txt")).unwrap(), "changed\n");
        assert_eq!(fs::read_to_string(worktree_b.join("c.txt")).unwrap(), "new\n");
        assert_eq!(git(&worktree_b, &["symbolic-ref", "--short", "HEAD"]).unwrap(), "work");
        assert!(!git(&worktree_b, &["status", "--porcelain"]).unwrap().is_empty(), "changes stay uncommitted");
        assert!(pending(&b, session).is_none(), "laid down once");

        // B works on and hands it back; A's worktree is still as A left it — it is replaced.
        sh(&worktree_b, "echo more >> a.txt && rm c.txt");
        assert!(snapshot(&b, session, None, Some(&worktree_b), true));
        for (_, path) in files(&b, session) {
            fs::copy(&path, dir(&a).join(path.file_name().unwrap())).unwrap();
        }
        let back = pending(&a, session).expect("pending on A");
        restore(&a, &folder_a, &back, None).unwrap();
        assert_eq!(fs::read_to_string(worktree_a.join("a.txt")).unwrap(), "changed\nmore\n");
        assert!(!worktree_a.join("c.txt").exists(), "a file removed there is removed here");

        // Something changed on A after that, unknown to Pult: B's next snapshot doesn't overwrite it.
        sh(&worktree_a, "echo mine > d.txt");
        sh(&worktree_b, "echo again >> a.txt");
        assert!(snapshot(&b, session, None, Some(&worktree_b), true));
        for (_, path) in files(&b, session) {
            fs::copy(&path, dir(&a).join(path.file_name().unwrap())).unwrap();
        }
        let third = pending(&a, session).unwrap();
        match restore(&a, &folder_a, &third, None) {
            Err(Stuck::Dirty { files, branch }) => {
                assert_eq!(files, 1);
                assert!(git_ok(&folder_a, &["rev-parse", "--verify", "-q", &branch]), "the incoming code is kept");
            }
            other => panic!("expected Dirty, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(worktree_a.join("d.txt")).unwrap(), "mine\n");
        let _ = fs::remove_dir_all(&base);
    }
}
