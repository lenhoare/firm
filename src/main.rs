mod app;
mod codex;
mod config;
mod meetings;
mod snapshots;
mod state;
mod usage;
mod v1;
mod web;
mod worker;

use anyhow::{Context, Result, bail, ensure};
use config::Config;
use std::{fs::OpenOptions, os::fd::AsRawFd, path::Path};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Firm 0.1 — experimental agent team\n\n  firm serve [--live] [--config PATH]\n  firm probe [--config PATH]\n  firm board --tasks PATH [--live] [--config PATH]\n\nBoard runs the v1 engine: several agents work in parallel on a task graph, each in its\nown git worktree; only work passing verify_command is merged. The workspace must be a\nclean git repository and your own branch is never modified.\n\nDefault: demo mode, localhost:7433, firm.toml.\nProbe reads Codex login type and rate limits; it never starts a model turn.\nLive mode uses your existing Codex subscription and configured worker CLI logins.\nStart app-server separately: codex app-server --listen ws://127.0.0.1:4500"
        );
        return Ok(());
    }
    let mut config_path = "firm.toml";
    let mut tasks_path: Option<&str> = None;
    let mut live = false;
    let mut watch = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--live" => live = true,
            "--config" => {
                index += 1;
                config_path = args.get(index).context("Missing config path")?;
            }
            "--tasks" => {
                index += 1;
                tasks_path = Some(args.get(index).context("Missing tasks path")?);
            }
            "--watch" => watch = true,
            other => bail!("Unknown argument: {other}"),
        }
        index += 1;
    }
    let config = Config::read(Path::new(config_path))?;
    if args[0] == "board" {
        return board(config, tasks_path, live, watch).await;
    }
    if args[0] == "probe" {
        let rpc = codex::Codex::connect(&config.codex_url)
            .await
            .context("Start the local Codex app-server first")?;
        let account = rpc
            .call("account/read", serde_json::json!({"refreshToken":false}))
            .await?;
        println!(
            "Account type: {}",
            account
                .pointer("/account/type")
                .and_then(|v| v.as_str())
                .unwrap_or("not logged in")
        );
        let usage = rpc
            .call("account/rateLimits/read", serde_json::json!({}))
            .await?;
        println!(
            "Rate limits: {}",
            serde_json::to_string_pretty(
                &serde_json::json!({"rateLimits":usage["rateLimits"],"rateLimitsByLimitId":usage["rateLimitsByLimitId"]})
            )?
        );
        let models = rpc.call("model/list", serde_json::json!({})).await?;
        let available = models["data"].as_array().is_some_and(|models| {
            models.iter().any(|model| {
                model["model"] == config.codex_model || model["id"] == config.codex_model
            })
        });
        println!(
            "Configured model {} advertised by server: {}",
            config.codex_model, available
        );
        return Ok(());
    }
    if args[0] != "serve" {
        bail!("Unknown command; use firm --help");
    }
    std::fs::create_dir_all(&config.state_dir)?;
    let name = if live { "live" } else { "demo" };
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(config.state_dir.join(format!("{name}.lock")))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Another controller owns this state database");
    }
    // Account telemetry is read-only in both modes; demo dispatch paths remain simulated.
    let rpc = {
        let connection: Result<codex::Codex> = async {
            let rpc = codex::Codex::connect(&config.codex_url)
                .await
                .context("Start the local Codex app-server first")?;
            let account = rpc
                .call("account/read", serde_json::json!({"refreshToken":false}))
                .await?;
            let kind = account
                .pointer("/account/type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if kind != "chatgpt" {
                bail!("Codex requires a ChatGPT subscription login; reported type: {kind:?}. No API-key fallback is used.");
            }
            Ok(rpc)
        }.await;
        match connection {
            Ok(rpc) => Some(rpc),
            Err(error) => {
                eprintln!(
                    "Codex unavailable: {error}. Dashboard starts paused; other configured CLI managers remain usable. Codex requires a working subscription app-server connection; no API-key fallback is used."
                );
                None
            }
        }
    };
    let mut initial_allowances = config.allowances.clone();
    if !live {
        initial_allowances.manager_interval_seconds = 0;
    }
    let (store, state) = state::Store::open(
        &config.state_dir.join(format!("{name}.db")),
        !live,
        initial_allowances,
    )?;
    let app = app::App::new(config.clone(), store, state, rpc);
    let scheduler = tokio::spawn(app.clone().schedule());
    let usage_monitor = tokio::spawn(app.clone().monitor_usage());
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    println!(
        "Firm {} dashboard: http://{}\nAutomatic dispatch is paused. Workspace: {}",
        if live {
            "LIVE"
        } else {
            "DEMO (no model calls)"
        },
        config.listen,
        config.workspace.display()
    );
    let shutdown_app = app.clone();
    axum::serve(listener, web::router(app.clone()))
        .with_graceful_shutdown(async move {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            let active = shutdown_app.core.lock().await.active;
            if active {
                let _ = shutdown_app.stop().await;
            } else {
                let _ = shutdown_app.pause().await;
            }
        })
        .await?;
    // Give worker cleanup / Codex interrupt confirmation a bounded chance to finish.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(40), async {
        while app.core.lock().await.active {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;
    scheduler.abort();
    usage_monitor.abort();
    let _ = scheduler.await;
    drop(lock);
    Ok(())
}

/// Run one v1 board to completion. Several agents work in parallel on a task graph, each
/// in an isolated git worktree; the controller scores every attempt and merges only what
/// passes. There is no manager in this loop.
async fn board(config: Config, tasks_path: Option<&str>, live: bool, watch: bool) -> Result<()> {
    let board_path = v1::dispatch::board_path(&config.state_dir, live);
    if watch {
        return watch_run(&board_path, &config.workspace).await;
    }
    // With no task file, report on the most recent run instead of starting one.
    let Some(tasks_path) = tasks_path else {
        let board = v1::board::Board::open_readonly(&board_path)?;
        let run_id = board
            .latest_run()?
            .context("No runs yet. Start one with: firm board --tasks PATH")?;
        return summarise(&board, &run_id, &config.workspace);
    };
    let spec: v1::board::RunSpec = serde_json::from_str(
        &std::fs::read_to_string(tasks_path)
            .with_context(|| format!("Could not read {tasks_path}"))?,
    )
    .with_context(|| format!("{tasks_path} is not a valid run specification"))?;
    ensure!(
        !config.verify_command.is_empty(),
        "v1 needs verify_command in firm.toml: it is the scorer, and nothing merges without it"
    );

    std::fs::create_dir_all(&config.state_dir)?;
    let name = if live { "v1-live" } else { "v1-demo" };
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(config.state_dir.join(format!("{name}.lock")))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Another controller owns this board");
    }

    let board = v1::board::Board::open(&board_path)?;
    let scorer = v1::scorer::Scorer::Command {
        command: config.verify_command.clone(),
        timeout_seconds: config.allowances.worker_timeout_seconds,
    };
    let (cancel, cancel_rx) = tokio::sync::watch::channel(0u64);
    let workspace = config.workspace.clone();
    let state_dir = config.state_dir.clone();
    let (engine, run_id) =
        v1::dispatch::Engine::create(config, board, scorer, cancel_rx, &spec).await?;
    let engine = std::sync::Arc::new(engine);

    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nStopping: agents are cancelled and no further work is dispatched.");
            cancel.send_modify(|v| *v += 1);
        }
    });

    println!(
        "Firm v1 {} — run {}\nObjective: {}\n{} tasks, up to {} agents at once. Workspace: {}\n",
        if live { "LIVE" } else { "DEMO-SAFE (real CLIs, disposable workspace)" },
        &run_id[..8],
        spec.objective,
        spec.tasks.len(),
        v1::dispatch::MAX_CONCURRENT,
        workspace.display()
    );

    let outcome = engine.drive(&run_id).await?;
    let board = v1::board::Board::open(&v1::dispatch::board_path(&state_dir, live))?;
    summarise(&board, &run_id, &workspace)?;
    if let Some(check) = &outcome.final_check {
        println!(
            "Whole-project check on the integrated result: {}",
            if check.passed { "passed" } else { "FAILED" }
        );
        if !check.passed {
            println!("{}", v1::dispatch::clip(&check.detail, 2000));
        }
    }
    drop(lock);
    Ok(())
}

/// Follow a run in progress. Strictly read-only: it takes no lock and creates nothing, so
/// it is safe to run alongside the controller that owns the board.
async fn watch_run(board_path: &Path, workspace: &Path) -> Result<()> {
    loop {
        let board = v1::board::Board::open_readonly(board_path)?;
        let Some(run_id) = board.latest_run()? else {
            println!("No runs yet. Start one with: firm board --tasks PATH");
            return Ok(());
        };
        let run = board.run(&run_id)?;
        let tasks = board.tasks(&run_id)?;
        let attempts = board.attempts(&run_id)?;
        let finished = board.is_finished(&run_id)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Redraw in place rather than scrolling.
        print!("\x1b[2J\x1b[H");
        println!(
            "FIRM  run {}  {}  {}\n{}\n",
            &run_id[..8],
            if finished { "finished" } else { "running" },
            elapsed(now.saturating_sub(run.created_at)),
            run.objective.lines().next().unwrap_or_default()
        );
        for task in &tasks {
            let mark = match task.state.as_str() {
                "merged" => "✓",
                "running" => "▶",
                "failed" | "blocked" => "✗",
                _ => "·",
            };
            println!("  {mark} {:<8} {:<12} {}", task.state.as_str(), task.id, task.title);
            for attempt in attempts.iter().filter(|a| a["task_id"] == task.id) {
                let started = attempt["started_at"].as_u64().unwrap_or(now);
                let until = attempt["finished_at"].as_u64().unwrap_or(now);
                let files = attempt["files_changed"]
                    .as_array()
                    .map(|f| {
                        f.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                println!(
                    "      {} {:<9} {:>6}  exit {}  check {}{}",
                    attempt["provider"].as_str().unwrap_or("?"),
                    attempt["state"].as_str().unwrap_or("?"),
                    elapsed(until.saturating_sub(started)),
                    attempt["native_exit"]
                        .as_i64()
                        .map_or("–".to_string(), |c| c.to_string()),
                    match attempt["passed"].as_bool() {
                        Some(true) => "passed",
                        Some(false) => "failed",
                        None => "pending",
                    },
                    if files.is_empty() {
                        String::new()
                    } else {
                        format!("  [{files}]")
                    }
                );
            }
            if !task.note.trim().is_empty() && task.state.as_str() != "merged" {
                println!("      {}", task.note.lines().next().unwrap_or_default());
            }
        }
        let count = |state: &str| tasks.iter().filter(|t| t.state.as_str() == state).count();
        println!(
            "\n  merged {} · running {} · open {} · failed {} · blocked {}",
            count("merged"),
            count("running"),
            count("open"),
            count("failed"),
            count("blocked")
        );
        if finished {
            println!(
                "\nReview the work: git -C {} log --oneline {}",
                workspace.display(),
                run.integration_branch
            );
            return Ok(());
        }
        println!("\n  watching — Ctrl+C to stop (the run keeps going)");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

fn elapsed(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

/// Report a run's real outcome: each task's state, every attempt with its agent exit code
/// and the controller's separate verdict, and the branch holding the merged work.
fn summarise(board: &v1::board::Board, run_id: &str, workspace: &Path) -> Result<()> {
    let run = board.run(run_id)?;
    let tasks = board.tasks(run_id)?;
    let attempts = board.attempts(run_id)?;
    println!("\nRun {} — {}", &run_id[..8], run.objective);
    for task in &tasks {
        println!(
            "\n  {:<8} {}  ({})",
            task.state.as_str(),
            task.id,
            task.title
        );
        if !task.note.trim().is_empty() {
            println!("           {}", task.note.lines().next().unwrap_or_default());
        }
        for attempt in attempts.iter().filter(|a| a["task_id"] == task.id) {
            let files = attempt["files_changed"]
                .as_array()
                .map_or(0, std::vec::Vec::len);
            println!(
                "           · {} exit {} · check {} · {files} file(s) changed · {}",
                attempt["provider"].as_str().unwrap_or("?"),
                attempt["native_exit"]
                    .as_i64()
                    .map_or("none".to_string(), |c| c.to_string()),
                match attempt["passed"].as_bool() {
                    Some(true) => "passed",
                    Some(false) => "failed",
                    None => "not run",
                },
                attempt["state"].as_str().unwrap_or("?"),
            );
        }
    }
    let count = |state: v1::board::TaskState| tasks.iter().filter(|t| t.state == state).count();
    let held = tasks.iter().filter(|t| !t.state.terminal()).count();
    println!(
        "\nmerged {} · failed {} · blocked {} · held {}",
        count(v1::board::TaskState::Merged),
        count(v1::board::TaskState::Failed),
        count(v1::board::TaskState::Blocked),
        held
    );
    if held > 0 {
        println!(
            "Held tasks are still open: they ran out of allowance, not out of options.\n\
             They can be picked up once the rolling window rolls."
        );
    }
    println!(
        "Review the work: git -C {} log --oneline {}",
        workspace.display(),
        run.integration_branch
    );
    Ok(())
}
