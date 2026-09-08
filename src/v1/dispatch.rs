//! The pull-based dispatcher. Ready tasks are claimed by whichever provider has capacity;
//! attempts run concurrently in isolated worktrees; the controller scores each one and
//! serialises merges. There is no manager in this loop — that is the whole point.

use super::{
    board::{Attempt, AttemptState, Board, RunSpec, TaskState},
    scorer::Scorer,
    worktree::Worktrees,
};
use crate::{
    config::{Config, Provider},
    state::now,
    worker,
};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use tokio::sync::{Mutex, Semaphore, watch};

/// Global ceiling on concurrent agents. Five is an honest limit for one workstation:
/// each CLI agent is a heavy process, and providers rate-limit independently.
pub const MAX_CONCURRENT: usize = 5;
/// How many times a task may be attempted before it is failed.
pub const MAX_ATTEMPTS: usize = 2;

pub struct Engine {
    config: Config,
    board: Arc<Mutex<Board>>,
    trees: Worktrees,
    scorer: Scorer,
    cancel: watch::Receiver<u64>,
    /// Merges are serialised so two attempts never touch the integration branch at once.
    merge_lock: Mutex<()>,
    inflight: Arc<Semaphore>,
    provider_slots: BTreeMap<String, Arc<Semaphore>>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Outcome {
    pub merged: usize,
    pub failed: usize,
    pub blocked: usize,
    /// Tasks left open because an allowance ran out. They are still doable later, so they
    /// are deliberately not failed.
    pub held: usize,
    pub hold_reason: Option<String>,
    /// The run-level check against the fully integrated result. Reported, not enforced:
    /// individual tasks are judged by their own checks.
    pub final_check: Option<super::scorer::Verdict>,
}

impl Engine {
    pub async fn create(
        config: Config,
        board: Board,
        scorer: Scorer,
        cancel: watch::Receiver<u64>,
        spec: &RunSpec,
    ) -> Result<(Self, String)> {
        let run_id = uuid::Uuid::new_v4().to_string();
        let (trees, base_commit) = Worktrees::create(
            &config.workspace,
            &config.state_dir.join("worktrees"),
            &run_id,
        )
        .await?;
        let mut board = board;
        // The board validates the graph. An authoring error must not leave a worktree
        // behind, so the integration worktree is removed before the error propagates.
        match board.create_run(&run_id, spec, &base_commit, trees.integration_branch()) {
            Ok(()) => {}
            Err(error) => {
                let _ = super::worktree::git(
                    &config.workspace,
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        &trees.integration_path().to_string_lossy(),
                    ],
                )
                .await;
                return Err(error);
            }
        }
        let provider_slots = config
            .providers
            .iter()
            .map(|p| (p.id.clone(), Arc::new(Semaphore::new(p.max_concurrent))))
            .collect();
        Ok((
            Self {
                config,
                board: Arc::new(Mutex::new(board)),
                trees,
                scorer,
                cancel,
                merge_lock: Mutex::new(()),
                inflight: Arc::new(Semaphore::new(MAX_CONCURRENT)),
                provider_slots,
            },
            run_id,
        ))
    }

    fn provider(&self, id: &str) -> Result<Provider> {
        self.config
            .providers
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .with_context(|| format!("Provider {id} is not configured"))
    }

    /// Choose a provider for a task. Milestone 1 honours an explicit pin and otherwise
    /// takes the cheapest enabled provider; tier-aware routing arrives in milestone 3.
    fn route(&self, pinned: Option<&str>) -> Result<Provider> {
        if let Some(id) = pinned {
            let provider = self.provider(id)?;
            ensure!(provider.enabled, "Provider {id} is disabled");
            return Ok(provider);
        }
        self.config
            .providers
            .iter()
            .filter(|p| p.enabled)
            .min_by_key(|p| (p.tier, p.id.clone()))
            .cloned()
            .context("No enabled provider")
    }

    /// Whether this provider may start another run right now. Counts are durable and span
    /// runs, so a new run never replenishes a rolling allowance.
    async fn budget_gate(&self, provider: &Provider) -> Result<Option<String>> {
        let allowances = &self.config.allowances;
        let since = now().saturating_sub(allowances.window_seconds);
        let board = self.board.lock().await;
        if board.recent_runs(None, since)? >= allowances.worker_runs {
            return Ok(Some(format!(
                "Overall worker run allowance exhausted ({} per {}s)",
                allowances.worker_runs, allowances.window_seconds
            )));
        }
        if board.recent_runs(Some(&provider.id), since)? >= provider.max_runs {
            return Ok(Some(format!(
                "{} run allowance exhausted ({} per {}s)",
                provider.name, provider.max_runs, allowances.window_seconds
            )));
        }
        let until = board.cooldown(&provider.id)?;
        if now() < until {
            return Ok(Some(format!(
                "{} is cooling down for {}s after a rate-limited response",
                provider.name,
                until - now()
            )));
        }
        Ok(None)
    }

    /// Drive one run to completion. Returns when every task has reached a terminal state,
    /// or when nothing further can be dispatched within the allowances.
    pub async fn drive(self: &Arc<Self>, run_id: &str) -> Result<Outcome> {
        let mut running: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        let mut hold_reason: Option<String> = None;
        loop {
            if self.cancel.has_changed().unwrap_or(false) {
                break;
            }
            running.retain(|handle| !handle.is_finished());

            let ready = {
                let mut board = self.board.lock().await;
                board.ready(run_id)?
            };
            let ready_count = ready.len();
            let mut dispatched = 0;
            hold_reason = None;

            for task in ready {
                // Acquire capacity before claiming, so a claimed task is always dispatched.
                let Ok(global) = self.inflight.clone().try_acquire_owned() else {
                    break;
                };
                let provider = match self.route(task.provider.as_deref()) {
                    Ok(provider) => provider,
                    Err(error) => {
                        let mut board = self.board.lock().await;
                        board.set_state(
                            run_id,
                            &task.id,
                            TaskState::Blocked,
                            &error.to_string(),
                        )?;
                        continue;
                    }
                };
                // Allowances are checked before anything external happens.
                if let Some(reason) = self.budget_gate(&provider).await? {
                    hold_reason = Some(reason);
                    continue;
                }
                let slots = self.provider_slots.get(&provider.id).cloned();
                let Some(Ok(slot)) = slots.map(|s| s.try_acquire_owned()) else {
                    continue; // This provider is busy; try the task on the next pass.
                };
                if !self.board.lock().await.claim(run_id, &task.id)? {
                    continue;
                }
                dispatched += 1;

                // Reserve durably before anything external happens, so that allowances
                // account for in-flight work and a crash cannot lose the reservation.
                let attempt_id = uuid::Uuid::new_v4().to_string();
                let from = self.trees.integration_tip().await?;
                self.board.lock().await.start_attempt(&Attempt::reserved(
                    attempt_id.clone(),
                    run_id,
                    &task.id,
                    &provider.id,
                    super::worktree::attempt_branch(&attempt_id),
                    from.clone(),
                ))?;

                let engine = Arc::clone(self);
                let run = run_id.to_string();
                running.push(tokio::spawn(async move {
                    let _global = global;
                    let _slot = slot;
                    if let Err(error) = engine.attempt(&run, &task, &provider, attempt_id, from).await {
                        let mut board = engine.board.lock().await;
                        let note = format!("Attempt error: {error}");
                        // `task.attempts` was read before this dispatch incremented it.
                        let state = if task.attempts + 1 >= MAX_ATTEMPTS {
                            TaskState::Failed
                        } else {
                            TaskState::Open
                        };
                        let _ = board.set_state(&run, &task.id, state, &note);
                    }
                }));
            }

            let tasks = self.board.lock().await.tasks(run_id)?;
            if tasks.iter().all(|t| t.state.terminal()) && running.is_empty() {
                break;
            }
            // Nothing running means every concurrency slot is free, so a pass that
            // dispatched nothing while work was ready can only have been held by an
            // allowance. Waiting for a rolling window to roll could take hours; stop and
            // report instead, leaving the held tasks open for a later run.
            if running.is_empty() && ready_count > 0 && dispatched == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }

        for handle in running {
            let _ = handle.await;
        }
        let tasks = self.board.lock().await.tasks(run_id)?;
        // Record why a held task stopped, so a later report explains itself.
        if let Some(reason) = &hold_reason {
            let mut board = self.board.lock().await;
            for task in tasks.iter().filter(|t| t.state == TaskState::Open) {
                board.set_state(run_id, &task.id, TaskState::Open, reason)?;
            }
        }
        self.board.lock().await.finish_run(run_id)?;
        let count = |state: TaskState| tasks.iter().filter(|t| t.state == state).count();
        let held = tasks.iter().filter(|t| !t.state.terminal()).count();
        // One final check of the integrated result, for information.
        let final_check = if count(TaskState::Merged) > 0 && !self.cancel.has_changed().unwrap_or(false)
        {
            self.scorer
                .score(self.trees.integration_path(), self.cancel.clone())
                .await
                .ok()
        } else {
            None
        };
        Ok(Outcome {
            merged: count(TaskState::Merged),
            failed: count(TaskState::Failed),
            blocked: count(TaskState::Blocked),
            held,
            hold_reason: hold_reason.filter(|_| held > 0),
            final_check,
        })
    }

    /// One attempt: isolate, run the agent, commit, score, and merge if it passed.
    async fn attempt(
        &self,
        run_id: &str,
        task: &super::board::Task,
        provider: &Provider,
        attempt_id: String,
        from: String,
    ) -> Result<()> {
        // Branch from the integration tip so dependencies already merged are present.
        let workspace = self.trees.create_attempt(&attempt_id, &from).await?;
        let mut record = Attempt::reserved(
            attempt_id,
            run_id,
            &task.id,
            &provider.id,
            workspace.branch.clone(),
            from,
        );

        let outcome = self
            .execute(task, provider, &workspace, &mut record)
            .await;

        record.finished_at = Some(now());
        if let Err(error) = &outcome {
            record.state = AttemptState::Error;
            record.detail = error.to_string();
        }
        self.board.lock().await.finish_attempt(&record)?;

        // The worktree goes; the branch stays as evidence of what was actually written.
        let _ = self.trees.discard(&workspace).await;

        let (state, note) = match (&outcome, record.state) {
            (Ok(()), AttemptState::Verified) => (TaskState::Merged, record.detail.clone()),
            _ if task.attempts + 1 >= MAX_ATTEMPTS => (
                TaskState::Failed,
                format!("{MAX_ATTEMPTS} attempts exhausted: {}", record.detail),
            ),
            _ => (TaskState::Open, record.detail.clone()),
        };
        let mut board = self.board.lock().await;
        board.set_state(run_id, &task.id, state, &clip(&note, 2000))?;
        outcome
    }

    async fn execute(
        &self,
        task: &super::board::Task,
        provider: &Provider,
        workspace: &super::worktree::Attempt,
        record: &mut Attempt,
    ) -> Result<()> {
        let prompt = self.prompt(task);
        let prepared = worker::prepare_prompt_in(
            &self.config,
            provider,
            prompt,
            workspace.path.clone(),
        )?;
        let result = worker::run(&self.config, &prepared, self.cancel.clone()).await?;

        // Native exit and the controller's verdict stay separate facts.
        record.native_exit = result.exit_code;
        record.interruption = result.interruption.clone();
        record.output = clip(&result.output, 48 * 1024);

        // A limit on one account says nothing about another, so only this provider is held.
        if worker::looks_rate_limited(&result.output) {
            let until = now().saturating_add(self.config.allowances.provider_cooldown_seconds);
            self.board.lock().await.set_cooldown(&provider.id, until)?;
        }

        // Commit whatever exists even when the agent exited badly: a run that failed or
        // ran out of turns can still have produced useful work, and the scorer — not the
        // exit code — decides whether it is worth keeping.
        let changed = workspace
            .commit(&format!("firm: {} ({})", task.id, provider.id))
            .await?;
        record.files_changed = workspace.files_changed().await?;
        if !changed {
            record.state = AttemptState::Rejected;
            record.passed = Some(false);
            record.detail = match (result.exit_code, &result.interruption) {
                (_, Some(reason)) => format!("Agent was interrupted ({reason}) and changed no files"),
                (Some(0), _) => "Agent changed no files".into(),
                (code, _) => format!("Agent exited {code:?} and changed no files"),
            };
            return Ok(());
        }

        // A task-scoped check where one is given, so focused work is not judged by
        // failures belonging to tasks that have not been done yet.
        let scorer = self.task_scorer(task);
        let verdict = scorer.score(&workspace.path, self.cancel.clone()).await?;
        record.passed = Some(verdict.passed);
        record.score = verdict.score;
        record.detail = verdict.detail.clone();
        if !verdict.passed {
            record.state = AttemptState::Rejected;
            return Ok(());
        }

        // Serialised: only one attempt merges at a time.
        let _guard = self.merge_lock.lock().await;
        if let Err(error) = self
            .trees
            .merge(workspace, &format!("firm: merge {} ({})", task.id, provider.id))
            .await
        {
            record.state = AttemptState::Rejected;
            record.passed = Some(false);
            record.detail = format!("Merge conflict against the integration branch: {error}");
            return Ok(());
        }
        // Re-run the same check on the merged tree: passing alone does not prove the work
        // still holds once combined with everything else already merged.
        let integrated = scorer
            .score(self.trees.integration_path(), self.cancel.clone())
            .await?;
        if !integrated.passed {
            self.trees.revert_last_merge().await?;
            record.state = AttemptState::Rejected;
            record.passed = Some(false);
            record.detail = format!("Passed alone but broke integration: {}", integrated.detail);
            return Ok(());
        }
        record.state = AttemptState::Verified;
        Ok(())
    }

    /// The check for one task: its own where given, otherwise the run-level scorer.
    fn task_scorer(&self, task: &super::board::Task) -> Scorer {
        match &task.verify {
            Some(command) if !command.is_empty() => Scorer::Command {
                command: command.clone(),
                timeout_seconds: self.config.allowances.worker_timeout_seconds,
            },
            _ => self.scorer.clone(),
        }
    }

    fn prompt(&self, task: &super::board::Task) -> String {
        let objective = &task.brief;
        let acceptance = if task.acceptance.is_empty() {
            "No explicit criteria were supplied; satisfy the brief.".to_string()
        } else {
            task.acceptance
                .iter()
                .map(|a| format!("- {a}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        format!(
            "{}\n\nYou are working alone in a private git worktree. Other agents are \
             working in parallel on other tasks; you cannot see their directories and \
             must not attempt to. Change only what this task requires.\n\n\
             Task: {}\n\n{objective}\n\nAcceptance criteria:\n{acceptance}\n\n\
             When you finish, the controller runs this check itself and only merges your \
             work if it passes: {check}\n\
             Other parts of the project may still be unimplemented; that is expected and \
             is not yours to fix. Do not claim results you did not verify, and report \
             blockers rather than guessing repeatedly.",
            include_str!("../../prompts/worker.md"),
            task.title,
            check = match self.task_scorer(task) {
                Scorer::Command { command, .. } => command.join(" "),
            },
        )
    }
}

pub fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
}

/// Open the board for a mode, alongside v0's separate state databases.
pub fn board_path(state_dir: &Path, live: bool) -> std::path::PathBuf {
    state_dir.join(if live { "v1-live.db" } else { "v1-demo.db" })
}
