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

pub struct Board {
    conn: Connection,
}

impl Board {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS runs (id TEXT PRIMARY KEY, objective TEXT NOT NULL, base_commit TEXT NOT NULL, integration_branch TEXT NOT NULL, created_at INTEGER NOT NULL, finished_at INTEGER);
            CREATE TABLE IF NOT EXISTS tasks (id TEXT NOT NULL, run_id TEXT NOT NULL, title TEXT NOT NULL, brief TEXT NOT NULL, acceptance TEXT NOT NULL, depends_on TEXT NOT NULL, class TEXT NOT NULL, provider TEXT, verify TEXT, state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, note TEXT NOT NULL DEFAULT '', PRIMARY KEY (run_id, id));
            CREATE TABLE IF NOT EXISTS attempts (id TEXT PRIMARY KEY, run_id TEXT NOT NULL, task_id TEXT NOT NULL, provider TEXT NOT NULL, branch TEXT NOT NULL, base_commit TEXT NOT NULL, started_at INTEGER NOT NULL, finished_at INTEGER, native_exit INTEGER, interruption TEXT, passed INTEGER, score REAL, detail TEXT NOT NULL DEFAULT '', files_changed TEXT NOT NULL DEFAULT '[]', output TEXT NOT NULL DEFAULT '', activity TEXT NOT NULL DEFAULT '', observation TEXT NOT NULL DEFAULT '', state TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS attempts_task ON attempts(run_id, task_id);
            CREATE INDEX IF NOT EXISTS attempts_usage ON attempts(provider, started_at);
            CREATE TABLE IF NOT EXISTS cooldowns (provider TEXT PRIMARY KEY, until INTEGER NOT NULL);",
        )?;
        // `CREATE TABLE IF NOT EXISTS` never alters an existing table, so columns added
        // after a board was created must be migrated in explicitly.
        add_column(&conn, "attempts", "activity", "TEXT NOT NULL DEFAULT ''")?;
        add_column(&conn, "attempts", "observation", "TEXT NOT NULL DEFAULT ''")?;
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
                "INSERT INTO tasks(id,run_id,title,brief,acceptance,depends_on,class,provider,verify,state) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
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
            "SELECT id,run_id,title,brief,acceptance,depends_on,class,provider,verify,state,attempts,note FROM tasks WHERE run_id=?1 ORDER BY rowid",
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
                r.get::<_, usize>(10)?,
                r.get::<_, String>(11)?,
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
                state: TaskState::parse(&row.9)?,
                attempts: row.10,
                note: row.11,
            })
        })
        .collect()
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
