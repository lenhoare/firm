use crate::{
    config::{Config, PromptInput, Provider},
    state::Assignment,
};
use anyhow::{Context, Result, bail};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::watch,
    time::timeout,
};

const OUTPUT_LIMIT: usize = 48 * 1024;

pub struct WorkerResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub interruption: Option<String>,
}

// A task-owned process group cannot outlive a normal controller exit or cancelled future.
struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin) -> Result<String> {
    let mut retained = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let keep = n.min(OUTPUT_LIMIT.saturating_sub(retained.len()));
        retained.extend_from_slice(&chunk[..keep]);
        truncated |= keep < n;
    }
    let mut text = String::from_utf8_lossy(&retained).into_owned();
    if truncated {
        text.push_str("\n[Output exceeded retention limit; inspect workspace evidence.]");
    }
    Ok(text)
}

pub fn prompt(assignment: &Assignment) -> String {
    format!(
        "{}\nAutonomy: {}\n\n{}\n{}\n\nAcceptance criteria:\n{}",
        include_str!("../prompts/worker.md"),
        assignment.autonomy,
        assignment.title,
        assignment.brief,
        assignment.acceptance.join("\n")
    )
}

pub struct Prepared {
    provider: String,
    program: String,
    args: Vec<String>,
    prompt: String,
    // Keep the private prompt file alive until the worker and its descendants exit.
    prompt_file: Option<tempfile::NamedTempFile>,
}

impl Prepared {
    pub fn request(&self, config: &Config, demo: bool) -> serde_json::Value {
        serde_json::json!({"demo":demo,"provider":self.provider,"program":self.program,"args":self.args,
            "stdin":if self.prompt_file.is_none() { Some(&self.prompt) } else { None },
            "prompt_file":self.prompt_file.as_ref().map(|f| serde_json::json!({"path":f.path(),"content":self.prompt,"temporary":true})),
            "cwd":config.workspace,"verification":config.verify_command,
            "controller_timeout_seconds":config.allowances.worker_timeout_seconds})
    }
}

pub fn prepare(config: &Config, provider: &Provider, assignment: &Assignment) -> Result<Prepared> {
    anyhow::ensure!(
        provider.id == assignment.provider,
        "Worker provider does not match the assignment"
    );
    prepare_prompt(config, provider, prompt(assignment))
}

pub fn prepare_prompt(config: &Config, provider: &Provider, prompt: String) -> Result<Prepared> {
    use std::io::Write;
    anyhow::ensure!(
        provider.enabled,
        "Worker provider is disabled or does not match the assignment"
    );
    let prompt_file = if provider.input == PromptInput::PromptFile {
        let mut file = tempfile::Builder::new().prefix("firm-prompt-").tempfile()?;
        file.write_all(prompt.as_bytes())?;
        file.flush()?;
        Some(file)
    } else {
        None
    };
    let file_path = prompt_file
        .as_ref()
        .map(|f| f.path().to_string_lossy().into_owned())
        .unwrap_or_default();
    let args = provider
        .args
        .iter()
        .map(|arg| {
            arg.replace(
                "{max_turns}",
                &config.allowances.worker_max_turns.to_string(),
            )
            .replace(
                "{max_tool_calls}",
                &config.allowances.worker_max_tool_calls.to_string(),
            )
            .replace(
                "{timeout_seconds}",
                &config.allowances.worker_timeout_seconds.to_string(),
            )
            .replace("{codex_model}", &config.codex_model)
            .replace("{workspace}", &config.workspace.to_string_lossy())
            .replace("{prompt_file}", &file_path)
        })
        .collect();
    Ok(Prepared {
        provider: provider.id.clone(),
        program: provider.command.clone(),
        args,
        prompt,
        prompt_file,
    })
}

pub async fn run(
    config: &Config,
    prepared: &Prepared,
    mut cancel: watch::Receiver<u64>,
) -> Result<WorkerResult> {
    anyhow::ensure!(!cancel.has_changed()?, "Cancelled before worker spawn");
    let mut command = Command::new(&prepared.program);
    command
        .args(&prepared.args)
        .current_dir(&config.workspace)
        .stdin(if prepared.prompt_file.is_none() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().with_context(|| {
        format!(
            "Could not start {} ({})",
            prepared.provider, prepared.program
        )
    })?;
    let group = ProcessGroup(child.id().context("Worker has no PID")?);
    let stdout = tokio::spawn(read_bounded(child.stdout.take().unwrap()));
    let stderr = tokio::spawn(read_bounded(child.stderr.take().unwrap()));
    if let Some(mut stdin) = child.stdin.take() {
        tokio::select! {
            result = timeout(Duration::from_secs(5), stdin.write_all(prepared.prompt.as_bytes())) => result??,
            _ = cancel.changed() => bail!("Worker cancelled while sending prompt"),
        }
    }
    let outcome = tokio::select! {
        result = child.wait() => result.map_err(anyhow::Error::from),
        _ = cancel.changed() => Err(anyhow::anyhow!("Worker cancelled by controller")),
        _ = tokio::time::sleep(Duration::from_secs(config.allowances.worker_timeout_seconds)) => Err(anyhow::anyhow!("Worker reached its time limit")),
    };
    drop(group); // Includes descendants holding stdout/stderr open after parent exit.
    if outcome.is_err() {
        let _ = timeout(Duration::from_secs(3), child.wait()).await;
    }
    let out = timeout(Duration::from_secs(3), stdout)
        .await
        .context("Worker stdout did not close")???;
    let err = timeout(Duration::from_secs(3), stderr)
        .await
        .context("Worker stderr did not close")???;
    let (exit_code, interruption) = match outcome {
        Ok(status) => (status.code(), None),
        Err(error) => (None, Some(error.to_string())),
    };
    Ok(WorkerResult {
        output: format!("{out}\n\nWorker stderr:\n{err}"),
        exit_code,
        interruption,
    })
}

pub async fn verify(config: &Config, mut cancel: watch::Receiver<u64>) -> Result<WorkerResult> {
    anyhow::ensure!(
        !cancel.has_changed()?,
        "Cancelled before verification spawn"
    );
    let (program, args) = config
        .verify_command
        .split_first()
        .context("No verification command")?;
    let mut child = Command::new(program)
        .args(args)
        .current_dir(&config.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()
        .context("Verification could not start")?;
    let group = ProcessGroup(child.id().context("Verification has no PID")?);
    let stdout = tokio::spawn(read_bounded(child.stdout.take().unwrap()));
    let stderr = tokio::spawn(read_bounded(child.stderr.take().unwrap()));
    let outcome = tokio::select! {
        result = child.wait() => result.map_err(anyhow::Error::from),
        _ = cancel.changed() => Err(anyhow::anyhow!("Verification cancelled")),
        _ = tokio::time::sleep(Duration::from_secs(60)) => Err(anyhow::anyhow!("Verification timed out after 60s")),
    };
    drop(group);
    if outcome.is_err() {
        let _ = timeout(Duration::from_secs(3), child.wait()).await;
    }
    let out = timeout(Duration::from_secs(3), stdout).await???;
    let err = timeout(Duration::from_secs(3), stderr).await???;
    let (exit_code, interruption) = match outcome {
        Ok(status) => (status.code(), None),
        Err(error) => (None, Some(error.to_string())),
    };
    Ok(WorkerResult {
        output: format!(
            "Controller verification {:?}\nExit: {exit_code:?}\n{out}\n{err}",
            config.verify_command
        ),
        exit_code,
        interruption,
    })
}

pub fn looks_rate_limited(output: &str) -> bool {
    let output = output.to_ascii_lowercase();
    [
        "rate limit",
        "rate_limit",
        "quota exceeded",
        "too many requests",
        "usage limit",
    ]
    .iter()
    .any(|s| output.contains(s))
}

pub async fn demo(mut cancel: watch::Receiver<u64>, fail: bool) -> Result<WorkerResult> {
    tokio::select! {
        _ = cancel.changed() => bail!("Demo worker cancelled"),
        _ = tokio::time::sleep(Duration::from_secs(3)) => {}
    }
    Ok(WorkerResult { output: if fail { "DEMO: worker failed deliberately; no real model or file changes." } else { "DEMO: simulated implementation and successful checks. Observation: validate empty input as well as the happy path. No actual files were changed or checks performed by a model." }.into(), exit_code: Some(if fail { 1 } else { 0 }), interruption: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shipped_worker_budgets_and_grok_permissions_are_role_specific() {
        let config = Config::read(std::path::Path::new("firm.toml")).unwrap();
        assert_eq!(config.allowances.worker_max_turns, 64);
        assert_eq!(config.allowances.worker_max_tool_calls, 64);
        for provider in &config.providers {
            assert_eq!(provider.max_runs, 6);
            let prepared = prepare_prompt(&config, provider, "Fixture only".into()).unwrap();
            let turn_flag = match provider.id.as_str() {
                "grok" => "--max-turns",
                "qwen" => "--max-session-turns",
                "muse" => "--max-model-steps",
                _ => continue,
            };
            assert!(
                prepared
                    .args
                    .windows(2)
                    .any(|a| a[0] == turn_flag && a[1] == "64")
            );
            if provider.id == "qwen" {
                assert!(
                    prepared
                        .args
                        .windows(2)
                        .any(|a| a[0] == "--max-tool-calls" && a[1] == "64")
                );
                assert!(
                    prepared
                        .args
                        .windows(2)
                        .any(|a| a[0] == "--approval-mode" && a[1] == "yolo")
                );
                for args in [&provider.manager_args, &provider.meeting_args] {
                    let args = args.as_ref().unwrap();
                    assert!(
                        args.windows(2)
                            .any(|a| a[0] == "--approval-mode" && a[1] == "plan")
                    );
                    assert!(!args.iter().any(|a| a == "yolo"));
                }
            }
            if provider.id == "grok" {
                assert!(
                    prepared
                        .args
                        .windows(2)
                        .any(|a| a[0] == "--permission-mode" && a[1] == "bypassPermissions")
                );
                assert!(
                    prepared
                        .args
                        .windows(2)
                        .any(|a| a[0] == "--deny" && a[1] == "MCPTool(telegram__*)")
                );
                assert!(prepared.args.iter().any(|a| a == "--no-subagents"));
                for args in [&provider.manager_args, &provider.meeting_args] {
                    let args = args.as_ref().unwrap();
                    assert!(
                        args.windows(2)
                            .any(|a| a[0] == "--permission-mode" && a[1] == "plan")
                    );
                    assert!(!args.iter().any(|a| a == "bypassPermissions"));
                }
            }
            if provider.id == "codex" {
                assert!(prepared.args.iter().any(|a| a == "exec"));
                assert!(prepared.args.iter().any(|a| a == "gpt-6-astra"));
                assert!(prepared.args.iter().any(|a| a == "workspace-write"));
                assert!(prepared.args.iter().any(|a| a == "--approve-for-me"));
            }
        }
    }
    #[tokio::test]
    async fn all_providers_and_custom_cli_receive_prompts_without_shell_expansion() {
        use std::{os::unix::fs::PermissionsExt, path::Path};
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-cli");
        std::fs::write(&script, include_str!("../tests/fixtures/cli-worker.sh")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = Config::read(Path::new("firm.toml")).unwrap();
        config.workspace = dir.path().into();
        let mut custom = config.providers[0].clone();
        custom.id = "custom".into();
        custom.input = PromptInput::Stdin;
        custom.args = vec!["--headless".into(), "$(touch SHOULD_NOT_EXIST)".into()];
        config.providers.push(custom);
        for provider in &config.providers {
            let mut provider = provider.clone();
            provider.command = script.display().to_string();
            let assignment = Assignment {
                provider: provider.id.clone(),
                title: "Fixture".into(),
                brief: "Literal $(touch SHOULD_NOT_EXIST) `touch ALSO_NOT`".into(),
                acceptance: vec!["Keep discoveries".into()],
                autonomy: "bounded".into(),
            };
            let prepared = prepare(&config, &provider, &assignment).unwrap();
            let file = prepared.prompt_file.as_ref().map(|f| f.path().to_owned());
            if let Some(path) = &file {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            let request = prepared.request(&config, false);
            assert_eq!(request["provider"], provider.id);
            assert!(!prepared.args.iter().any(|a| a.contains("{max_turns}")
                || a.contains("{prompt_file}")
                || a.contains("{workspace}")));
            let (_tx, rx) = watch::channel(0);
            let result = run(&config, &prepared, rx).await.unwrap();
            assert_eq!(
                result.exit_code,
                Some(0),
                "{}: {}",
                provider.id,
                result.output
            );
            assert!(result.output.contains(&assignment.brief));
            assert!(result.output.contains("Keep discoveries"));
            assert!(!dir.path().join("SHOULD_NOT_EXIST").exists());
            assert!(!dir.path().join("ALSO_NOT").exists());
            drop(prepared);
            if let Some(path) = file {
                assert!(!path.exists());
            }
            provider.enabled = false;
            assert!(prepare(&config, &provider, &assignment).is_err());
        }
    }
    #[tokio::test]
    async fn bounded_output_keeps_draining() {
        let data = vec![b'x'; OUTPUT_LIMIT * 3];
        let out = read_bounded(&data[..]).await.unwrap();
        assert!(out.len() < OUTPUT_LIMIT + 100);
        assert!(out.contains("retention limit"));
    }
    #[tokio::test]
    async fn cancellation_stops_demo() {
        let (tx, rx) = watch::channel(0);
        tx.send(1).unwrap();
        assert!(demo(rx, false).await.is_err());
    }
    #[tokio::test]
    async fn timeout_retains_output_and_kills_descendants() {
        use std::{os::unix::fs::PermissionsExt, path::Path};
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-qwen");
        std::fs::write(&script, include_str!("../tests/fixtures/slow-worker.sh")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = Config::read(Path::new("firm.toml")).unwrap();
        config.workspace = dir.path().to_owned();
        let provider_index = config
            .providers
            .iter()
            .position(|p| p.id == "qwen")
            .unwrap();
        config.providers[provider_index].command = script.display().to_string();
        config.allowances.worker_timeout_seconds = 1;
        let (_tx, rx) = watch::channel(0);
        let assignment = Assignment {
            provider: "qwen".into(),
            title: "Test".into(),
            brief: "Test".into(),
            acceptance: vec!["Test".into()],
            autonomy: "bounded".into(),
        };
        let prepared = prepare(&config, &config.providers[provider_index], &assignment).unwrap();
        let result = run(&config, &prepared, rx).await.unwrap();
        assert!(result.output.contains("worker started"));
        assert!(result.interruption.unwrap().contains("time limit"));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!dir.path().join("child-survived").exists());
    }
}
