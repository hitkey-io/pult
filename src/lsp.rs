//! Language servers for Claude Code (Settings → Languages): one button puts in place everything a language needs —
//! the server itself, a link to it where every session finds it, Claude Code's official `*-lsp` plugin (user scope)
//! and a rule that tells sessions to use it whenever they work with that language.
//!
//! The plugin only says which program to start (`gopls`, `sourcekit-lsp`…); Claude Code looks for it on the PATH of
//! the session, which is the login shell's. Tools installed by a language's own installer (`~/go/bin`, `npm -g`)
//! are often not on it, so the server is linked into `~/.local/bin`, which is (Claude Code itself lives there).
//!
//! The rule is `~/.claude/rules/pult-lsp-<language>.md`, scoped with `paths:` to that language's files, so it is
//! loaded only when a session touches them.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::sessions;

const MARKETPLACE: &str = "claude-plugins-official";

/// How the server gets onto this computer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `go install <package>`; the binary lands in `$(go env GOPATH)/bin`.
    Go(&'static str),
    /// `rustup component add <component>`.
    Rustup(&'static str),
    /// `npm install -g <packages>`; the binary lands in `$(npm prefix -g)/bin`.
    Npm(&'static [&'static str]),
    /// Comes with Xcode or its Command Line Tools: `xcrun --find <server>`.
    Xcode,
}

#[derive(Clone, Copy, Debug)]
pub struct Language {
    pub id: &'static str,
    pub name: &'static str,
    /// The plugin's name in Claude Code's official marketplace.
    pub plugin: &'static str,
    /// The program the plugin starts.
    pub server: &'static str,
    pub source: Source,
    /// The files the rule applies to.
    pub globs: &'static [&'static str],
}

pub const LANGUAGES: &[Language] = &[
    Language { id: "go", name: "Go", plugin: "gopls-lsp", server: "gopls", source: Source::Go("golang.org/x/tools/gopls@latest"), globs: &["**/*.go", "**/go.mod"] },
    Language { id: "swift", name: "Swift", plugin: "swift-lsp", server: "sourcekit-lsp", source: Source::Xcode, globs: &["**/*.swift", "**/Package.swift"] },
    Language { id: "rust", name: "Rust", plugin: "rust-analyzer-lsp", server: "rust-analyzer", source: Source::Rustup("rust-analyzer"), globs: &["**/*.rs", "**/Cargo.toml"] },
    Language {
        id: "typescript",
        name: "TypeScript, JavaScript",
        plugin: "typescript-lsp",
        server: "typescript-language-server",
        source: Source::Npm(&["typescript", "typescript-language-server"]),
        globs: &["**/*.ts", "**/*.tsx", "**/*.js", "**/*.jsx", "**/*.mts", "**/*.cts"],
    },
    Language { id: "python", name: "Python", plugin: "pyright-lsp", server: "pyright-langserver", source: Source::Npm(&["pyright"]), globs: &["**/*.py", "**/*.pyi"] },
    Language {
        id: "c",
        name: "C, C++",
        plugin: "clangd-lsp",
        server: "clangd",
        source: Source::Xcode,
        globs: &["**/*.c", "**/*.h", "**/*.cc", "**/*.cpp", "**/*.cxx", "**/*.hpp", "**/*.m", "**/*.mm"],
    },
];

pub fn language(id: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|language| language.id == id)
}

/// What is in place for one language.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Where the server is, if it is anywhere a session finds it.
    pub server: Option<PathBuf>,
    /// The server is installed but not where sessions look (the link is missing).
    pub server_hidden: Option<PathBuf>,
    pub plugin: bool,
    pub rule: bool,
}

impl Status {
    pub fn ready(&self) -> bool {
        self.server.is_some() && self.plugin && self.rule
    }

    /// Something of ours is there: "Remove" makes sense.
    pub fn any(&self) -> bool {
        self.plugin || self.rule
    }
}

/// The steps of "Install", in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Server,
    Link,
    Plugin,
    Rule,
}

pub const STEPS: [Step; 4] = [Step::Server, Step::Link, Step::Plugin, Step::Rule];

/// Why a step failed. The text the user sees is chosen by the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The language's own tool is missing: `go`, `npm`, `rustup`.
    NoTool(&'static str),
    /// Xcode's Command Line Tools are missing; their installer has been opened.
    NoXcode,
    /// The command ran and failed: its last line of output.
    Command(String),
    /// The server was installed but is still not found.
    NotFound,
    /// `claude plugin …` failed: its last line of output.
    Plugin(String),
    Io(String),
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Where the links go: on every session's PATH (Claude Code lives there).
pub fn bin_dir() -> PathBuf {
    home().join(".local/bin")
}

pub fn rule_path(language: &Language) -> PathBuf {
    home().join(".claude/rules").join(format!("pult-lsp-{}.md", language.id))
}

fn plugin_id(language: &Language) -> String {
    format!("{}@{MARKETPLACE}", language.plugin)
}

/// A command run in the user's login environment, from the home folder.
fn shell(script: &str) -> Command {
    let mut command = sessions::login_command(script);
    command.current_dir(home());
    command
}

fn run(script: &str) -> Result<String, Failure> {
    let output = shell(script).output().map_err(|error| Failure::Io(error.to_string()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(Failure::Command(last_line(&output.stderr, &output.stdout)))
    }
}

fn last_line(stderr: &[u8], stdout: &[u8]) -> String {
    let text = if stderr.iter().any(|byte| !byte.is_ascii_whitespace()) { stderr } else { stdout };
    String::from_utf8_lossy(text).lines().rev().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default().chars().take(300).collect()
}

/// Is `tool` on the login PATH?
fn has_tool(tool: &str) -> bool {
    run(&format!("command -v {tool}")).is_ok_and(|path| !path.is_empty())
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Where the language's installer puts the server, when it isn't on the PATH.
fn installed_at(language: &Language) -> Option<PathBuf> {
    let dir = match language.source {
        Source::Go(_) => PathBuf::from(run("go env GOPATH").ok()?.lines().next()?.trim()).join("bin"),
        // `~/.cargo/bin/rust-analyzer` is rustup's proxy and is there even without the component: it then fails to
        // start ("Unknown binary 'rust-analyzer'"), which is what Claude Code reports. Ask rustup.
        Source::Rustup(component) => {
            run(&format!("rustup which {component}")).ok()?;
            home().join(".cargo/bin")
        }
        Source::Npm(_) => PathBuf::from(run("npm prefix -g").ok()?.trim()).join("bin"),
        Source::Xcode => return run(&format!("xcrun --find {}", language.server)).ok().map(PathBuf::from).filter(|path| executable(path)),
    };
    Some(dir.join(language.server)).filter(|path| executable(path))
}

/// The server as a session sees it: on the login PATH. A `/usr/bin` shim of Xcode counts only when it leads
/// somewhere (without the Command Line Tools it opens their installer instead).
fn on_path(language: &Language) -> Option<PathBuf> {
    let path = PathBuf::from(run(&format!("command -v {}", language.server)).ok()?);
    if language.source == Source::Xcode && path.starts_with("/usr/bin") {
        return installed_at(language).map(|_| path);
    }
    // The first `rust-analyzer` on the PATH may be rustup's proxy without the component: it has to start.
    if matches!(language.source, Source::Rustup(_)) {
        return Command::new(&path).arg("--version").stdin(std::process::Stdio::null()).output().ok()?.status.success().then_some(path);
    }
    executable(&path).then_some(path)
}

/// One plugin from `claude plugin list --json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plugin {
    pub id: String,
    pub enabled: bool,
}

/// The plugins installed for every project: user scope, where Pult installs. A project's own entries are not ours.
fn plugins_from(list_json: &[u8]) -> Vec<Plugin> {
    let Ok(value) = serde_json::from_slice::<Value>(list_json) else {
        return Vec::new();
    };
    let list = value.as_array().cloned().or_else(|| value.get("plugins").and_then(Value::as_array).cloned()).unwrap_or_default();
    list.iter()
        .filter(|plugin| plugin.get("scope").and_then(Value::as_str).is_none_or(|scope| scope == "user"))
        .filter_map(|plugin| {
            let id = plugin.get("id").or_else(|| plugin.get("name")).and_then(Value::as_str)?;
            Some(Plugin { id: id.into(), enabled: plugin.get("enabled").and_then(Value::as_bool) == Some(true) })
        })
        .collect()
}

fn plugins() -> Vec<Plugin> {
    let Ok(output) = sessions::claude(&home(), &["plugin", "list", "--json"]).output() else {
        return Vec::new();
    };
    plugins_from(&output.stdout)
}

fn enabled_plugins() -> Vec<String> {
    plugins().into_iter().filter(|plugin| plugin.enabled).map(|plugin| plugin.id).collect()
}

/// The status of every language (slow: starts shells and `claude`; not on the window's thread).
pub fn statuses() -> Vec<Status> {
    let plugins = enabled_plugins();
    LANGUAGES
        .iter()
        .map(|language| {
            let server = on_path(language);
            let server_hidden = if server.is_none() { installed_at(language) } else { None };
            Status { server, server_hidden, plugin: plugins.contains(&plugin_id(language)), rule: rule_path(language).exists() }
        })
        .collect()
}

/// One step of "Install". Steps that are already done return at once, so "Retry" starts from the beginning.
pub fn run_step(language: &Language, step: Step) -> Result<(), Failure> {
    match step {
        Step::Server => install_server(language),
        Step::Link => link(language),
        Step::Plugin => enable_plugin(language),
        Step::Rule => write_rule(language),
    }
}

fn install_server(language: &Language) -> Result<(), Failure> {
    if on_path(language).is_some() || installed_at(language).is_some() {
        return Ok(());
    }
    match language.source {
        Source::Go(package) => {
            if !has_tool("go") {
                return Err(Failure::NoTool("go"));
            }
            run(&format!("go install {package}"))?;
        }
        Source::Rustup(component) => {
            if !has_tool("rustup") {
                return Err(Failure::NoTool("rustup"));
            }
            run(&format!("rustup component add {component}"))?;
        }
        Source::Npm(packages) => {
            if !has_tool("npm") {
                return Err(Failure::NoTool("npm"));
            }
            run(&format!("npm install -g {}", packages.join(" ")))?;
        }
        Source::Xcode => {
            // Opens Apple's installer of the Command Line Tools; the user finishes it and presses "Retry".
            let _ = run("xcode-select --install");
            return Err(Failure::NoXcode);
        }
    }
    if installed_at(language).is_some() || on_path(language).is_some() { Ok(()) } else { Err(Failure::NotFound) }
}

/// Make the server visible to every session: a link in `~/.local/bin`. Not needed when it is on the PATH already;
/// a file of the user's own with that name is never replaced.
fn link(language: &Language) -> Result<(), Failure> {
    if on_path(language).is_some() {
        return Ok(());
    }
    let target = installed_at(language).ok_or(Failure::NotFound)?;
    let dir = bin_dir();
    std::fs::create_dir_all(&dir).map_err(|error| Failure::Io(error.to_string()))?;
    let link = dir.join(language.server);
    if std::fs::symlink_metadata(&link).is_ok() {
        if std::fs::read_link(&link).is_err() {
            return Err(Failure::Io(format!("{} exists", link.display())));
        }
        std::fs::remove_file(&link).map_err(|error| Failure::Io(error.to_string()))?;
    }
    std::os::unix::fs::symlink(&target, &link).map_err(|error| Failure::Io(error.to_string()))?;
    if on_path(language).is_some() { Ok(()) } else { Err(Failure::NotFound) }
}

fn claude(args: &[&str]) -> Result<(), Failure> {
    let output = sessions::claude(&home(), args).output().map_err(|error| Failure::Io(error.to_string()))?;
    if output.status.success() { Ok(()) } else { Err(Failure::Plugin(last_line(&output.stderr, &output.stdout))) }
}

/// What the plugin step has to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Nothing,
    Enable,
    Install,
}

fn action(listed: &[Plugin], id: &str) -> Action {
    match listed.iter().find(|plugin| plugin.id == id) {
        Some(plugin) if plugin.enabled => Action::Nothing,
        Some(_) => Action::Enable,
        None => Action::Install,
    }
}

fn enable_plugin(language: &Language) -> Result<(), Failure> {
    let id = plugin_id(language);
    // The list decides, not `enable`'s word: for a plugin that isn't installed it answers "Successfully enabled",
    // exits with 0 and only writes the setting — the plugin itself is still missing (found with pyright-lsp).
    let act = match action(&plugins(), &id) {
        Action::Nothing => return Ok(()),
        act => act,
    };
    let result = match act {
        Action::Enable => claude(&["plugin", "enable", &id, "--scope", "user"]),
        _ => claude(&["plugin", "install", &id, "--scope", "user", "--yes"]),
    };
    result?;
    if action(&plugins(), &id) == Action::Nothing { Ok(()) } else { Err(Failure::Plugin(String::new())) }
}

pub fn rule_text(language: &Language) -> String {
    let paths: String = language.globs.iter().map(|glob| format!("  - \"{glob}\"\n")).collect();
    format!(
        "---\npaths:\n{paths}---\n\n\
# {name}: use the language server\n\n\
Written by Pult (Settings → Languages): `{server}` is installed and Claude Code's `{plugin}` plugin is on.\n\n\
When you read, navigate or change {name} code, use the LSP tool first, not text search:\n\
- where a symbol is defined — `goToDefinition`; its implementations — `goToImplementation`;\n\
- who uses it — `findReferences`, `incomingCalls`; what it calls — `outgoingCalls`;\n\
- what it is (type, signature, docs) — `hover`;\n\
- a file's outline — `documentSymbol`; a symbol by name across the project — `workspaceSymbol`.\n\n\
Before renaming a symbol or changing a signature, run `findReferences` on it and change every place it returns.\n\
After editing, fix the errors the language server reports.\n\
Use grep only for text that is not code (comments, strings, configuration) or when the LSP tool finds nothing.\n",
        name = language.name,
        server = language.server,
        plugin = language.plugin,
    )
}

fn write_rule(language: &Language) -> Result<(), Failure> {
    let path = rule_path(language);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| Failure::Io(error.to_string()))?;
    }
    std::fs::write(&path, rule_text(language)).map_err(|error| Failure::Io(error.to_string()))
}

/// "Remove": the plugin is turned off, the rule and our link are deleted. The server and the language's tools stay.
pub fn remove(language: &Language) -> Result<(), Failure> {
    let id = plugin_id(language);
    if enabled_plugins().contains(&id) {
        claude(&["plugin", "disable", &id, "--scope", "user"])?;
    }
    let rule = rule_path(language);
    if rule.exists() {
        std::fs::remove_file(&rule).map_err(|error| Failure::Io(error.to_string()))?;
    }
    let link = bin_dir().join(language.server);
    if std::fs::read_link(&link).is_ok() {
        std::fs::remove_file(&link).map_err(|error| Failure::Io(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_a_plugin_a_server_and_files() {
        let mut ids = std::collections::BTreeSet::new();
        for language in LANGUAGES {
            assert!(ids.insert(language.id), "{} twice", language.id);
            assert!(language.plugin.ends_with("-lsp") && !language.server.is_empty() && !language.globs.is_empty());
        }
        assert_eq!(language("go").map(|go| go.server), Some("gopls"));
    }

    #[test]
    fn the_rule_is_scoped_to_the_languages_files() {
        let text = rule_text(language("go").unwrap());
        assert!(text.starts_with("---\npaths:\n  - \"**/*.go\"\n  - \"**/go.mod\"\n---\n"));
        assert!(text.contains("`gopls`") && text.contains("`gopls-lsp`") && text.contains("findReferences"));
    }

    #[test]
    fn a_plugin_that_is_not_installed_is_installed_and_one_that_is_off_is_enabled() {
        let on = |id: &str| Plugin { id: id.into(), enabled: true };
        let off = |id: &str| Plugin { id: id.into(), enabled: false };
        let id = "pyright-lsp@claude-plugins-official";
        assert_eq!(action(&[], id), Action::Install);
        assert_eq!(action(&[on("gopls-lsp@claude-plugins-official")], id), Action::Install);
        assert_eq!(action(&[off(id)], id), Action::Enable);
        assert_eq!(action(&[off("gopls-lsp@claude-plugins-official"), on(id)], id), Action::Nothing);
    }

    #[test]
    fn the_plugin_list_keeps_the_user_scope_with_its_switch() {
        let list = serde_json::json!([
            {"id": "pyright-lsp@claude-plugins-official", "enabled": true, "scope": "user", "version": "1.0.0"},
            {"id": "swift-lsp@x", "enabled": true, "scope": "project"},
            {"name": "no-scope@y", "enabled": false}
        ]);
        let kept = plugins_from(list.to_string().as_bytes());
        assert_eq!(kept, [Plugin { id: "pyright-lsp@claude-plugins-official".into(), enabled: true }, Plugin { id: "no-scope@y".into(), enabled: false }]);
        assert!(plugins_from(b"not json").is_empty());
    }

    /// Installs for real on this computer: `PULT_LSP=go,rust cargo test live_install -- --ignored`.
    #[test]
    #[ignore]
    fn live_install() {
        for id in std::env::var("PULT_LSP").unwrap_or_default().split(',').filter(|id| !id.is_empty()) {
            let language = language(id).expect("known language");
            for step in STEPS {
                run_step(language, step).unwrap_or_else(|failure| panic!("{id} {step:?}: {failure:?}"));
            }
        }
        for (language, status) in LANGUAGES.iter().zip(statuses()) {
            println!("{:<12} ready={} {:?}", language.id, status.ready(), status);
        }
    }

    #[test]
    fn a_failed_command_says_its_last_line() {
        assert_eq!(last_line(b"warn\nerror: no network\n\n", b"ignored"), "error: no network");
        assert_eq!(last_line(b"  \n", b"out line\n"), "out line");
    }
}
