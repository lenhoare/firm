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
/// Structured event streams are voluminous, and the interesting part is the end. Keep the
/// opening for context and as much of the tail as the budget allows.
const HEAD_LIMIT: usize = 8 * 1024;
const TAIL_LIMIT: usize = OUTPUT_LIMIT - HEAD_LIMIT;

pub struct WorkerResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub interruption: Option<String>,
}

/// What a running worker is doing right now, updated as its output arrives. This is the
/// "look in on it" signal: an agent that has stopped emitting events has either finished
/// without exiting or is stuck, and either way should not hold a slot for the full timeout.
#[derive(Debug)]
pub struct ActivityState {
    pub lines: u64,
    pub bytes: u64,
    pub label: String,
    last: std::time::Instant,
}

impl ActivityState {
    fn new() -> Self {
        Self {
            lines: 0,
            bytes: 0,
            label: "starting".into(),
            last: std::time::Instant::now(),
        }
    }
    pub fn idle_for(&self) -> Duration {
        self.last.elapsed()
    }
    pub fn summary(&self) -> String {
        format!(
            "{} events · quiet {}s · {}",
            self.lines,
            self.idle_for().as_secs(),
            self.label
        )
    }
}

pub type Activity = std::sync::Arc<std::sync::Mutex<ActivityState>>;

pub fn activity() -> Activity {
    std::sync::Arc::new(std::sync::Mutex::new(ActivityState::new()))
}

/// A short label for one line of agent output. Both shipped event formats are JSON per
/// line but disagree on the discriminator, so try each and fall back to raw text — the
/// point is a human-readable hint, not a parsed model.
fn label_for(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        for key in ["payload_type", "type", "phase", "kind"] {
            if let Some(label) = value.get(key).and_then(serde_json::Value::as_str) {
                return Some(label.to_string());
            }
        }
    }
    Some(trimmed.chars().take(60).collect())
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

/// Read a worker's output line by line, updating `activity` as it arrives, and retain a
/// bounded head and tail. Reading incrementally is what makes live progress and idle
/// detection possible; the previous reader only surfaced anything once the stream closed.
async fn read_streaming(reader: impl AsyncRead + Unpin, activity: Activity) -> Result<String> {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(reader).lines();
    let mut head = String::new();
    let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut tail_bytes = 0usize;
    let mut total = 0u64;
    let mut dropped = 0u64;
    while let Some(line) = lines.next_line().await? {
        total += line.len() as u64 + 1;
        if let Ok(mut state) = activity.lock() {
            state.lines += 1;
            state.bytes = total;
            state.last = std::time::Instant::now();
            if let Some(label) = label_for(&line) {
                state.label = label;
            }
        }
        if head.len() < HEAD_LIMIT {
            head.push_str(&line);
            head.push('\n');
            continue;
        }
        tail_bytes += line.len() + 1;
        tail.push_back(line);
        while tail_bytes > TAIL_LIMIT {
            match tail.pop_front() {
                Some(old) => {
                    tail_bytes -= old.len() + 1;
                    dropped += 1;
                }
                None => break,
            }
        }
    }
    if tail.is_empty() {
        return Ok(head);
    }
    Ok(format!(
        "{head}\n[{dropped} intermediate lines dropped; tail follows]\n{}",
        tail.into_iter().collect::<Vec<_>>().join("\n")
    ))
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
    /// Directory the worker runs in. v0 always uses the configured workspace; v1 gives
    /// each attempt its own git worktree so parallel workers cannot collide.
    workspace: std::path::PathBuf,
    // Keep the private prompt file alive until the worker and its descendants exit.
    prompt_file: Option<tempfile::NamedTempFile>,
}

impl Prepared {
    pub fn request(&self, config: &Config, demo: bool) -> serde_json::Value {
        serde_json::json!({"demo":demo,"provider":self.provider,"program":self.program,"args":self.args,
            "stdin":if self.prompt_file.is_none() { Some(&self.prompt) } else { None },
            "prompt_file":self.prompt_file.as_ref().map(|f| serde_json::json!({"path":f.path(),"content":self.prompt,"temporary":true})),
            "cwd":self.workspace,"verification":config.verify_command,
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
    prepare_prompt_in(config, provider, prompt, config.workspace.clone())
}

/// Same as `prepare_prompt`, but runs the worker in `workspace` — both as its working
/// directory and as the `{workspace}` argument substitution, so a CLI told where to work
/// by flag (Muse, Codex) agrees with its own cwd.
pub fn prepare_prompt_in(
    config: &Config,
    provider: &Provider,
    prompt: String,
    workspace: std::path::PathBuf,
) -> Result<Prepared> {
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
            .replace("{workspace}", &workspace.to_string_lossy())
            .replace("{prompt_file}", &file_path)
        })
        .collect();
    Ok(Prepared {
        provider: provider.id.clone(),
        program: provider.command.clone(),
        args,
        prompt,
        workspace,
        prompt_file,
    })
}

pub async fn run(
    config: &Config,
    prepared: &Prepared,
    cancel: watch::Receiver<u64>,
) -> Result<WorkerResult> {
    run_watched(config, prepared, cancel, activity()).await
}

/// As `run`, but the caller keeps a handle on the worker's live activity so it can be
/// shown while the run is in progress.
pub async fn run_watched(
    config: &Config,
    prepared: &Prepared,
    mut cancel: watch::Receiver<u64>,
    activity: Activity,
) -> Result<WorkerResult> {
    anyhow::ensure!(!cancel.has_changed()?, "Cancelled before worker spawn");
    let mut command = Command::new(&prepared.program);
    command
        .args(&prepared.args)
        .current_dir(&prepared.workspace)
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
    let stdout = tokio::spawn(read_streaming(
        child.stdout.take().unwrap(),
        activity.clone(),
    ));
    let stderr = tokio::spawn(read_bounded(child.stderr.take().unwrap()));
    if let Some(mut stdin) = child.stdin.take() {
        tokio::select! {
            result = timeout(Duration::from_secs(5), stdin.write_all(prepared.prompt.as_bytes())) => result??,
            _ = cancel.changed() => bail!("Worker cancelled while sending prompt"),
        }
    }
    let deadline = tokio::time::sleep(Duration::from_secs(
        config.allowances.worker_timeout_seconds,
    ));
    tokio::pin!(deadline);
    let idle_limit = config.allowances.idle_timeout_seconds;
    let mut poll = tokio::time::interval(Duration::from_secs(5));
    poll.tick().await; // The first tick completes immediately.
    let outcome = loop {
        tokio::select! {
            result = child.wait() => break result.map_err(anyhow::Error::from),
            _ = cancel.changed() => break Err(anyhow::anyhow!("Worker cancelled by controller")),
            _ = &mut deadline => break Err(anyhow::anyhow!("Worker reached its time limit")),
            _ = poll.tick() => {
                // An agent that has gone quiet has finished without exiting, or is stuck.
                // Either way its work is already on disk and the slot should be released.
                if idle_limit > 0
                    && let Ok(state) = activity.lock()
                    && state.lines > 0
                    && state.idle_for() >= Duration::from_secs(idle_limit)
                {
                    break Err(anyhow::anyhow!(
                        "Worker went quiet for {idle_limit}s after {} events; treated as finished",
                        state.lines
                    ));
                }
            }
        }
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

pub async fn verify(config: &Config, cancel: watch::Receiver<u64>) -> Result<WorkerResult> {
    let result = run_command_in(&config.verify_command, &config.workspace, 60, cancel).await?;
    Ok(WorkerResult {
        output: format!(
            "Controller verification {:?}\nExit: {:?}\n{}",
            config.verify_command, result.exit_code, result.output
        ),
        ..result
    })
}

/// Run a controller-owned command in `cwd` with bounded output and process-group cleanup.
/// Used for v0 verification and for the v1 command scorer, which must run inside an
/// attempt's own worktree and is never executed by the agent itself.
pub async fn run_command_in(
    command: &[String],
    cwd: &std::path::Path,
    timeout_seconds: u64,
    mut cancel: watch::Receiver<u64>,
) -> Result<WorkerResult> {
    anyhow::ensure!(!cancel.has_changed()?, "Cancelled before command spawn");
    let (program, args) = command.split_first().context("Empty command")?;
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()
        .with_context(|| format!("Could not start {program}"))?;
    let group = ProcessGroup(child.id().context("Command has no PID")?);
    let stdout = tokio::spawn(read_bounded(child.stdout.take().unwrap()));
    let stderr = tokio::spawn(read_bounded(child.stderr.take().unwrap()));
    let outcome = tokio::select! {
        result = child.wait() => result.map_err(anyhow::Error::from),
        _ = cancel.changed() => Err(anyhow::anyhow!("Command cancelled")),
        _ = tokio::time::sleep(Duration::from_secs(timeout_seconds)) => Err(anyhow::anyhow!("Command timed out after {timeout_seconds}s")),
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
        output: format!("{out}\n{err}"),
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
    async fn an_agent_that_finishes_but_never_exits_is_released_early() {
        // Exactly the muse failure: emit some events, do the work, then hang forever.
        // Without the idle check this holds a slot for the whole worker timeout.
        use std::{os::unix::fs::PermissionsExt, path::Path};
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hangs-after-working");
        std::fs::write(
            &script,
            "#!/bin/sh\necho '{\"payload_type\":\"run.output.delta\"}'\necho '{\"payload_type\":\"run.done\"}'\nsleep 300\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = Config::read(Path::new("firm.toml")).unwrap();
        config.workspace = dir.path().to_owned();
        config.allowances.worker_timeout_seconds = 300;
        config.allowances.idle_timeout_seconds = 6;
        let mut provider = config.providers[0].clone();
        provider.command = script.display().to_string();
        provider.args = vec![];
        provider.input = PromptInput::Stdin;

        let prepared = prepare_prompt(&config, &provider, "go".into()).unwrap();
        let (_tx, rx) = watch::channel(0);
        let started = std::time::Instant::now();
        let result = run(&config, &prepared, rx).await.unwrap();

        assert!(started.elapsed() < Duration::from_secs(60), "released early");
        let reason = result.interruption.expect("recorded as an interruption");
        assert!(reason.contains("went quiet"), "{reason}");
        assert!(reason.contains("2 events"), "reports how much it emitted: {reason}");
        // The work it did emit is still retained for scoring.
        assert!(result.output.contains("run.output.delta"));
    }

    #[tokio::test]
    async fn a_steadily_working_agent_is_not_cut_off_as_idle() {
        use std::{os::unix::fs::PermissionsExt, path::Path};
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("keeps-talking");
        std::fs::write(
            &script,
            "#!/bin/sh\nfor i in 1 2 3 4 5 6 7 8; do echo \"{\\\"payload_type\\\":\\\"step\\\"}\"; sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = Config::read(Path::new("firm.toml")).unwrap();
        config.workspace = dir.path().to_owned();
        config.allowances.idle_timeout_seconds = 5;
        let mut provider = config.providers[0].clone();
        provider.command = script.display().to_string();
        provider.args = vec![];
        provider.input = PromptInput::Stdin;
        let prepared = prepare_prompt(&config, &provider, "go".into()).unwrap();
        let (_tx, rx) = watch::channel(0);
        let result = run(&config, &prepared, rx).await.unwrap();
        assert_eq!(result.exit_code, Some(0), "ran to completion");
        assert!(result.interruption.is_none(), "steady output is not idleness");
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
