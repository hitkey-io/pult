//! What is connected to Claude Code in a project: plugins, skills, rules.
//!
//! Read from disk every time the panel is shown, so edits show up at once:
//! - plugins: `enabledPlugins` in the project's `.claude/settings.json` and `.claude/settings.local.json`
//!   and in `~/.claude/settings.json`;
//! - skills: folders with `SKILL.md` in the project's `.claude/skills/` and in `~/.claude/skills/`;
//! - rules: the project's `CLAUDE.md`, `CLAUDE.local.md`, `.claude/CLAUDE.md`, `.claude/rules/**/*.md`
//!   and `~/.claude/CLAUDE.md`, `~/.claude/rules/**/*.md`.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

/// Connected items of one kind: the project's own and those shared by all projects.
#[derive(Default)]
pub struct Scoped {
    pub project: Vec<String>,
    pub global: Vec<String>,
}

impl Scoped {
    pub fn len(&self) -> usize {
        self.project.len() + self.global.len()
    }

    /// A shared item the project also connects itself is shown once, as the project's.
    fn new(project: BTreeSet<String>, global: BTreeSet<String>) -> Self {
        let global = global.difference(&project).cloned().collect();
        Self {
            project: project.into_iter().collect(),
            global,
        }
    }
}

pub struct ClaudeSetup {
    pub plugins: Scoped,
    pub skills: Scoped,
    pub rules: Scoped,
}

impl ClaudeSetup {
    pub fn read(project: &Path) -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        let user = home.join(".claude");
        let project_claude = project.join(".claude");
        Self {
            plugins: Scoped::new(
                plugins(&[
                    project_claude.join("settings.json"),
                    project_claude.join("settings.local.json"),
                ]),
                plugins(&[user.join("settings.json")]),
            ),
            skills: Scoped::new(skills(&project_claude.join("skills")), skills(&user.join("skills"))),
            rules: Scoped::new(
                rules(project, &[
                    "CLAUDE.md",
                    "CLAUDE.local.md",
                    ".claude/CLAUDE.md",
                ], &project_claude.join("rules"), None),
                rules(&user, &["CLAUDE.md"], &user.join("rules"), Some(&home)),
            ),
        }
    }
}

/// Enabled plugins; the name drops the marketplace (`figma@claude-plugins-official` → `figma`).
fn plugins(files: &[PathBuf]) -> BTreeSet<String> {
    files
        .iter()
        .filter_map(|file| fs::read_to_string(file).ok())
        .filter_map(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter_map(|settings| settings.get("enabledPlugins")?.as_object().cloned())
        .flat_map(|plugins| plugins.into_iter())
        .filter(|(_, enabled)| enabled.as_bool() == Some(true))
        .map(|(id, _)| id.split('@').next().unwrap_or(&id).to_string())
        .collect()
}

fn skills(dir: &Path) -> BTreeSet<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return BTreeSet::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("SKILL.md").is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Rule files relative to `base`; shared ones are shown from the home folder (`~/…`).
fn rules(base: &Path, names: &[&str], rules_dir: &Path, home: Option<&Path>) -> BTreeSet<String> {
    let mut files: Vec<PathBuf> = names
        .iter()
        .map(|name| base.join(name))
        .filter(|path| path.is_file())
        .collect();
    collect_markdown(rules_dir, &mut files);
    files
        .into_iter()
        .map(|path| match home {
            Some(home) => path
                .strip_prefix(home)
                .map(|rest| format!("~/{}", rest.display()))
                .unwrap_or_else(|_| path.display().to_string()),
            None => path
                .strip_prefix(base)
                .map(|rest| rest.display().to_string())
                .unwrap_or_else(|_| path.display().to_string()),
        })
        .collect()
}

fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_markdown(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
}
