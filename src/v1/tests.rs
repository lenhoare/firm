//! End-to-end tests for the v1 engine, driven by a fake agent CLI so the whole
//! board → worktree → agent → scorer → merge pipeline runs without model credits.

use super::{
    board::{Board, RunSpec, TaskSpec, TaskState},
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
        planner_args: None,
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
        objective: "Respect a provider's own limits".into(),
        tasks: vec![task("slow", "CREATE:slow.txt", &[])],
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
        objective: "Let what is learned change what is done".into(),
        tasks: vec![task("propose-followup", "CREATE:first.txt", &[])],
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
        objective: "A task with nothing left to do".into(),
        tasks: vec![first, second],
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
        objective: "Stay in your lane".into(),
        tasks: vec![scoped],
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
        objective: "Declared files are fine".into(),
        tasks: vec![scoped],
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
        objective: "A test must actually test something".into(),
        tasks: vec![seam],
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
        objective: "A real test is accepted".into(),
        tasks: vec![seam],
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
async fn a_worker_can_leave_a_note_for_the_team() {
    let mut harness = harness().await;
    harness.config.forum_observer = String::new();
    let spec = RunSpec {
        objective: "Let workers contribute".into(),
        tasks: vec![task("noted", "CREATE:noted.txt NOTE:prefer-iterators", &[])],
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
        objective: "Record failures too".into(),
        tasks: vec![task("doomed", "This cannot work. FAILNOW", &[])],
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
        objective: "Judge each task on its own check".into(),
        tasks: vec![first, second],
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
    let board = Board::open(&harness.board_path).unwrap();
    let (_tx, rx) = watch::channel(0);
    let (engine, run_id) = Engine::create(harness.config.clone(), board, scorer(), rx, &spec)
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
