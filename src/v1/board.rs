//! The task board: runs, a dependency graph of tasks, and the attempts made against
//! them. Replaces v0's single-task-at-a-time `State`, in which only `tasks.last()` was
//! ever actionable. Agents pull from this board; nothing pushes work at them.

use crate::state::now;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// A task's lifecycle. `Merged` and `Failed` are terminal; `Blocked` needs a decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Open,
    Running,
    Merged,
    Blocked,
    Failed,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Running => "running",
            Self::Merged => "merged",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "open" => Self::Open,
            "running" => Self::Running,
            "merged" => Self::Merged,
            "blocked" => Self::Blocked,
            "failed" => Self::Failed,
            other => bail!("Unknown task state: {other}"),
        })
    }
    pub fn terminal(self) -> bool {
        matches!(self, Self::Merged | Self::Failed | Self::Blocked)
    }
}

/// How an attempt ended. The agent's own exit status and the controller's verdict are
/// recorded separately — v0 let verification overwrite the native exit code, which made
/// good focused work indistinguishable from a failed process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Running,
    Verified,
    Rejected,
    Error,
}

impl AttemptState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Verified => "verified",
            Self::Rejected => "rejected",
            Self::Error => "error",
        }
    }
}

/// Authoring format for a run's tasks. Ids are human-chosen and stable so that
/// `depends_on` can be written by hand or by the manager.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub id: String,
    pub title: String,
    pub brief: String,
    #[serde(default)]
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default = "implement")]
    pub class: String,
    /// Pin this task to one provider. Left unset, routing chooses (v1 milestone 3).
    #[serde(default)]
    pub provider: Option<String>,
    /// Files this task may modify. Declared, an attempt that touches anything else is
    /// rejected — which is what stops an implementer quietly editing the test it cannot
    /// pass. Left empty, nothing is enforced.
    #[serde(default)]
    pub files: Vec<String>,
    /// A command that must still **fail** for this task's work to be accepted. A task that
    /// writes a test uses it to prove the test actually tests something: a new test that
    /// passes against unimplemented code is not a test.
    #[serde(default)]
    pub must_fail: Option<Vec<String>>,
    /// A check scoped to this task, run instead of the run-level scorer. Essential in
    /// partition mode: while other tasks are still stubs the whole suite necessarily
    /// fails, so judging one task by it would reject perfectly good focused work.
    #[serde(default)]
    pub verify: Option<Vec<String>>,
}

fn implement() -> String {
    "implement".into()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RunSpec {
    pub objective: String,
    pub tasks: Vec<TaskSpec>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Task {
    pub id: String,
    pub run_id: String,
    pub title: String,
    pub brief: String,
    pub acceptance: Vec<String>,
    pub depends_on: Vec<String>,
    pub class: String,
    pub provider: Option<String>,
    pub verify: Option<Vec<String>>,
    pub files: Vec<String>,
    pub must_fail: Option<Vec<String>>,
    pub state: TaskState,
    pub attempts: usize,
    pub note: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Attempt {
    pub id: String,
    pub run_id: String,
    pub task_id: String,
    pub provider: String,
    pub branch: String,
    pub base_commit: String,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub native_exit: Option<i32>,
    pub interruption: Option<String>,
    pub passed: Option<bool>,
    pub score: Option<f64>,
    pub detail: String,
    pub files_changed: Vec<String>,
    pub output: String,
    pub state: AttemptState,
}

impl Attempt {
    /// A reservation, written before the agent starts so that rolling-window counts
    /// include work that is in flight.
    pub fn reserved(
        id: String,
        run_id: &str,
        task_id: &str,
        provider: &str,
        branch: String,
        base_commit: String,
    ) -> Self {
        Self {
            id,
            run_id: run_id.into(),
            task_id: task_id.into(),
            provider: provider.into(),
            branch,
            base_commit,
            started_at: now(),
            finished_at: None,
            native_exit: None,
            interruption: None,
            passed: None,
            score: None,
            detail: String::new(),
            files_changed: Vec::new(),
            output: String::new(),
            state: AttemptState::Running,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Run {
    pub id: String,
    pub objective: String,
    pub base_commit: String,
    pub integration_branch: String,
    pub created_at: u64,
    pub finished_at: Option<u64>,
}

/// How many tasks a run may hold. A proposal is a spend primitive: an agent that can add
/// work can commit the budget, so the graph cannot grow without limit.
pub const MAX_TASKS_PER_RUN: usize = 60;

/// A change to the graph, proposed by an agent or the controller and decided here.
///
/// Deliberately few primitives. Splitting a task is `Add` the children plus `Block` the
/// parent; there is no separate operation, and nothing needs one yet.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Mutation {
    /// New work the plan did not anticipate.
    Add { task: TaskSpec },
    /// This task cannot proceed, and says why.
    Block { task: String, reason: String },
    /// This task turns out to need another's result first.
    DependOn { task: String, on: String },
}

impl Mutation {
    fn kind(&self) -> &'static str {
        match self {
            Self::Add { .. } => "add",
            Self::Block { .. } => "block",
            Self::DependOn { .. } => "depend_on",
        }
    }
    fn target(&self) -> String {
        match self {
            Self::Add { task } => task.id.clone(),
            Self::Block { task, .. } | Self::DependOn { task, .. } => task.clone(),
        }
    }
}

/// Everything one provider consumed, summed across runs.
#[derive(Clone, Debug, Serialize)]
pub struct UsageTotal {
    pub provider: String,
    pub metric: String,
    pub label: String,
    pub value: f64,
    pub runs: usize,
}

/// One provider's observed record.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    pub attempts: usize,
    pub verified: usize,
    pub durations: Vec<u64>,
}

impl Stats {
    /// Share of attempts that were accepted, as a percentage. An unproven provider is
    /// given the benefit of the doubt rather than ranked last on no evidence.
    pub fn success_percent(&self) -> u32 {
        if self.attempts == 0 {
            return 100;
        }
        ((self.verified as f64 / self.attempts as f64) * 100.0).round() as u32
    }

    /// Typical time to succeed. The median, not the mean: one agent that hung for fifteen
    /// minutes should not redefine how long a provider usually takes.
    pub fn median_seconds(&self) -> u64 {
        if self.durations.is_empty() {
            return 0;
        }
        let mut sorted = self.durations.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }
}

pub struct Board {
    conn: Connection,
}

impl Board {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS runs (id TEXT PRIMARY KEY, objective TEXT NOT NULL, base_commit TEXT NOT NULL, integration_branch TEXT NOT NULL, created_at INTEGER NOT NULL, finished_at INTEGER);
            CREATE TABLE IF NOT EXISTS tasks (id TEXT NOT NULL, run_id TEXT NOT NULL, title TEXT NOT NULL, brief TEXT NOT NULL, acceptance TEXT NOT NULL, depends_on TEXT NOT NULL, class TEXT NOT NULL, provider TEXT, verify TEXT, files TEXT NOT NULL DEFAULT '[]', must_fail TEXT, state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, note TEXT NOT NULL DEFAULT '', PRIMARY KEY (run_id, id));
            CREATE TABLE IF NOT EXISTS attempts (id TEXT PRIMARY KEY, run_id TEXT NOT NULL, task_id TEXT NOT NULL, provider TEXT NOT NULL, branch TEXT NOT NULL, base_commit TEXT NOT NULL, started_at INTEGER NOT NULL, finished_at INTEGER, native_exit INTEGER, interruption TEXT, passed INTEGER, score REAL, detail TEXT NOT NULL DEFAULT '', files_changed TEXT NOT NULL DEFAULT '[]', output TEXT NOT NULL DEFAULT '', activity TEXT NOT NULL DEFAULT '', observation TEXT NOT NULL DEFAULT '', state TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS attempts_task ON attempts(run_id, task_id);
            CREATE INDEX IF NOT EXISTS attempts_usage ON attempts(provider, started_at);
            CREATE TABLE IF NOT EXISTS cooldowns (provider TEXT PRIMARY KEY, until INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS usage_samples (id INTEGER PRIMARY KEY AUTOINCREMENT, run_id TEXT NOT NULL, phase TEXT NOT NULL, provider TEXT NOT NULL, metric TEXT NOT NULL, label TEXT NOT NULL, value REAL NOT NULL, at INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS usage_run ON usage_samples(run_id, phase);
            CREATE TABLE IF NOT EXISTS mutations (id INTEGER PRIMARY KEY AUTOINCREMENT, run_id TEXT NOT NULL, author TEXT NOT NULL, kind TEXT NOT NULL, target TEXT NOT NULL, detail TEXT NOT NULL, accepted INTEGER NOT NULL, reason TEXT NOT NULL, at INTEGER NOT NULL);",
        )?;
        // `CREATE TABLE IF NOT EXISTS` never alters an existing table, so columns added
        // after a board was created must be migrated in explicitly.
        add_column(&conn, "attempts", "activity", "TEXT NOT NULL DEFAULT ''")?;
        add_column(&conn, "attempts", "observation", "TEXT NOT NULL DEFAULT ''")?;
        add_column(&conn, "tasks", "files", "TEXT NOT NULL DEFAULT '[]'")?;
        add_column(&conn, "tasks", "must_fail", "TEXT")?;
        super::forum::Forum::initialize(&conn)?;
        Ok(Self { conn })
    }

    /// The forum shares the board's database, so entries and attempts commit together.
    pub fn forum(&self) -> super::forum::Forum<'_> {
        super::forum::Forum::new(&self.conn)
    }

    /// Read-only handle for watching a run another process owns. Takes no lock and
    /// creates nothing, so it cannot disturb a run in flight.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("No board at {}", path.display()))?;
        Ok(Self { conn })
    }

    /// Whether a run has finished, for watchers.
    pub fn is_finished(&self, run_id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT finished_at FROM runs WHERE id=?1",
                [run_id],
                |r| r.get::<_, Option<u64>>(0),
            )
            .optional()?
            .flatten()
            .is_some())
    }

    /// Validate the graph before anything is written: unknown dependencies and cycles are
    /// authoring errors, and a run that cannot finish should never start.
    /// The caller supplies the run id so that the integration branch, the worktree paths
    /// and the board row all name the same run.
    pub fn create_run(
        &mut self,
        id: &str,
        spec: &RunSpec,
        base_commit: &str,
        integration_branch: &str,
    ) -> Result<()> {
        validate_graph(spec)?;

        let transaction = self.conn.transaction()?;
        transaction.execute(
            "INSERT INTO runs(id,objective,base_commit,integration_branch,created_at) VALUES(?1,?2,?3,?4,?5)",
            params![id, spec.objective, base_commit, integration_branch, now()],
        )?;
        for task in &spec.tasks {
            transaction.execute(
                "INSERT INTO tasks(id,run_id,title,brief,acceptance,depends_on,class,provider,verify,files,must_fail,state) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    task.id,
                    id,
                    task.title,
                    task.brief,
                    serde_json::to_string(&task.acceptance)?,
                    serde_json::to_string(&task.depends_on)?,
                    task.class,
                    task.provider,
                    task.verify.as_ref().map(serde_json::to_string).transpose()?,
                    serde_json::to_string(&task.files)?,
                    task.must_fail.as_ref().map(serde_json::to_string).transpose()?,
                    TaskState::Open.as_str()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn run(&self, run_id: &str) -> Result<Run> {
        self.conn
            .query_row(
                "SELECT id,objective,base_commit,integration_branch,created_at,finished_at FROM runs WHERE id=?1",
                [run_id],
                |r| {
                    Ok(Run {
                        id: r.get(0)?,
                        objective: r.get(1)?,
                        base_commit: r.get(2)?,
                        integration_branch: r.get(3)?,
                        created_at: r.get(4)?,
                        finished_at: r.get(5)?,
                    })
                },
            )
            .context("Unknown run")
    }

    pub fn finish_run(&mut self, run_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET finished_at=?2 WHERE id=?1",
            params![run_id, now()],
        )?;
        Ok(())
    }

    pub fn tasks(&self, run_id: &str) -> Result<Vec<Task>> {
        let mut query = self.conn.prepare(
            "SELECT id,run_id,title,brief,acceptance,depends_on,class,provider,verify,files,must_fail,state,attempts,note FROM tasks WHERE run_id=?1 ORDER BY rowid",
        )?;
        let rows = query.query_map([run_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, String>(11)?,
                r.get::<_, usize>(12)?,
                r.get::<_, String>(13)?,
            ))
        })?;
        rows.map(|row| {
            let row = row?;
            Ok(Task {
                id: row.0,
                run_id: row.1,
                title: row.2,
                brief: row.3,
                acceptance: serde_json::from_str(&row.4)?,
                depends_on: serde_json::from_str(&row.5)?,
                class: row.6,
                provider: row.7,
                verify: row.8.map(|v| serde_json::from_str(&v)).transpose()?,
                files: serde_json::from_str(&row.9)?,
                must_fail: row.10.map(|v| serde_json::from_str(&v)).transpose()?,
                state: TaskState::parse(&row.11)?,
                attempts: row.12,
                note: row.13,
            })
        })
        .collect()
    }

    /// Put a stopped run back into a state it can continue from.
    ///
    /// A task left `running` has no agent behind it any more — the process died with the
    /// controller — so it returns to `open` to be dispatched again. Its attempt is closed
    /// as interrupted rather than left hanging, which also keeps the ledger honest: an
    /// attempt that never finished is not evidence about a provider.
    pub fn reconcile(&mut self, run_id: &str) -> Result<usize> {
        let attempts = self.conn.execute(
            "UPDATE attempts SET state='error', finished_at=?2, interruption='Interrupted when the controller stopped' WHERE run_id=?1 AND state='running'",
            params![run_id, now()],
        )?;
        let tasks = self.conn.execute(
            "UPDATE tasks SET state=?2, note=?3 WHERE run_id=?1 AND state=?4",
            params![
                run_id,
                TaskState::Open.as_str(),
                "Returned to the queue after the controller stopped",
                TaskState::Running.as_str()
            ],
        )?;
        self.conn.execute(
            "UPDATE runs SET finished_at=NULL WHERE id=?1",
            params![run_id],
        )?;
        Ok(attempts.max(tasks))
    }

    /// Tasks whose dependencies have all merged. A task whose dependency failed or
    /// blocked can never run, so it is blocked here rather than waiting forever.
    pub fn ready(&mut self, run_id: &str) -> Result<Vec<Task>> {
        let tasks = self.tasks(run_id)?;
        let states: BTreeMap<&str, TaskState> =
            tasks.iter().map(|t| (t.id.as_str(), t.state)).collect();
        let mut ready = Vec::new();
        let mut block = Vec::new();
        for task in &tasks {
            if task.state != TaskState::Open {
                continue;
            }
            let dependencies: Vec<TaskState> = task
                .depends_on
                .iter()
                .filter_map(|d| states.get(d.as_str()).copied())
                .collect();
            if dependencies
                .iter()
                .any(|s| matches!(s, TaskState::Failed | TaskState::Blocked))
            {
                block.push(task.id.clone());
            } else if dependencies.iter().all(|s| *s == TaskState::Merged) {
                ready.push(task.clone());
            }
        }
        for id in block {
            self.set_state(
                run_id,
                &id,
                TaskState::Blocked,
                "A dependency failed or is blocked",
            )?;
        }
        Ok(ready)
    }

    pub fn set_state(
        &mut self,
        run_id: &str,
        task_id: &str,
        state: TaskState,
        note: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET state=?3, note=?4 WHERE run_id=?1 AND id=?2",
            params![run_id, task_id, state.as_str(), note],
        )?;
        Ok(())
    }

    /// Claim a task for dispatch. Returns false when another dispatcher pass already took
    /// it, which keeps the pull model safe under concurrency.
    pub fn claim(&mut self, run_id: &str, task_id: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE tasks SET state=?3, attempts=attempts+1 WHERE run_id=?1 AND id=?2 AND state=?4",
            params![
                run_id,
                task_id,
                TaskState::Running.as_str(),
                TaskState::Open.as_str()
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn start_attempt(&mut self, attempt: &Attempt) -> Result<()> {
        self.conn.execute(
            "INSERT INTO attempts(id,run_id,task_id,provider,branch,base_commit,started_at,state) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![attempt.id, attempt.run_id, attempt.task_id, attempt.provider, attempt.branch, attempt.base_commit, attempt.started_at, attempt.state.as_str()],
        )?;
        Ok(())
    }

    pub fn finish_attempt(&mut self, attempt: &Attempt) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET finished_at=?2, native_exit=?3, interruption=?4, passed=?5, score=?6, detail=?7, files_changed=?8, output=?9, state=?10 WHERE id=?1",
            params![
                attempt.id,
                attempt.finished_at,
                attempt.native_exit,
                attempt.interruption,
                attempt.passed,
                attempt.score,
                attempt.detail,
                serde_json::to_string(&attempt.files_changed)?,
                attempt.output,
                attempt.state.as_str()
            ],
        )?;
        Ok(())
    }

    /// Record what a running attempt is doing, so a watcher can look in on it.
    pub fn set_activity(&mut self, attempt_id: &str, activity: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET activity=?2 WHERE id=?1",
            params![attempt_id, activity],
        )?;
        Ok(())
    }

    /// Decide a proposed change to the graph.
    ///
    /// Every proposal is recorded whether accepted or not: a rejected one is evidence about
    /// the proposer, and silently dropping it would hide that an agent kept asking for
    /// something the controller would never allow.
    pub fn propose(
        &mut self,
        run_id: &str,
        author: &str,
        mutation: &Mutation,
    ) -> Result<std::result::Result<String, String>> {
        let verdict = self.decide(run_id, mutation);
        let (accepted, reason) = match &verdict {
            Ok(accepted) => (true, accepted.clone()),
            Err(rejected) => (false, rejected.clone()),
        };
        self.conn.execute(
            "INSERT INTO mutations(run_id,author,kind,target,detail,accepted,reason,at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                run_id,
                author,
                mutation.kind(),
                mutation.target(),
                serde_json::to_string(mutation)?,
                accepted,
                reason,
                now()
            ],
        )?;
        Ok(verdict)
    }

    /// Apply a mutation if it is safe. Returns why it was refused otherwise.
    fn decide(
        &mut self,
        run_id: &str,
        mutation: &Mutation,
    ) -> std::result::Result<String, String> {
        let tasks = self.tasks(run_id).map_err(|e| e.to_string())?;
        let known: BTreeSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        match mutation {
            Mutation::Add { task } => {
                if tasks.len() >= MAX_TASKS_PER_RUN {
                    return Err(format!(
                        "The run already holds {} tasks, the limit",
                        tasks.len()
                    ));
                }
                if known.contains(task.id.as_str()) {
                    return Err(format!("A task {} already exists", task.id));
                }
                if task.verify.as_ref().is_none_or(|v| v.is_empty()) {
                    return Err("A new task needs its own verify command".into());
                }
                for dependency in &task.depends_on {
                    if !known.contains(dependency.as_str()) {
                        return Err(format!("Depends on unknown task {dependency}"));
                    }
                }
                // Validate the whole graph with the addition in place, so an added task
                // cannot introduce a cycle or an invalid id.
                let mut specs: Vec<TaskSpec> = tasks.iter().map(task_to_spec).collect();
                specs.push(task.clone());
                validate_graph(&RunSpec {
                    objective: "check".into(),
                    tasks: specs,
                })
                .map_err(|e| e.to_string())?;

                self.conn.execute(
                    "INSERT INTO tasks(id,run_id,title,brief,acceptance,depends_on,class,provider,verify,files,must_fail,state) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    params![
                        task.id, run_id, task.title, task.brief,
                        serde_json::to_string(&task.acceptance).unwrap_or_default(),
                        serde_json::to_string(&task.depends_on).unwrap_or_default(),
                        task.class, task.provider,
                        task.verify.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default()),
                        serde_json::to_string(&task.files).unwrap_or_else(|_| "[]".into()),
                        task.must_fail.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default()),
                        TaskState::Open.as_str()
                    ],
                ).map_err(|e| e.to_string())?;
                Ok(format!("Added {}", task.id))
            }
            Mutation::Block { task, reason } => {
                let Some(existing) = tasks.iter().find(|t| &t.id == task) else {
                    return Err(format!("Unknown task {task}"));
                };
                if existing.state.terminal() {
                    return Err(format!("{task} has already finished"));
                }
                self.set_state(run_id, task, TaskState::Blocked, reason)
                    .map_err(|e| e.to_string())?;
                Ok(format!("Blocked {task}"))
            }
            Mutation::DependOn { task, on } => {
                let Some(existing) = tasks.iter().find(|t| &t.id == task) else {
                    return Err(format!("Unknown task {task}"));
                };
                if !known.contains(on.as_str()) {
                    return Err(format!("Unknown task {on}"));
                }
                if existing.state != TaskState::Open {
                    return Err(format!("{task} is no longer open"));
                }
                let mut depends_on = existing.depends_on.clone();
                if depends_on.contains(on) {
                    return Err(format!("{task} already depends on {on}"));
                }
                depends_on.push(on.clone());
                // Reject an edge that would make the graph unsatisfiable.
                let mut specs: Vec<TaskSpec> = tasks.iter().map(task_to_spec).collect();
                if let Some(spec) = specs.iter_mut().find(|s| &s.id == task) {
                    spec.depends_on = depends_on.clone();
                }
                validate_graph(&RunSpec {
                    objective: "check".into(),
                    tasks: specs,
                })
                .map_err(|e| e.to_string())?;

                self.conn
                    .execute(
                        "UPDATE tasks SET depends_on=?3 WHERE run_id=?1 AND id=?2",
                        params![
                            run_id,
                            task,
                            serde_json::to_string(&depends_on).unwrap_or_default()
                        ],
                    )
                    .map_err(|e| e.to_string())?;
                Ok(format!("{task} now waits for {on}"))
            }
        }
    }

    /// Every proposal made against a run, accepted or not.
    pub fn mutations(&self, run_id: &str) -> Result<Vec<Value>> {
        let mut query = self.conn.prepare(
            "SELECT author,kind,target,accepted,reason,at FROM mutations WHERE run_id=?1 ORDER BY at",
        )?;
        let rows = query.query_map([run_id], |r| {
            Ok(serde_json::json!({
                "author": r.get::<_, String>(0)?,
                "kind": r.get::<_, String>(1)?,
                "target": r.get::<_, String>(2)?,
                "accepted": r.get::<_, bool>(3)?,
                "reason": r.get::<_, String>(4)?,
                "at": r.get::<_, u64>(5)?,
            }))
        })?;
        rows.map(|r| r.map_err(anyhow::Error::from)).collect()
    }

    /// Everything consumed across every run, per provider and metric. Percentages of a
    /// rolling window are additive only loosely — the window moves — so they are reported
    /// as a total consumed, not as a current level.
    pub fn usage_totals(&self) -> Result<Vec<UsageTotal>> {
        let runs: Vec<String> = {
            let mut query = self.conn.prepare("SELECT id FROM runs ORDER BY created_at")?;
            let rows = query.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()?
        };
        let mut totals: BTreeMap<(String, String, String), (f64, usize)> = BTreeMap::new();
        for run in runs {
            for sample in self.usage_consumed(&run)? {
                let entry = totals
                    .entry((sample.provider, sample.metric, sample.label))
                    .or_insert((0.0, 0));
                entry.0 += sample.value;
                entry.1 += 1;
            }
        }
        Ok(totals
            .into_iter()
            .map(|((provider, metric, label), (value, runs))| UsageTotal {
                provider,
                metric,
                label,
                value,
                runs,
            })
            .collect())
    }

    /// What each provider has actually done, from the attempts ledger. This is the
    /// evidence routing uses: real outcomes and real durations, rather than a model's
    /// opinion of its own capability.
    pub fn provider_stats(&self, since: u64) -> Result<BTreeMap<String, Stats>> {
        let mut query = self.conn.prepare(
            "SELECT provider,state,started_at,finished_at FROM attempts WHERE started_at>=?1",
        )?;
        let rows = query.query_map([since], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u64>(2)?,
                r.get::<_, Option<u64>>(3)?,
            ))
        })?;
        let mut stats: BTreeMap<String, Stats> = BTreeMap::new();
        for row in rows {
            let (provider, state, started, finished) = row?;
            let entry = stats.entry(provider).or_default();
            entry.attempts += 1;
            if state == "verified" {
                entry.verified += 1;
                // Only successful work tells you how long the provider takes to succeed.
                if let Some(finished) = finished {
                    entry.durations.push(finished.saturating_sub(started));
                }
            }
        }
        Ok(stats)
    }

    /// Store raw readings rather than differences, so later analysis can ask questions we
    /// have not thought of yet.
    pub fn record_usage(
        &mut self,
        run_id: &str,
        phase: &str,
        samples: &[super::usage::Sample],
    ) -> Result<()> {
        let transaction = self.conn.transaction()?;
        for sample in samples {
            transaction.execute(
                "INSERT INTO usage_samples(run_id,phase,provider,metric,label,value,at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![run_id, phase, sample.provider, sample.metric, sample.label, sample.value, now()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// What a run consumed: differences between the before and after readings, plus any
    /// figure recorded by hand. Codex reports through app-server, which a board run does
    /// not hold open, so its cost is entered manually from what the operator can see.
    pub fn usage_consumed(&self, run_id: &str) -> Result<Vec<super::usage::Sample>> {
        let read = |phase: &str| -> Result<Vec<super::usage::Sample>> {
            let mut query = self.conn.prepare(
                "SELECT provider,metric,label,value FROM usage_samples WHERE run_id=?1 AND phase=?2",
            )?;
            let rows = query.query_map(params![run_id, phase], |r| {
                Ok(super::usage::Sample {
                    provider: r.get(0)?,
                    metric: r.get(1)?,
                    label: r.get(2)?,
                    value: r.get(3)?,
                })
            })?;
            rows.map(|r| r.map_err(anyhow::Error::from)).collect()
        };
        let mut consumed = super::usage::consumed(&read("before")?, &read("after")?);
        consumed.extend(read("manual")?);
        Ok(consumed)
    }

    /// Keep what the observer actually replied, so a silent observer can be diagnosed.
    pub fn set_observation(&mut self, attempt_id: &str, reply: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET observation=?2 WHERE id=?1",
            params![attempt_id, reply],
        )?;
        Ok(())
    }

    pub fn attempts(&self, run_id: &str) -> Result<Vec<Value>> {
        let mut query = self.conn.prepare(
            "SELECT id,task_id,provider,branch,base_commit,started_at,finished_at,native_exit,interruption,passed,score,detail,files_changed,output,activity,state FROM attempts WHERE run_id=?1 ORDER BY started_at",
        )?;
        let rows = query.query_map([run_id], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?,
                "task_id": r.get::<_, String>(1)?,
                "provider": r.get::<_, String>(2)?,
                "branch": r.get::<_, String>(3)?,
                "base_commit": r.get::<_, String>(4)?,
                "started_at": r.get::<_, u64>(5)?,
                "finished_at": r.get::<_, Option<u64>>(6)?,
                "native_exit": r.get::<_, Option<i32>>(7)?,
                "interruption": r.get::<_, Option<String>>(8)?,
                "passed": r.get::<_, Option<bool>>(9)?,
                "score": r.get::<_, Option<f64>>(10)?,
                "detail": r.get::<_, String>(11)?,
                "files_changed": serde_json::from_str::<Value>(&r.get::<_, String>(12)?).unwrap_or(Value::Null),
                "output": r.get::<_, String>(13)?,
                "activity": r.get::<_, String>(14)?,
                "state": r.get::<_, String>(15)?,
            }))
        })?;
        rows.map(|r| r.map_err(anyhow::Error::from)).collect()
    }

    /// Agent runs started since `since`, optionally for one provider. The attempts table
    /// is the ledger, so the rolling window spans runs and survives restarts — starting a
    /// new run never replenishes an allowance.
    pub fn recent_runs(&self, provider: Option<&str>, since: u64) -> Result<usize> {
        let count = match provider {
            Some(provider) => self.conn.query_row(
                "SELECT COUNT(*) FROM attempts WHERE provider=?1 AND started_at>=?2",
                params![provider, since],
                |r| r.get::<_, usize>(0),
            ),
            None => self.conn.query_row(
                "SELECT COUNT(*) FROM attempts WHERE started_at>=?1",
                params![since],
                |r| r.get::<_, usize>(0),
            ),
        }?;
        Ok(count)
    }

    pub fn cooldown(&self, provider: &str) -> Result<u64> {
        Ok(self
            .conn
            .query_row(
                "SELECT until FROM cooldowns WHERE provider=?1",
                [provider],
                |r| r.get::<_, u64>(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Hold one provider after a response that looks rate-limited. Other providers are
    /// unaffected: a limit on one account is not evidence about another.
    pub fn set_cooldown(&mut self, provider: &str, until: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO cooldowns(provider,until) VALUES(?1,?2) ON CONFLICT(provider) DO UPDATE SET until=excluded.until",
            params![provider, until],
        )?;
        Ok(())
    }

    pub fn latest_run(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM runs ORDER BY created_at DESC LIMIT 1", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?)
    }
}

/// A stored task back in authoring form, for revalidating the graph after a change.
fn task_to_spec(task: &Task) -> TaskSpec {
    TaskSpec {
        id: task.id.clone(),
        title: task.title.clone(),
        brief: task.brief.clone(),
        acceptance: task.acceptance.clone(),
        depends_on: task.depends_on.clone(),
        class: task.class.clone(),
        provider: task.provider.clone(),
        verify: task.verify.clone(),
        files: task.files.clone(),
        must_fail: task.must_fail.clone(),
    }
}

/// Add a column to an existing table if it is not already present.
fn add_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let present = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<String>>>()?
        .iter()
        .any(|name| name == column);
    if !present {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
    Ok(())
}


/// Validate an authored graph. Unknown dependencies, duplicate ids and cycles are
/// authoring errors, and a run that cannot finish should never start. Public so a manager's
/// proposed graph can be checked before it becomes a run.
pub fn validate_graph(spec: &RunSpec) -> Result<()> {
    ensure!(
            !spec.objective.trim().is_empty() && spec.objective.len() <= 12000,
            "Objective must contain 1-12000 bytes"
        );
    ensure!(
            !spec.tasks.is_empty() && spec.tasks.len() <= 200,
            "A run needs 1-200 tasks"
        );
        let mut ids = BTreeSet::new();
    for task in &spec.tasks {
        ensure!(
                !task.id.trim().is_empty()
                    && task.id.len() <= 64
                    && task
                        .id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "Invalid task id: {}",
                task.id
            );
        ensure!(ids.insert(task.id.clone()), "Duplicate task id: {}", task.id);
        ensure!(
                !task.title.trim().is_empty() && !task.brief.trim().is_empty(),
                "Task {} needs a title and a brief",
                task.id
            );
    }
    for task in &spec.tasks {
        for dependency in &task.depends_on {
        ensure!(
                    ids.contains(dependency),
                    "Task {} depends on unknown task {dependency}",
                    task.id
                );
        ensure!(dependency != &task.id, "Task {} depends on itself", task.id);
        }
    }
    detect_cycle(&spec.tasks)?;
    Ok(())
}

/// Depth-first cycle detection over the authored graph.
fn detect_cycle(tasks: &[TaskSpec]) -> Result<()> {
    let edges: BTreeMap<&str, &Vec<String>> = tasks
        .iter()
        .map(|t| (t.id.as_str(), &t.depends_on))
        .collect();
    let mut done = BTreeSet::new();
    let mut stack = BTreeSet::new();
    fn visit<'a>(
        node: &'a str,
        edges: &BTreeMap<&'a str, &'a Vec<String>>,
        done: &mut BTreeSet<&'a str>,
        stack: &mut BTreeSet<&'a str>,
    ) -> Result<()> {
        if done.contains(node) {
            return Ok(());
        }
        ensure!(stack.insert(node), "Dependency cycle involving task {node}");
        if let Some(dependencies) = edges.get(node) {
            for dependency in dependencies.iter() {
                let key = edges
                    .keys()
                    .find(|k| **k == dependency.as_str())
                    .copied()
                    .context("Unknown dependency")?;
                visit(key, edges, done, stack)?;
            }
        }
        stack.remove(node);
        done.insert(node);
        Ok(())
    }
    for task in tasks {
        visit(task.id.as_str(), &edges, &mut done, &mut stack)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(tasks: Vec<TaskSpec>) -> RunSpec {
        RunSpec {
            objective: "Test objective".into(),
            tasks,
        }
    }
    fn task(id: &str, depends_on: &[&str]) -> TaskSpec {
        TaskSpec {
            id: id.into(),
            title: format!("Task {id}"),
            brief: "Do the thing".into(),
            acceptance: vec!["It works".into()],
            depends_on: depends_on.iter().map(|s| (*s).into()).collect(),
            class: implement(),
            provider: None,
            verify: None,
            files: Vec::new(),
            must_fail: None,
        }
    }
    fn board() -> (tempfile::TempDir, Board) {
        let dir = tempfile::tempdir().unwrap();
        let board = Board::open(&dir.path().join("board.db")).unwrap();
        (dir, board)
    }

    #[test]
    fn invalid_graphs_are_rejected_before_a_run_exists() {
        let (_dir, mut board) = board();
        assert!(
            board
                .create_run("run-1", &spec(vec![task("a", &["missing"])]), "abc", "firm/run")
                .unwrap_err()
                .to_string()
                .contains("unknown task")
        );
        assert!(
            board
                .create_run("run-1", &spec(vec![task("a", &["b"]), task("b", &["a"])]), "abc", "firm/run")
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
        assert!(
            board
                .create_run("run-1", &spec(vec![task("a", &[]), task("a", &[])]), "abc", "firm/run")
                .unwrap_err()
                .to_string()
                .contains("Duplicate")
        );
        assert!(board.create_run("run-1", &spec(vec![]), "abc", "firm/run").is_err());
        assert_eq!(board.latest_run().unwrap(), None);
    }

    #[test]
    fn readiness_follows_dependencies_and_claiming_is_exclusive() {
        let (_dir, mut board) = board();
        let run = "run-1";
        board
            .create_run(
                run,
                &spec(vec![task("a", &[]), task("b", &["a"]), task("c", &["b"])]),
                "abc",
                "firm/run",
            )
            .unwrap();
        let ready: Vec<String> = board.ready(run).unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(ready, ["a"], "only the root task is ready");

        assert!(board.claim(run, "a").unwrap());
        assert!(!board.claim(run, "a").unwrap(), "a claimed task is not re-claimable");
        assert!(board.ready(run).unwrap().is_empty());

        board.set_state(run, "a", TaskState::Merged, "").unwrap();
        let ready: Vec<String> = board.ready(run).unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(ready, ["b"]);

        // A failed dependency blocks everything downstream instead of stalling it.
        board.set_state(run, "b", TaskState::Failed, "gave up").unwrap();
        assert!(board.ready(run).unwrap().is_empty());
        let c = board.tasks(run).unwrap().into_iter().find(|t| t.id == "c").unwrap();
        assert_eq!(c.state, TaskState::Blocked);
        assert_eq!(board.tasks(run).unwrap()[0].attempts, 1);
    }

    fn added(id: &str, depends_on: &[&str]) -> TaskSpec {
        let mut spec = task(id, depends_on);
        spec.verify = Some(vec!["true".into()]);
        spec
    }

    #[test]
    fn a_proposal_can_extend_the_graph_and_the_dispatcher_sees_it() {
        let (_dir, mut board) = board();
        let run = "run-1";
        board.create_run(run, &spec(vec![task("a", &[])]), "abc", "firm/run").unwrap();

        // Work the plan did not anticipate, discovered while running.
        let outcome = board
            .propose(run, "grok", &Mutation::Add { task: added("b", &["a"]) })
            .unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(board.tasks(run).unwrap().len(), 2);

        // It is not ready until its dependency has merged — the same rule as any task.
        assert_eq!(board.ready(run).unwrap().len(), 1, "only a is ready");
        board.set_state(run, "a", TaskState::Merged, "").unwrap();
        let ready: Vec<String> = board.ready(run).unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(ready, ["b"], "the added task is dispatched like any other");
    }

    #[test]
    fn proposals_that_would_break_the_graph_are_refused() {
        let (_dir, mut board) = board();
        let run = "run-1";
        board
            .create_run(run, &spec(vec![task("a", &[]), task("b", &["a"])]), "abc", "firm/run")
            .unwrap();
        let refuse = |board: &mut Board, m: Mutation| board.propose(run, "grok", &m).unwrap().unwrap_err();

        assert!(refuse(&mut board, Mutation::Add { task: added("a", &[]) }).contains("already exists"));
        assert!(refuse(&mut board, Mutation::Add { task: added("c", &["nope"]) }).contains("unknown task"));
        // A task nothing can judge is worse than no task.
        assert!(refuse(&mut board, Mutation::Add { task: task("c", &[]) }).contains("verify"));
        // A cycle would make the run unfinishable.
        assert!(refuse(&mut board, Mutation::DependOn { task: "a".into(), on: "b".into() }).contains("cycle"));
        assert!(refuse(&mut board, Mutation::DependOn { task: "a".into(), on: "ghost".into() }).contains("Unknown"));
        assert!(refuse(&mut board, Mutation::Block { task: "ghost".into(), reason: "x".into() }).contains("Unknown"));

        // Finished work is not revised.
        board.set_state(run, "a", TaskState::Merged, "").unwrap();
        assert!(refuse(&mut board, Mutation::Block { task: "a".into(), reason: "too late".into() }).contains("already finished"));

        // Every refusal is on the record, not silently dropped.
        let proposals = board.mutations(run).unwrap();
        assert_eq!(proposals.len(), 7);
        assert!(proposals.iter().all(|p| p["accepted"] == false));
        assert!(proposals.iter().all(|p| p["author"] == "grok"));
    }

    #[test]
    fn the_graph_cannot_grow_without_limit() {
        // A proposal commits budget, so an agent that keeps adding work must hit a wall.
        let (_dir, mut board) = board();
        let run = "run-1";
        board.create_run(run, &spec(vec![task("seed", &[])]), "abc", "firm/run").unwrap();
        let mut accepted = 1;
        for index in 0..MAX_TASKS_PER_RUN + 5 {
            let proposal = Mutation::Add { task: added(&format!("t{index}"), &[]) };
            if board.propose(run, "grok", &proposal).unwrap().is_ok() {
                accepted += 1;
            }
        }
        assert_eq!(accepted, MAX_TASKS_PER_RUN, "capped at the limit");
        assert_eq!(board.tasks(run).unwrap().len(), MAX_TASKS_PER_RUN);
    }

    #[test]
    fn independent_tasks_are_all_ready_at_once() {
        let (_dir, mut board) = board();
        let run = "run-1";
        board
            .create_run(
                run,
                &spec(vec![task("a", &[]), task("b", &[]), task("c", &["a", "b"])]),
                "abc",
                "firm/run",
            )
            .unwrap();
        assert_eq!(board.ready(run).unwrap().len(), 2, "partition mode runs a and b in parallel");
        board.set_state(run, "a", TaskState::Merged, "").unwrap();
        assert_eq!(board.ready(run).unwrap().len(), 1, "c still waits for b");
        board.set_state(run, "b", TaskState::Merged, "").unwrap();
        let ready: Vec<String> = board.ready(run).unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(ready, ["c"]);
        assert_eq!(board.run(run).unwrap().integration_branch, "firm/run");
        assert_eq!(board.latest_run().unwrap().as_deref(), Some(run));
    }
}
