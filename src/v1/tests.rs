//! End-to-end tests for the v1 engine, driven by a fake agent CLI so the whole
//! board → worktree → agent → scorer → merge pipeline runs without model credits.

use super::{
    board::{Attempt, AttemptState, Board, RunSpec, TaskSpec, TaskState},
    dispatch::Engine,
    scorer::Scorer,
    worktree::{git, tests::repo},
};
use crate::config::{Config, PromptInput, Provider};
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc};
use tokio::sync::watch;

struct Harness {
    _workspace: tempfile::TempDir,
    _state: tempfile::TempDir,
    workspace_path: std::path::PathBuf,
    config: Config,
    board_path: std::path::PathBuf,
}

async fn harness() -> Harness {
    let workspace = repo().await;
    let state = tempfile::tempdir().unwrap();
    let script = state.path().join("fake-agent");
    std::fs::write(&script, include_str!("../../tests/fixtures/v1-worker.sh")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut config = Config::read(Path::new("firm.toml")).unwrap();
    config.workspace = workspace.path().to_path_buf();
    config.state_dir = state.path().to_path_buf();
    // Generous by default so only the tests that are about budgets are limited by them.
    config.allowances.worker_runs = 50;
    config.forum_observer = "fake".into();
    config.providers = vec![Provider {
        id: "fake".into(),
        name: "Fake".into(),
        command: script.display().to_string(),
        args: vec![],
        input: PromptInput::Stdin,
        enabled: true,
        worker: true,
        max_runs: 12,
        tier: 0,
        max_concurrent: 3,
        description: "Test agent".into(),
        meeting_args: None,
        manager_args: None,
        observer_args: None,
        reviewer_args: None,
        validator_args: None,
        planner_args: None,
        resume_args: None,
        worker_timeout_seconds: None,
        idle_timeout_seconds: None,
    }];
    Harness {
        workspace_path: workspace.path().to_path_buf(),
        board_path: state.path().join("board.db"),
        config,
        _workspace: workspace,
        _state: state,
    }
}

fn task(id: &str, brief: &str, depends_on: &[&str]) -> TaskSpec {
    TaskSpec {
        id: id.into(),
        title: format!("Task {id}"),
        brief: brief.into(),
        acceptance: vec!["The controller check passes".into()],
        depends_on: depends_on.iter().map(|s| (*s).into()).collect(),
        class: "implement".into(),
        provider: Some("fake".into()),
        verify: None,
        files: Vec::new(),
        must_fail: None,
    }
}

/// Passes unless a sentinel file is present, so a task can deliberately fail the check.
fn scorer() -> Scorer {
    Scorer::Command {
        command: vec!["sh".into(), "-c".into(), "! test -f BROKEN".into()],
        timeout_seconds: 30,
    }
}

#[tokio::test]
async fn a_provider_may_set_its_own_timeouts() {
    // A slow agent under a provider-specific idle limit is cut off, while the run-wide
    // allowance is generous. One global limit cannot express that.
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    harness.config.allowances.worker_timeout_seconds = 600;
    harness.config.allowances.idle_timeout_seconds = 600;
    harness.config.providers[0].idle_timeout_seconds = Some(5);

    let script = harness.config.providers[0].command.clone();
    std::fs::write(
        &script,
        "#!/bin/sh\necho '{\"payload_type\":\"start\"}'\nsleep 300\n",
    )
    .unwrap();

    let spec = RunSpec {
        brief: String::new(),
        objective: "Respect a provider's own limits".into(),
        tasks: vec![task("slow", "CREATE:slow.txt", &[])],
        validation: Default::default(),
    };
    let started = std::time::Instant::now();
    let (run_id, board) = drive(&harness, spec).await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(120),
        "the provider's 5s idle limit applied, not the run's 600s"
    );
    let attempts = board.attempts(&run_id).unwrap();
    assert!(
        attempts[0]["interruption"]
            .as_str()
            .unwrap_or_default()
            .contains("went quiet"),
        "{:?}",
        attempts[0]["interruption"]
    );
}

#[tokio::test]
async fn the_controller_publishes_outcomes_and_a_later_agent_is_shown_them() {
    let mut harness = harness().await;
    harness.config.forum_observer = String::new(); // no observer model in tests
    // The fake agent echoes its prompt into the file it writes, so we can prove what the
    // second agent was actually told.
    let spec = RunSpec {
        brief: String::new(),
        validation: Default::default(),
        objective: "Share what happened".into(),
        tasks: vec![
            task("first", "CREATE:first.txt", &[]),
            task("second", "CREATE:second.txt", &["first"]),
        ],
    };
    let (run_id, board) = drive(&harness, spec).await;

    let entries = board.forum().entries(&run_id).unwrap();
    assert!(!entries.is_empty(), "the controller writes without any agent cooperation");
    let first = entries.iter().find(|e| e.task_id.as_deref() == Some("first")).unwrap();
    assert_eq!(first.author, "controller");
    assert!(first.title.contains("first"), "{}", first.title);
    assert!(first.body.contains("first.txt"), "records the evidence: {}", first.body);

    // The second task, dispatched after the first merged, is shown the first's entry —
    // and is not shown notes about its own task.
    let slice = board.forum().slice_for(&run_id, "second", 8192, false).unwrap();
    assert!(slice.contains("first"), "{slice}");
    assert!(!slice.contains("second"), "an agent is not shown notes about its own task");
}

#[tokio::test]
async fn an_observer_can_change_the_plan_and_the_dispatcher_runs_the_new_task() {
    // The bridge from shared cognition to shared execution state: something learned while
    // working becomes work that actually gets done.
    let mut harness = harness().await;
    harness.config.forum_observer = "fake".into();
    harness.config.providers[0].observer_args = Some(vec![]);

    let spec = RunSpec {
        brief: String::new(),
        objective: "Let what is learned change what is done".into(),
        tasks: vec![task("propose-followup", "CREATE:first.txt", &[])],
        validation: Default::default(),
    };
    let (run_id, board) = drive(&harness, spec).await;

    let tasks = board.tasks(&run_id).unwrap();
    assert_eq!(tasks.len(), 2, "the observer's task joined the graph: {tasks:?}");
    let followup = tasks.iter().find(|t| t.id == "followup").expect("added task");
    assert_eq!(
        followup.state,
        TaskState::Merged,
        "and the dispatcher ran it like any other: {}",
        followup.note
    );

    let proposals = board.mutations(&run_id).unwrap();
    assert!(proposals.iter().any(|p| p["accepted"] == true && p["kind"] == "add"));
    assert!(
        proposals[0]["author"].as_str().unwrap().contains("observer"),
        "recorded against the proposer: {:?}",
        proposals[0]["author"]
    );
}

#[tokio::test]
async fn a_task_its_dependencies_already_satisfied_is_not_failed_for_doing_nothing() {
    // Found in a live trial: a final integration task had nothing to do, because the tasks
    // it depended on had already satisfied its check. The agent correctly changed nothing
    // and was failed for it. The check decides, not the diff.
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    let mut first = task("does-the-work", "CREATE:done.txt", &[]);
    first.verify = Some(vec!["sh".into(), "-c".into(), "test -f done.txt".into()]);
    // Nothing for this one to do: its check already passes once the first has merged.
    let mut second = task("nothing-to-do", "The work is already done.", &["does-the-work"]);
    second.verify = Some(vec!["sh".into(), "-c".into(), "test -f done.txt".into()]);

    let spec = RunSpec {
        brief: String::new(),
        objective: "A task with nothing left to do".into(),
        tasks: vec![first, second],
        validation: Default::default(),
    };
    let (run_id, board) = drive(&harness, spec).await;

    let tasks = board.tasks(&run_id).unwrap();
    for task in &tasks {
        assert_eq!(task.state, TaskState::Merged, "{} — {}", task.id, task.note);
    }
    let attempts = board.attempts(&run_id).unwrap();
    let quiet = attempts.iter().find(|a| a["task_id"] == "nothing-to-do").unwrap();
    assert_eq!(quiet["passed"], true);
    assert_eq!(quiet["files_changed"].as_array().unwrap().len(), 0);
    assert!(
        quiet["detail"].as_str().unwrap().contains("already passes"),
        "{:?}",
        quiet["detail"]
    );
}

#[tokio::test]
async fn an_agent_that_edits_a_file_outside_its_scope_is_rejected() {
    // The obvious way to pass a test you cannot satisfy is to edit the test. Declared
    // scope makes that a rejection rather than something nobody notices.
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    let mut scoped = task("scoped", "CREATE:forbidden.txt", &[]);
    scoped.files = vec!["allowed.txt".into()];
    scoped.verify = Some(vec!["sh".into(), "-c".into(), "test -f forbidden.txt".into()]);

    let spec = RunSpec {
        brief: String::new(),
        objective: "Stay in your lane".into(),
        tasks: vec![scoped],
        validation: Default::default(),
    };
    let (run_id, board) = drive(&harness, spec).await;

    let tasks = board.tasks(&run_id).unwrap();
    assert_eq!(tasks[0].state, TaskState::Failed, "{}", tasks[0].note);
    let attempts = board.attempts(&run_id).unwrap();
    assert_eq!(attempts[0]["state"], "rejected");
    let detail = attempts[0]["detail"].as_str().unwrap();
    assert!(detail.contains("forbidden.txt"), "{detail}");
    assert!(detail.contains("may not touch"), "{detail}");
}

#[tokio::test]
async fn a_declared_file_and_an_exempt_lockfile_are_both_allowed() {
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    let mut scoped = task("scoped", "CREATE:allowed.txt", &[]);
    scoped.files = vec!["allowed.txt".into()];
    let spec = RunSpec {
        brief: String::new(),
        objective: "Declared files are fine".into(),
        tasks: vec![scoped],
        validation: Default::default(),
    };
    let (run_id, board) = drive(&harness, spec).await;
    assert_eq!(board.tasks(&run_id).unwrap()[0].state, TaskState::Merged);
}

#[tokio::test]
async fn a_test_that_passes_against_unwritten_code_is_rejected() {
    // A new test which already succeeds asserts nothing. Without this the team can write
    // its own exam and leave it blank.
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    let mut seam = task("seam", "CREATE:seam.txt", &[]);
    seam.verify = Some(vec!["sh".into(), "-c".into(), "test -f seam.txt".into()]);
    // The proof-of-failure command succeeds, so the "test" demonstrates nothing.
    seam.must_fail = Some(vec!["true".into()]);

    let spec = RunSpec {
        brief: String::new(),
        objective: "A test must actually test something".into(),
        tasks: vec![seam],
        validation: Default::default(),
    };
    let (run_id, board) = drive(&harness, spec).await;

    assert_eq!(board.tasks(&run_id).unwrap()[0].state, TaskState::Failed);
    let detail = board.attempts(&run_id).unwrap()[0]["detail"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(detail.contains("proves nothing"), "{detail}");
}

#[tokio::test]
async fn a_test_that_genuinely_fails_first_is_accepted() {
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    let mut seam = task("seam", "CREATE:seam.txt", &[]);
    seam.verify = Some(vec!["sh".into(), "-c".into(), "test -f seam.txt".into()]);
    // Still fails, because the thing it checks for has not been built.
    seam.must_fail = Some(vec!["sh".into(), "-c".into(), "test -f not-built-yet".into()]);

    let spec = RunSpec {
        brief: String::new(),
        objective: "A real test is accepted".into(),
        tasks: vec![seam],
        validation: Default::default(),
    };
    let (run_id, board) = drive(&harness, spec).await;
    assert_eq!(
        board.tasks(&run_id).unwrap()[0].state,
        TaskState::Merged,
        "{}",
        board.tasks(&run_id).unwrap()[0].note
    );
}

#[tokio::test]
async fn a_stopped_run_resumes_without_redoing_finished_work() {
    // Stopping a long run must be safe *and* reversible. Work already merged stays merged,
    // work caught mid-flight returns to the queue, and the rest is picked up.
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    harness.config.allowances.worker_runs = 1; // only the first task can run

    let spec = RunSpec {
        brief: String::new(),
        validation: Default::default(),
        objective: "Stop, then carry on".into(),
        tasks: vec![
            task("first", "CREATE:first.txt", &[]),
            task("second", "CREATE:second.txt", &[]),
        ],
    };
    let board = Board::open(&harness.board_path).unwrap();
    let (_tx, rx) = watch::channel(0);
    let (engine, run_id) = Engine::create(harness.config.clone(), board, scorer(), rx, &spec)
        .await
        .unwrap();
    let branch = {
        let engine = Arc::new(engine);
        let outcome = engine.drive(&run_id).await.unwrap();
        assert_eq!(outcome.merged, 1, "the allowance stopped it after one");
        assert_eq!(outcome.held, 1);
        Board::open(&harness.board_path)
            .unwrap()
            .run(&run_id)
            .unwrap()
            .integration_branch
    };

    // Simulate an agent caught mid-flight when the controller stopped.
    let mut board = Board::open(&harness.board_path).unwrap();
    board.set_state(&run_id, "second", TaskState::Running, "in flight").unwrap();

    // Now with allowance restored, resume rather than start again.
    harness.config.allowances.worker_runs = 50;
    let (_tx, rx) = watch::channel(0);
    let engine = Engine::attach(harness.config.clone(), board, scorer(), rx, &run_id)
        .await
        .unwrap();
    Arc::new(engine).drive(&run_id).await.unwrap();

    let board = Board::open(&harness.board_path).unwrap();
    let tasks = board.tasks(&run_id).unwrap();
    for task in &tasks {
        assert_eq!(task.state, TaskState::Merged, "{} — {}", task.id, task.note);
    }
    assert_eq!(
        board.run(&run_id).unwrap().integration_branch,
        branch,
        "the same branch is continued, not a new one"
    );
    // The finished task was not attempted a second time.
    let attempts = board.attempts(&run_id).unwrap();
    let first_attempts = attempts.iter().filter(|a| a["task_id"] == "first").count();
    assert_eq!(first_attempts, 1, "merged work is not redone");

    // Both files are present on the branch: the first from before the stop.
    let listing = git(
        &harness.workspace_path,
        &["ls-tree", "-r", "--name-only", &branch],
    )
    .await
    .unwrap();
    assert!(listing.contains("first.txt"), "{listing}");
    assert!(listing.contains("second.txt"), "{listing}");
}

#[tokio::test]
async fn pausing_mid_task_keeps_what_the_agent_had_already_written() {
    // The operator stops everything. Agents are killed part-way through. What they had
    // written must survive somewhere recoverable rather than being thrown away.
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    let script = harness.config.providers[0].command.clone();
    // Writes its file, then keeps going until it is killed.
    std::fs::write(
        &script,
        "#!/bin/sh\ncat > /dev/null\necho 'partial work' > partial.txt\necho '{\"payload_type\":\"working\"}'\nsleep 300\n",
    )
    .unwrap();

    let spec = RunSpec {
        brief: String::new(),
        objective: "Stop everything mid-task".into(),
        tasks: vec![task("interrupted", "CREATE:partial.txt", &[])],
        validation: Default::default(),
    };
    let board = Board::open(&harness.board_path).unwrap();
    let (cancel, rx) = watch::channel(0);
    let (engine, run_id) = Engine::create(harness.config.clone(), board, scorer(), rx, &spec)
        .await
        .unwrap();
    let engine = Arc::new(engine);

    // Let the agent get going, then pause everything.
    let driving = {
        let engine = Arc::clone(&engine);
        let run = run_id.clone();
        tokio::spawn(async move { engine.drive(&run).await })
    };
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    cancel.send_modify(|v| *v += 1);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(60), driving).await;

    let board = Board::open(&harness.board_path).unwrap();
    let attempts = board.attempts(&run_id).unwrap();
    assert_eq!(attempts.len(), 1);
    let branch = attempts[0]["branch"].as_str().unwrap().to_string();

    // The partial work is committed on the attempt's own branch, which is kept.
    let listing = git(
        &harness.workspace_path,
        &["ls-tree", "-r", "--name-only", &branch],
    )
    .await
    .unwrap_or_default();
    assert!(
        listing.contains("partial.txt"),
        "what the agent wrote before the pause survives on {branch}:\n{listing}"
    );

    // And the task is back in the queue, not failed, so resuming picks it up.
    let task = &board.tasks(&run_id).unwrap()[0];
    assert_ne!(task.state, TaskState::Merged);
    assert!(!task.state.terminal(), "left resumable, not failed: {:?}", task.state);
}

#[tokio::test]
async fn stopping_the_run_is_not_a_verdict_on_the_agent() {
    // An agent killed mid-task used to be recorded as rejected when it had written
    // nothing: a judgement it never earned, which both spent one of the task's two tries
    // and put a mark on the provider's record that routing later read as incompetence.
    // Seen in a live run, where it failed a task outright.
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    let script = harness.config.providers[0].command.clone();
    // Starts, writes nothing, and keeps going until it is killed.
    std::fs::write(
        &script,
        "#!/bin/sh\ncat > /dev/null\necho '{\"payload_type\":\"working\"}'\nsleep 300\n",
    )
    .unwrap();

    let spec = RunSpec {
        brief: String::new(),
        objective: "Stop an agent before it writes".into(),
        tasks: vec![task("stopped", "CREATE:never.txt", &[])],
        validation: Default::default(),
    };
    let board = Board::open(&harness.board_path).unwrap();
    let (cancel, rx) = watch::channel(0);
    let (engine, run_id) = Engine::create(harness.config.clone(), board, scorer(), rx, &spec)
        .await
        .unwrap();
    let engine = Arc::new(engine);
    let driving = {
        let engine = Arc::clone(&engine);
        let run = run_id.clone();
        tokio::spawn(async move { engine.drive(&run).await })
    };
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    cancel.send_modify(|v| *v += 1);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(60), driving).await;

    let board = Board::open(&harness.board_path).unwrap();
    let attempts = board.attempts(&run_id).unwrap();
    assert_eq!(
        attempts[0]["state"], "interrupted",
        "stopped, not judged: {:?}",
        attempts[0]["detail"]
    );
    assert!(
        attempts[0]["passed"].is_null(),
        "no verdict was reached, so none is recorded: {:?}",
        attempts[0]["passed"]
    );

    let task = &board.tasks(&run_id).unwrap()[0];
    assert!(!task.state.terminal(), "left to be picked up again: {:?}", task.state);
    assert_eq!(task.attempts, 0, "an interruption does not spend one of the task's tries");
}

#[tokio::test]
async fn an_interrupted_attempt_carries_on_rather_than_starting_over() {
    // Stopping an agent is not a judgement on its work. Resuming keeps the context it had
    // built and the partial work already in its worktree, exactly as reloading a session
    // does for a person.
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    harness.config.providers[0].resume_args = Some(vec!["--resumed".into()]);
    let script = harness.config.providers[0].command.clone();
    // First invocation writes half the work and hangs. On resume — recognisable by the
    // flag — it finds its own earlier work and completes the job.
    std::fs::write(
        &script,
        "#!/bin/sh\ncat > /dev/null\ncase \"$*\" in\n  *--resumed*)\n    test -f half.txt && echo done > whole.txt\n    exit 0 ;;\nesac\necho half > half.txt\necho '{\"payload_type\":\"working\"}'\nsleep 300\n",
    )
    .unwrap();

    let mut first = task("long", "carry on where you left off", &[]);
    first.verify = Some(vec!["sh".into(), "-c".into(), "test -f whole.txt".into()]);
    let spec = RunSpec {
        brief: String::new(),
        objective: "Continue interrupted work".into(),
        tasks: vec![first],
        validation: Default::default(),
    };

    // Start, then stop everything while the agent is mid-task.
    let board = Board::open(&harness.board_path).unwrap();
    let (cancel, rx) = watch::channel(0);
    let (engine, run_id) = Engine::create(harness.config.clone(), board, scorer(), rx, &spec)
        .await
        .unwrap();
    let engine = Arc::new(engine);
    let driving = {
        let engine = Arc::clone(&engine);
        let run = run_id.clone();
        tokio::spawn(async move { engine.drive(&run).await })
    };
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    cancel.send_modify(|v| *v += 1);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(60), driving).await;

    let board = Board::open(&harness.board_path).unwrap();
    let attempts = board.attempts(&run_id).unwrap();
    assert_eq!(attempts[0]["state"], "interrupted", "stopped, not judged");
    let first_id = attempts[0]["id"].as_str().unwrap().to_string();
    assert!(
        board.resumable(&run_id, "long", "fake").unwrap().is_some(),
        "its worktree is kept so the work can be continued"
    );

    // Resume: the agent should find its own half-finished work and finish it.
    let (_tx, rx) = watch::channel(0);
    let engine = Engine::attach(harness.config.clone(), board, scorer(), rx, &run_id)
        .await
        .unwrap();
    Arc::new(engine).drive(&run_id).await.unwrap();

    let board = Board::open(&harness.board_path).unwrap();
    assert_eq!(
        board.tasks(&run_id).unwrap()[0].state,
        TaskState::Merged,
        "{}",
        board.tasks(&run_id).unwrap()[0].note
    );
    let attempts = board.attempts(&run_id).unwrap();
    assert_eq!(attempts.len(), 1, "the same attempt continued, not a second one");
    assert_eq!(attempts[0]["id"].as_str().unwrap(), first_id);
    let files = attempts[0]["files_changed"].as_array().unwrap();
    assert_eq!(files.len(), 2, "both halves of the work are there: {files:?}");
}

#[tokio::test]
async fn a_worker_can_leave_a_note_for_the_team() {
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    let spec = RunSpec {
        brief: String::new(),
        objective: "Let workers contribute".into(),
        tasks: vec![task("noted", "CREATE:noted.txt NOTE:prefer-iterators", &[])],
        validation: Default::default(),
    };
    let (run_id, board) = drive(&harness, spec).await;

    let entries = board.forum().entries(&run_id).unwrap();
    let note = entries
        .iter()
        .find(|e| e.author == "fake")
        .expect("the worker's own note reaches the forum, under its own name");
    assert!(
        !entries.iter().any(|e| e.author.contains("observer")),
        "a worker's note is not attributed to the observer"
    );
    assert_eq!(note.kind, super::forum::Kind::Approach, "good thinking has a kind of its own");
    assert!(note.title.contains("prefer-iterators"), "{}", note.title);

    // The note lives outside the worktree, so it never shows up as a changed file.
    let attempts = board.attempts(&run_id).unwrap();
    let files = attempts[0]["files_changed"].as_array().unwrap();
    assert_eq!(files.len(), 1, "writing a note is not changing a file: {files:?}");
    assert_eq!(files[0], "noted.txt");
}

#[tokio::test]
async fn a_failed_attempt_warns_the_group_rather_than_disappearing() {
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    let spec = RunSpec {
        brief: String::new(),
        objective: "Record failures too".into(),
        tasks: vec![task("doomed", "This cannot work. FAILNOW", &[])],
        validation: Default::default(),
    };
    let (run_id, board) = drive(&harness, spec).await;

    let entries = board.forum().entries(&run_id).unwrap();
    let blocker = entries
        .iter()
        .find(|e| e.kind == super::forum::Kind::Blocker)
        .expect("a rejected attempt is published as a blocker");
    assert!(blocker.title.contains("doomed"), "{}", blocker.title);
    assert!(
        blocker.body.contains("Agent exit: 1"),
        "the agent's own exit code is in the note: {}",
        blocker.body
    );
}

/// Give a provider a record by writing attempts straight into the ledger.
fn record_history(board: &mut Board, provider: &str, verified: usize, rejected: usize) {
    for index in 0..(verified + rejected) {
        let id = format!("{provider}-history-{index}");
        let mut attempt = super::board::Attempt::reserved(
            id,
            "old-run",
            "t",
            provider,
            "b".into(),
            "c".into(),
        );
        board.start_attempt(&attempt).unwrap();
        attempt.finished_at = Some(attempt.started_at + 10);
        attempt.state = if index < verified {
            super::board::AttemptState::Verified
        } else {
            super::board::AttemptState::Rejected
        };
        board.finish_attempt(&attempt).unwrap();
    }
}

#[tokio::test]
async fn a_cheap_provider_that_keeps_failing_is_no_longer_tried_first() {
    // A cheap agent whose work is usually rejected is not cheap: every rejection costs
    // another run. Evidence has to be able to overrule price.
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    harness.config.providers[0].max_concurrent = 1;
    let mut dear = harness.config.providers[0].clone();
    dear.id = "dear".into();
    dear.name = "Dear".into();
    dear.tier = 2;
    harness.config.providers.push(dear);

    let mut board = Board::open(&harness.board_path).unwrap();
    // The cheap one has a bad record; the dear one a good one.
    record_history(&mut board, "fake", 1, 9);
    record_history(&mut board, "dear", 9, 1);
    drop(board);

    let spec = RunSpec {
        brief: String::new(),
        validation: Default::default(),
        objective: "Evidence overrules price".into(),
        tasks: vec![{
            let mut t = task("one", "CREATE:one.txt", &[]);
            t.provider = None;
            t
        }],
    };
    let (run_id, board) = drive(&harness, spec).await;
    let used = board.attempts(&run_id).unwrap();
    let chosen = used
        .iter()
        .find(|a| a["task_id"] == "one")
        .unwrap()["provider"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(chosen, "dear", "the unreliable cheap provider was passed over");
}

#[tokio::test]
async fn an_operator_override_beats_the_evidence() {
    // Routing is a default, not a verdict: a deliberate comparison must be possible.
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    let mut dear = harness.config.providers[0].clone();
    dear.id = "dear".into();
    dear.name = "Dear".into();
    dear.tier = 2;
    harness.config.providers.push(dear);

    let mut board = Board::open(&harness.board_path).unwrap();
    record_history(&mut board, "dear", 1, 9); // a poor record, deliberately chosen anyway
    drop(board);

    let spec = RunSpec {
        brief: String::new(),
        validation: Default::default(),
        objective: "The operator decides".into(),
        tasks: vec![{
            let mut t = task("one", "CREATE:one.txt", &[]);
            t.provider = None;
            t
        }],
    };
    let board = Board::open(&harness.board_path).unwrap();
    let (_tx, rx) = watch::channel(0);
    let (engine, run_id) = Engine::create(harness.config.clone(), board, scorer(), rx, &spec)
        .await
        .unwrap();
    Arc::new(engine.with_forced_provider(Some("dear".into())))
        .drive(&run_id)
        .await
        .unwrap();

    let board = Board::open(&harness.board_path).unwrap();
    let chosen = board
        .attempts(&run_id)
        .unwrap()
        .iter()
        .find(|a| a["task_id"] == "one")
        .unwrap()["provider"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(chosen, "dear", "--provider overrides routing entirely");
}

#[tokio::test]
async fn unpinned_work_fills_the_cheapest_tier_then_spills_to_the_next() {
    let mut harness = harness().await;
    // A cheap provider with one slot, and a dearer one with two.
    harness.config.providers[0].max_concurrent = 1;
    let mut dear = harness.config.providers[0].clone();
    dear.id = "dear".into();
    dear.name = "Dear".into();
    dear.tier = 2;
    dear.max_concurrent = 2;
    harness.config.providers.push(dear);

    // Three independent tasks, none pinned to a provider.
    let spec = RunSpec {
        brief: String::new(),
        validation: Default::default(),
        objective: "Spread unpinned work across the roster".into(),
        tasks: ["one", "two", "three"]
            .iter()
            .map(|id| {
                let mut t = task(id, &format!("CREATE:{id}.txt"), &[]);
                t.provider = None;
                t
            })
            .collect(),
    };
    let board = Board::open(&harness.board_path).unwrap();
    let (_tx, rx) = watch::channel(0);
    let (engine, run_id) = Engine::create(harness.config.clone(), board, scorer(), rx, &spec)
        .await
        .unwrap();
    let outcome = Arc::new(engine).drive(&run_id).await.unwrap();
    assert_eq!(outcome.merged, 3);

    let board = Board::open(&harness.board_path).unwrap();
    let used: Vec<String> = board
        .attempts(&run_id)
        .unwrap()
        .iter()
        .map(|a| a["provider"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(used.len(), 3);
    assert!(
        used.contains(&"fake".to_string()),
        "the cheapest provider is used: {used:?}"
    );
    assert!(
        used.contains(&"dear".to_string()),
        "work spills to the next tier rather than queueing behind a busy provider: {used:?}"
    );
}

#[tokio::test]
async fn a_task_is_judged_by_its_own_check_not_by_unfinished_work_elsewhere() {
    // The run-level check demands every file, so it cannot pass until the last task is
    // done. This is the shape of a real project: a whole-suite check fails while other
    // modules are still stubs. Each task carries its own check so focused work is judged
    // on its own terms — the exact failure the second v0 live trial reported.
    let harness = harness().await;
    let whole_suite = vec![
        "sh".to_string(),
        "-c".to_string(),
        "test -f one.txt && test -f two.txt".to_string(),
    ];
    let scoped = |file: &str| {
        Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("test -f {file}"),
        ])
    };
    let mut first = task("one", "CREATE:one.txt", &[]);
    first.verify = scoped("one.txt");
    let mut second = task("two", "CREATE:two.txt", &["one"]);
    second.verify = scoped("two.txt");

    let board = Board::open(&harness.board_path).unwrap();
    let (_tx, rx) = watch::channel(0);
    let scorer = Scorer::Command {
        command: whole_suite,
        timeout_seconds: 30,
    };
    let spec = RunSpec {
        brief: String::new(),
        objective: "Judge each task on its own check".into(),
        tasks: vec![first, second],
        validation: Default::default(),
    };
    let (engine, run_id) = Engine::create(harness.config.clone(), board, scorer, rx, &spec)
        .await
        .unwrap();
    let outcome = Arc::new(engine).drive(&run_id).await.unwrap();

    assert_eq!(
        outcome.merged, 2,
        "both tasks merge even though the whole-suite check fails partway through"
    );
    // And the run still reports the honest whole-project result at the end.
    let final_check = outcome.final_check.expect("a final check runs once work merged");
    assert!(final_check.passed, "everything composes once both tasks are done");
}

#[tokio::test]
async fn the_rolling_allowance_caps_agent_runs_and_holds_the_rest() {
    let mut harness = harness().await;
    // Three tasks are ready at once, but only two agent runs are allowed in the window.
    harness.config.allowances.worker_runs = 2;
    let spec = RunSpec {
        brief: String::new(),
        validation: Default::default(),
        objective: "Respect the budget".into(),
        tasks: vec![
            task("one", "CREATE:one.txt", &[]),
            task("two", "CREATE:two.txt", &[]),
            task("three", "CREATE:three.txt", &[]),
        ],
    };
    let board = Board::open(&harness.board_path).unwrap();
    let (_tx, rx) = watch::channel(0);
    let (engine, run_id) = Engine::create(harness.config.clone(), board, scorer(), rx, &spec)
        .await
        .unwrap();
    let outcome = Arc::new(engine).drive(&run_id).await.unwrap();

    let board = Board::open(&harness.board_path).unwrap();
    assert_eq!(
        board.attempts(&run_id).unwrap().len(),
        2,
        "the allowance is a hard cap on agent runs, not a suggestion"
    );
    assert_eq!(outcome.merged, 2);
    assert_eq!(outcome.held, 1, "the third task is held, not failed");
    assert!(
        outcome.hold_reason.unwrap().contains("allowance exhausted"),
        "the run says which allowance stopped it"
    );

    // A held task stays open so it can be picked up once the window rolls.
    let held: Vec<_> = board
        .tasks(&run_id)
        .unwrap()
        .into_iter()
        .filter(|t| !t.state.terminal())
        .collect();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].state, TaskState::Open);

    // Counting spans runs: a brand new run gets no fresh allowance.
    assert_eq!(board.recent_runs(None, 0).unwrap(), 2);
}

#[tokio::test]
async fn a_per_provider_cap_is_enforced_independently_of_the_overall_allowance() {
    let mut harness = harness().await;
    harness.config.allowances.worker_runs = 10;
    harness.config.providers[0].max_runs = 1;
    let spec = RunSpec {
        brief: String::new(),
        validation: Default::default(),
        objective: "Respect the per-provider cap".into(),
        tasks: vec![
            task("one", "CREATE:one.txt", &[]),
            task("two", "CREATE:two.txt", &[]),
        ],
    };
    let board = Board::open(&harness.board_path).unwrap();
    let (_tx, rx) = watch::channel(0);
    let (engine, run_id) = Engine::create(harness.config.clone(), board, scorer(), rx, &spec)
        .await
        .unwrap();
    let outcome = Arc::new(engine).drive(&run_id).await.unwrap();

    assert_eq!(outcome.merged, 1);
    assert_eq!(outcome.held, 1);
    assert!(outcome.hold_reason.unwrap().contains("Fake run allowance exhausted"));
}

async fn drive(harness: &Harness, spec: RunSpec) -> (String, Board) {
    drive_with(harness, spec, scorer()).await
}

/// Drive a run under a given check. Build mode reads the project's own check as a
/// regression guard, so its tests need to choose what that check says.
async fn drive_with(harness: &Harness, spec: RunSpec, scorer: Scorer) -> (String, Board) {
    let board = Board::open(&harness.board_path).unwrap();
    let (_tx, rx) = watch::channel(0);
    let (engine, run_id) = Engine::create(harness.config.clone(), board, scorer, rx, &spec)
        .await
        .unwrap();
    let engine = Arc::new(engine);
    engine.drive(&run_id).await.unwrap();
    (run_id, Board::open(&harness.board_path).unwrap())
}

#[tokio::test]
async fn parallel_tasks_are_isolated_scored_and_merged_in_dependency_order() {
    let harness = harness().await;
    let spec = RunSpec {
        brief: String::new(),
        validation: Default::default(),
        objective: "Build three files".into(),
        tasks: vec![
            task("alpha", "Write the alpha file. CREATE:alpha.txt", &[]),
            task("beta", "Write the beta file. CREATE:beta.txt", &[]),
            // Depends on both, so it may only run once they have merged.
            task("gamma", "Write the gamma file. CREATE:gamma.txt", &["alpha", "beta"]),
        ],
    };
    let (run_id, board) = drive(&harness, spec).await;

    let tasks = board.tasks(&run_id).unwrap();
    for task in &tasks {
        assert_eq!(task.state, TaskState::Merged, "{} — {}", task.id, task.note);
    }

    // All three files landed on the integration branch, and each attempt reported only
    // the file it actually wrote.
    let branch = board.run(&run_id).unwrap().integration_branch;
    assert!(
        branch.contains(&run_id[..8]),
        "the integration branch must name its own run: {branch} vs {run_id}"
    );
    let listing = git(&harness.workspace_path, &["ls-tree", "-r", "--name-only", &branch])
        .await
        .unwrap();
    for name in ["alpha.txt", "beta.txt", "gamma.txt"] {
        assert!(listing.contains(name), "{name} missing from {listing}");
    }

    let attempts = board.attempts(&run_id).unwrap();
    assert_eq!(attempts.len(), 3, "no task needed a retry");
    for attempt in &attempts {
        assert_eq!(attempt["state"], "verified");
        assert_eq!(attempt["native_exit"], 0);
        assert_eq!(attempt["passed"], true);
        let changed = attempt["files_changed"].as_array().unwrap();
        assert_eq!(changed.len(), 1, "attempts stay in their own lane: {changed:?}");
    }

    // The operator's own branch never moved.
    assert_eq!(
        git(&harness.workspace_path, &["rev-parse", "HEAD"]).await.unwrap(),
        board.run(&run_id).unwrap().base_commit
    );
    assert!(
        git(&harness.workspace_path, &["status", "--porcelain"]).await.unwrap().is_empty()
    );
}

#[tokio::test]
async fn a_failing_agent_is_retried_then_fails_the_task_and_blocks_dependents() {
    let harness = harness().await;
    let spec = RunSpec {
        brief: String::new(),
        validation: Default::default(),
        objective: "Handle failure honestly".into(),
        tasks: vec![
            task("good", "Write the good file. CREATE:good.txt", &[]),
            task("bad", "This one cannot work. FAILNOW", &[]),
            task("after-bad", "Never reached. CREATE:never.txt", &["bad"]),
        ],
    };
    let (run_id, board) = drive(&harness, spec).await;

    let state = |id: &str| {
        board
            .tasks(&run_id)
            .unwrap()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap()
    };
    assert_eq!(state("good").state, TaskState::Merged, "unrelated work still lands");
    assert_eq!(state("bad").state, TaskState::Failed);
    assert_eq!(
        state("after-bad").state,
        TaskState::Blocked,
        "a dependent of a failed task is blocked, not left waiting forever"
    );
    assert_eq!(state("bad").attempts, super::dispatch::MAX_ATTEMPTS);

    // The failing agent's non-zero exit is recorded as its own fact.
    let attempts = board.attempts(&run_id).unwrap();
    let failed: Vec<_> = attempts.iter().filter(|a| a["task_id"] == "bad").collect();
    assert_eq!(failed.len(), super::dispatch::MAX_ATTEMPTS);
    for attempt in failed {
        assert_eq!(attempt["native_exit"], 1);
    }

    let branch = board.run(&run_id).unwrap().integration_branch;
    let listing = git(&harness.workspace_path, &["ls-tree", "-r", "--name-only", &branch])
        .await
        .unwrap();
    assert!(listing.contains("good.txt"));
    assert!(!listing.contains("never.txt"));
}

#[tokio::test]
async fn an_agent_that_writes_nothing_or_fails_the_check_is_not_merged() {
    let harness = harness().await;
    let spec = RunSpec {
        brief: String::new(),
        validation: Default::default(),
        objective: "Reject work that cannot be verified".into(),
        tasks: vec![
            // Announces nothing and writes nothing — v0's first live trial saw exactly this.
            task("silent", "Think about it but change no files.", &[]),
            // Writes the sentinel the scorer refuses.
            task("breaks", "Break the check. CREATE:BROKEN", &[]),
        ],
    };
    let (run_id, board) = drive(&harness, spec).await;

    let tasks = board.tasks(&run_id).unwrap();
    for task in &tasks {
        assert_eq!(task.state, TaskState::Failed, "{}", task.id);
    }

    let attempts = board.attempts(&run_id).unwrap();
    let silent: Vec<_> = attempts.iter().filter(|a| a["task_id"] == "silent").collect();
    assert!(
        silent[0]["detail"].as_str().unwrap().contains("changed no files"),
        "a no-op agent is reported honestly, not treated as success"
    );
    assert_eq!(silent[0]["native_exit"], 0, "it exited cleanly while doing nothing");
    assert_eq!(silent[0]["state"], "rejected");

    let breaks: Vec<_> = attempts.iter().filter(|a| a["task_id"] == "breaks").collect();
    assert_eq!(breaks[0]["state"], "rejected");
    assert_eq!(breaks[0]["passed"], false);
    assert_eq!(breaks[0]["native_exit"], 0, "the agent succeeded; the check did not");

    // Nothing unverified reached the integration branch.
    let branch = board.run(&run_id).unwrap().integration_branch;
    let listing = git(&harness.workspace_path, &["ls-tree", "-r", "--name-only", &branch])
        .await
        .unwrap();
    assert!(!listing.contains("BROKEN"), "{listing}");
}

// ---------------------------------------------------------------------------------------
// Build mode: ordinary projects, where no check can prove a task was done.
// ---------------------------------------------------------------------------------------

/// A roster of three tiers, so escalation has somewhere to escalate to.
fn tiered(harness: &mut Harness) {
    let base = harness.config.providers[0].clone();
    harness.config.providers = (0..3)
        .map(|tier| Provider {
            id: format!("tier{tier}"),
            name: format!("Tier {tier}"),
            tier,
            ..base.clone()
        })
        .collect();
}

#[tokio::test]
async fn build_mode_merges_work_no_check_could_have_proved() {
    // The whole point of the mode: an ordinary task, no per-task check, and the project's
    // own suite says only that nothing broke. Trial mode would have nothing to merge on.
    let mut harness = harness().await;
    harness.config.mode = "build".into();
    harness.config.forum_observer = String::new();
    harness.config.reviewer = String::new(); // no second provider, so no reviewer exists
    harness.config.verify_command = vec!["true".into()];

    let mut only = task("feature", "CREATE:feature.txt", &[]);
    only.verify = None;
    let spec = RunSpec {
        brief: String::new(),
        objective: "Do ordinary work".into(),
        tasks: vec![only],
        validation: Default::default(),
    };
    let passing = Scorer::Command {
        command: vec!["true".into()],
        timeout_seconds: 30,
    };
    let (run_id, board) = drive_with(&harness, spec, passing).await;

    let task = &board.tasks(&run_id).unwrap()[0];
    assert_eq!(task.state, TaskState::Merged, "{}", task.note);
}

#[tokio::test]
async fn build_mode_rejects_an_attempt_that_breaks_what_was_working() {
    // The project's own check cannot say the task was done, but it can say the agent broke
    // something that worked. That is the one thing it is allowed to condemn an attempt for.
    let mut harness = harness().await;
    harness.config.mode = "build".into();
    harness.config.forum_observer = String::new();
    // Passes until an agent creates BROKEN, which the fake agent does on this instruction.
    harness.config.verify_command =
        vec!["sh".into(), "-c".into(), "! test -f BROKEN".into()];

    let mut only = task("breaks-it", "CREATE:BROKEN", &[]);
    only.verify = None;
    let spec = RunSpec {
        brief: String::new(),
        objective: "Break the build".into(),
        tasks: vec![only],
        validation: Default::default(),
    };
    let (run_id, board) = drive(&harness, spec).await;

    let task = &board.tasks(&run_id).unwrap()[0];
    assert_eq!(task.state, TaskState::Failed, "{}", task.note);
    assert!(
        task.note.contains("was passing before this run"),
        "says it was a regression, not a verdict on the task: {}",
        task.note
    );
}

#[tokio::test]
async fn a_check_that_was_already_failing_does_not_condemn_anyone() {
    // Pointing Firm at a project whose suite is already red must not reject every attempt
    // in turn for a breakage that was there before any agent arrived.
    let mut harness = harness().await;
    harness.config.mode = "build".into();
    harness.config.forum_observer = String::new();
    harness.config.verify_command = vec!["false".into()];

    let mut only = task("regardless", "CREATE:work.txt", &[]);
    only.verify = None;
    let spec = RunSpec {
        brief: String::new(),
        objective: "Work on a project that is already broken".into(),
        tasks: vec![only],
        validation: Default::default(),
    };
    let already_red = Scorer::Command {
        command: vec!["false".into()],
        timeout_seconds: 30,
    };
    let (run_id, board) = drive_with(&harness, spec, already_red).await;

    let task = &board.tasks(&run_id).unwrap()[0];
    assert_eq!(task.state, TaskState::Merged, "{}", task.note);
    let attempts = board.attempts(&run_id).unwrap();
    assert!(
        attempts[0]["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("already failing"),
        "and says why it could not judge: {:?}",
        attempts[0]["detail"]
    );
}

#[tokio::test]
async fn a_rejected_task_escalates_to_a_dearer_tier_rather_than_retrying_the_cheap_one() {
    // Paying more only where cheap demonstrably failed is the whole cost argument. Trying
    // the same cheap model twice spends two runs to learn one thing.
    let mut harness = harness().await;
    harness.config.mode = "build".into();
    harness.config.forum_observer = String::new();
    harness.config.verify_command = vec!["true".into()];
    tiered(&mut harness);

    // Fails the task's own check the first time; the second attempt is a different tier.
    let script = harness.config.providers[0].command.clone();
    std::fs::write(
        &script,
        "#!/bin/sh\ncat > /dev/null\ntest -f first-go && echo done > done.txt\ntouch first-go\nexit 0\n",
    )
    .unwrap();

    let mut only = task("hard", "do the difficult thing", &[]);
    only.provider = None; // let routing choose
    only.verify = Some(vec!["sh".into(), "-c".into(), "test -f done.txt".into()]);
    let spec = RunSpec {
        brief: String::new(),
        objective: "Escalate when the cheap model fails".into(),
        tasks: vec![only],
        validation: Default::default(),
    };
    let passing = Scorer::Command {
        command: vec!["true".into()],
        timeout_seconds: 30,
    };
    let (run_id, board) = drive_with(&harness, spec, passing).await;

    let attempts = board.attempts(&run_id).unwrap();
    assert_eq!(attempts.len(), 2, "one cheap try, then one escalation");
    assert_eq!(attempts[0]["provider"], "tier0", "cheapest first, as always");
    assert_ne!(
        attempts[1]["provider"], "tier0",
        "the retry went up a tier rather than re-rolling the same model"
    );
}

#[tokio::test]
async fn build_outcomes_stay_out_of_the_trial_ledger() {
    // The trial ledger is a benchmark. A build-mode merge means a reviewer was satisfied
    // and nothing broke; letting that count as evidence would quietly soften the record.
    let mut harness = harness().await;
    harness.config.mode = "build".into();
    harness.config.forum_observer = String::new();
    harness.config.verify_command = vec!["true".into()];

    let mut only = task("ordinary", "CREATE:thing.txt", &[]);
    only.verify = None;
    let spec = RunSpec {
        brief: String::new(),
        objective: "Ordinary work".into(),
        tasks: vec![only],
        validation: Default::default(),
    };
    let passing = Scorer::Command {
        command: vec!["true".into()],
        timeout_seconds: 30,
    };
    let (_run_id, board) = drive_with(&harness, spec, passing).await;

    let build = board.provider_stats(0, "build").unwrap();
    let trial = board.provider_stats(0, "trial").unwrap();
    assert_eq!(build.get("fake").map(|s| s.attempts), Some(1));
    assert!(
        trial.is_empty(),
        "the benchmark never saw this run: {trial:?}"
    );
}

#[tokio::test]
async fn an_operator_stop_never_becomes_evidence_about_a_provider() {
    // Last night's ledger read grok at 33% when it had failed nothing: one verified
    // attempt and two that were interrupted by Ctrl+C. The spec already said an attempt
    // that never finished is not evidence; the query did not agree.
    let harness = harness().await;
    let mut board = Board::open(&harness.board_path).unwrap();
    let spec = RunSpec {
        brief: String::new(),
        objective: "Evidence".into(),
        tasks: vec![task("one", "CREATE:one.txt", &[])],
        validation: Default::default(),
    };
    board
        .create_run("run-evidence", &spec, "abc", "firm/run-evidence", "trial")
        .unwrap();

    for (id, state) in [
        ("a", AttemptState::Verified),
        ("b", AttemptState::Interrupted),
        ("c", AttemptState::Interrupted),
    ] {
        let mut attempt = Attempt::reserved(
            id.into(),
            "run-evidence",
            "one",
            "fake",
            format!("firm/attempt-{id}"),
            "abc".into(),
        );
        board.start_attempt(&attempt).unwrap();
        attempt.state = state;
        attempt.finished_at = Some(attempt.started_at + 10);
        board.finish_attempt(&attempt).unwrap();
    }

    let stats = board.provider_stats(0, "trial").unwrap();
    let record = stats.get("fake").expect("the provider has a record");
    assert_eq!(record.attempts, 1, "only the judged attempt counts");
    assert_eq!(record.success_percent(), 100, "it passed the one it was judged on");
}

#[tokio::test]
async fn validation_asks_whether_the_result_is_any_use_not_whether_tasks_were_done() {
    // The handwriting trial's lesson: every task can be carried out correctly and the
    // objective still not met. Probes are written before the plan, run on the assembled
    // result, and report rather than gate — what to do about a failure is a person's call.
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();

    let spec = RunSpec {
        brief: String::new(),
        objective: "Build the thing".into(),
        tasks: vec![task("build-it", "CREATE:thing.txt", &[])],
        validation: super::validate::Validation {
            probes: vec![
                super::validate::Probe {
                    id: "produces-output".into(),
                    description: "the thing it was asked for exists".into(),
                    command: vec!["test".into(), "-f".into(), "thing.txt".into()],
                },
                super::validate::Probe {
                    id: "is-actually-useful".into(),
                    description: "and does the job it was for".into(),
                    command: vec!["test".into(), "-f".into(), "never-written.txt".into()],
                },
            ],
            criteria: vec!["Read the output and decide whether it reads like handwriting".into()],
        },
    };

    let board = Board::open(&harness.board_path).unwrap();
    let (_tx, rx) = watch::channel(0);
    let (engine, run_id) = Engine::create(harness.config.clone(), board, scorer(), rx, &spec)
        .await
        .unwrap();
    let outcome = Arc::new(engine).drive(&run_id).await.unwrap();

    assert_eq!(outcome.merged, 1, "the task itself was done");
    assert_eq!(outcome.probes.len(), 2, "and then the result was questioned");
    assert!(outcome.probes[0].passed, "{:?}", outcome.probes[0]);
    assert!(
        !outcome.probes[1].passed,
        "a task done correctly does not mean the objective was met"
    );
    assert_eq!(outcome.criteria.len(), 1, "and what cannot be automated is carried through");

    // A failed probe reverts nothing and fails nothing: it is a report for a person.
    let board = Board::open(&harness.board_path).unwrap();
    assert_eq!(board.tasks(&run_id).unwrap()[0].state, TaskState::Merged);
    let stored = board.probe_results(&run_id).unwrap();
    assert_eq!(stored.len(), 2, "and it outlives the process that produced it");
    assert_eq!(stored[1].id, "is-actually-useful");
}

#[tokio::test]
async fn a_run_planned_without_validation_still_runs() {
    // Hand-authored task lists predate all of this and must keep working.
    let harness = harness().await;
    let spec = RunSpec {
        brief: String::new(),
        objective: "No validation model".into(),
        tasks: vec![task("plain", "CREATE:plain.txt", &[])],
        validation: Default::default(),
    };
    let (run_id, board) = drive(&harness, spec).await;
    assert_eq!(board.tasks(&run_id).unwrap()[0].state, TaskState::Merged);
    assert!(board.probe_results(&run_id).unwrap().is_empty());
}

#[tokio::test]
async fn a_worker_is_shown_the_point_of_the_run_without_being_given_a_second_brief() {
    // Every agent downstream of the planner worked through a keyhole, which is the likely
    // reason the mutable graph has never once been used: nobody could see far enough to
    // notice their task no longer served the objective.
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    // The fake agent writes its prompt into the file it creates, so the prompt is readable.
    let spec = RunSpec {
        objective: "Measure handwriting so it can later be generated".into(),
        brief: "The generator cannot be built until the measurements exist.".into(),
        tasks: vec![task("measure", "DUMP:measured.txt", &[])],
        validation: super::validate::Validation {
            probes: vec![super::validate::Probe {
                id: "spread".into(),
                description: "the metric is not degenerate over real inputs".into(),
                command: vec!["true".into()],
            }],
            criteria: vec!["Does the output look like handwriting?".into()],
        },
    };
    let (run_id, board) = drive(&harness, spec).await;
    assert_eq!(board.tasks(&run_id).unwrap()[0].state, TaskState::Merged);

    let integration = harness
        .config
        .state_dir
        .join("worktrees")
        .join(format!("run-{}", &run_id[..8]))
        .join("integration/measured.txt");
    let written =
        std::fs::read_to_string(integration).expect("the agent dumped its prompt to the file");

    assert!(written.contains("Measure handwriting"), "the objective is there");
    assert!(
        written.contains("cannot be built until the measurements exist"),
        "and the brief behind it"
    );
    assert!(
        written.contains("not degenerate over real inputs"),
        "and how the finished thing will be judged"
    );
    // The guard that keeps it context rather than a rival specification.
    assert!(
        written.contains("authority on what you do"),
        "the task still outranks the objective"
    );
}
