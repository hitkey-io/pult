//! Which models Claude Code talks to: Claude or GLM (Z.ai).
//!
//! GLM serves the Anthropic protocol on its own address, so Claude Code only needs to be told where to go:
//! the `env` block of `~/.claude/settings.json` (read once when a Claude Code process starts). The
//! `@z_ai/coding-helper` utility does exactly that. Pult does the same, with two differences: the values the
//! user had before are remembered and put back when switching to Claude (the utility just deletes the keys),
//! and the running sessions are restarted right away (`sessions::restart`) so the switch takes effect now.
//!
//! The GLM settings (plan, key, models) live in `models.json` in Pult's data folder (mode 600). The key is
//! never printed or logged.
//!
//! Since 0.2.0 the models are chosen per project. `~/.claude/settings.json` stays Claude's; a GLM project's session
//! is started with `--settings <glm-session.json>` (`session_flag`), a file in Pult's data folder (mode 600) that
//! carries GLM's variables and the shared-checkout setting. Claude Code keeps the flag in the job's `respawnFlags`,
//! so the session stays on GLM across restarts; switching a project rewrites that flag and restarts its session.
//! A Mac where the old global switch left GLM in `settings.json` is moved over once (`migrate_global`).

use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::sync::Paths;

/// The models GLM serves for Claude Code, newest first. The first one the utility offers by default is
/// `DEFAULT_MODEL`.
pub const MODELS: [&str; 9] = ["glm-5.4", "glm-5.3-flash", "glm-5.3", "glm-5.2", "glm-5.1", "glm-5", "glm-5-turbo", "glm-4.7", "glm-4.5-air"];
pub const DEFAULT_MODEL: &str = "glm-5.3-flash";

/// Variables the switch owns. Their previous values are saved before GLM sets them and returned afterwards.
const MANAGED: [&str; 12] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "API_TIMEOUT_MS",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "DISABLE_TELEMETRY",
    "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Glm,
}

/// Which Z.ai service the key belongs to: they are separate accounts on separate addresses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Plan {
    #[default]
    Global,
    China,
}

impl Plan {
    /// Where Claude Code is sent.
    pub fn base_url(self) -> &'static str {
        match self {
            Plan::Global => "https://api.z.ai/api/anthropic",
            Plan::China => "https://open.bigmodel.cn/api/anthropic",
        }
    }

    /// Where the key is checked: the list of models the key can use.
    fn models_url(self) -> &'static str {
        match self {
            Plan::Global => "https://api.z.ai/api/coding/paas/v4/models",
            Plan::China => "https://open.bigmodel.cn/api/coding/paas/v4/models",
        }
    }

}

fn default_model() -> String {
    DEFAULT_MODEL.into()
}

/// `models.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub plan: Plan,
    #[serde(default)]
    pub key: String,
    /// Gets Sonnet and Opus.
    #[serde(default = "default_model")]
    pub main: String,
    /// Gets Haiku.
    #[serde(default = "default_model")]
    pub small: String,
    /// What the managed variables held before GLM took them: `null` — the variable wasn't there.
    #[serde(default)]
    pub saved: Option<BTreeMap<String, Option<Value>>>,
    /// New projects (and projects with no choice of their own) use GLM.
    #[serde(default)]
    pub default_glm: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { plan: Plan::Global, key: String::new(), main: default_model(), small: default_model(), saved: None, default_glm: false }
    }
}

impl Config {
    pub fn has_key(&self) -> bool {
        !self.key.trim().is_empty()
    }
}

fn config_path(paths: &Paths) -> PathBuf {
    paths.app.join("models.json")
}

fn settings_path(paths: &Paths) -> PathBuf {
    paths.claude.join("settings.json")
}

pub fn load(paths: &Paths) -> Config {
    fs::read_to_string(config_path(paths)).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

/// Write a file so that nobody ever reads half of it; a new file gets `mode`.
fn write_atomic(path: &Path, text: &str, mode: u32) -> Result<(), String> {
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(mode);
    let result = options.open(&temp).and_then(|mut file| file.write_all(text.as_bytes())).and_then(|()| fs::rename(&temp, path));
    result.map_err(|error| {
        let _ = fs::remove_file(&temp);
        error.to_string()
    })
}

/// The key and plan the `@z_ai/coding-helper` utility saved (`~/.chelper/config.yaml`, `api_key:` and `plan:`).
fn helper_settings(paths: &Paths) -> Option<(Plan, String)> {
    let text = fs::read_to_string(paths.home.join(".chelper").join("config.yaml")).ok()?;
    let value = |name: &str| {
        text.lines().find_map(|line| {
            let rest = line.trim().strip_prefix(name)?.strip_prefix(':')?;
            let rest = rest.trim().trim_matches(|c| c == '"' || c == '\'');
            (!rest.is_empty()).then(|| rest.to_string())
        })
    };
    let plan = if value("plan").is_some_and(|plan| plan.contains("china") || plan.contains("cn")) { Plan::China } else { Plan::Global };
    Some((plan, value("api_key")?))
}

/// If Pult has no GLM key yet but the utility saved one, take it (and its plan): the user already gave it to
/// the same service on this Mac. The utility's file stays as it is. `true` — something was taken.
pub fn adopt_helper_key(paths: &Paths) -> bool {
    let mut config = load(paths);
    if config.has_key() {
        return false;
    }
    let Some((plan, key)) = helper_settings(paths) else {
        return false;
    };
    config.plan = plan;
    config.key = key;
    save(paths, &config).is_ok()
}

pub fn save(paths: &Paths, config: &Config) -> Result<(), String> {
    fs::create_dir_all(&paths.app).map_err(|error| error.to_string())?;
    let text = serde_json::to_string_pretty(config).map_err(|error| error.to_string())?;
    write_atomic(&config_path(paths), &text, 0o600)
}

fn read_settings(paths: &Paths) -> Result<Value, String> {
    match fs::read_to_string(settings_path(paths)) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text).map_err(|_| "Claude Code settings aren't valid JSON".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error.to_string()),
    }
}

fn write_settings(paths: &Paths, settings: &Value) -> Result<(), String> {
    let path = settings_path(paths);
    // Once, before the first change: the file is the user's.
    let backup = paths.app.join("claude-settings.bak-models.json");
    if path.exists() && !backup.exists() {
        let _ = fs::create_dir_all(&paths.app);
        let _ = fs::copy(&path, &backup);
    }
    let text = serde_json::to_string_pretty(settings).map_err(|error| error.to_string())?;
    // The file keeps the permissions it had; a new one is private — it holds the key while GLM is on, as with the utility.
    let mode = fs::metadata(&path).map(|meta| std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777).unwrap_or(0o600);
    write_atomic(&path, &(text + "\n"), mode)
}

fn env_of(settings: &Value) -> Option<&Map<String, Value>> {
    settings.get("env").and_then(Value::as_object)
}

/// Who Claude Code talks to now — read from the file, not from what Pult remembers: the utility, another
/// computer's copy of the file or the user's own editing can change it.
pub fn current(paths: &Paths) -> Provider {
    let settings = read_settings(paths).unwrap_or_else(|_| json!({}));
    let url = env_of(&settings).and_then(|env| env.get("ANTHROPIC_BASE_URL")).and_then(Value::as_str).unwrap_or("");
    if url.contains("api.z.ai") || url.contains("bigmodel.cn") { Provider::Glm } else { Provider::Claude }
}

/// The model with the `[1m]` suffix Claude Code needs for a 1M context window (all known GLM models have it).
fn with_window(model: &str) -> String {
    let known = MODELS.contains(&model.trim_end_matches("[1m]"));
    if known && !model.to_lowercase().ends_with("[1m]") { format!("{model}[1m]") } else { model.to_string() }
}

fn glm_env(config: &Config) -> Vec<(&'static str, String)> {
    vec![
        ("ANTHROPIC_AUTH_TOKEN", config.key.trim().to_string()),
        ("ANTHROPIC_BASE_URL", config.plan.base_url().to_string()),
        ("API_TIMEOUT_MS", "3000000".into()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".into()),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", with_window(&config.small)),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", with_window(&config.main)),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", with_window(&config.main)),
        ("DISABLE_TELEMETRY", "1".into()),
        ("CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT", "0".into()),
        ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "1000000".into()),
        ("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "1000000".into()),
    ]
}

/// Point Claude Code at `to`. The settings of GLM (`models.json`) are read here; a missing key is an error.
pub fn apply(paths: &Paths, to: Provider) -> Result<(), String> {
    let mut config = load(paths);
    let mut settings = read_settings(paths)?;
    if !settings.is_object() {
        settings = json!({});
    }
    let was = current(paths);
    let root = settings.as_object_mut().expect("checked above");
    let env = root.entry("env").or_insert_with(|| json!({}));
    if !env.is_object() {
        *env = json!({});
    }
    let env = env.as_object_mut().expect("checked above");
    match to {
        Provider::Glm => {
            if !config.has_key() {
                return Err("no GLM key".into());
            }
            // Remember what was there — unless GLM is already on: then those are GLM's own values (or the
            // utility's, and there is nothing of the user's to return).
            if was == Provider::Claude {
                config.saved = Some(MANAGED.iter().map(|name| ((*name).to_string(), env.get(*name).cloned())).collect());
                save(paths, &config)?;
            }
            env.remove("ANTHROPIC_API_KEY");
            for (name, value) in glm_env(&config) {
                env.insert(name.into(), Value::String(value));
            }
            write_settings(paths, &settings)
        }
        Provider::Claude => {
            for name in MANAGED {
                match config.saved.as_ref().and_then(|saved| saved.get(name)) {
                    Some(Some(value)) => {
                        env.insert(name.into(), value.clone());
                    }
                    _ => {
                        env.remove(name);
                    }
                }
            }
            if env.is_empty() {
                root.remove("env");
            }
            write_settings(paths, &settings)?;
            // Only now: if the file didn't go through, the saved values are still needed.
            config.saved = None;
            save(paths, &config)
        }
    }
}

/// `claude` / `glm` as stored with a project.
pub fn provider_key(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Glm => "glm",
    }
}

pub fn provider_from_key(key: &str) -> Option<Provider> {
    match key {
        "claude" => Some(Provider::Claude),
        "glm" => Some(Provider::Glm),
        _ => None,
    }
}

/// The settings file GLM sessions are started with.
const GLM_SESSION: &str = "glm-session.json";

/// Whether a `--settings` value is Pult's GLM file.
pub fn is_glm_flag(value: &str) -> bool {
    value.ends_with(&format!("/{GLM_SESSION}"))
}

/// Write the GLM session settings: the shared-checkout setting every Pult session has, plus GLM's variables.
/// Returns its path. A missing key is an error.
pub fn write_glm_session(paths: &Paths) -> Result<PathBuf, String> {
    let config = load(paths);
    if !config.has_key() {
        return Err("no GLM key".into());
    }
    let mut settings: Value = serde_json::from_str(crate::sessions::SHARED_CHECKOUT[1]).unwrap_or_else(|_| json!({}));
    let env: Map<String, Value> = glm_env(&config).into_iter().map(|(name, value)| (name.to_string(), Value::String(value))).collect();
    settings["env"] = Value::Object(env);
    fs::create_dir_all(&paths.app).map_err(|error| error.to_string())?;
    let path = paths.app.join(GLM_SESSION);
    let text = serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())?;
    write_atomic(&path, &(text + "\n"), 0o600)?;
    Ok(path)
}

/// The `--settings` value a session of a project on `provider` is started with.
pub fn session_flag(paths: &Paths, provider: Provider) -> Result<String, String> {
    match provider {
        Provider::Claude => Ok(crate::sessions::SHARED_CHECKOUT[1].to_string()),
        Provider::Glm => write_glm_session(paths).map(|path| path.to_string_lossy().into_owned()),
    }
}

/// Keep the GLM session file in step with the saved plan, key and models (sessions read it when they start).
pub fn refresh_glm_session(paths: &Paths) {
    if paths.app.join(GLM_SESSION).exists() || load(paths).has_key() {
        let _ = write_glm_session(paths);
    }
}

/// Once, on a Mac where the old global switch left GLM in `~/.claude/settings.json`: the file goes back to Claude,
/// GLM becomes the default, and the background sessions Pult started are told to come back on GLM (their
/// `respawnFlags`; the running processes keep what they have until they restart). `true` — moved over.
pub fn migrate_global(paths: &Paths) -> bool {
    if current(paths) != Provider::Glm || !load(paths).has_key() {
        return false;
    }
    let Ok(glm) = write_glm_session(paths) else {
        return false;
    };
    if apply(paths, Provider::Claude).is_err() {
        return false;
    }
    let mut config = load(paths);
    config.default_glm = true;
    let _ = save(paths, &config);
    let glm = glm.to_string_lossy().into_owned();
    if let Ok(jobs) = fs::read_dir(paths.claude.join("jobs")) {
        for job in jobs.filter_map(Result::ok) {
            crate::sessions::replace_settings_flag(&job.path().join("state.json"), &glm, false);
        }
    }
    true
}

/// Names that matter in the shell environment.
pub const SHELL_NAMES: [&str; 3] = ["ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

/// `model` in the Claude Code settings: a full Claude name doesn't work through GLM.
pub fn pinned_claude_model(paths: &Paths) -> Option<String> {
    let settings = read_settings(paths).ok()?;
    let model = settings.get("model")?.as_str()?;
    let lower = model.to_lowercase();
    (lower.starts_with("claude-") || lower.contains("claude-")).then(|| model.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCheck {
    Valid,
    Rejected,
    Offline,
}

/// Whether Z.ai accepts the key: the list of models is requested with it. Blocking — run it in the background.
pub fn check_key(plan: Plan, key: &str) -> KeyCheck {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .user_agent(concat!("pult/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    classify(agent.get(plan.models_url()).header("Authorization", format!("Bearer {}", key.trim())).call().map(|response| response.status().as_u16()))
}

fn classify<E>(status: Result<u16, E>) -> KeyCheck {
    match status {
        Ok(200..=299) => KeyCheck::Valid,
        Ok(401 | 403) => KeyCheck::Rejected,
        // 4xx/5xx other than a refusal of the key, or no answer at all: the key itself wasn't judged.
        Ok(_) | Err(_) => KeyCheck::Offline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> Paths {
        let base = std::env::temp_dir().join(format!("pult-models-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let paths = Paths { claude: base.join(".claude"), app: base.join("app"), root: base.join("root"), home: base.clone() };
        fs::create_dir_all(&paths.claude).unwrap();
        paths
    }

    fn settings(paths: &Paths) -> Value {
        serde_json::from_str(&fs::read_to_string(settings_path(paths)).unwrap()).unwrap()
    }

    fn with_key(paths: &Paths) -> Config {
        let config = Config { key: "sk-test".into(), ..Config::default() };
        save(paths, &config).unwrap();
        config
    }

    #[test]
    fn a_glm_project_gets_its_own_settings_file_and_claude_the_shared_checkout() {
        let paths = scratch("session");
        assert_eq!(session_flag(&paths, Provider::Claude).unwrap(), crate::sessions::SHARED_CHECKOUT[1]);
        assert!(session_flag(&paths, Provider::Glm).is_err(), "no key — no GLM");
        with_key(&paths);
        let flag = session_flag(&paths, Provider::Glm).unwrap();
        assert!(is_glm_flag(&flag) && !is_glm_flag(crate::sessions::SHARED_CHECKOUT[1]));
        let file: Value = serde_json::from_str(&fs::read_to_string(&flag).unwrap()).unwrap();
        assert_eq!(file["worktree"]["bgIsolation"], "none");
        assert_eq!(file["env"]["ANTHROPIC_BASE_URL"], "https://api.z.ai/api/anthropic");
        assert_eq!(file["env"]["ANTHROPIC_AUTH_TOKEN"], "sk-test");
        let mode = std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&flag).unwrap().permissions()) & 0o777;
        assert_eq!(mode, 0o600, "the file holds the key");
        // Claude Code's own settings stay untouched.
        assert!(!settings_path(&paths).exists());
    }

    #[test]
    fn the_old_global_glm_moves_to_a_default_and_the_sessions_follow() {
        let paths = scratch("migrate");
        with_key(&paths);
        fs::write(settings_path(&paths), json!({"model": "opus"}).to_string()).unwrap();
        apply(&paths, Provider::Glm).unwrap();
        let job = paths.claude.join("jobs/abc12345/state.json");
        fs::create_dir_all(job.parent().unwrap()).unwrap();
        fs::write(&job, json!({"respawnFlags": ["--settings", crate::sessions::SHARED_CHECKOUT[1]]}).to_string()).unwrap();
        assert!(migrate_global(&paths));
        assert_eq!(current(&paths), Provider::Claude, "settings.json is Claude's again");
        assert_eq!(settings(&paths), json!({"model": "opus"}));
        assert!(load(&paths).default_glm);
        let flags: Value = serde_json::from_str(&fs::read_to_string(&job).unwrap()).unwrap();
        assert!(is_glm_flag(flags["respawnFlags"][1].as_str().unwrap()));
        assert!(!migrate_global(&paths), "once");
    }

    #[test]
    fn glm_writes_what_the_utility_writes() {
        let paths = scratch("write");
        with_key(&paths);
        apply(&paths, Provider::Glm).unwrap();
        let env = settings(&paths)["env"].clone();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://api.z.ai/api/anthropic");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "sk-test");
        assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "glm-5.3-flash[1m]");
        assert_eq!(env["ANTHROPIC_DEFAULT_OPUS_MODEL"], "glm-5.3-flash[1m]");
        assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "glm-5.3-flash[1m]");
        assert_eq!(env["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "1000000");
        assert_eq!(env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "1000000");
        assert_eq!(current(&paths), Provider::Glm);
    }

    #[test]
    fn the_big_model_keeps_the_full_window_and_the_plan_picks_the_address() {
        let paths = scratch("plan");
        let mut config = with_key(&paths);
        config.main = "glm-5.4".into();
        config.plan = Plan::China;
        save(&paths, &config).unwrap();
        apply(&paths, Provider::Glm).unwrap();
        let env = settings(&paths)["env"].clone();
        assert_eq!(env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "1000000");
        assert_eq!(env["ANTHROPIC_BASE_URL"], "https://open.bigmodel.cn/api/anthropic");
        assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "glm-5.4[1m]");
        assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "glm-5.3-flash[1m]");
    }

    #[test]
    fn going_back_returns_the_users_own_values_and_leaves_the_rest_alone() {
        let paths = scratch("back");
        let before = json!({
            "model": "opus",
            "hooks": {"Stop": []},
            "env": {"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "200000", "MY_OWN": "keep", "ANTHROPIC_API_KEY": "sk-mine"}
        });
        fs::write(settings_path(&paths), serde_json::to_string_pretty(&before).unwrap()).unwrap();
        with_key(&paths);
        apply(&paths, Provider::Glm).unwrap();
        let on = settings(&paths);
        assert_eq!(on["env"]["MY_OWN"], "keep");
        assert!(on["env"].get("ANTHROPIC_API_KEY").is_none(), "an API key would beat the token");
        assert_eq!(on["model"], "opus");
        apply(&paths, Provider::Claude).unwrap();
        assert_eq!(settings(&paths), before, "exactly what it was");
        assert_eq!(current(&paths), Provider::Claude);
        assert!(load(&paths).saved.is_none());
    }

    #[test]
    fn an_untouched_file_goes_back_to_having_no_env_at_all() {
        let paths = scratch("empty");
        fs::write(settings_path(&paths), r#"{"theme":"dark"}"#).unwrap();
        with_key(&paths);
        apply(&paths, Provider::Glm).unwrap();
        apply(&paths, Provider::Claude).unwrap();
        assert_eq!(settings(&paths), json!({"theme": "dark"}));
    }

    #[test]
    fn switching_to_glm_twice_does_not_save_glms_own_values_as_the_users() {
        let paths = scratch("twice");
        fs::write(settings_path(&paths), r#"{"env":{"DISABLE_TELEMETRY":"0"}}"#).unwrap();
        with_key(&paths);
        apply(&paths, Provider::Glm).unwrap();
        // The model changed in Settings and GLM is applied again.
        let mut config = load(&paths);
        config.main = "glm-5.2".into();
        save(&paths, &config).unwrap();
        apply(&paths, Provider::Glm).unwrap();
        assert_eq!(settings(&paths)["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "glm-5.2[1m]");
        apply(&paths, Provider::Claude).unwrap();
        assert_eq!(settings(&paths), json!({"env": {"DISABLE_TELEMETRY": "0"}}));
    }

    #[test]
    fn a_missing_key_is_refused_and_nothing_is_written() {
        let paths = scratch("nokey");
        fs::write(settings_path(&paths), r#"{"a":1}"#).unwrap();
        assert!(apply(&paths, Provider::Glm).is_err());
        assert_eq!(settings(&paths), json!({"a": 1}));
    }

    #[test]
    fn the_utility_made_setup_is_read_as_glm_and_removed_like_the_utility_does() {
        let paths = scratch("utility");
        fs::write(settings_path(&paths), r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.z.ai/api/anthropic","ANTHROPIC_AUTH_TOKEN":"x","OTHER":"1"}}"#).unwrap();
        assert_eq!(current(&paths), Provider::Glm);
        apply(&paths, Provider::Claude).unwrap();
        assert_eq!(settings(&paths), json!({"env": {"OTHER": "1"}}));
    }

    #[test]
    fn a_broken_settings_file_is_not_overwritten() {
        let paths = scratch("broken");
        fs::write(settings_path(&paths), "{not json").unwrap();
        with_key(&paths);
        assert!(apply(&paths, Provider::Glm).is_err());
        assert_eq!(fs::read_to_string(settings_path(&paths)).unwrap(), "{not json");
    }

    #[test]
    fn the_first_change_keeps_a_copy_of_the_original_file() {
        let paths = scratch("backup");
        fs::write(settings_path(&paths), r#"{"a":1}"#).unwrap();
        with_key(&paths);
        apply(&paths, Provider::Glm).unwrap();
        apply(&paths, Provider::Claude).unwrap();
        apply(&paths, Provider::Glm).unwrap();
        let copy = fs::read_to_string(paths.app.join("claude-settings.bak-models.json")).unwrap();
        assert_eq!(copy, r#"{"a":1}"#, "the second change doesn't replace the original");
    }

    #[test]
    fn a_pinned_claude_model_is_noticed() {
        let paths = scratch("pinned");
        fs::write(settings_path(&paths), r#"{"model":"claude-opus-5-5"}"#).unwrap();
        assert_eq!(pinned_claude_model(&paths).as_deref(), Some("claude-opus-5-5"));
        fs::write(settings_path(&paths), r#"{"model":"opus"}"#).unwrap();
        assert!(pinned_claude_model(&paths).is_none());
    }

    #[test]
    fn the_key_the_utility_saved_is_adopted_once_and_the_utilitys_file_is_left_alone() {
        let paths = scratch("adopt");
        let helper = paths.home.join(".chelper");
        fs::create_dir_all(&helper).unwrap();
        let text = "api_key: sk-from-utility\nplan: glm_coding_plan_china\nlang: ru\n";
        fs::write(helper.join("config.yaml"), text).unwrap();
        assert!(adopt_helper_key(&paths));
        let config = load(&paths);
        assert_eq!((config.key.as_str(), config.plan), ("sk-from-utility", Plan::China));
        assert_eq!(fs::read_to_string(helper.join("config.yaml")).unwrap(), text);
        // A key entered in Pult is never replaced by the utility's.
        let mine = Config { key: "sk-mine".into(), ..config };
        save(&paths, &mine).unwrap();
        assert!(!adopt_helper_key(&paths));
        assert_eq!(load(&paths).key, "sk-mine");
    }

    #[test]
    fn no_utility_file_means_nothing_to_adopt() {
        let paths = scratch("noadopt");
        assert!(!adopt_helper_key(&paths));
        assert!(!load(&paths).has_key());
    }

    #[test]
    fn the_config_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let paths = scratch("mode");
        with_key(&paths);
        let mode = fs::metadata(config_path(&paths)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn only_a_refused_key_is_called_rejected() {
        assert_eq!(classify::<()>(Ok(200)), KeyCheck::Valid);
        assert_eq!(classify::<()>(Ok(401)), KeyCheck::Rejected);
        assert_eq!(classify::<()>(Ok(403)), KeyCheck::Rejected);
        assert_eq!(classify::<()>(Ok(500)), KeyCheck::Offline);
        assert_eq!(classify::<()>(Ok(429)), KeyCheck::Offline);
        assert_eq!(classify(Err(())), KeyCheck::Offline);
    }
}
