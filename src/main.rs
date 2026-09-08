mod app;
mod codex;
mod config;
mod meetings;
mod snapshots;
mod state;
mod usage;
mod web;
mod worker;

use anyhow::{Context, Result, bail};
use config::Config;
use std::{fs::OpenOptions, os::fd::AsRawFd, path::Path};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Firm 0.1 — experimental agent team\n\n  firm serve [--live] [--config PATH]\n  firm probe [--config PATH]\n\nDefault: demo mode, localhost:7433, firm.toml.\nProbe reads Codex login type and rate limits; it never starts a model turn.\nLive mode uses your existing Codex subscription and configured worker CLI logins.\nStart app-server separately: codex app-server --listen ws://127.0.0.1:4500"
        );
        return Ok(());
    }
    let mut config_path = "firm.toml";
    let mut live = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--live" => live = true,
            "--config" => {
                index += 1;
                config_path = args.get(index).context("Missing config path")?;
            }
            other => bail!("Unknown argument: {other}"),
        }
        index += 1;
    }
    let config = Config::read(Path::new(config_path))?;
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
