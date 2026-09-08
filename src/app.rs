use crate::{
    codex::{Codex, decision_schema},
    config::{Allowances, Config, Provider},
    snapshots::{Archive, Capture},
    state::{Assignment, Decision, ProviderSettings, State, Store, Task, Usage, now},
    worker,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, watch};

pub struct Core {
    pub meeting_active: Option<String>,
    pub state: State,
    pub store: Store,
    pub active: bool,
}
impl Core {
    pub fn save(&mut self) -> Result<()> {
        if let Err(error) = self.store.save(&self.state) {
            self.state.paused = true;
            self.state.reason = format!("Persistence failed; scheduling paused: {error}");
            return Err(error);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct App {
    pub config: Config,
    pub core: Arc<Mutex<Core>>,
    pub rpc: Option<Codex>,
    pub(crate) cancel: watch::Sender<u64>,
}

impl App {
    pub fn new(config: Config, store: Store, state: State, rpc: Option<Codex>) -> Self {
        let (cancel, _) = watch::channel(0);
        Self {
            config,
            core: Arc::new(Mutex::new(Core {
                meeting_active: None,
                state,
                store,
                active: false,
            })),
            rpc,
            cancel,
        }
    }
    pub async fn snapshot(&self) -> Value {
        let core = self.core.lock().await;
        json!({"state":core.state,"providers":self.provider_roster(&core.state),"codex_connected":self.rpc.is_some(),"active":core.active,"workspace":self.config.workspace,"codex_url":self.config.codex_url,"codex_model":self.config.codex_model,"now":now()})
    }
    pub async fn select_manager(&self, id: &str) -> Result<()> {
        let mut core = self.core.lock().await;
        ensure!(
            core.state.paused && !core.active && core.state.active_turn.is_none(),
            "Pause and wait for active work; reconcile any uncertain Codex turn before changing manager"
        );
        let roster = self.provider_roster(&core.state);
        let manager = roster
            .iter()
            .find(|p| p["id"] == id)
            .context("Unknown manager provider")?;
        ensure!(
            manager["enabled"] == true && manager["manager_configured"] == true,
            "Enable this provider and configure manager_args before selecting it"
        );
        if core.state.manager_provider == id {
            return Ok(());
        }
        let previous = std::mem::replace(&mut core.state.manager_provider, id.into());
        if core.state.stage == "blocked"
            && let Some(stage) = core.state.manager_retry_stage.clone()
        {
            core.state.stage = stage;
        }
        core.state.reason = format!("Manager changed to {id}; paused until you press Start");
        core.state.event("manager-selection", format!("Len changed manager from {previous} to {id}; plan, evidence and all counters retained"));
        core.save()
    }
    fn provider_roster(&self, state: &State) -> Vec<Value> {
        let time = now();
        state.providers(&self.config.providers).iter().map(|p| {
            let usage = state.provider_usage.get(&p.id).cloned().unwrap_or_default();
            let used = state.recent(&usage.starts, time);
            let reason = self.worker_provider_gate(state, p, time).or_else(|| {
                (state.recent(&state.worker_starts, time) >= state.allowances.worker_runs)
                    .then(|| "Overall worker run allowance exhausted".into())
            });
            json!({"id":p.id,"name":p.name,"description":p.description,"enabled":p.enabled,
                "manager_configured":p.id == "codex" || p.manager_args.is_some(),
                "max_runs":p.max_runs,"used_runs":used,"remaining_runs":p.max_runs.saturating_sub(used),
                "cooldown_until":usage.cooldown_until,"available":reason.is_none(),"blocked_reason":reason,
                "command":p.command,"args":p.args,"input":p.input})
        }).collect()
    }
    fn worker_provider_gate(
        &self,
        state: &State,
        provider: &Provider,
        time: u64,
    ) -> Option<String> {
        state.provider_gate(provider, time).or_else(|| {
            if provider.id == "codex" && !state.demo {
                crate::state::usage_gate(state.usage.as_ref(), &state.allowances, time)
            } else {
                None
            }
        })
    }
    fn dispatch_gate(&self, state: &State, stage: &str) -> Option<String> {
        let time = now();
        let gate = if stage == "ready_worker" {
            state.gate(false, time)
        } else {
            state.manager_gate(&self.config.providers, time)
        };
        if let Some(reason) = gate {
            return Some(reason);
        }
        if stage != "ready_worker"
            && state.manager_provider == "codex"
            && !state.demo
            && self.rpc.is_none()
        {
            return Some("Codex is disconnected; select another manager or restart with app-server available".into());
        }
        if stage == "ready_worker" {
            let Some(task) = state.tasks.last() else {
                return Some("No queued assignment".into());
            };
            let providers = state.providers(&self.config.providers);
            let Some(provider) = providers.iter().find(|p| p.id == task.assignment.provider) else {
                return Some(format!(
                    "Queued provider {} is no longer configured",
                    task.assignment.provider
                ));
            };
            return self.worker_provider_gate(state, provider, time);
        }
        if stage == "ready_plan"
            && !self
                .provider_roster(state)
                .iter()
                .any(|p| p["available"] == true)
        {
            return Some(
                "No worker provider available; enable a provider or wait for its allowance".into(),
            );
        }
        None
    }
    #[cfg(test)]
    pub async fn provider_settings(&self, id: &str, settings: ProviderSettings) -> Result<()> {
        self.provider_update(id, settings, None).await
    }
    pub async fn provider_update(
        &self,
        id: &str,
        settings: ProviderSettings,
        description: Option<String>,
    ) -> Result<()> {
        ensure!(
            self.config.providers.iter().any(|p| p.id == id),
            "Unknown provider: {id}"
        );
        if let Some(description) = &description {
            ensure!(description.len() <= 4000, "Role description is too long");
        }
        let mut core = self.core.lock().await;
        ensure!(
            core.state.paused && !core.active && core.state.active_turn.is_none(),
            "Take control and wait for active work before changing providers"
        );
        core.state.event(
            "provider",
            format!(
                "Len updated {id}: enabled={}, max runs={}{}; counters retained",
                settings.enabled,
                settings.max_runs,
                if description.is_some() {
                    ", role edited"
                } else {
                    ""
                }
            ),
        );
        core.state.provider_settings.insert(id.into(), settings);
        if let Some(description) = description {
            core.state
                .provider_descriptions
                .insert(id.into(), description);
        }
        core.save()
    }
    pub async fn archive_root(&self) -> std::path::PathBuf {
        let demo = self.core.lock().await.state.demo;
        self.config.state_dir.join(if demo {
            "snapshots-demo"
        } else {
            "snapshots-live"
        })
    }
    pub async fn archive_action<T: Send + 'static>(
        &self,
        action: impl FnOnce(&mut Archive) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let root = self.archive_root().await;
        tokio::task::spawn_blocking(move || action(&mut Archive::open(&root)?)).await?
    }
    pub async fn capture_manual(&self, label: String) -> Result<String> {
        {
            let mut core = self.core.lock().await;
            ensure!(
                core.state.paused && !core.active && core.state.active_turn.is_none(),
                "Take control and wait for active work before capturing a snapshot"
            );
            // Hold a capture lease so objective/config edits cannot race with context collection.
            core.active = true;
        }
        let result = self
            .capture_record("manual", &label, json!({"source":"manual checkpoint"}))
            .await;
        self.core.lock().await.active = false;
        result
    }
    pub(crate) async fn capture_record(
        &self,
        kind: &str,
        label: &str,
        request: Value,
    ) -> Result<String> {
        ensure!(label.len() <= 1024, "Snapshot label too long");
        let state = self.core.lock().await.state.clone();
        let mut config = self.config.clone();
        config.allowances = state.allowances.clone();
        config.providers = state.providers(&config.providers);
        let parent = if kind == "result" {
            state
                .active_snapshot
                .clone()
                .or(state.latest_snapshot.clone())
        } else {
            state.latest_snapshot.clone()
        };
        let transcript = if let (Some(rpc), Some(thread)) = (
            &self.rpc,
            state.side_thread_id.as_ref().or(state.thread_id.as_ref()),
        ) {
            rpc.call(
                "thread/read",
                json!({"threadId":thread,"includeTurns":true}),
            )
            .await
            .ok()
        } else {
            None
        };
        // Do not launch arbitrary custom worker commands merely to inspect versions:
        // a wrapper may ignore --version and start inference. Command/args are captured.
        let mut versions = json!({"firm":env!("CARGO_PKG_VERSION"),"codex":"not inspected in demo","workers":"not probed automatically; configured commands and arguments recorded in recipe"});
        if !state.demo && !cfg!(test) {
            for (name, command) in [("codex", "codex")] {
                let output = tokio::time::timeout(
                    Duration::from_secs(3),
                    tokio::process::Command::new(command)
                        .arg("--version")
                        .kill_on_drop(true)
                        .output(),
                )
                .await;
                versions[name] = match output {
                    Ok(Ok(output)) if output.status.success() && output.stdout.len() < 4096 => {
                        json!(String::from_utf8_lossy(&output.stdout).trim())
                    }
                    _ => json!("unavailable"),
                };
            }
        }
        let kind_owned = kind.to_string();
        let label_owned = label.to_string();
        let id = self
            .archive_action(move |archive| {
                archive.capture(Capture {
                    config: &config,
                    state: &state,
                    kind: &kind_owned,
                    label: &label_owned,
                    parent,
                    request,
                    transcript,
                    versions,
                })
            })
            .await?;
        let mut core = self.core.lock().await;
        core.state.latest_snapshot = Some(id.clone());
        if kind == "input" {
            core.state.active_snapshot = Some(id.clone());
        }
        if kind == "result" {
            core.state.active_snapshot = None;
        }
        core.state
            .event("snapshot", format!("Saved {kind} snapshot {}", &id[..12]));
        core.save()?;
        Ok(id)
    }
    pub async fn new_experiment(&self, objective: String) -> Result<()> {
        ensure!(
            !objective.trim().is_empty() && objective.len() <= 12000,
            "Objective must contain 1–12000 bytes"
        );
        let mut core = self.core.lock().await;
        ensure!(
            !core.active && core.state.paused,
            "Take control and wait for active work to finish first"
        );
        ensure!(
            core.state.active_turn.is_none(),
            "A Codex turn needs reconciliation first"
        );
        ensure!(
            ["idle", "complete", "blocked", "cancelled"].contains(&core.state.stage.as_str()),
            "Finish or stop the existing experiment first"
        );
        let old = core.state.clone();
        core.store.archive(&old)?;
        let mut state = State::new(old.demo, old.allowances);
        state.manager_provider = old.manager_provider;
        // Starting another experiment never replenishes a rolling allowance.
        state.manager_starts = old.manager_starts;
        state.worker_starts = old.worker_starts;
        state.provider_settings = old.provider_settings;
        state.provider_descriptions = old.provider_descriptions;
        state.provider_usage = old.provider_usage;
        state.provider_account_usage = old.provider_account_usage;
        state.cooldown_until = old.cooldown_until;
        state.usage = old.usage;
        state.objective = objective;
        state.stage = "ready_plan".into();
        state.reason = "Objective saved — press Start to allow background work".into();
        state.event("objective", state.objective.clone());
        core.state = state;
        core.save()
    }
    pub async fn resume(&self) -> Result<()> {
        let mut core = self.core.lock().await;
        ensure!(
            core.meeting_active.is_none() && core.state.active_turn.is_none(),
            "Wait for the meeting or reconcile the active Codex turn first"
        );
        ensure!(
            [
                "ready_plan",
                "ready_worker",
                "ready_review",
                "planning",
                "working",
                "reviewing"
            ]
            .contains(&core.state.stage.as_str()),
            "Save a new objective before starting; blocked experiments need inspection"
        );
        core.state.paused = false;
        core.state.reason = "Background scheduling enabled".into();
        core.state
            .event("control", "Len enabled background scheduling");
        core.save()
    }
    pub async fn pause(&self) -> Result<()> {
        let mut core = self.core.lock().await;
        core.state.paused = true;
        core.state.reason =
            "Len has control; active work may finish, no new work will start".into();
        core.state
            .event("control", "Len took control — scheduling paused");
        core.save()
    }
    pub async fn stop(&self) -> Result<()> {
        let mut core = self.core.lock().await;
        if core.meeting_active.is_some() {
            self.cancel.send_modify(|v| *v += 1);
            core.state
                .event("meeting", "Meeting cancellation requested");
            return core.save();
        }
        core.state.paused = true;
        core.state.reason = if core.active {
            "Cancellation requested; waiting for confirmation"
        } else {
            "Experiment stopped"
        }
        .into();
        core.state.stage = "cancelled".into();
        core.state.event("control", "Stop requested");
        core.save()?;
        self.cancel.send_modify(|v| *v += 1);
        Ok(())
    }
    pub async fn allowances(&self, allowances: Allowances) -> Result<()> {
        allowances.validate()?;
        let mut core = self.core.lock().await;
        ensure!(
            core.state.paused && !core.active,
            "Take control and wait for active work before changing allowances"
        );
        core.state.allowances = allowances;
        core.state.event(
            "allowance",
            "Len updated experiment allowances; usage counters retained",
        );
        core.save()
    }
    pub async fn reconcile(&self) -> Result<()> {
        let rpc = self
            .rpc
            .as_ref()
            .context("Reconciliation is only needed in live mode")?;
        let thread = {
            let core = self.core.lock().await;
            ensure!(
                core.state.paused && !core.active,
                "Take control and wait for active work"
            );
            core.state
                .side_thread_id
                .clone()
                .or_else(|| core.state.thread_id.clone())
                .context("No Codex thread")?
        };
        rpc.require_idle(&thread).await?;
        let mut core = self.core.lock().await;
        core.state.active_turn = None;
        core.state.side_thread_id = None;
        core.state.event(
            "recovery",
            "Codex confirms manager thread is idle; no automatic retry performed",
        );
        core.save()
    }
    pub(crate) async fn refresh_usage(&self) {
        let Some(rpc) = &self.rpc else {
            return;
        };
        if let Ok(raw) = rpc.call("account/rateLimits/read", json!({})).await {
            let mut core = self.core.lock().await;
            core.state.usage = Some(Usage {
                fetched_at: now(),
                raw,
            });
            if let Err(error) = core.save() {
                core.state.paused = true;
                core.state.reason = format!("Persistence failed: {error}");
            }
        }
    }
    pub async fn schedule(self) {
        loop {
            let work = {
                let mut core = self.core.lock().await;
                let stage = core.state.stage.clone();
                if !["ready_plan", "ready_worker", "ready_review"].contains(&stage.as_str())
                    || core.active
                    || core.state.paused
                {
                    None
                } else if let Some(reason) = self.dispatch_gate(&core.state, &stage) {
                    core.state.reason = reason;
                    None
                } else {
                    let manager = stage != "ready_worker";
                    if core.state.tasks.len() >= 30 {
                        core.state.paused = true;
                        core.state.reason =
                            "Experiment task cap reached; review the work with Len".into();
                        None
                    } else {
                        let time = now();
                        // Reserve dispatch durably before any external operation, including failed attempts.
                        if manager {
                            core.state.manager_starts.push(time);
                            core.state.manager_retry_stage = Some(stage.clone());
                            let id = core.state.manager_provider.clone();
                            core.state
                                .provider_usage
                                .entry(id)
                                .or_default()
                                .starts
                                .push(time);
                        } else {
                            core.state.worker_starts.push(time);
                            let provider =
                                core.state.tasks.last().unwrap().assignment.provider.clone();
                            core.state
                                .provider_usage
                                .entry(provider)
                                .or_default()
                                .starts
                                .push(time);
                            if let Some(task) = core.state.tasks.last_mut() {
                                task.status = "running".into();
                                task.started_at = Some(time);
                            }
                        }
                        core.state.stage = match stage.as_str() {
                            "ready_plan" => "planning",
                            "ready_review" => "reviewing",
                            _ => "working",
                        }
                        .into();
                        core.state.reason = if manager {
                            format!("{} manager is thinking", core.state.manager_provider)
                        } else {
                            format!(
                                "{} is working",
                                core.state.tasks.last().unwrap().assignment.provider
                            )
                        };
                        let actor = if manager {
                            format!("{} manager", core.state.manager_provider)
                        } else {
                            core.state.tasks.last().unwrap().assignment.provider.clone()
                        };
                        core.state
                            .event("dispatch", format!("Reserved {} run", actor));
                        match core.save() {
                            Ok(()) => {
                                core.active = true;
                                Some((stage, self.cancel.subscribe()))
                            }
                            Err(error) => {
                                core.state.paused = true;
                                core.state.reason =
                                    format!("Persistence failed; dispatch prevented: {error}");
                                None
                            }
                        }
                    }
                }
            };
            if let Some((stage, cancel)) = work {
                let result = if stage == "ready_worker" {
                    self.worker_turn(cancel).await
                } else {
                    self.manager_turn(stage == "ready_review", cancel).await
                };
                let mut core = self.core.lock().await;
                if let Err(error) = result {
                    let cancelled = core.state.stage == "cancelled";
                    core.state.paused = true;
                    if !cancelled {
                        core.state.stage = "blocked".into();
                    }
                    core.state.reason = error.to_string();
                    if let Some(task) = core.state.tasks.last_mut()
                        && task.status == "running"
                    {
                        task.status = if cancelled { "cancelled" } else { "failed" }.into();
                        task.finished_at = Some(now());
                    }
                    core.state.event("error", error.to_string());
                }
                if let Err(error) = core.save() {
                    core.state.paused = true;
                    core.state.reason = format!("Persistence failed: {error}");
                }
                drop(core);
                if let Err(error) = self
                    .capture_record(
                        "result",
                        "Run outcome",
                        json!({"source":"controller result"}),
                    )
                    .await
                {
                    let mut core = self.core.lock().await;
                    core.state.paused = true;
                    core.state.reason = format!("Result snapshot failed: {error}");
                    core.state
                        .event("error", format!("Result snapshot failed: {error}"));
                    let _ = core.save();
                }
                self.core.lock().await.active = false;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    async fn worker_turn(&self, cancel: watch::Receiver<u64>) -> Result<()> {
        let (assignment, demo, objective, allowances, providers) = {
            let core = self.core.lock().await;
            (
                core.state
                    .tasks
                    .last()
                    .context("No queued assignment")?
                    .assignment
                    .clone(),
                core.state.demo,
                core.state.objective.clone(),
                core.state.allowances.clone(),
                core.state.providers(&self.config.providers),
            )
        };
        ensure!(!cancel.has_changed()?, "Cancelled before worker dispatch");
        let mut dispatch_config = self.config.clone();
        dispatch_config.allowances = allowances.clone();
        let provider = providers
            .iter()
            .find(|p| p.id == assignment.provider)
            .context("Assigned provider is no longer configured")?;
        let prepared = worker::prepare(&dispatch_config, provider, &assignment)?;
        self.capture_record(
            "input",
            &format!("{} worker input", provider.name),
            prepared.request(&dispatch_config, demo),
        )
        .await?;
        ensure!(!cancel.has_changed()?, "Cancelled during worker snapshot");
        let mut result = if demo {
            worker::demo(cancel.clone(), objective.contains("[fail]")).await?
        } else {
            worker::run(&dispatch_config, &prepared, cancel.clone()).await?
        };
        if !demo && result.interruption.is_none() && !self.config.verify_command.is_empty() {
            // Persist the worker output before running independent checks.
            {
                let mut core = self.core.lock().await;
                if let Some(task) = core.state.tasks.last_mut() {
                    task.output = result.output.clone();
                    task.exit_code = result.exit_code;
                }
                core.state.event(
                    "verification",
                    "Running configured checks in the worker workspace",
                );
                core.save()?;
            }
            let check = worker::verify(&self.config, cancel).await?;
            result.output.push_str(&format!("\n\n{}", check.output));
            if check.exit_code != Some(0) {
                result.exit_code = check.exit_code;
            }
            result.interruption = check.interruption;
        }
        let mut core = self.core.lock().await;
        let cancelled = core.state.stage == "cancelled";
        let limited = worker::looks_rate_limited(&result.output);
        let task = core.state.tasks.last_mut().context("Missing task")?;
        task.output = result.output;
        task.exit_code = result.exit_code;
        task.finished_at = Some(now());
        task.status = if cancelled {
            "cancelled"
        } else if result.exit_code == Some(0) {
            "ready_review"
        } else {
            "failed"
        }
        .into();
        if limited {
            let until = now().saturating_add(core.state.allowances.provider_cooldown_seconds);
            core.state
                .provider_usage
                .entry(assignment.provider.clone())
                .or_default()
                .cooldown_until = until;
            core.state.event(
                "cooldown",
                format!(
                    "{} reported a possible usage limit; its local cooldown is active",
                    assignment.provider
                ),
            );
        }
        core.state.event(
            "worker",
            format!(
                "{} exited with {:?}; evidence retained for review",
                assignment.provider, result.exit_code
            ),
        );
        if !cancelled {
            core.state.stage = "ready_review".into();
            core.state.reason = "Worker result awaits manager review".into();
        }
        core.save()?;
        if let Some(reason) = result.interruption {
            bail!(reason);
        }
        Ok(())
    }
    async fn manager_turn(&self, reviewing: bool, mut cancel: watch::Receiver<u64>) -> Result<()> {
        let snapshot = self.core.lock().await.state.clone();
        let mut decision = if snapshot.demo {
            self.capture_record(
                "input",
                "Demo manager input",
                json!({"demo":true,"manager_provider":snapshot.manager_provider,"reviewing":reviewing,"prompt":self.manager_prompt(&snapshot, reviewing)?}),
            )
            .await?;
            tokio::select! { _ = cancel.changed() => bail!("Demo manager cancelled"), _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
            let mut decision = demo_decision(&snapshot, reviewing);
            if let Some(assignment) = &mut decision.assignment {
                assignment.provider = self
                    .provider_roster(&snapshot)
                    .iter()
                    .find(|p| p["available"] == true)
                    .and_then(|p| p["id"].as_str())
                    .context("No available demo worker")?
                    .into();
            }
            decision
        } else if snapshot.manager_provider != "codex" {
            self.cli_manager(&snapshot, reviewing, cancel).await?
        } else {
            self.live_manager(&snapshot, reviewing, cancel).await?
        };
        decision.manager_provider = snapshot.manager_provider.clone();
        decision.validate(reviewing)?;
        let mut core = self.core.lock().await;
        if core.state.stage == "cancelled" {
            return Ok(());
        }
        if let Some(assignment) = &decision.assignment {
            let providers = core.state.providers(&self.config.providers);
            let provider = providers
                .iter()
                .find(|p| p.id == assignment.provider)
                .with_context(|| {
                    format!("Manager selected unknown provider: {}", assignment.provider)
                })?;
            if let Some(reason) = self.worker_provider_gate(&core.state, provider, now()) {
                bail!("Manager selected unavailable provider: {reason}");
            }
        }
        core.state.phases = decision.phases.clone();
        core.state.event("manager", decision.summary.clone());
        for observation in &decision.observations {
            core.state.event("observation", observation.clone());
        }
        if reviewing && let Some(task) = core.state.tasks.last_mut() {
            task.status = if decision.action == "complete" {
                "accepted"
            } else {
                "reviewed"
            }
            .into();
        }
        match decision.action.as_str() {
            "delegate" => {
                core.state.tasks.push(Task {
                    id: uuid::Uuid::new_v4().to_string(),
                    assignment: decision.assignment.clone().unwrap(),
                    status: "queued".into(),
                    output: String::new(),
                    exit_code: None,
                    started_at: None,
                    finished_at: None,
                });
                core.state.stage = "ready_worker".into();
                core.state.reason = format!(
                    "Assignment queued for {}",
                    decision.assignment.as_ref().unwrap().provider
                );
            }
            "complete" => {
                core.state.stage = "complete".into();
                core.state.paused = true;
                core.state.reason = decision.summary.clone();
            }
            _ => {
                core.state.stage = "blocked".into();
                core.state.paused = true;
                core.state.reason = decision.summary.clone();
            }
        }
        core.state.decisions.push(decision);
        core.state.manager_retry_stage = None;
        core.save()
    }
    fn manager_prompt(&self, snapshot: &State, reviewing: bool) -> Result<String> {
        let latest = if reviewing {
            let mut task = snapshot.tasks.last().context("No worker evidence")?.clone();
            task.output = crate::meetings::clip(&task.output, 32 * 1024);
            Some(task)
        } else {
            None
        };
        let decisions: Vec<_> = snapshot.decisions.iter().rev().take(5).map(|d| json!({"manager":d.manager_provider,"action":d.action,"summary":crate::meetings::clip(&d.summary, 2000),"observations":d.observations.iter().take(8).map(|o|crate::meetings::clip(o, 500)).collect::<Vec<_>>()})).collect();
        let prompt = format!(
            "Selected manager: {}. {}\nObjective: {}\nWorkspace: {}\nCurrent phases: {}\nRecent decisions (newest first, excerpts): {}\nLatest task and worker evidence (long output explicitly excerpted): {}\nCurrent worker roster (local limits, not provider-reported credits; run only available providers):\n{}\nA provider change does not reset the plan. Assess evidence independently; do not claim tests or implementation that did not happen. Return blocked if evidence is insufficient.",
            snapshot.manager_provider,
            if reviewing {
                "Review the latest worker result against its acceptance criteria, then return the next decision."
            } else {
                "Plan one bounded assignment, with broad phases and detailed acceptance criteria."
            },
            snapshot.objective,
            self.config.workspace.display(),
            serde_json::to_string(&snapshot.phases)?,
            serde_json::to_string(&decisions)?,
            serde_json::to_string(&latest)?,
            serde_json::to_string(&self.provider_roster(snapshot))?
        );
        ensure!(
            prompt.len() <= 128 * 1024,
            "Manager handoff context exceeds 128 KiB; inspect and reduce the plan before continuing"
        );
        Ok(prompt)
    }
    async fn cli_manager(
        &self,
        snapshot: &State,
        reviewing: bool,
        cancel: watch::Receiver<u64>,
    ) -> Result<Decision> {
        let mut config = self.config.clone();
        config.allowances = snapshot.allowances.clone();
        config.allowances.worker_timeout_seconds = snapshot.allowances.max_turn_seconds;
        config.verify_command.clear(); // Managers never run implementation verification.
        let mut provider = snapshot
            .providers(&config.providers)
            .into_iter()
            .find(|p| p.id == snapshot.manager_provider)
            .context("Manager provider missing")?;
        provider.args = provider
            .manager_args
            .clone()
            .context("Manager invocation is not configured")?;
        let prompt = format!(
            "{}\n{}\nReturn exactly one JSON object matching this schema, without explanation or Markdown. Do not implement, edit files, execute commands or call tools: assess the supplied evidence.\n{}",
            include_str!("../prompts/manager.md"),
            self.manager_prompt(snapshot, reviewing)?,
            decision_schema()
        );
        let prepared = worker::prepare_prompt(&config, &provider, prompt)?;
        self.capture_record("input", "CLI manager input", json!({"role":"manager","manager_provider":snapshot.manager_provider,"reviewing":reviewing,"request":prepared.request(&config,false)})).await?;
        let result = worker::run(&config, &prepared, cancel).await?;
        // Retain raw evidence even when the CLI exits unsuccessfully or returns malformed JSON.
        self.capture_record("result", "CLI manager response", json!({"role":"manager","manager_provider":snapshot.manager_provider,"output":result.output,"exit_code":result.exit_code,"interruption":result.interruption})).await?;
        if worker::looks_rate_limited(&result.output) {
            let mut core = self.core.lock().await;
            let until = now().saturating_add(snapshot.allowances.provider_cooldown_seconds);
            core.state
                .provider_usage
                .entry(snapshot.manager_provider.clone())
                .or_default()
                .cooldown_until = until;
            core.save()?;
        }
        if let Some(reason) = result.interruption {
            bail!(reason);
        }
        ensure!(
            result.exit_code == Some(0),
            "{} manager exited {:?}; response retained in snapshots",
            snapshot.manager_provider,
            result.exit_code
        );
        parse_decision(
            result
                .output
                .split("\n\nWorker stderr:\n")
                .next()
                .unwrap_or(""),
        )
    }
    async fn live_manager(
        &self,
        snapshot: &State,
        reviewing: bool,
        cancel: watch::Receiver<u64>,
    ) -> Result<Decision> {
        let rpc = self.rpc.as_ref().context("Codex connection missing")?;
        let thread = if let Some(thread) = &snapshot.thread_id {
            rpc.require_idle(thread).await?;
            rpc.call(
                "thread/resume",
                json!({"threadId":thread,"excludeTurns":true,"developerInstructions":include_str!("../prompts/manager.md")}),
            )
            .await?;
            thread.clone()
        } else {
            let response = rpc.call("thread/start", json!({"model":self.config.codex_model,"cwd":self.config.workspace,"approvalPolicy":"never","sandbox":"read-only","developerInstructions":include_str!("../prompts/manager.md")})).await?;
            let id = response
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .context("Missing thread ID")?
                .to_string();
            let mut core = self.core.lock().await;
            core.state.thread_id = Some(id.clone());
            core.state
                .event("thread", format!("Manager thread created: {id}"));
            core.save()?;
            id
        };
        // Re-check cancellation immediately before requesting inference.
        ensure!(!cancel.has_changed()?, "Cancelled before manager turn");
        let prompt = self.manager_prompt(snapshot, reviewing)?;
        let params = json!({"threadId":thread,"input":[{"type":"text","text":prompt}],"model":self.config.codex_model,"approvalPolicy":"never","sandboxPolicy":{"type":"readOnly"},"outputSchema":decision_schema()});
        let final_text = self.codex_turn(snapshot, thread, params, cancel).await?;
        parse_decision(&final_text)
    }
    pub(crate) async fn codex_turn(
        &self,
        snapshot: &State,
        thread: String,
        params: Value,
        mut cancel: watch::Receiver<u64>,
    ) -> Result<String> {
        let rpc = self.rpc.as_ref().context("Codex connection missing")?;
        self.capture_record(
            "input",
            "Manager input",
            json!({"method":"turn/start","params":params}),
        )
        .await?;
        ensure!(!cancel.has_changed()?, "Cancelled during manager snapshot");
        let mut events = rpc.events.subscribe();
        {
            let mut core = self.core.lock().await;
            core.state.active_turn = Some("unknown — turn/start not confirmed".into());
            core.save()?;
        }
        let response = rpc.call("turn/start", params).await;
        let response = match response {
            Ok(v) => v,
            Err(error) => {
                let mut core = self.core.lock().await;
                core.state.active_turn = Some("unknown — turn/start not confirmed".into());
                core.save()?;
                return Err(error);
            }
        };
        let turn = response
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .context("Missing turn ID")?
            .to_string();
        {
            let mut core = self.core.lock().await;
            core.state.active_turn = Some(turn.clone());
            core.save()?;
        }
        let deadline =
            tokio::time::sleep(Duration::from_secs(snapshot.allowances.max_turn_seconds));
        tokio::pin!(deadline);
        let mut final_text = String::new();
        loop {
            let event = tokio::select! {
                _ = cancel.changed() => { self.interrupt(rpc, &thread, &turn, &mut events).await?; bail!("Manager cancellation confirmed"); }
                _ = &mut deadline => { self.interrupt(rpc, &thread, &turn, &mut events).await?; bail!("Manager time limit reached; interruption confirmed"); }
                event = events.recv() => event.context("Lost Codex event stream; inspect manager before retrying")?
            };
            let method = event["method"].as_str().unwrap_or("");
            if method == "firm/disconnected" {
                bail!("Codex disconnected; turn state requires reconciliation");
            }
            if method == "account/rateLimits/updated" {
                let mut core = self.core.lock().await;
                core.state.usage = Some(Usage {
                    fetched_at: now(),
                    raw: event["params"].clone(),
                });
                core.save()?;
                continue;
            }
            if event.pointer("/params/threadId").and_then(Value::as_str) != Some(&thread) {
                continue;
            }
            let event_turn = event
                .pointer("/params/turnId")
                .or_else(|| event.pointer("/params/turn/id"))
                .and_then(Value::as_str);
            if event_turn.is_some_and(|id| id != turn) {
                let mut core = self.core.lock().await;
                core.state.paused = true;
                core.state.event(
                    "control",
                    "Another turn observed in manager thread; automatic scheduling paused",
                );
                core.save()?;
                continue;
            }
            if method == "thread/tokenUsage/updated" {
                let mut core = self.core.lock().await;
                core.state.token_usage = Some(event["params"].clone());
                core.save()?;
            }
            if method == "item/completed" {
                let item = &event["params"]["item"];
                if item["type"] == "agentMessage" && item["phase"] != "commentary" {
                    final_text = item["text"].as_str().unwrap_or("").to_string();
                }
                let kind = item["type"].as_str().unwrap_or("item");
                if ["agentMessage", "commandExecution", "fileChange"].contains(&kind) {
                    let mut core = self.core.lock().await;
                    core.state.event("codex", format!("Completed {kind}"));
                    core.save()?;
                }
            }
            if method == "turn/completed" {
                {
                    let mut core = self.core.lock().await;
                    core.state.active_turn = None;
                    core.save()?;
                }
                ensure!(
                    event["params"]["turn"]["status"] == "completed",
                    "Codex turn did not complete: {}",
                    event["params"]["turn"]["error"]
                );
                return Ok(final_text);
            }
        }
    }
    async fn interrupt(
        &self,
        rpc: &Codex,
        thread: &str,
        turn: &str,
        events: &mut tokio::sync::broadcast::Receiver<Value>,
    ) -> Result<()> {
        rpc.call("turn/interrupt", json!({"threadId":thread,"turnId":turn}))
            .await
            .context("Cancellation unconfirmed; inspect Codex manually")?;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let event = events.recv().await?;
                if event["method"] == "turn/completed"
                    && event.pointer("/params/turn/id").and_then(Value::as_str) == Some(turn)
                {
                    let mut core = self.core.lock().await;
                    core.state.active_turn = None;
                    core.save()?;
                    return Ok::<(), anyhow::Error>(());
                }
            }
        })
        .await
        .context("Cancellation requested but not confirmed; inspect Codex manually")??;
        Ok(())
    }
}

fn parse_decision(text: &str) -> Result<Decision> {
    let text = text.trim();
    let text = if let Some(inner) = text
        .strip_prefix("```json\n")
        .or_else(|| text.strip_prefix("```\n"))
    {
        inner
            .strip_suffix("```")
            .context("Unclosed manager JSON fence")?
            .trim()
    } else {
        text
    };
    let value: Value = serde_json::from_str(text)
        .context("Manager returned invalid structured output; no worker was launched")?;
    ensure!(
        value["action"] != "delegate"
            || value
                .pointer("/assignment/provider")
                .and_then(Value::as_str)
                .is_some(),
        "Manager must explicitly select assignment.provider"
    );
    serde_json::from_value(value)
        .context("Manager returned invalid structured output; no worker was launched")
}

fn demo_decision(state: &State, reviewing: bool) -> Decision {
    let phases = vec![
        "Define one verifiable outcome".into(),
        "Implement a bounded assignment".into(),
        "Review evidence and discoveries".into(),
    ];
    if reviewing {
        let success = state.tasks.last().is_some_and(|t| t.exit_code == Some(0));
        Decision { manager_provider: state.manager_provider.clone(), action: if success { "complete" } else { "blocked" }.into(), summary: if success { "Demo complete: the simulated worker result was accepted. No model credits were used." } else { "Demo blocked: the worker failed. Inspect its evidence before starting another experiment." }.into(), phases, assignment: None, observations: vec!["DEMO: worker suggested checking empty input; manager adopted it into the acceptance criteria. This exchange was simulated.".into()] }
    } else {
        Decision { manager_provider: state.manager_provider.clone(), action: "delegate".into(), summary: "Demo manager prepared a bounded assignment; all agent activity in this mode is simulated.".into(), phases, assignment: Some(Assignment { provider: "qwen".into(), title: "First experimental assignment".into(), brief: state.objective.clone(), acceptance: vec!["Demonstrate the agreed outcome and report evidence".into(), "Report useful discoveries and unresolved questions".into()], autonomy: "bounded".into() }), observations: vec![] }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    pub fn fixture() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::read(Path::new("firm.toml")).unwrap();
        // Keep fixture defaults deterministic without constraining Len's live roster order.
        config.providers.sort_by_key(|p| match p.id.as_str() {
            "qwen" => 0,
            "muse" => 1,
            _ => 2,
        });
        config.state_dir = dir.path().to_owned();
        config.allowances.manager_interval_seconds = 0;
        let (store, state) =
            Store::open(&dir.path().join("test.db"), true, config.allowances.clone()).unwrap();
        (dir, App::new(config, store, state, None))
    }
    async fn ticks(n: usize) {
        tokio::time::sleep(Duration::from_secs(n as u64)).await;
    }
    #[tokio::test]
    async fn manager_selection_preserves_context_counters_and_retry_step() {
        let (dir, app) = fixture();
        assert_eq!(app.core.lock().await.state.manager_provider, "grok");
        app.new_experiment("Retain this objective".into())
            .await
            .unwrap();
        {
            let mut core = app.core.lock().await;
            core.state.manager_starts.push(now());
            core.state.worker_starts.push(now());
            core.state.phases.push("Retain this phase".into());
            core.state.stage = "blocked".into();
            core.state.manager_retry_stage = Some("ready_review".into());
            core.save().unwrap();
        }
        assert!(app.select_manager("missing").await.is_err());
        app.select_manager("muse").await.unwrap();
        let (_, state) = Store::open(
            &dir.path().join("test.db"),
            true,
            app.config.allowances.clone(),
        )
        .unwrap();
        assert_eq!(state.manager_provider, "muse");
        assert_eq!(state.stage, "ready_review");
        assert_eq!(state.objective, "Retain this objective");
        assert_eq!(state.phases, ["Retain this phase"]);
        assert_eq!(state.manager_starts.len(), 1);
        assert_eq!(state.worker_starts.len(), 1);
        assert!(state.paused);
        app.stop().await.unwrap();
        app.new_experiment("Next objective".into()).await.unwrap();
        assert_eq!(app.core.lock().await.state.manager_provider, "muse");
        app.core.lock().await.state.paused = false;
        assert!(app.select_manager("codex").await.is_err());
        {
            let mut core = app.core.lock().await;
            core.state.paused = true;
            core.state.active_turn = Some("uncertain".into());
        }
        assert!(app.select_manager("codex").await.is_err());
        app.core.lock().await.state.active_turn = None;
        app.provider_settings(
            "grok",
            ProviderSettings {
                enabled: false,
                max_runs: 3,
            },
        )
        .await
        .unwrap();
        assert!(app.select_manager("grok").await.is_err());
        // A model's deliberate blocked decision (or worker failure) isn't retried.
        app.core.lock().await.state.stage = "blocked".into();
        app.select_manager("codex").await.unwrap();
        assert_eq!(app.core.lock().await.state.stage, "blocked");
        let mut old =
            serde_json::to_value(State::new(true, app.config.allowances.clone())).unwrap();
        old.as_object_mut().unwrap().remove("manager_provider");
        old.as_object_mut().unwrap().remove("manager_retry_stage");
        assert_eq!(
            serde_json::from_value::<State>(old)
                .unwrap()
                .manager_provider,
            "grok"
        );
    }

    #[tokio::test]
    async fn cli_manager_gates_ignore_codex_but_keep_role_and_provider_caps() {
        let (_dir, app) = fixture();
        let mut state = app.core.lock().await.state.clone();
        state.demo = false;
        state.paused = false;
        state.manager_provider = "muse".into();
        state.allowances.worker_runs = 0;
        assert!(app.dispatch_gate(&state, "ready_review").is_none());
        state.manager_provider = "codex".into();
        assert!(app.dispatch_gate(&state, "ready_review").is_some());
        state.manager_provider = "muse".into();
        state.allowances.manager_turns = 0;
        assert!(app.dispatch_gate(&state, "ready_review").is_some());
        state.allowances.manager_turns = 4;
        state.provider_settings.insert(
            "muse".into(),
            ProviderSettings {
                enabled: true,
                max_runs: 0,
            },
        );
        assert!(app.dispatch_gate(&state, "ready_review").is_some());
        state.provider_settings.remove("muse");
        state
            .provider_usage
            .entry("muse".into())
            .or_default()
            .cooldown_until = now() + 60;
        assert!(app.dispatch_gate(&state, "ready_review").is_some());
    }

    #[tokio::test]
    async fn fake_cli_manager_plans_reviews_and_rejects_bad_output() {
        let (_dir, mut app) = fixture();
        app.new_experiment("A fake CLI task".into()).await.unwrap();
        // Only /bin/sh runs: this test cannot call a provider or spend credits.
        let provider = app
            .config
            .providers
            .iter_mut()
            .find(|p| p.id == "muse")
            .unwrap();
        provider.command = "/bin/sh".into();
        provider.input = crate::config::PromptInput::Stdin;
        let plan =
            serde_json::to_string(&demo_decision(&app.core.lock().await.state, false)).unwrap();
        provider.manager_args = Some(vec![
            "-c".into(),
            "input=$(cat); test -n \"$input\" || exit 2; printf '%s' \"$1\"".into(),
            "fixture".into(),
            plan.clone(),
        ]);
        app.select_manager("muse").await.unwrap();
        app.core.lock().await.state.demo = false;
        app.manager_turn(false, app.cancel.subscribe())
            .await
            .unwrap();
        {
            let mut core = app.core.lock().await;
            assert_eq!(core.state.decisions[0].manager_provider, "muse");
            assert_eq!(core.state.stage, "ready_worker");
            let task = core.state.tasks.last_mut().unwrap();
            task.exit_code = Some(0);
            task.output = "Independent verification passed".into();
        }
        let snapshot = app.core.lock().await.state.clone();
        let review = serde_json::to_string(&demo_decision(&snapshot, true)).unwrap();
        app.config
            .providers
            .iter_mut()
            .find(|p| p.id == "muse")
            .unwrap()
            .manager_args
            .as_mut()
            .unwrap()[3] = review;
        assert!(
            app.manager_prompt(&snapshot, true)
                .unwrap()
                .contains("Independent verification passed")
        );
        app.manager_turn(true, app.cancel.subscribe())
            .await
            .unwrap();
        assert_eq!(app.core.lock().await.state.stage, "complete");
        assert!(parse_decision(&format!("```json\n{plan}\n```")).is_ok());
        assert!(parse_decision("I could not finish").is_err());
        let mut missing: Value = serde_json::from_str(&plan).unwrap();
        missing["assignment"]
            .as_object_mut()
            .unwrap()
            .remove("provider");
        assert!(parse_decision(&missing.to_string()).is_err());
        app.config
            .providers
            .iter_mut()
            .find(|p| p.id == "muse")
            .unwrap()
            .manager_args
            .as_mut()
            .unwrap()[3] = "rate limit exceeded".into();
        let before = app.core.lock().await.state.decisions.len();
        assert!(
            app.manager_turn(true, app.cancel.subscribe())
                .await
                .is_err()
        );
        let core = app.core.lock().await;
        assert_eq!(core.state.decisions.len(), before);
        assert!(core.state.provider_usage["muse"].cooldown_until > now());
        assert!(core.state.latest_snapshot.is_some());
    }

    #[tokio::test]
    async fn provider_settings_survive_new_objectives_restart_and_snapshots() {
        let (dir, app) = fixture();
        {
            let mut core = app.core.lock().await;
            core.state.worker_starts = vec![now()];
            core.state
                .provider_usage
                .entry("muse".into())
                .or_default()
                .starts = vec![now()];
        }
        app.provider_update(
            "muse",
            ProviderSettings {
                enabled: false,
                max_runs: 1,
            },
            Some("Design critic".into()),
        )
        .await
        .unwrap();
        assert!(
            app.provider_settings(
                "missing",
                ProviderSettings {
                    enabled: true,
                    max_runs: 1
                }
            )
            .await
            .is_err()
        );
        let id = app
            .capture_manual("Provider settings".into())
            .await
            .unwrap();
        let recipe = app
            .archive_action(move |archive| archive.recipe_editor(&id))
            .await
            .unwrap();
        assert_eq!(recipe["config"]["providers"][1]["enabled"], false);
        assert_eq!(recipe["config"]["providers"][1]["max_runs"], 1);
        assert_eq!(
            recipe["config"]["providers"][1]["description"],
            "Design critic"
        );
        app.new_experiment("New objective".into()).await.unwrap();
        let (_, state) = Store::open(
            &dir.path().join("test.db"),
            true,
            app.config.allowances.clone(),
        )
        .unwrap();
        assert!(!state.providers(&app.config.providers)[1].enabled);
        assert_eq!(
            state.providers(&app.config.providers)[1].description,
            "Design critic"
        );
        assert_eq!(state.provider_usage["muse"].starts.len(), 1);
        assert_eq!(state.worker_starts.len(), 1);
        app.resume().await.unwrap();
        assert!(
            app.provider_settings(
                "muse",
                ProviderSettings {
                    enabled: true,
                    max_runs: 3
                }
            )
            .await
            .is_err()
        );
        app.pause().await.unwrap();
        app.core.lock().await.active = true;
        assert!(
            app.provider_settings(
                "muse",
                ProviderSettings {
                    enabled: true,
                    max_runs: 3
                }
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn disabled_queued_provider_holds_without_spending_or_silent_failover() {
        let (_dir, app) = fixture();
        app.new_experiment("A task".into()).await.unwrap();
        app.manager_turn(false, app.cancel.subscribe())
            .await
            .unwrap();
        app.provider_settings(
            "qwen",
            ProviderSettings {
                enabled: false,
                max_runs: 3,
            },
        )
        .await
        .unwrap();
        app.resume().await.unwrap();
        let scheduler = tokio::spawn(app.clone().schedule());
        ticks(2).await;
        {
            let core = app.core.lock().await;
            assert_eq!(core.state.stage, "ready_worker");
            assert!(core.state.reason.contains("disabled"));
            assert!(core.state.worker_starts.is_empty());
            assert_eq!(core.state.tasks[0].assignment.provider, "qwen");
        }
        app.pause().await.unwrap();
        for provider in &app.config.providers {
            app.provider_settings(
                &provider.id,
                ProviderSettings {
                    enabled: false,
                    max_runs: 3,
                },
            )
            .await
            .unwrap();
        }
        {
            let mut core = app.core.lock().await;
            core.state.paused = false;
            assert!(
                app.dispatch_gate(&core.state, "ready_plan")
                    .unwrap()
                    .contains("disabled")
            );
            // The manager is part of the same roster, so disabling every provider
            // disables review as well as implementation.
            assert!(app.dispatch_gate(&core.state, "ready_review").is_some());
        }
        scheduler.abort();
    }
    #[tokio::test]
    async fn demo_routes_to_grok_and_records_provider_provenance() {
        let (_dir, app) = fixture();
        for id in ["qwen", "muse", "codex"] {
            app.provider_settings(
                id,
                ProviderSettings {
                    enabled: false,
                    max_runs: 3,
                },
            )
            .await
            .unwrap();
        }
        app.new_experiment("A Grok simulation".into())
            .await
            .unwrap();
        app.resume().await.unwrap();
        let scheduler = tokio::spawn(app.clone().schedule());
        ticks(20).await;
        {
            let core = app.core.lock().await;
            assert_eq!(core.state.stage, "complete");
            assert_eq!(core.state.tasks[0].assignment.provider, "grok");
            // Grok's provider cap covers its planning, worker and review roles.
            assert_eq!(core.state.provider_usage["grok"].starts.len(), 3);
            assert!(!core.state.provider_usage.contains_key("qwen"));
        }
        app.archive_action(|archive| {
            let records = archive.list()?;
            let input = records
                .iter()
                .find(|r| r["label"] == "Grok worker input")
                .unwrap();
            let artifact =
                archive.preview(input["id"].as_str().unwrap(), "context/request.json")?;
            let request: Value = serde_json::from_str(artifact["content"].as_str().unwrap())?;
            assert_eq!(request["provider"], "grok");
            assert_eq!(request["program"], "grok");
            assert!(
                request["prompt_file"]["content"]
                    .as_str()
                    .unwrap()
                    .contains("A Grok simulation")
            );
            Ok(())
        })
        .await
        .unwrap();
        scheduler.abort();
    }
    #[tokio::test]
    async fn end_to_end_demo_accepts_evidence_and_retains_counters() {
        let (_dir, app) = fixture();
        app.select_manager("muse").await.unwrap();
        app.new_experiment("A small task".into()).await.unwrap();
        app.resume().await.unwrap();
        let scheduler = tokio::spawn(app.clone().schedule());
        ticks(20).await;
        {
            let core = app.core.lock().await;
            assert_eq!(core.state.stage, "complete");
            assert_eq!(core.state.tasks[0].status, "accepted");
            assert_eq!(core.state.manager_starts.len(), 2);
            assert_eq!(core.state.worker_starts.len(), 1);
            assert_eq!(core.state.decisions.len(), 2);
            assert!(
                core.state
                    .decisions
                    .iter()
                    .all(|d| d.manager_provider == "muse")
            );
            assert_eq!(core.state.provider_usage["muse"].starts.len(), 2);
            assert_eq!(core.state.provider_usage["qwen"].starts.len(), 1);
        }
        let snapshots = app.archive_action(|archive| archive.list()).await.unwrap();
        assert_eq!(snapshots.iter().filter(|m| m["kind"] == "input").count(), 3);
        assert_eq!(
            snapshots.iter().filter(|m| m["kind"] == "result").count(),
            3
        );
        assert_eq!(
            snapshots.iter().filter(|m| m["kind"] == "recipe").count(),
            1
        );
        app.new_experiment("Another task".into()).await.unwrap();
        let core = app.core.lock().await;
        assert_eq!(core.state.manager_starts.len(), 2);
        assert_eq!(core.store.history().unwrap().len(), 1);
        scheduler.abort();
    }
    #[tokio::test]
    async fn pause_prevents_dispatch_and_stop_cancels_active_turn() {
        let (_dir, app) = fixture();
        app.new_experiment("A task".into()).await.unwrap();
        let scheduler = tokio::spawn(app.clone().schedule());
        ticks(5).await;
        assert!(app.core.lock().await.state.manager_starts.is_empty());
        app.resume().await.unwrap();
        ticks(1).await;
        assert!(app.core.lock().await.active);
        app.stop().await.unwrap();
        ticks(5).await;
        let core = app.core.lock().await;
        assert_eq!(core.state.stage, "cancelled");
        assert!(!core.active);
        assert!(core.state.worker_starts.is_empty());
        scheduler.abort();
    }
    #[tokio::test]
    async fn exhausted_budget_holds_review_without_launching_manager() {
        let (_dir, app) = fixture();
        app.core.lock().await.state.allowances.manager_turns = 1;
        app.new_experiment("A task".into()).await.unwrap();
        app.resume().await.unwrap();
        let scheduler = tokio::spawn(app.clone().schedule());
        ticks(20).await;
        let core = app.core.lock().await;
        assert_eq!(core.state.stage, "ready_review");
        assert_eq!(core.state.manager_starts.len(), 1);
        assert!(core.state.reason.contains("exhausted"));
        scheduler.abort();
    }
    #[tokio::test]
    async fn failed_worker_becomes_a_reviewable_blocker() {
        let (_dir, app) = fixture();
        app.new_experiment("[fail]".into()).await.unwrap();
        app.resume().await.unwrap();
        let scheduler = tokio::spawn(app.clone().schedule());
        ticks(20).await;
        let core = app.core.lock().await;
        assert_eq!(core.state.stage, "blocked");
        assert_eq!(core.state.tasks[0].exit_code, Some(1));
        assert!(!core.state.tasks[0].output.is_empty());
        scheduler.abort();
    }
    #[test]
    fn invalid_decision_cannot_dispatch_or_complete() {
        let (_dir, app) = fixture();
        let state = app.core.try_lock().unwrap().state.clone();
        let mut decision = demo_decision(&state, false);
        decision.assignment = None;
        assert!(decision.validate(false).is_err());
        decision.action = "complete".into();
        assert!(decision.validate(false).is_err());
    }

    #[tokio::test]
    async fn live_protocol_handles_early_notifications_and_structured_result() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::{accept_async, tungstenite::Message};
        for (provider, disabled, accepted) in [
            ("muse", false, true),
            ("missing", false, false),
            ("muse", true, false),
            ("omit", false, false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(socket).await.unwrap();
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    if request.get("id").is_none() {
                        continue;
                    }
                    let result = match request["method"].as_str().unwrap() {
                        "initialize" => json!({"userAgent":"fixture"}),
                        "thread/start" => json!({"thread":{"id":"test-thread"}}),
                        "thread/read" => {
                            json!({"thread":{"id":"test-thread","turns":[],"status":{"type":"idle"}}})
                        }
                        "turn/start" => {
                            assert_eq!(request["params"]["approvalPolicy"], "never");
                            assert_eq!(request["params"]["sandboxPolicy"]["type"], "readOnly");
                            let prompt = request["params"]["input"][0]["text"].as_str().unwrap();
                            assert!(prompt.contains("Current worker roster"));
                            assert!(prompt.contains("\"id\":\"muse\""));
                            assert!(prompt.contains("\"id\":\"grok\""));
                            assert!(
                            request["params"]["outputSchema"]["properties"]["assignment"]["anyOf"]
                                [1]["required"]
                                .as_array()
                                .unwrap()
                                .contains(&json!("provider"))
                        );
                            let mut decision = json!({"action":"delegate","summary":"A verified assignment","phases":["Implement"],"assignment":{"provider":provider,"title":"Task","brief":"Do one thing","acceptance":["Check it"],"autonomy":"bounded"},"observations":[]});
                            if provider == "omit" {
                                decision["assignment"]
                                    .as_object_mut()
                                    .unwrap()
                                    .remove("provider");
                            }
                            // Real app-server may notify before acknowledging turn/start.
                            for event in [
                                json!({"method":"item/completed","params":{"threadId":"test-thread","turnId":"test-turn","item":{"type":"agentMessage","phase":"final_answer","text":decision.to_string()}}}),
                                json!({"method":"turn/completed","params":{"threadId":"test-thread","turn":{"id":"test-turn","status":"completed"}}}),
                            ] {
                                ws.send(Message::Text(event.to_string().into()))
                                    .await
                                    .unwrap();
                            }
                            json!({"turn":{"id":"test-turn"}})
                        }
                        method => panic!("Unexpected RPC method {method}"),
                    };
                    ws.send(Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                }
            });
            let (_dir, mut app) = fixture();
            if disabled {
                app.provider_settings(
                    provider,
                    ProviderSettings {
                        enabled: false,
                        max_runs: 3,
                    },
                )
                .await
                .unwrap();
            }
            app.rpc = Some(Codex::connect(&format!("ws://{addr}")).await.unwrap());
            {
                let mut core = app.core.lock().await;
                core.state.demo = false;
                core.state.manager_provider = "codex".into();
            }
            let outcome = app.manager_turn(false, app.cancel.subscribe()).await;
            if !accepted {
                let error = outcome.unwrap_err().to_string();
                assert!(error.contains("provider"), "{error}");
                assert!(app.core.lock().await.state.tasks.is_empty());
                server.abort();
                continue;
            }
            outcome.unwrap();
            let core = app.core.lock().await;
            assert_eq!(core.state.stage, "ready_worker");
            assert!(core.state.active_turn.is_none());
            assert_eq!(core.state.tasks.len(), 1);
            assert_eq!(core.state.tasks[0].assignment.provider, "muse");
            server.abort();
        }
    }
}
