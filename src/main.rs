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
            "Firm 0.1 — experimental agent team\n\n  firm serve [--live] [--config PATH]\n  firm probe [--config PATH]\n  firm usage                                 (what has been spent, and current allowances)\n  firm usage --provider ID (--percent N | --tokens N) [--label TEXT] [--run ID]\n  firm plan --brief BRIEF.md [--out tasks.json] [--build] [--config PATH]\n  firm board --tasks PATH [--live] [--build] [--config PATH]\n  firm board --resume RUN|latest             (continue a run that was stopped)\n  firm board [--watch | --forum | --retire ID | --prune | --stats] [--config PATH]\n  firm board --tasks PATH --provider ID   (force one provider, for comparisons)\n\nBoard runs the v1 engine: several agents work in parallel on a task graph, each in its\nown git worktree; only work passing verify_command is merged. The workspace must be a\nclean git repository and your own branch is never modified.\n\n--build is for ordinary projects, where no check can prove a task was done: the project's\nown tests become a regression guard, a roster model reviews each diff, and a rejected\ntask escalates to a dearer tier instead of retrying the same one. Its record is kept\napart from trial mode's, because only one of them is proof.\n\nDefault: demo mode, localhost:7433, firm.toml.\nProbe reads Codex login type and rate limits; it never starts a model turn.\nLive mode uses your existing Codex subscription and configured worker CLI logins.\nStart app-server separately: codex app-server --listen ws://127.0.0.1:4500"
        );
        return Ok(());
    }
    let mut config_path = "firm.toml";
    let mut tasks_path: Option<&str> = None;
    let mut live = false;
    let mut build = false;
    let mut watch = false;
    let mut forum = false;
    let mut retire: Option<&str> = None;
    let mut prune = false;
    let mut stats = false;
    let mut resume: Option<&str> = None;
    let mut validation: Option<&str> = None;
    let mut brief_path: Option<&str> = None;
    let mut out_path: Option<&str> = None;
    let mut provider: Option<&str> = None;
    let mut percent: Option<f64> = None;
    let mut tokens: Option<f64> = None;
    let mut label: Option<&str> = None;
    let mut run: Option<&str> = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--live" => live = true,
            "--build" => build = true,
            "--config" => {
                index += 1;
                config_path = args.get(index).context("Missing config path")?;
            }
            "--tasks" => {
                index += 1;
                tasks_path = Some(args.get(index).context("Missing tasks path")?);
            }
            "--watch" => watch = true,
            "--forum" => forum = true,
            "--prune" => prune = true,
            "--stats" => stats = true,
            "--resume" => {
                index += 1;
                resume = Some(args.get(index).context("Missing run id, or 'latest'")?);
            }
            "--provider" => {
                index += 1;
                provider = Some(args.get(index).context("Missing provider id")?);
            }
            "--percent" => {
                index += 1;
                percent = args.get(index).context("Missing percentage")?.parse().ok();
            }
            "--tokens" => {
                index += 1;
                tokens = args.get(index).context("Missing token count")?.parse().ok();
            }
            "--label" => {
                index += 1;
                label = Some(args.get(index).context("Missing label")?);
            }
            "--run" => {
                index += 1;
                run = Some(args.get(index).context("Missing run id")?);
            }
            "--validation" => {
                index += 1;
                validation = Some(args.get(index).map_or("latest", String::as_str));
            }
            "--brief" => {
                index += 1;
                brief_path = Some(args.get(index).context("Missing brief path")?);
            }
            "--out" => {
                index += 1;
                out_path = Some(args.get(index).context("Missing output path")?);
            }
            "--retire" => {
                index += 1;
                retire = Some(args.get(index).context("Missing forum entry id")?);
            }
            other => bail!("Unknown argument: {other}"),
        }
        index += 1;
    }
    let mut config = Config::read(Path::new(config_path))?;
    // Build mode is for ordinary projects, where no check can prove a task was done. It
    // changes what a merge means, so it is never inferred — only asked for.
    if build {
        config.mode = "build".into();
    }
    if args[0] == "usage" {
        return usage_command(config, live, provider, percent, tokens, label, run).await;
    }
    if args[0] == "plan" {
        return plan(config, brief_path, out_path, live).await;
    }
    if args[0] == "board" {
        return board(
            config,
            BoardOptions {
                tasks_path,
                live,
                watch,
                forum,
                retire,
                prune,
                stats,
                force_provider: provider,
                resume,
                validation,
            },
        )
        .await;
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
struct BoardOptions<'a> {
    tasks_path: Option<&'a str>,
    live: bool,
    watch: bool,
    forum: bool,
    retire: Option<&'a str>,
    prune: bool,
    stats: bool,
    force_provider: Option<&'a str>,
    resume: Option<&'a str>,
    validation: Option<&'a str>,
}

async fn board(config: Config, options: BoardOptions<'_>) -> Result<()> {
    let BoardOptions {
        tasks_path,
        live,
        watch,
        forum,
        retire,
        prune,
        stats,
        force_provider,
        resume,
        validation,
    } = options;
    let board_path = v1::dispatch::board_path(&config.state_dir, live);
    if prune {
        return prune_worktrees(&config, live).await;
    }
    if let Some(target) = validation {
        return show_validation(&config, live, target);
    }
    if stats {
        // What routing is actually going on, so the choice is inspectable rather than
        // something the system does silently.
        let board = v1::board::Board::open_readonly(&board_path)?;
        let since = v1::dispatch::evidence_since();
        // Records are kept per regime, so say which one is being shown. A build-mode
        // merge means a reviewer was satisfied; a trial-mode merge means a check proved it.
        let stats = board.provider_stats(since, &config.mode)?;
        println!(
            "Provider record in {} mode over the last {} days, as routing sees it:\n",
            config.mode,
            v1::dispatch::EVIDENCE_WINDOW_SECONDS / 86400
        );
        if stats.is_empty() {
            println!("  No attempts yet; routing falls back to cost alone.");
        }
        for (id, record) in &stats {
            let enough = record.attempts >= v1::dispatch::MIN_EVIDENCE;
            println!(
                "  {id:<8} {:>3} attempts  {:>3}% accepted  median {}s{}",
                record.attempts,
                record.success_percent(),
                record.median_seconds(),
                if !enough {
                    "   (too few to judge; given the benefit of the doubt)"
                } else if record.success_percent() < v1::dispatch::MIN_SUCCESS_PERCENT {
                    "   (tried last: too often rejected to be cheap)"
                } else {
                    ""
                }
            );
        }
        println!("\nRouting: {}. Override with --provider ID, or pin a task.", config.routing);
        return Ok(());
    }
    if let Some(id) = retire {
        // Knowledge goes stale: an entry true of one environment is false after a fix.
        let board = v1::board::Board::open(&board_path)?;
        let retired = board.forum().retire(id)?;
        println!(
            "{}",
            if retired {
                format!("Retired {id}; it will not be shown to agents again.")
            } else {
                format!("No forum entry {id}")
            }
        );
        return Ok(());
    }
    if forum {
        let board = v1::board::Board::open_readonly(&board_path)?;
        let run_id = board.latest_run()?.context("No runs yet")?;
        let entries = board.forum().entries(&run_id)?;
        println!("Forum for run {} — {} entries\n", &run_id[..8], entries.len());
        for entry in entries {
            println!(
                "[{}] {}  (by {})\n    id {}",
                entry.kind.as_str(),
                entry.title,
                entry.author,
                entry.id
            );
            for line in entry.body.lines() {
                println!("    {line}");
            }
            println!();
        }
        return Ok(());
    }
    if watch {
        return watch_run(&board_path, &config.workspace).await;
    }
    // With no task file, report on the most recent run instead of starting one.
    if let Some(target) = resume {
        return resume_run(config, live, target, force_provider).await;
    }
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
    let config_usage = config.record_usage;
    let usage_config = config.clone();
    let (engine, run_id) =
        v1::dispatch::Engine::create(config, board, scorer, cancel_rx, &spec).await?;

    // Report progress as it happens, so a run is legible without a second terminal.
    let (progress, mut events) = tokio::sync::mpsc::unbounded_channel();
    let engine = std::sync::Arc::new(
        engine
            .with_progress(progress)
            .with_forced_provider(force_provider.map(str::to_string)),
    );
    let started = std::time::Instant::now();
    let printer = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            print_progress(event, started);
        }
    });

    // Stopping must be safe and reversible: everything merged is already a commit on the
    // run's branch, and the run can be picked up where it left off.
    let canceller = cancel.clone();
    let stop_id = run_id.clone();
    tokio::spawn(async move {
        stop_signal().await;
        eprintln!(
            "\nStopping. Work already merged is safe on the run branch; continue with:\n  \
             firm board --resume {}",
            &stop_id[..8]
        );
        canceller.send_modify(|v| *v += 1);
    });

    let before = if config_usage {
        v1::usage::sample(&usage_config).await
    } else {
        Vec::new()
    };
    println!(
        "Firm v1 {} — run {}\nObjective: {}\n{} tasks, up to {} agents at once. Workspace: {}\nFollow along here, or in another terminal with: firm board --watch\n",
        if live { "LIVE" } else { "DEMO-SAFE (real CLIs, disposable workspace)" },
        &run_id[..8],
        spec.objective,
        spec.tasks.len(),
        v1::dispatch::MAX_CONCURRENT,
        workspace.display()
    );

    let outcome = engine.drive(&run_id).await?;
    // Close the channel so the printer drains and stops before the summary.
    drop(engine);
    let _ = printer.await;

    // What the run consumed, in the currency that matters for a subscription: share of a
    // rolling window. Sampled twice, never per call.
    let consumed = if config_usage {
        let after = v1::usage::sample(&usage_config).await;
        let used = v1::usage::consumed(&before, &after);
        if let Ok(mut board) = v1::board::Board::open(&board_path) {
            let _ = board.record_usage(&run_id, "before", &before);
            let _ = board.record_usage(&run_id, "after", &after);
        }
        used
    } else {
        Vec::new()
    };
    let board = v1::board::Board::open(&v1::dispatch::board_path(&state_dir, live))?;
    summarise(&board, &run_id, &workspace)?;
    if !consumed.is_empty() {
        println!(
            "\nThis run consumed: {}",
            consumed
                .iter()
                .map(v1::usage::describe)
                .collect::<Vec<_>>()
                .join(" · ")
        );
    }
    if let Some(check) = &outcome.final_check {
        println!(
            "Whole-project check on the integrated result: {}",
            if check.passed { "passed" } else { "FAILED" }
        );
        if !check.passed {
            println!("{}", v1::dispatch::clip(&check.detail, 2000));
        }
    }

    // Validation, kept visually apart from the task-by-task record because it answers a
    // different question: not whether the work was done, but whether the result is any use.
    if !outcome.probes.is_empty() {
        let failed = outcome.probes.iter().filter(|p| !p.passed).count();
        println!("\nDoes it do the job? Probes written before the plan, run on the result:");
        for probe in &outcome.probes {
            println!(
                "  {} {:<18} {}",
                if probe.passed { "✓" } else { "✗" },
                probe.id,
                probe.description
            );
            if !probe.passed {
                println!("      {}", probe.command);
                for line in probe.detail.lines().take(4) {
                    println!("      {line}");
                }
            }
        }
        if failed > 0 {
            println!(
                "\n{failed} of {} probes failed. The tasks may all have been done correctly \
                 and the objective still not met — that is what these are for. Nothing has \
                 been reverted; decide what to change and run the project again.",
                outcome.probes.len()
            );
        }
    }
    if !outcome.criteria.is_empty() {
        println!("\nFor you to judge — nobody should pretend to automate these:");
        for criterion in &outcome.criteria {
            println!("  - {criterion}");
        }
    }
    drop(lock);
    Ok(())
}

/// Reprint what validation said about a finished run.
///
/// Separate from the run's own output because it is the part worth coming back to: the
/// task record says what was built, this says whether it was worth building.
fn show_validation(config: &Config, live: bool, target: &str) -> Result<()> {
    let board_path = v1::dispatch::board_path(&config.state_dir, live);
    let board = v1::board::Board::open_readonly(&board_path)?;
    let run_id = match target {
        "latest" => board.latest_run()?.context("No runs yet")?,
        id => board.run(id).map(|run| run.id)?,
    };
    let run = board.run(&run_id)?;
    println!("{}\n{}\n", run.objective, "-".repeat(run.objective.len().min(78)));

    let results = board.probe_results(&run_id)?;
    if results.is_empty() {
        if run.validation.probes.is_empty() {
            println!("This run had no validation model: nothing asked whether the objective was met.");
        } else {
            println!("Validation was defined but never run — the run was stopped before the end.");
            for probe in &run.validation.probes {
                println!("  pending  {:<18} {}", probe.id, probe.description);
            }
        }
    } else {
        let failed = results.iter().filter(|p| !p.passed).count();
        println!("Probes, written before the plan and run on the finished result:");
        for probe in &results {
            println!(
                "  {} {:<18} {}",
                if probe.passed { "✓" } else { "✗" },
                probe.id,
                probe.description
            );
            if !probe.passed {
                println!("      {}", probe.command);
                for line in probe.detail.lines().take(6) {
                    println!("      {line}");
                }
            }
        }
        println!(
            "\n{} of {} passed.",
            results.len() - failed,
            results.len()
        );
    }
    if !run.validation.criteria.is_empty() {
        println!("\nFor you to judge — nobody should pretend to automate these:");
        for criterion in &run.validation.criteria {
            println!("  - {criterion}");
        }
    }
    Ok(())
}

/// Record a cost figure observed by hand.
///
/// Some providers cannot be probed from a board run — Codex reports through app-server,
/// which a run does not hold open — so what the operator can see is entered directly and
/// stored alongside the sampled readings.
async fn usage_command(
    config: Config,
    live: bool,
    provider: Option<&str>,
    percent: Option<f64>,
    tokens: Option<f64>,
    label: Option<&str>,
    run: Option<&str>,
) -> Result<()> {
    // With nothing to record, report instead: what has been spent, and where things stand.
    if percent.is_none() && tokens.is_none() {
        return show_usage(&config, live).await;
    }
    let provider = provider.context("Recording a figure requires --provider ID")?;
    let (metric, value) = match (percent, tokens) {
        (Some(percent), None) => ("percent", percent),
        (None, Some(tokens)) => ("tokens", tokens),
        _ => bail!("Give exactly one of --percent or --tokens"),
    };
    let board_path = v1::dispatch::board_path(&config.state_dir, live);
    let mut board = v1::board::Board::open(&board_path)?;
    let run_id = match run {
        Some(id) => id.to_string(),
        None => board
            .latest_run()?
            .context("No runs yet; give --run ID to attribute this to one")?,
    };
    let sample = v1::usage::Sample {
        provider: provider.to_string(),
        metric: metric.to_string(),
        label: label.unwrap_or("observed by hand").to_string(),
        value,
    };
    board.record_usage(&run_id, "manual", std::slice::from_ref(&sample))?;
    println!(
        "Recorded against run {}: {}",
        &run_id[..8.min(run_id.len())],
        v1::usage::describe(&sample)
    );
    Ok(())
}

/// What has been spent, and what each provider reports right now.
async fn show_usage(config: &Config, live: bool) -> Result<()> {
    let board_path = v1::dispatch::board_path(&config.state_dir, live);
    if let Ok(board) = v1::board::Board::open_readonly(&board_path) {
        let totals = board.usage_totals().unwrap_or_default();
        println!("Consumed across all recorded runs:\n");
        if totals.is_empty() {
            println!("  Nothing recorded yet.");
        }
        for total in totals {
            if total.metric == "percent" {
                println!(
                    "  {:<8} {:>8.1}% of {}   over {} run(s)",
                    total.provider, total.value, total.label, total.runs
                );
            } else {
                println!(
                    "  {:<8} {:>8} tokens   over {} run(s)",
                    total.provider, total.value as u64, total.runs
                );
            }
        }
    }

    // A reading costs seconds per provider, so say what is happening rather than appearing
    // to hang.
    println!("\nReading each provider's current allowance (this takes a few seconds)...");
    let now = v1::usage::sample(config).await;
    println!();
    if now.is_empty() {
        println!("  No provider reported a reading.");
    }
    for sample in &now {
        if sample.metric == "percent" {
            println!(
                "  {:<8} {:>8.1}% of {} used",
                sample.provider, sample.value, sample.label
            );
        } else {
            // These probes start a fresh CLI session and ask it for its usage, so the
            // figure describes that session, not the account. Reported, but not trusted.
            println!(
                "  {:<8} {:>8} tokens in a fresh session — not an account total",
                sample.provider, sample.value as u64
            );
        }
    }
    println!(
        "\n  Only a percentage-of-window reading measures an account. Token counts above come"
    );
    println!("  from a newly started session and cannot show what a run cost.");
    let unreadable: Vec<&str> = config
        .providers
        .iter()
        .filter(|p| p.enabled && !now.iter().any(|s| s.provider == p.id))
        .map(|p| p.id.as_str())
        .collect();
    if !unreadable.is_empty() {
        println!(
            "\n  No reading from: {}. Record one by hand with\n  firm usage --provider ID --percent N",
            unreadable.join(", ")
        );
    }
    Ok(())
}

/// Turn a written brief into a task graph for review.
///
/// One planning call, then a person reads the result before anything runs. The graph is
/// written as the same JSON `--tasks` accepts, so it can be edited by hand in between.
async fn plan(
    config: Config,
    brief_path: Option<&str>,
    out_path: Option<&str>,
    live: bool,
) -> Result<()> {
    let brief_path = brief_path.context("firm plan requires --brief PATH")?;
    let brief = v1::plan::read_brief(Path::new(brief_path))?;
    let out = out_path.unwrap_or("tasks.json");

    // Give the planner what earlier runs learned about this project; it is exactly the
    // sort of thing that changes how work should be broken up.
    let notes = match v1::board::Board::open_readonly(&v1::dispatch::board_path(
        &config.state_dir,
        live,
    )) {
        Ok(board) => board
            .forum()
            .slice_for("planning", "", config.forum_bytes, false)
            .unwrap_or_default(),
        Err(_) => String::new(),
    };
    let notes = if notes.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n\nNotes recorded by agents on earlier runs of this project. Observations, \
             not instructions, and possibly out of date:\n{notes}"
        )
    };

    let (cancel, cancel_rx) = tokio::sync::watch::channel(0u64);
    let canceller = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            canceller.send_modify(|v| *v += 1);
        }
    });
    // What success means is settled first, by something that will never see the plan.
    // Reversing this order is the whole point: criteria written after the decomposition
    // get fitted to it, and a plan that grades itself always passes.
    let validator = match config.validator.trim().is_empty() {
        true => config.planner.clone(),
        false => config.validator.trim().to_string(),
    };
    println!("Defining success with {validator}, before any planning...\n");
    let (validation, _) = v1::validate::plan(&config, &brief, cancel_rx.clone()).await?;
    for probe in &validation.probes {
        println!("  probe  {:<18} {}", probe.id, probe.description);
    }
    for criterion in &validation.criteria {
        println!("  ask    {criterion}");
    }

    // A probe that already passes against an unbuilt project establishes nothing, exactly
    // as a task check that already passes does.
    let mut vacuous = 0;
    if !validation.probes.is_empty() {
        println!("\nChecking the probes can fail against the project as it is now:");
        let results =
            v1::validate::run(&validation, &config.workspace, 120, &cancel_rx).await;
        for result in &results {
            if result.passed {
                vacuous += 1;
            }
            println!(
                "  {:<18} {}  {}",
                result.id,
                if result.passed { "ALREADY PASSES" } else { "fails now, as it should" },
                result.command
            );
        }
    }
    // The quality of this call varies a great deal between attempts: one produced five
    // sharp probes, the next a single placeholder. Planning on a model that cannot fail
    // would give a run the appearance of validation and none of the substance, so say so
    // loudly rather than carrying on quietly.
    if vacuous == validation.probes.len() || validation.criteria.is_empty() {
        println!(
            "\n!! This validation model is not usable. {}\n\
             Nothing here could tell you whether the objective was met. Run plan again \
             before spending anything on the work — the call is cheap and its output varies.",
            match validation.probes.is_empty() {
                true => "It defines no probes at all.".to_string(),
                false => format!(
                    "All {} probes already pass against a project that has not been built, \
                     and there {}.",
                    validation.probes.len(),
                    match validation.criteria.is_empty() {
                        true => "are no criteria either",
                        false => "are criteria, but nothing executable",
                    }
                ),
            }
        );
    }

    println!("\nPlanning with {} from {brief_path}...\n", config.planner);
    let (mut spec, _raw) =
        v1::plan::decompose(&config, &brief, &notes, &validation, cancel_rx.clone()).await?;
    spec.brief = brief.clone();

    println!("{} — {} tasks\n", spec.objective, spec.tasks.len());
    for task in &spec.tasks {
        let deps = if task.depends_on.is_empty() {
            "no dependencies".to_string()
        } else {
            format!("after {}", task.depends_on.join(", "))
        };
        println!("  {:<16} {}  ({deps})", task.id, task.title);
    }

    // A proposed check is worth nothing until it has been run. One that cannot execute
    // fails every attempt; one that already passes merges everything unconditionally.
    println!("\nChecking the proposed verify commands against the workspace as it is now:");
    let reports = v1::plan::check_proposals(&config, &spec, &cancel_rx).await;
    let mut unsound = 0;
    for report in &reports {
        let verdict = if !report.ran {
            unsound += 1;
            "WILL NOT RUN"
        } else if report.vacuous {
            unsound += 1;
            "ALREADY PASSES"
        } else {
            "fails now, as it should"
        };
        let exit = report
            .exit
            .map_or_else(|| "no exit".to_string(), |code| format!("exit {code}"));
        println!(
            "  {:<16} {verdict:<24} {exit:<9} {}",
            report.task, report.command
        );
        if !report.sound() && !report.detail.trim().is_empty() {
            println!("      {}", report.detail.lines().next().unwrap_or_default());
        }
    }

    let unguarded = v1::plan::ungarded(&spec);
    if !unguarded.is_empty() {
        println!("\nGuards this plan leaves off:");
        for warning in &unguarded {
            println!("  {warning}");
        }
    }

    std::fs::write(out, v1::plan::to_tasks_json(&spec)?)?;
    println!("\nWritten to {out}.");
    if unsound > 0 {
        println!(
            "{unsound} of {} checks are unsound. Fix them in {out} before running, or the \
             affected tasks cannot be judged.",
            reports.len()
        );
    }
    println!("Review it, then: firm board --tasks {out}");
    Ok(())
}

/// Any signal that means "stop now".
///
/// Ctrl+C is not the only way a run ends: a closed terminal sends SIGHUP and `kill` sends
/// SIGTERM, and both previously killed the controller outright — which strands whatever the
/// agents had written, because nothing got the chance to commit it.
async fn stop_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(handler) => handler,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    let mut hangup = match signal(SignalKind::hangup()) {
        Ok(handler) => handler,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
        _ = hangup.recv() => {}
    }
}

/// One line of run progress, shared by a fresh run and a resumed one.
fn print_progress(event: v1::dispatch::Progress, started: std::time::Instant) {
    let at = elapsed(started.elapsed().as_secs());
    match event {
        v1::dispatch::Progress::Dispatched { task, provider } => {
            println!("  [{at:>6}] → {task:<12} dispatched to {provider}");
        }
        v1::dispatch::Progress::Attempt {
            task,
            provider,
            state,
            seconds,
            native_exit,
            passed,
            files,
            detail,
        } => {
            println!(
                "  [{at:>6}] {} {task:<12} {provider} {} in {} · exit {} · check {}{}",
                if state == v1::board::AttemptState::Verified { "✓" } else { "✗" },
                state.as_str(),
                elapsed(seconds),
                native_exit.map_or("–".to_string(), |c| c.to_string()),
                match passed {
                    Some(true) => "passed",
                    Some(false) => "failed",
                    None => "not run",
                },
                if files.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", files.join(", "))
                }
            );
            if state != v1::board::AttemptState::Verified && !detail.trim().is_empty() {
                println!("            {}", detail.lines().next().unwrap_or_default());
            }
        }
        v1::dispatch::Progress::Task { task, state, note } => match state.as_str() {
            "merged" => println!("  [{at:>6}] ✓ {task:<12} merged into the integration branch"),
            "running" | "open" => {}
            other => println!(
                "  [{at:>6}] ✗ {task:<12} {other} — {}",
                note.lines().next().unwrap_or_default()
            ),
        },
    }
}

/// Continue a run that was stopped, rather than starting the work again.
///
/// Everything already merged stays merged: the run keeps its integration branch, and only
/// tasks still outstanding are dispatched. Work interrupted mid-flight returns to the queue.
async fn resume_run(
    config: Config,
    live: bool,
    target: &str,
    force_provider: Option<&str>,
) -> Result<()> {
    let board_path = v1::dispatch::board_path(&config.state_dir, live);
    let run_id = {
        let board = v1::board::Board::open_readonly(&board_path)?;
        if target == "latest" {
            board.latest_run()?.context("No runs to resume")?
        } else {
            board.run(target).map(|run| run.id)?
        }
    };

    let board = v1::board::Board::open(&board_path)?;
    let (cancel, cancel_rx) = tokio::sync::watch::channel(0u64);
    let scorer = v1::scorer::Scorer::Command {
        command: config.verify_command.clone(),
        timeout_seconds: config.allowances.worker_timeout_seconds,
    };
    let workspace = config.workspace.clone();
    let state_dir = config.state_dir.clone();
    let engine =
        v1::dispatch::Engine::attach(config, board, scorer, cancel_rx, &run_id).await?;

    // An unclean stop leaves work uncommitted in an attempt worktree. Rescue it onto its
    // branch before anything else, or it is lost the moment the worktree is reused.
    for (branch, files) in engine.salvage(&run_id).await.unwrap_or_default() {
        println!(
            "  Recovered work left by an unclean stop onto {branch}: {}",
            files.join(", ")
        );
    }

    let board = v1::board::Board::open_readonly(&board_path)?;
    let outstanding = board
        .tasks(&run_id)?
        .into_iter()
        .filter(|t| !t.state.terminal())
        .count();
    drop(board);
    println!(
        "Resuming run {} — {outstanding} task(s) still outstanding.",
        &run_id[..8]
    );
    // Anything an agent had written when it was stopped is on its own branch. It is not
    // reused automatically — a half-finished attempt is not a good starting point — but it
    // should not disappear silently either.
    let board = v1::board::Board::open_readonly(&board_path)?;
    let salvage: Vec<String> = board
        .attempts(&run_id)?
        .iter()
        .filter(|a| {
            a["state"] == "error"
                && a["files_changed"].as_array().is_some_and(|f| !f.is_empty())
        })
        .map(|a| {
            format!(
                "  {} left work on {}: {}",
                a["task_id"].as_str().unwrap_or("?"),
                a["branch"].as_str().unwrap_or("?"),
                a["files_changed"]
                    .as_array()
                    .map(|f| f.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default()
            )
        })
        .collect();
    drop(board);
    if !salvage.is_empty() {
        println!(
            "\nInterrupted attempts kept what they had written. These are not reused; the\n             tasks start again cleanly. Inspect or cherry-pick if any of it was worth having:"
        );
        for line in &salvage {
            println!("{line}");
        }
    }
    println!();
    if outstanding == 0 {
        println!("Nothing left to do.");
        return Ok(());
    }

    let (progress, mut events) = tokio::sync::mpsc::unbounded_channel();
    let engine = std::sync::Arc::new(
        engine
            .with_progress(progress)
            .with_forced_provider(force_provider.map(str::to_string)),
    );
    let started = std::time::Instant::now();
    let printer = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            print_progress(event, started);
        }
    });
    let canceller = cancel.clone();
    let stop_id = run_id.clone();
    tokio::spawn(async move {
        stop_signal().await;
        eprintln!(
            "\nStopping. Merged work is safe on the run branch; continue with:\n  \
             firm board --resume {}",
            &stop_id[..8]
        );
        canceller.send_modify(|v| *v += 1);
    });

    engine.drive(&run_id).await?;
    drop(engine);
    let _ = printer.await;
    let board = v1::board::Board::open(&v1::dispatch::board_path(&state_dir, live))?;
    summarise(&board, &run_id, &workspace)
}

/// Remove the checked-out integration worktrees of finished runs.
///
/// Each is a full checkout plus whatever the scorer built, around 48 MiB for a small Rust
/// crate, and they accumulate one per run. Nothing is lost: the run's branch retains every
/// commit, and a worktree can be recreated with `git worktree add`. The most recent run is
/// kept, since that is the one you are most likely to want to look at.
async fn prune_worktrees(config: &Config, live: bool) -> Result<()> {
    let root = config.state_dir.join("worktrees");
    if !root.exists() {
        println!("No worktrees to prune.");
        return Ok(());
    }
    let board = v1::board::Board::open_readonly(&v1::dispatch::board_path(&config.state_dir, live));
    let keep = match &board {
        Ok(board) => board.latest_run().unwrap_or_default(),
        Err(_) => None,
    };
    let keep_dir = keep.as_ref().map(|id| format!("run-{}", &id[..8]));

    let mut removed = 0usize;
    let mut freed = 0u64;
    for entry in std::fs::read_dir(&root)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if !path.is_dir() || Some(&name) == keep_dir.as_ref() {
            continue;
        }
        let size = directory_size(&path);
        // Ask git to release it first so the repository's worktree list stays consistent.
        let _ = v1::worktree::git(
            &config.workspace,
            &["worktree", "remove", "--force", &path.join("integration").to_string_lossy()],
        )
        .await;
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
        removed += 1;
        freed += size;
    }
    let _ = v1::worktree::git(&config.workspace, &["worktree", "prune"]).await;
    println!(
        "Pruned {removed} worktree(s), {:.0} MiB freed.{}\nEvery run's work remains on its firm/run-<id> branch.",
        freed as f64 / (1024.0 * 1024.0),
        keep_dir.map_or(String::new(), |k| format!(" Kept the most recent, {k}."))
    );
    Ok(())
}

fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => directory_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
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
                // What the agent is doing right now, while it is still running.
                let doing = attempt["activity"].as_str().unwrap_or_default();
                if attempt["state"] == "running" && !doing.is_empty() {
                    println!("        {doing}");
                }
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
    // A plan that changed while it ran is worth seeing, including changes that were asked
    // for and refused.
    let proposals = board.mutations(run_id)?;
    if !proposals.is_empty() {
        println!("\n  Plan changes proposed during the run:");
        for proposal in &proposals {
            println!(
                "    {} {} {} — {}",
                if proposal["accepted"] == true { "applied " } else { "refused " },
                proposal["kind"].as_str().unwrap_or("?"),
                proposal["target"].as_str().unwrap_or("?"),
                proposal["reason"].as_str().unwrap_or_default()
            );
        }
    }

    let consumed = board.usage_consumed(run_id).unwrap_or_default();
    if !consumed.is_empty() {
        println!(
            "\n  Consumed: {}",
            consumed.iter().map(v1::usage::describe).collect::<Vec<_>>().join(" · ")
        );
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
