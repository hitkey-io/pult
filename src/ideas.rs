//! The project's ideas file, `.pult/ideas.md`, for Claude Code sessions.
//!
//! Two ways in, both set up by Pult for every project:
//! - Knowing: a hook (`pult ideas hook`) tells the session that the file exists and what it holds (path, number of
//!   links, section headings — not the text). On `SessionStart` of any kind — a new session, a resumed one, after
//!   `/clear` and after compaction — and on any prompt that talks about ideas or bookmarks.
//! - Reading: an MCP server (`pult mcp`, registered for the user as `pult`) with `ideas_outline` and `ideas_read`
//!   (the whole file, a section, or the lines matching a word). Without it the session can still use `Read` on the
//!   path the hook gave.
//!
//! The file is found from the session's working folder upwards (a session may run in a subfolder or in a git worktree
//! inside the project), never above the home folder.

use std::{
    fs,
    io::{BufRead as _, Read as _, Write as _},
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use crate::i18n::t;

/// Inside a project: `<project>/.pult/ideas.md`.
pub const FILE: &str = "ideas.md";

/// Marks Pult's ideas hook among others in Claude Code settings.
const HOOK_MARK: &str = " ideas hook";

/// Name of the MCP server in Claude Code (its tools are `mcp__pult__…`).
const SERVER: &str = "pult";

/// The ideas file of the project `cwd` belongs to.
pub fn find(cwd: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        let path = current.join(crate::sync::DOT_PULT).join(FILE);
        if path.is_file() {
            return Some(path);
        }
        if current == home || current.parent().is_none() {
            break;
        }
        dir = current.parent();
    }
    None
}

/// Section headings (`#`…`###`) in order, without the marks.
fn headings(text: &str) -> Vec<String> {
    let mut fenced = false;
    text.lines()
        .filter_map(|line| {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                return None;
            }
            let rest = line.strip_prefix('#')?;
            let rest = rest.trim_start_matches('#');
            let level = line.len() - rest.len();
            (!fenced && level <= 3 && rest.starts_with(' ')).then(|| rest.trim().to_string())
        })
        .filter(|heading| !heading.is_empty())
        .collect()
}

/// How many links the file holds.
fn links(text: &str) -> usize {
    text.matches("https://").count() + text.matches("http://").count()
}

/// What a session gets told: where the file is, what it holds and how to read it.
fn context(path: &Path, text: &str) -> String {
    let sections = headings(text);
    let sections = if sections.is_empty() { "—".to_string() } else { sections.join("; ") };
    tr!("ideas.context", path = path.display(), links = links(text), sections = sections)
}

/// Whether a prompt is about the ideas: ideas, bookmarks, links to look at.
fn mentions_ideas(prompt: &str) -> bool {
    let prompt = prompt.to_lowercase();
    prompt.contains(".pult")
        || prompt
            .split(|char: char| !char.is_alphanumeric())
            .any(|word| ["иде", "закладк", "idea", "bookmark"].iter().any(|stem| word.starts_with(stem)))
}

/// The hook's answer for one event, if the session should hear about the ideas.
fn handle(event: &Value) -> Option<(String, String)> {
    let name = event.get("hook_event_name").and_then(Value::as_str)?;
    let cwd = event.get("cwd").and_then(Value::as_str).map(PathBuf::from)?;
    match name {
        "SessionStart" => {}
        "UserPromptSubmit" if event.get("prompt").and_then(Value::as_str).is_some_and(mentions_ideas) => {}
        _ => return None,
    }
    let path = find(&cwd)?;
    let text = fs::read_to_string(&path).ok()?;
    Some((name.to_string(), context(&path, &text)))
}

/// `pult ideas hook`: reads the event from stdin, prints the text for the session (if any). Never fails.
pub fn hook() {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let Ok(event) = serde_json::from_str::<Value>(&input) else {
        return;
    };
    if let Some((name, context)) = handle(&event) {
        println!("{}", json!({"hookSpecificOutput": {"hookEventName": name, "additionalContext": context}}));
    }
}

// ───────────────────────────── reading: the MCP server ─────────────────────────────

/// A section: from its heading to the next heading of the same or a higher level. `None` — no such heading.
fn section(text: &str, wanted: &str) -> Option<String> {
    let wanted = wanted.trim().trim_start_matches('#').trim().to_lowercase();
    let level = |line: &str| -> Option<usize> {
        let rest = line.trim_start_matches('#');
        let level = line.len() - rest.len();
        (level > 0 && rest.starts_with(' ')).then_some(level)
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|line| level(line).is_some() && line.trim_start_matches('#').trim().to_lowercase().contains(&wanted))?;
    let top = level(lines[start])?;
    let end = lines[start + 1..].iter().position(|line| level(line).is_some_and(|other| other <= top)).map_or(lines.len(), |offset| start + 1 + offset);
    Some(lines[start..end].join("\n"))
}

/// The paragraphs and list items that mention a word (case does not matter).
fn search(text: &str, word: &str) -> String {
    let word = word.to_lowercase();
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let flush = |current: &mut Vec<&str>, blocks: &mut Vec<String>| {
        let block = current.join("\n");
        if block.to_lowercase().contains(&word) {
            blocks.push(block);
        }
        current.clear();
    };
    for line in text.lines() {
        let starts_item = line.trim_start().starts_with(['-', '*']) || line.starts_with('#');
        if line.trim().is_empty() || starts_item {
            flush(&mut current, &mut blocks);
        }
        if !line.trim().is_empty() {
            current.push(line);
        }
    }
    flush(&mut current, &mut blocks);
    blocks.join("\n\n")
}

/// The project folder a tool call is about: given, else the session's (Claude Code starts the server there).
fn project_dir(arguments: &Value) -> PathBuf {
    arguments
        .get("project_dir")
        .and_then(Value::as_str)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

fn tools() -> Value {
    let project = json!({"type": "string", "description": "Project folder; defaults to the session's folder."});
    json!([
        {
            "name": "ideas_outline",
            "description": "The project's ideas and bookmarks file (.pult/ideas.md, kept by Pult): its path, number of links and section headings. Use it first when the user talks about ideas, bookmarks, links to look at or similar tools.",
            "inputSchema": {"type": "object", "properties": {"project_dir": project}}
        },
        {
            "name": "ideas_read",
            "description": "Read the project's ideas and bookmarks file (.pult/ideas.md): the whole file, one section (by its heading), or the entries mentioning a word.",
            "inputSchema": {"type": "object", "properties": {
                "section": {"type": "string", "description": "Heading of the section to read (part of it is enough)."},
                "query": {"type": "string", "description": "Return only the entries that mention this word."},
                "project_dir": project
            }}
        }
    ])
}

/// One tool call: the text for the model, and whether it is an error.
fn call(name: &str, arguments: &Value) -> (String, bool) {
    let dir = project_dir(arguments);
    let Some(path) = find(&dir) else {
        return (tr!("ideas.no_file", dir = dir.display()), true);
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return (tr!("ideas.unreadable", path = path.display()), true);
    };
    match name {
        "ideas_outline" => (context(&path, &text), false),
        "ideas_read" => {
            let get = |key: &str| arguments.get(key).and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty());
            if let Some(wanted) = get("section") {
                return match section(&text, wanted) {
                    Some(found) => (found, false),
                    None => (tr!("ideas.no_section", section = wanted, sections = headings(&text).join("; ")), true),
                };
            }
            if let Some(word) = get("query") {
                let found = search(&text, word);
                return if found.is_empty() { (tr!("ideas.nothing_found", query = word), false) } else { (found, false) };
            }
            (format!("{}\n\n{text}", path.display()), false)
        }
        _ => (t("ideas.unknown_tool").to_string(), true),
    }
}

/// The answer to one JSON-RPC message; `None` for notifications.
fn respond(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
    let result = match method {
        "initialize" => json!({
            "protocolVersion": message.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER, "version": env!("CARGO_PKG_VERSION")}
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = message.pointer("/params/name").and_then(Value::as_str).unwrap_or_default();
            let arguments = message.pointer("/params/arguments").cloned().unwrap_or(json!({}));
            let (text, error) = call(name, &arguments);
            json!({"content": [{"type": "text", "text": text}], "isError": error})
        }
        _ => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {method}")}})),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// `pult mcp`: the MCP server over stdio, one JSON message per line.
pub fn serve() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(answer) = respond(&message) {
            let _ = writeln!(stdout, "{answer}");
            let _ = stdout.flush();
        }
    }
}

// ───────────────────────────── installing ─────────────────────────────

/// (event) pairs the hook listens to.
const EVENTS: [&str; 2] = ["SessionStart", "UserPromptSubmit"];

fn is_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| hooks.iter().any(|hook| hook.get("command").and_then(Value::as_str).is_some_and(|command| command.ends_with(HOOK_MARK))))
}

/// Write the ideas hook into Claude Code settings next to others'. `false` — already there.
fn merge_hooks(settings: &mut Value, command: &str) -> bool {
    if !settings.is_object() {
        *settings = json!({});
    }
    let hooks = settings.as_object_mut().unwrap().entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let ours = json!({"hooks": [{"type": "command", "command": command, "timeout": 5}]});
    let mut changed = false;
    for event in EVENTS {
        let groups = hooks.as_object_mut().unwrap().entry(event).or_insert_with(|| json!([]));
        if !groups.is_array() {
            *groups = json!([]);
        }
        let groups = groups.as_array_mut().unwrap();
        match groups.iter().position(is_ours) {
            Some(index) if groups[index] == ours => {}
            Some(index) => {
                groups[index] = ours.clone();
                changed = true;
            }
            None => {
                groups.push(ours.clone());
                changed = true;
            }
        }
    }
    changed
}

/// Put in the hook and register the MCP server, if the installed Pult knows these commands. Runs a `claude` command,
/// so call it off the window's thread.
pub fn ensure() {
    let exe = crate::sync::hook_exe();
    if !crate::journal::supports_commands(&exe) {
        return;
    }
    let paths = crate::sync::Paths::real();
    let command = format!("'{}'{HOOK_MARK}", exe.display());
    let _ = crate::sync::edit_settings(&paths, |settings| merge_hooks(settings, &command));
    register_server(&exe);
}

/// `claude mcp add --scope user pult -- <pult> mcp`, unless it is there already with this program.
fn register_server(exe: &Path) {
    let home = dirs::home_dir().unwrap_or_default();
    let limit = std::time::Duration::from_secs(30);
    let wanted = exe.display().to_string();
    if let Ok(out) = crate::sessions::output_within(&mut crate::sessions::claude(&home, &["mcp", "get", SERVER]), limit)
        && out.status.success()
    {
        if String::from_utf8_lossy(&out.stdout).contains(&wanted) {
            return;
        }
        let _ = crate::sessions::output_within(&mut crate::sessions::claude(&home, &["mcp", "remove", "--scope", "user", SERVER]), limit);
    }
    let _ = crate::sessions::output_within(
        &mut crate::sessions::claude(&home, &["mcp", "add", "--scope", "user", SERVER, "--", &wanted, "mcp"]),
        limit,
    );
}

/// `pult ideas …` — without a window.
pub fn cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("hook") => hook(),
        Some("install") => ensure(),
        Some("show") => {
            let dir = args.get(1).map(PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_default();
            let (text, error) = call("ideas_outline", &json!({"project_dir": dir}));
            println!("{text}");
            return i32::from(error);
        }
        _ => {
            eprintln!("pult ideas hook | install | show [folder]");
            return 2;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "# Ideas\n\nIntro.\n\n## Tools\n\n- **bb** — https://getbb.app/ agents in one window\n- **Vibe** — https://vibekanban.com/ kanban\n\n```\n# not a heading\n```\n\n## Methods\n\n- **superpowers** — https://github.com/obra/superpowers skills\n";

    fn project(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pult-ideas-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".pult")).unwrap();
        fs::create_dir_all(dir.join(".claude/worktrees/task/src")).unwrap();
        fs::write(dir.join(".pult/ideas.md"), TEXT).unwrap();
        dir
    }

    #[test]
    fn the_file_is_found_from_a_subfolder_or_a_worktree() {
        let dir = project("find");
        assert_eq!(find(&dir.join(".claude/worktrees/task/src")), Some(dir.join(".pult/ideas.md")));
        assert_eq!(find(&std::env::temp_dir().join("pult-ideas-nowhere")), None);
    }

    #[test]
    fn the_session_hears_about_the_ideas_on_start_resume_and_when_they_come_up() {
        let dir = project("hook");
        let cwd = dir.join(".claude/worktrees/task");
        for source in ["startup", "resume", "compact", "clear"] {
            let event = json!({"hook_event_name": "SessionStart", "source": source, "cwd": cwd});
            let (name, text) = handle(&event).unwrap();
            assert_eq!(name, "SessionStart");
            assert!(text.contains(".pult/ideas.md") && text.contains("Tools; Methods") && !text.contains("not a heading"), "{text}");
            assert!(text.contains("mcp__pult__ideas_read"), "{text}");
        }
        let prompt = |prompt: &str| handle(&json!({"hook_event_name": "UserPromptSubmit", "cwd": cwd, "prompt": prompt}));
        assert!(prompt("Глянь мои идеи по колеям").is_some());
        assert!(prompt("what bookmarks do I have").is_some());
        assert!(prompt("почини сборку").is_none());
        assert!(prompt("посмотри видео").is_none());
        assert!(prompt("что в .pult/ideas.md?").is_some());
        // No file — nothing is said.
        assert!(handle(&json!({"hook_event_name": "SessionStart", "cwd": std::env::temp_dir().join("pult-ideas-none")})).is_none());
    }

    #[test]
    fn the_server_reads_the_whole_file_a_section_or_matching_entries() {
        let dir = project("mcp");
        let args = |extra: Value| {
            let mut args = json!({"project_dir": dir});
            args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            args
        };
        let (whole, error) = call("ideas_read", &args(json!({})));
        assert!(!error && whole.contains("superpowers") && whole.contains("bb"));
        let (tools, _) = call("ideas_read", &args(json!({"section": "tools"})));
        assert!(tools.starts_with("## Tools") && tools.contains("Vibe") && !tools.contains("superpowers"), "{tools}");
        let (found, _) = call("ideas_read", &args(json!({"query": "KANBAN"})));
        assert!(found.contains("Vibe") && !found.contains("bb**"), "{found}");
        let (missing, error) = call("ideas_read", &args(json!({"section": "Nope"})));
        assert!(error && missing.contains("Tools; Methods"), "{missing}");
        let (outline, _) = call("ideas_outline", &args(json!({})));
        assert!(outline.contains('3'), "{outline}");
    }

    #[test]
    fn the_server_speaks_json_rpc() {
        let init = respond(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(respond(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        let list = respond(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 2);
        let unknown = respond(&json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"})).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[test]
    fn the_hook_goes_in_once_next_to_others() {
        let mut settings = json!({"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "other"}]}]}});
        assert!(merge_hooks(&mut settings, "'/p/pult' ideas hook"));
        assert!(!merge_hooks(&mut settings, "'/p/pult' ideas hook"));
        assert_eq!(settings["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        assert_eq!(settings["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 1);
    }
}
