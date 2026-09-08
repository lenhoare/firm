use crate::config::{Allowances, Provider};
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Assignment {
    #[serde(default = "legacy_provider")]
    pub provider: String,
    pub title: String,
    pub brief: String,
    pub acceptance: Vec<String>,
    pub autonomy: String,
}

fn legacy_provider() -> String {
    "qwen".into()
}

pub fn default_manager() -> String {
    "grok".into()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {
    pub enabled: bool,
    pub max_runs: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProviderUsage {
    pub starts: Vec<u64>,
    pub cooldown_until: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Decision {
    #[serde(default = "default_manager")]
    pub manager_provider: String,
    pub action: String,
    pub summary: String,
    pub phases: Vec<String>,
    pub assignment: Option<Assignment>,
    pub observations: Vec<String>,
}

impl Decision {
    pub fn validate(&self, reviewing: bool) -> Result<()> {
        ensure!(
            ["delegate", "complete", "blocked"].contains(&self.action.as_str()),
            "Unknown manager action"
        );
        ensure!(
            self.summary.len() <= 16000 && !self.summary.trim().is_empty(),
            "Invalid summary"
        );
        ensure!(
            self.action != "complete" || reviewing,
            "Cannot complete before worker evidence exists"
        );
        if self.action == "delegate" {
            let a = self
                .assignment
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Missing assignment"))?;
            ensure!(
                !a.title.trim().is_empty()
                    && !a.brief.trim().is_empty()
                    && !a.acceptance.is_empty(),
                "Incomplete assignment"
            );
            ensure!(
                ["guided", "bounded", "exploratory"].contains(&a.autonomy.as_str()),
                "Unknown autonomy level"
            );
            ensure!(
                a.brief.len() <= 20000 && a.acceptance.len() <= 30,
                "Assignment too large"
            );
        } else {
            ensure!(self.assignment.is_none(), "Unexpected assignment");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Task {
    pub id: String,
    pub assignment: Assignment,
    pub status: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Event {
    pub time: u64,
    pub kind: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Usage {
    pub fetched_at: u64,
    pub raw: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct State {
    #[serde(default = "default_manager")]
    pub manager_provider: String,
    #[serde(default)]
    pub manager_retry_stage: Option<String>,
    #[serde(default)]
    pub side_thread_id: Option<String>,
    pub demo: bool,
    pub paused: bool,
    pub stage: String,
    pub reason: String,
    pub objective: String,
    pub thread_id: Option<String>,
    pub active_turn: Option<String>,
    pub phases: Vec<String>,
    pub tasks: Vec<Task>,
    pub decisions: Vec<Decision>,
    pub events: Vec<Event>,
    pub manager_starts: Vec<u64>,
    pub worker_starts: Vec<u64>,
    pub cooldown_until: u64,
    #[serde(default)]
    pub provider_settings: BTreeMap<String, ProviderSettings>,
    #[serde(default)]
    pub provider_descriptions: BTreeMap<String, String>,
    #[serde(default)]
    pub provider_usage: BTreeMap<String, ProviderUsage>,
    #[serde(default)]
    pub provider_account_usage: BTreeMap<String, Usage>,
    pub usage: Option<Usage>,
    pub token_usage: Option<Value>,
    pub allowances: Allowances,
    #[serde(default)]
    pub latest_snapshot: Option<String>,
    #[serde(default)]
    pub active_snapshot: Option<String>,
}

impl State {
    pub fn new(demo: bool, allowances: Allowances) -> Self {
        Self {
            manager_provider: default_manager(),
            manager_retry_stage: None,
            side_thread_id: None,
            demo,
            paused: true,
            stage: "idle".into(),
            reason: "Choose an objective, then start the experiment".into(),
            objective: String::new(),
            thread_id: None,
            active_turn: None,
            phases: vec![],
            tasks: vec![],
            decisions: vec![],
            events: vec![],
            manager_starts: vec![],
            worker_starts: vec![],
            cooldown_until: 0,
            provider_settings: BTreeMap::new(),
            provider_descriptions: BTreeMap::new(),
            provider_usage: BTreeMap::new(),
            provider_account_usage: BTreeMap::new(),
            usage: None,
            token_usage: None,
            allowances,
            latest_snapshot: None,
            active_snapshot: None,
        }
    }
    pub fn event(&mut self, kind: &str, message: impl Into<String>) {
        self.events.push(Event {
            time: now(),
            kind: kind.into(),
            message: message.into(),
        });
        if self.events.len() > 300 {
            self.events.remove(0);
        }
    }
    pub fn recent(&self, starts: &[u64], time: u64) -> usize {
        starts
            .iter()
            .filter(|&&t| time.saturating_sub(t) < self.allowances.window_seconds)
            .count()
    }
    pub fn providers(&self, configured: &[Provider]) -> Vec<Provider> {
        configured
            .iter()
            .cloned()
            .map(|mut provider| {
                if let Some(settings) = self.provider_settings.get(&provider.id) {
                    provider.enabled = settings.enabled;
                    provider.max_runs = settings.max_runs;
                }
                if let Some(description) = self.provider_descriptions.get(&provider.id) {
                    provider.description = description.clone();
                }
                provider
            })
            .collect()
    }
    pub fn provider_gate(&self, provider: &Provider, time: u64) -> Option<String> {
        if !provider.enabled {
            return Some(format!("{} is disabled", provider.name));
        }
        let usage = self
            .provider_usage
            .get(&provider.id)
            .cloned()
            .unwrap_or_default();
        if self.recent(&usage.starts, time) >= provider.max_runs {
            return Some(format!("{} run allowance exhausted", provider.name));
        }
        if time < usage.cooldown_until {
            return Some(format!(
                "{} cooldown: {}s remaining",
                provider.name,
                usage.cooldown_until - time
            ));
        }
        None
    }
    pub fn gate(&self, manager: bool, time: u64) -> Option<String> {
        self.role_gate(manager, time).or_else(|| {
            if manager && !self.demo {
                usage_gate(self.usage.as_ref(), &self.allowances, time)
            } else {
                None
            }
        })
    }
    /// Local role limits apply across provider switches. Meetings call gate(true)
    /// explicitly for their Codex speaker, independent of the workshop selection.
    pub fn role_gate(&self, manager: bool, time: u64) -> Option<String> {
        if self.paused {
            return Some("Paused — Len has control".into());
        }
        let a = &self.allowances;
        if manager {
            if self.recent(&self.manager_starts, time) >= a.manager_turns {
                return Some("Manager turn allowance exhausted".into());
            }
            if let Some(last) = self.manager_starts.last() {
                let wait = last
                    .saturating_add(a.manager_interval_seconds)
                    .saturating_sub(time);
                if wait > 0 {
                    return Some(format!("Next manager turn in {wait}s"));
                }
            }
        } else {
            if self.recent(&self.worker_starts, time) >= a.worker_runs {
                return Some("Worker run allowance exhausted".into());
            }
        }
        None
    }
    pub fn manager_gate(&self, configured: &[Provider], time: u64) -> Option<String> {
        if let Some(reason) = self.role_gate(true, time) {
            return Some(reason);
        }
        let providers = self.providers(configured);
        let Some(provider) = providers.iter().find(|p| p.id == self.manager_provider) else {
            return Some(format!(
                "Manager {} is no longer configured",
                self.manager_provider
            ));
        };
        if self.manager_provider != "codex" && provider.manager_args.is_none() {
            return Some(format!(
                "{} has no manager invocation configured",
                provider.name
            ));
        }
        self.provider_gate(provider, time).or_else(|| {
            if self.manager_provider == "codex" && !self.demo {
                usage_gate(self.usage.as_ref(), &self.allowances, time)
            } else {
                None
            }
        })
    }
}

pub fn usage_gate(usage: Option<&Usage>, a: &Allowances, time: u64) -> Option<String> {
    let Some(usage) = usage else {
        return Some("Waiting for Codex usage information".into());
    };
    if time.saturating_sub(usage.fetched_at) > a.usage_max_age_seconds {
        return Some("Codex usage information is stale".into());
    }
    let buckets: Vec<&Value> = match usage
        .raw
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
    {
        Some(b) if !b.is_empty() => b.values().collect(),
        _ => usage.raw.get("rateLimits").into_iter().collect(),
    };
    let mut windows = 0;
    for bucket in buckets {
        if bucket
            .get("rateLimitReachedType")
            .is_some_and(|v| !v.is_null())
        {
            return Some("Codex reports a reached usage limit".into());
        }
        for key in ["primary", "secondary"] {
            if let Some(window) = bucket.get(key).filter(|v| !v.is_null()) {
                let Some(percent) = window.get("usedPercent").and_then(Value::as_f64) else {
                    return Some("Unrecognised Codex usage window".into());
                };
                windows += 1;
                if percent >= a.max_used_percent {
                    return Some(format!(
                        "Codex usage {percent:.1}% reaches the {:.1}% experiment threshold",
                        a.max_used_percent
                    ));
                }
            }
        }
    }
    if windows == 0 {
        return Some("Codex returned no usable limit windows".into());
    }
    None
}

pub struct Store {
    pub(crate) conn: Connection,
}
impl Store {
    pub fn open(path: &Path, demo: bool, allowances: Allowances) -> Result<(Self, State)> {
        let conn = Connection::open(path)?;
        crate::meetings::initialize(&conn)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS state (id INTEGER PRIMARY KEY CHECK(id=1), data TEXT NOT NULL); CREATE TABLE IF NOT EXISTS journal (id INTEGER PRIMARY KEY, time INTEGER NOT NULL, data TEXT NOT NULL);")?;
        let mut state = match conn.query_row("SELECT data FROM state WHERE id=1", [], |r| {
            r.get::<_, String>(0)
        }) {
            Ok(data) => serde_json::from_str::<State>(&data)?,
            Err(rusqlite::Error::QueryReturnedNoRows) => State::new(demo, allowances),
            Err(e) => return Err(e.into()),
        };
        ensure!(
            state.demo == demo,
            "Demo and live state must use separate databases"
        );
        // Old databases only tracked Qwen. Attribute their reservations and cooldown
        // once, without replenishing either the global or provider allowance.
        if state.provider_usage.is_empty()
            && (!state.worker_starts.is_empty() || state.cooldown_until > 0)
        {
            state.provider_usage.insert(
                "qwen".into(),
                ProviderUsage {
                    starts: state.worker_starts.clone(),
                    cooldown_until: state.cooldown_until,
                },
            );
        }
        state.cooldown_until = 0;
        state.paused = true;
        if ["planning", "reviewing", "working"].contains(&state.stage.as_str()) {
            state.stage = "blocked".into();
            state.reason = "Interrupted by restart. Inspect the workspace and active Codex turn before creating another experiment; no job was retried.".into();
            for task in &mut state.tasks {
                if task.status == "running" {
                    task.status = "interrupted".into();
                }
            }
        } else {
            state.reason = "Paused on startup — resume when ready".into();
        }
        state.event(
            "controller",
            "Controller started; automatic dispatch is paused",
        );
        let mut store = Self { conn };
        store.save(&state)?;
        Ok((store, state))
    }
    pub fn save(&mut self, state: &State) -> Result<()> {
        let data = serde_json::to_string(state)?;
        self.conn.execute("INSERT INTO state(id,data) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data", [data])?;
        Ok(())
    }
    pub fn archive(&mut self, state: &State) -> Result<()> {
        if state.objective.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO journal(time,data) VALUES(?1,?2)",
            params![now(), serde_json::to_string(state)?],
        )?;
        Ok(())
    }
    pub fn history(&self) -> Result<Vec<Value>> {
        let mut query = self
            .conn
            .prepare("SELECT id,time,data FROM journal ORDER BY id DESC LIMIT 50")?;
        let rows = query.query_map([], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (id, time, data) = row?;
            let state: Value = serde_json::from_str(&data)?;
            Ok(serde_json::json!({"id":id,"time":time,"state":state}))
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn a() -> Allowances {
        crate::config::Config::read(Path::new("firm.toml"))
            .unwrap()
            .allowances
    }
    #[test]
    fn provider_limits_and_cooldowns_are_independent() {
        let config = crate::config::Config::read(Path::new("firm.toml")).unwrap();
        let qwen = config.providers.iter().find(|p| p.id == "qwen").unwrap();
        let muse = config.providers.iter().find(|p| p.id == "muse").unwrap();
        let mut s = State::new(true, a());
        s.paused = false;
        s.provider_usage.insert(
            "qwen".into(),
            ProviderUsage {
                starts: vec![100; qwen.max_runs],
                cooldown_until: 200,
            },
        );
        assert!(s.provider_gate(qwen, 101).unwrap().contains("exhausted"));
        assert!(s.provider_gate(muse, 101).is_none());
        s.provider_usage.get_mut("qwen").unwrap().starts.clear();
        assert!(s.provider_gate(qwen, 101).unwrap().contains("cooldown"));
        assert!(s.provider_gate(qwen, 201).is_none());
        s.provider_settings.insert(
            "muse".into(),
            ProviderSettings {
                enabled: false,
                max_runs: 1,
            },
        );
        assert!(
            s.provider_gate(
                s.providers(&config.providers)
                    .iter()
                    .find(|p| p.id == "muse")
                    .unwrap(),
                101
            )
            .unwrap()
            .contains("disabled")
        );
        s.worker_starts = vec![100; 3];
        assert!(s.gate(false, 101).unwrap().contains("exhausted"));
    }
    #[test]
    fn legacy_database_migration_retains_qwen_usage_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (db, s) = Store::open(&path, true, a()).unwrap();
        let mut value = serde_json::to_value(s).unwrap();
        value.as_object_mut().unwrap().remove("provider_usage");
        value.as_object_mut().unwrap().remove("provider_settings");
        value["worker_starts"] = serde_json::json!([100, 101]);
        value["cooldown_until"] = serde_json::json!(300);
        db.conn
            .execute("UPDATE state SET data=?1 WHERE id=1", [value.to_string()])
            .unwrap();
        drop(db);
        for _ in 0..2 {
            let (_, state) = Store::open(&path, true, a()).unwrap();
            assert!(state.paused);
            assert_eq!(state.worker_starts, [100, 101]);
            assert_eq!(state.provider_usage["qwen"].starts, [100, 101]);
            assert_eq!(state.provider_usage["qwen"].cooldown_until, 300);
            assert_eq!(state.cooldown_until, 0);
        }
        let old_assignment: Assignment = serde_json::from_value(serde_json::json!({"title":"Old", "brief":"Old", "acceptance":["Old"], "autonomy":"bounded"})).unwrap();
        assert_eq!(old_assignment.provider, "qwen");
    }
    #[test]
    fn limits_are_rolling_and_pause_is_authoritative() {
        let mut s = State::new(true, a());
        assert!(s.gate(true, 100).is_some());
        s.paused = false;
        s.manager_starts = vec![100; 4];
        assert!(s.gate(true, 500).unwrap().contains("exhausted"));
        assert!(s.gate(true, 18100).is_none());
        s.worker_starts = vec![100; 3];
        assert!(s.gate(false, 101).is_some());
    }
    #[test]
    fn telemetry_fails_closed_and_checks_every_bucket() {
        let a = a();
        assert!(usage_gate(None, &a, 100).is_some());
        let mut u = Usage {
            fetched_at: 100,
            raw: serde_json::json!({"rateLimitsByLimitId":{"a":{"primary":{"usedPercent":2}},"b":{"secondary":{"usedPercent":80}}}}),
        };
        assert!(usage_gate(Some(&u), &a, 100).unwrap().contains("80"));
        u.raw = serde_json::json!({"rateLimits":{"primary":{"usedPercent":2}}});
        assert!(usage_gate(Some(&u), &a, 100).is_none());
        assert!(usage_gate(Some(&u), &a, 1000).unwrap().contains("stale"));
        u.raw = serde_json::json!({});
        assert!(usage_gate(Some(&u), &a, 100).is_some());
    }
    #[test]
    fn restart_blocks_uncertain_dispatch_and_preserves_allowance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (mut db, mut s) = Store::open(&path, true, a()).unwrap();
        s.stage = "planning".into();
        s.paused = false;
        s.manager_starts.push(100);
        db.save(&s).unwrap();
        drop(db);
        let (_, s) = Store::open(&path, true, a()).unwrap();
        assert!(s.paused);
        assert_eq!(s.stage, "blocked");
        assert_eq!(s.manager_starts, [100]);
    }
}
