//! Where a session is running right now: one Claude Code session must run on one computer, otherwise its
//! transcript diverges.
//!
//! Each computer keeps `presence.json` in Pult's data folder and shares it with the others inside its projects
//! snapshot (`projects-<computer>.json`, field `presence`):
//! - `active` — sessions running here, with an "alive" mark (refreshed once a minute);
//! - `claimed` — when the session was started here;
//! - `takeover` — "move it here": this computer asked for the session;
//! - `released` — the session was stopped here because another computer asked for it: when, and how long the
//!   transcript was (the taker waits until it has arrived in full).
//!
//! Handover: B sets `takeover`; A sees a request newer than its `claimed`, pauses the session and writes
//! `released`; B waits for `released` and a transcript of the right length (or for A to stay silent longer
//! than `STALE_SECS`) and starts the session locally.

use std::{collections::BTreeMap, fs, path::PathBuf};

use serde::{Deserialize, Serialize};

/// The "alive" mark is refreshed at most once a minute; silent longer than this — the computer is considered gone.
pub const BEAT_SECS: u64 = 60;
pub const STALE_SECS: u64 = 150;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Released {
    pub at: u64,
    /// Transcript length in bytes at the moment it was stopped.
    pub len: u64,
    /// The session's code goes out too (Pult 0.8+): wait for its snapshot made after `at`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub code: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub active: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub claimed: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub takeover: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub released: BTreeMap<String, Released>,
}

fn file() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("pult").join("presence.json"))
}

impl Presence {
    pub fn load() -> Self {
        file()
            .and_then(|file| fs::read_to_string(file).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(file) = file() else {
            return;
        };
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let tmp = file.with_extension("json.tmp");
            if fs::write(&tmp, text).is_ok() {
                let _ = fs::rename(&tmp, &file);
            }
        }
        // Share with other computers right away (the snapshot is sent only if it changed).
        crate::sync::share_projects_now();
    }

    /// Mark sessions running here. `true` — something changed and should be saved.
    pub fn beat(&mut self, running: &[String], now: u64) -> bool {
        let before = self.active.clone();
        self.active.retain(|session, _| running.contains(session));
        for session in running {
            let at = self.active.entry(session.clone()).or_insert(0);
            if now.saturating_sub(*at) >= BEAT_SECS {
                *at = now;
            }
        }
        before != self.active
    }

    /// The session was started here: the earlier stop and request no longer apply.
    pub fn claim(&mut self, session: &str, now: u64) {
        self.claimed.insert(session.to_string(), now);
        self.released.remove(session);
    }
}

/// A session as another computer sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Remote {
    pub device: String,
    pub active_at: u64,
    pub takeover_at: u64,
    pub released: Option<Released>,
}

impl Remote {
    pub fn active(&self, now: u64) -> bool {
        self.active_at > 0 && now.saturating_sub(self.active_at) <= STALE_SECS
    }

    /// The other computer went silent while the session was running there: it was not stopped or handed over
    /// (a stopped session leaves `active`), its marks just stopped coming — no connection, Pult closed, the lid
    /// shut. The session may still be running there, so it isn't free: only an explicit "Continue here" takes it.
    pub fn silent(&self, now: u64) -> bool {
        self.active_at > 0 && !self.active(now) && self.released.is_none_or(|released| released.at < self.active_at)
    }
}

/// Sessions of other computers: from their snapshots in the `shared` folder (our own snapshot is skipped).
pub fn remote(dir: &std::path::Path, own_file: &str) -> BTreeMap<String, Remote> {
    #[derive(Deserialize)]
    struct Snapshot {
        #[serde(default)]
        device_name: String,
        #[serde(default)]
        presence: Presence,
    }
    let mut out: BTreeMap<String, Remote> = BTreeMap::new();
    let Ok(files) = fs::read_dir(dir) else {
        return out;
    };
    for file in files.filter_map(Result::ok) {
        if file.file_name().to_string_lossy() == own_file {
            continue;
        }
        let Some(snapshot) =
            fs::read_to_string(file.path()).ok().and_then(|text| serde_json::from_str::<Snapshot>(&text).ok())
        else {
            continue;
        };
        let presence = snapshot.presence;
        let sessions = presence.active.keys().chain(presence.takeover.keys()).chain(presence.released.keys());
        for session in sessions {
            let entry = out.entry(session.clone()).or_default();
            let active_at = presence.active.get(session).copied().unwrap_or(0);
            let takeover_at = presence.takeover.get(session).copied().unwrap_or(0);
            if active_at > entry.active_at || takeover_at > entry.takeover_at || entry.device.is_empty() {
                entry.device = snapshot.device_name.clone();
            }
            entry.active_at = entry.active_at.max(active_at);
            entry.takeover_at = entry.takeover_at.max(takeover_at);
            if let Some(released) = presence.released.get(session)
                && entry.released.is_none_or(|known| released.at > known.at)
            {
                entry.released = Some(*released);
            }
        }
    }
    out
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|time| time.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_computer_keeps_its_session() {
        let remote = |active_at: u64, released: Option<u64>| Remote {
            device: "Mac".into(),
            active_at,
            takeover_at: 0,
            released: released.map(|at| Released { at, len: 0, code: false }),
        };
        let now = 100_000;
        // Marks stopped coming while it was running: not free.
        assert!(remote(now - 3600, None).silent(now));
        assert!(!remote(now - 30, None).silent(now), "fresh marks: simply active");
        // Stopped and handed over after the last mark: free.
        assert!(!remote(now - 3600, Some(now - 3000)).silent(now));
        // Never ran there: nothing to wait for.
        assert!(!remote(0, None).silent(now));
    }

    #[test]
    fn beat_marks_running_sessions_once_a_minute() {
        let mut presence = Presence::default();
        assert!(presence.beat(&["a".into()], 1000));
        assert!(!presence.beat(&["a".into()], 1030), "does not write more often than once a minute");
        assert!(presence.beat(&["a".into()], 1061));
        assert!(presence.beat(&[], 1062), "a stopped session is dropped");
        assert!(presence.active.is_empty());
    }

    #[test]
    fn remote_reads_other_computers_only() {
        let dir = std::env::temp_dir().join(format!("pult-presence-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("projects-me.json"), r#"{"device_name": "Me", "presence": {"active": {"s": 999}}}"#).unwrap();
        fs::write(
            dir.join("projects-vm.json"),
            r#"{"device_name": "VM", "presence": {"active": {"s": 100}, "released": {"t": {"at": 50, "len": 7}}}}"#,
        )
        .unwrap();
        let remote = remote(&dir, "projects-me.json");
        assert_eq!(remote["s"].device, "VM");
        assert_eq!(remote["s"].active_at, 100);
        assert!(remote["s"].active(200) && !remote["s"].active(300));
        assert_eq!(remote["t"].released, Some(Released { at: 50, len: 7, code: false }));
        let _ = fs::remove_dir_all(&dir);
    }
}
