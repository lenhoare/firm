//! Account quota display, deliberately separate from Firm's local run allowances.
use crate::{
    app::App,
    state::{Usage, now},
};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

const CLI_REFRESH_SECONDS: u64 = 600;
const CLI_MAX_AGE_SECONDS: u64 = 900;
const QWEN_WEEKLY_TOKENS: f64 = 40_000.0;

impl App {
    pub async fn account_usage(&self) -> Value {
        let core = self.core.lock().await;
        let time = now();
        let mut agents = Vec::new();
        for provider in core.state.providers(&self.config.providers) {
            let mut reading = if provider.id == "codex" {
                codex_usage(
                    core.state.usage.as_ref(),
                    time,
                    core.state.allowances.usage_max_age_seconds,
                )
            } else if provider.id == "grok" {
                grok_usage(
                    core.state.provider_account_usage.get("grok"),
                    time,
                    CLI_MAX_AGE_SECONDS,
                )
            } else if provider.id == "qwen" {
                qwen_usage(
                    core.state.provider_account_usage.get("qwen"),
                    time,
                    CLI_MAX_AGE_SECONDS,
                )
            } else if provider.id == "muse" {
                muse_usage(
                    core.state.provider_account_usage.get("muse"),
                    time,
                    CLI_MAX_AGE_SECONDS,
                )
            } else {
                json!({"status":"unavailable","windows":[],"reason":"No account-usage adapter configured for this provider.","fetched_at":null})
            };
            reading["id"] = json!(provider.id);
            reading["name"] = json!(provider.name);
            reading["enabled"] = json!(provider.enabled);
            agents.push(reading);
        }
        json!({"agents":agents,"now":time,"demo":core.state.demo})
    }

    /// Poll independently of model turns. Interactive usage commands are opened
    /// through short-lived PTYs only while the controller and meetings are idle.
    pub async fn monitor_usage(self) {
        let mut next_cli = std::collections::BTreeMap::<String, u64>::new();
        loop {
            self.refresh_usage().await;
            let time = now();
            let idle = {
                let core = self.core.lock().await;
                !core.active && core.meeting_active.is_none()
            };
            let probes: Vec<_> = self
                .config
                .providers
                .iter()
                .filter(|provider| ["grok", "qwen", "muse"].contains(&provider.id.as_str()))
                .map(|provider| (provider.id.clone(), provider.command.clone()))
                .collect();
            for (id, command) in probes {
                if !idle || time < next_cli.get(&id).copied().unwrap_or(0) {
                    continue;
                }
                let result = match id.as_str() {
                    "grok" => probe_grok_usage(&command, &self.config.workspace).await,
                    "qwen" => probe_qwen_usage(&command, &self.config.workspace).await,
                    "muse" => probe_muse_usage(&command, &self.config.workspace).await,
                    _ => unreachable!(),
                };
                match result {
                    Ok(raw) => {
                        let mut core = self.core.lock().await;
                        core.state.provider_account_usage.insert(
                            id.clone(),
                            Usage {
                                fetched_at: now(),
                                raw,
                            },
                        );
                        let _ = core.save();
                    }
                    Err(error) => eprintln!("{id} usage unavailable: {error}"),
                }
                next_cli.insert(id, now().saturating_add(CLI_REFRESH_SECONDS));
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}

fn grok_usage(usage: Option<&Usage>, time: u64, max_age: u64) -> Value {
    let Some(usage) = usage else {
        return json!({"status":"unavailable","windows":[],"fetched_at":null,
            "reason":"Waiting for Grok /usage weekly limit."});
    };
    let stale = usage.fetched_at > time || time.saturating_sub(usage.fetched_at) > max_age;
    let Some(percent) = usage.raw["used_percent"]
        .as_f64()
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
    else {
        return json!({"status":"unavailable","windows":[],"fetched_at":usage.fetched_at,
            "reason":"Grok /usage did not return a valid weekly percentage."});
    };
    json!({
        "status": if stale { "stale" } else { "ok" },
        "fetched_at": usage.fetched_at,
        "reason": if stale { "Last Grok /usage reading is stale." } else { "Read from Grok's local /usage weekly limit." },
        "windows": [{
            "bucket": "grok-weekly",
            "label": usage.raw["label"],
            "used_percent": percent,
            "reset_label": usage.raw["reset_label"],
            "resets_at": null,
            "stale": stale
        }]
    })
}

fn qwen_usage(usage: Option<&Usage>, time: u64, max_age: u64) -> Value {
    let Some(usage) = usage else {
        return json!({"status":"unavailable","windows":[],"fetched_at":null,
            "reason":"Waiting for Qwen /usage token totals."});
    };
    let stale = usage.fetched_at > time || time.saturating_sub(usage.fetched_at) > max_age;
    let Some(total) = usage.raw["total_tokens"].as_u64() else {
        return json!({"status":"unavailable","windows":[],"fetched_at":usage.fetched_at,
            "reason":"Qwen /usage did not return valid prompt and output token totals."});
    };
    let percent = total as f64 / QWEN_WEEKLY_TOKENS * 100.0;
    json!({
        "status": if stale { "stale" } else { "ok" },
        "fetched_at": usage.fetched_at,
        "reason": if stale { "Last Qwen /usage reading is stale." } else { "Prompt plus output tokens divided by the 40,000-token weekly allowance." },
        "windows": [{
            "bucket":"qwen-weekly",
            "label":"Weekly tokens · 40,000",
            "used_percent":percent,
            "reset_label":null,
            "resets_at":null,
            "stale":stale
        }]
    })
}

fn muse_usage(usage: Option<&Usage>, time: u64, max_age: u64) -> Value {
    let Some(usage) = usage else {
        return json!({"status":"unavailable","windows":[],"raw_metrics":[],"fetched_at":null,
            "reason":"Waiting for Muse /usage total tokens."});
    };
    let stale = usage.fetched_at > time || time.saturating_sub(usage.fetched_at) > max_age;
    let Some(total) = usage.raw["total_tokens"].as_u64() else {
        return json!({"status":"unavailable","windows":[],"raw_metrics":[],"fetched_at":usage.fetched_at,
            "reason":"Muse /usage did not return a valid total-token count."});
    };
    json!({
        "status": if stale { "stale" } else { "ok" },
        "fetched_at":usage.fetched_at,
        "reason":if stale { "Last Muse /usage reading is stale." } else { "Raw total reported by Muse /usage; no percentage inferred." },
        "windows":[],
        "raw_metrics":[{"label":"Total tokens","value":total,"unit":"tokens","stale":stale}]
    })
}

pub(crate) async fn probe_grok_usage(command: &str, workspace: &Path) -> anyhow::Result<Value> {
    let shell_command = format!(
        "stty rows 24 cols 80; exec {} --continue --no-alt-screen",
        shell_quote(command)
    );
    let mut child = Command::new("script")
        .args(["-qfec", &shell_command, "/dev/null"])
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("Missing PTY input"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Missing PTY output"))?;
    let reader = tokio::spawn(async move {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await.map(|_| output)
    });

    tokio::time::sleep(Duration::from_secs(1)).await;
    // Grok asks the terminal for the cursor position before rendering. `script`
    // supplies the PTY; this response supplies its small, fixed virtual screen.
    stdin.write_all(b"\x1b[24;1R").await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    stdin.write_all(b"/usage show\r").await?;
    stdin.flush().await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    stdin.write_all(b"\x1b").await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    stdin.write_all(b"/quit\r").await?;
    stdin.shutdown().await?;

    if tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        child.start_kill()?;
        let _ = child.wait().await;
    }
    let output = tokio::time::timeout(Duration::from_secs(2), reader).await???;
    parse_grok_usage(&String::from_utf8_lossy(&output))
}

pub(crate) async fn probe_qwen_usage(command: &str, workspace: &Path) -> anyhow::Result<Value> {
    let mut child = Command::new(command);
    child
        .arg("/usage")
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), child.output()).await??;
    anyhow::ensure!(
        output.status.success(),
        "Qwen /usage exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_qwen_usage(&text)
}

pub(crate) async fn probe_muse_usage(command: &str, workspace: &Path) -> anyhow::Result<Value> {
    let shell_command = format!("stty rows 24 cols 80; exec {}", shell_quote(command));
    let mut child = Command::new("script")
        .args(["-qfec", &shell_command, "/dev/null"])
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("Missing Muse PTY input"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Missing Muse PTY output"))?;
    let reader = tokio::spawn(async move {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await.map(|_| output)
    });

    tokio::time::sleep(Duration::from_secs(1)).await;
    stdin.write_all(b"\x1b[24;1R").await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    stdin.write_all(b"/usage\r").await?;
    stdin.flush().await?;
    // A second Enter is harmless after execution and handles Muse receiving the
    // first one while its initial screen is still settling.
    tokio::time::sleep(Duration::from_secs(1)).await;
    stdin.write_all(b"\r").await?;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let _ = stdin.shutdown().await;
    if child.try_wait()?.is_none() {
        child.start_kill()?;
        let _ = child.wait().await;
    }
    let output = tokio::time::timeout(Duration::from_secs(2), reader).await???;
    parse_muse_usage(&String::from_utf8_lossy(&output))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn parse_grok_usage(output: &str) -> anyhow::Result<Value> {
    let limit = regex::Regex::new(r"(?s)Weekly limit \(([^)]+)\).*?([0-9]{1,3}(?:\.[0-9]+)?)%")?
        .captures(output)
        .ok_or_else(|| anyhow::anyhow!("Grok weekly limit was not found"))?;
    let reset = regex::Regex::new(r"Resets:\s*([A-Za-z]+\s+[0-9]{1,2},\s+[0-9]{2}:[0-9]{2})")?
        .captures(output)
        .and_then(|capture| capture.get(1))
        .map(|value| value.as_str().to_string());
    Ok(json!({
        "label": format!("Weekly limit ({})", &limit[1]),
        "used_percent": limit[2].parse::<f64>()?,
        "reset_label": reset
    }))
}

fn parse_qwen_usage(output: &str) -> anyhow::Result<Value> {
    let tokens = regex::Regex::new(
        r"Tokens[^\r\n]*?prompt:\s*([0-9][0-9,]*)\s*,\s*output:\s*([0-9][0-9,]*)",
    )?
    .captures(output)
    .ok_or_else(|| anyhow::anyhow!("Qwen token totals were not found"))?;
    let prompt = tokens[1].replace(',', "").parse::<u64>()?;
    let output = tokens[2].replace(',', "").parse::<u64>()?;
    Ok(json!({
        "prompt_tokens":prompt,
        "output_tokens":output,
        "total_tokens":prompt.checked_add(output).ok_or_else(|| anyhow::anyhow!("Qwen token total overflow"))?
    }))
}

fn parse_muse_usage(output: &str) -> anyhow::Result<Value> {
    // Muse may position the number with a CSI cursor escape rather than spaces.
    let total =
        regex::Regex::new(r"(?m)\bTotal(?:(?:\s)|(?:\x1b\[[0-9;?]*[ -/]*[@-~]))+([0-9][0-9,]*)\b")?
            .captures_iter(output)
            .last()
            .ok_or_else(|| anyhow::anyhow!("Muse total tokens were not found"))?;
    Ok(json!({"total_tokens":total[1].replace(',', "").parse::<u64>()?}))
}

fn codex_usage(usage: Option<&Usage>, time: u64, max_age: u64) -> Value {
    let mut result = json!({"status":"unavailable","windows":[],"fetched_at":null,
        "reason":"Waiting for Codex app-server usage information."});
    let Some(usage) = usage else {
        return result;
    };
    result["fetched_at"] = json!(usage.fetched_at);
    let stale = usage.fetched_at > time || time.saturating_sub(usage.fetched_at) > max_age;
    let buckets: Vec<(&str, &Value)> = match usage
        .raw
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
    {
        Some(map) if !map.is_empty() => map
            .iter()
            .map(|(id, bucket)| (id.as_str(), bucket))
            .collect(),
        _ => usage
            .raw
            .get("rateLimits")
            .map(|b| vec![(b["limitId"].as_str().unwrap_or("codex"), b)])
            .unwrap_or_default(),
    };
    let mut windows = Vec::new();
    for (id, bucket) in buckets {
        for key in ["primary", "secondary"] {
            let window = &bucket[key];
            if window["windowDurationMins"].as_f64() != Some(300.0) {
                continue;
            }
            let Some(percent) = window["usedPercent"]
                .as_f64()
                .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
            else {
                continue;
            };
            let reset = window["resetsAt"].as_u64().filter(|v| *v > 0);
            let expired = reset.is_some_and(|r| r <= time);
            windows.push(
                json!({"bucket":id,"label":"5-hour limit","used_percent":percent,"resets_at":reset,
                "reset_label":null,
                "stale":stale || expired}),
            );
        }
    }
    result["status"] = json!(if windows.is_empty() {
        "unavailable"
    } else if windows.iter().any(|w| w["stale"] == true) {
        "stale"
    } else {
        "ok"
    });
    result["reason"] = json!(if windows.is_empty() {
        "Codex did not return a valid 5-hour quota window."
    } else if stale {
        "Last reading is stale; waiting for a fresh account check."
    } else {
        "Provider-reported usage, not Firm's local turn allowance."
    });
    result["windows"] = json!(windows);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reading(raw: Value) -> Usage {
        Usage {
            fetched_at: 1000,
            raw,
        }
    }
    #[test]
    fn parses_grok_weekly_limit_from_pty_output() {
        let output = "\x1b[6;18H\x1b[1mWeekly limit (SuperGrok)\x1b[22m\x1b[8;50H7%\x1b[9;18H\x1b[2mResets: September 7, 23:02\x1b[m";
        let parsed = parse_grok_usage(output).unwrap();
        assert_eq!(parsed["label"], "Weekly limit (SuperGrok)");
        assert_eq!(parsed["used_percent"], 7.0);
        assert_eq!(parsed["reset_label"], "September 7, 23:02");
    }
    #[test]
    fn grok_weekly_readings_are_labelled_and_age_out() {
        let usage = reading(json!({
            "label":"Weekly limit (SuperGrok)",
            "used_percent":7,
            "reset_label":"September 7, 23:02"
        }));
        let fresh = grok_usage(Some(&usage), 1010, 90);
        assert_eq!(fresh["status"], "ok");
        assert_eq!(fresh["windows"][0]["label"], "Weekly limit (SuperGrok)");
        assert_eq!(fresh["windows"][0]["used_percent"], 7.0);
        assert_eq!(grok_usage(Some(&usage), 1100, 90)["status"], "stale");
    }
    #[test]
    fn qwen_tokens_become_a_weekly_percentage() {
        let parsed = parse_qwen_usage(
            "Session duration: 1ms\nTokens — prompt: 8,000, output: 2,000\nTool calls: 0",
        )
        .unwrap();
        assert_eq!(parsed["total_tokens"], 10_000);
        let usage = reading(parsed);
        let result = qwen_usage(Some(&usage), 1010, 90);
        assert_eq!(result["status"], "ok");
        assert_eq!(result["windows"][0]["used_percent"], 25.0);
        assert_eq!(qwen_usage(Some(&usage), 1100, 90)["status"], "stale");
    }
    #[test]
    fn muse_usage_retains_raw_total_without_inventing_a_percentage() {
        let parsed =
            parse_muse_usage("\x1b[8;3HInput 1,000\nOutput 234\nTotal\x1b[13;16H1,234\nTurns 4")
                .unwrap();
        let usage = reading(parsed);
        let result = muse_usage(Some(&usage), 1010, 90);
        assert_eq!(result["status"], "ok");
        assert!(result["windows"].as_array().unwrap().is_empty());
        assert_eq!(result["raw_metrics"][0]["value"], 1234);
        assert_eq!(result["raw_metrics"][0]["unit"], "tokens");
    }
    #[tokio::test]
    async fn demo_monitor_reads_account_even_while_busy_without_model_calls() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::{accept_async, tungstenite::Message};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(socket).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => json!({}),
                    "initialized" => continue,
                    "account/rateLimits/read" => {
                        json!({"rateLimits":{"primary":{"usedPercent":37,"windowDurationMins":300}}})
                    }
                    method => panic!("Unexpected call from account monitor: {method}"),
                };
                ws.send(Message::Text(
                    json!({"id":request["id"],"result":result})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
        });
        let (_dir, mut app) = crate::app::tests::fixture();
        app.rpc = Some(
            crate::codex::Codex::connect(&format!("ws://{address}"))
                .await
                .unwrap(),
        );
        app.core.lock().await.active = true;
        let monitor = tokio::spawn(app.clone().monitor_usage());
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if app.core.lock().await.state.usage.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let usage = app.account_usage().await;
        let codex = usage["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["id"] == "codex")
            .unwrap();
        assert_eq!(codex["windows"][0]["used_percent"], 37.0);
        let core = app.core.lock().await;
        assert!(core.state.manager_starts.is_empty());
        assert!(core.state.worker_starts.is_empty());
        assert!(core.state.provider_usage.is_empty());
        assert!(core.state.thread_id.is_none());
        monitor.abort();
        server.abort();
    }
    #[test]
    fn selects_only_five_hour_windows_and_preserves_distinct_buckets() {
        let usage = reading(
            json!({"rateLimits":{"primary":{"usedPercent":99,"windowDurationMins":300}},"rateLimitsByLimitId":{
                "codex":{"primary":{"usedPercent":24.5,"windowDurationMins":300,"resetsAt":19000},"secondary":{"usedPercent":99,"windowDurationMins":10080}},
                "other":{"primary":{"usedPercent":40,"windowDurationMins":300}},
                "short":{"primary":{"usedPercent":80,"windowDurationMins":15}}
            }}),
        );
        let result = codex_usage(Some(&usage), 1010, 90);
        assert_eq!(result["status"], "ok");
        assert_eq!(result["windows"].as_array().unwrap().len(), 2);
        assert_eq!(result["windows"][0]["used_percent"], 24.5);
        assert_eq!(result["windows"][1]["bucket"], "other");
        assert_eq!(codex_usage(Some(&usage), 1100, 90)["status"], "stale");
    }
    #[test]
    fn missing_malformed_expired_and_zero_are_not_confused() {
        assert_eq!(codex_usage(None, 1000, 90)["status"], "unavailable");
        for window in [
            json!({"usedPercent":50}),
            json!({"usedPercent":-1,"windowDurationMins":300}),
            json!({"usedPercent":101,"windowDurationMins":300}),
            json!({"usedPercent":"20","windowDurationMins":300}),
            json!({"usedPercent":20,"windowDurationMins":10080}),
        ] {
            let usage = reading(json!({"rateLimits":{"primary":window}}));
            assert_eq!(codex_usage(Some(&usage), 1000, 90)["status"], "unavailable");
        }
        let usage = reading(
            json!({"rateLimits":{"primary":{"usedPercent":0,"windowDurationMins":300,"resetsAt":2000}}}),
        );
        assert_eq!(
            codex_usage(Some(&usage), 1000, 90)["windows"][0]["used_percent"],
            0.0
        );
        assert_eq!(codex_usage(Some(&usage), 2000, 5000)["status"], "stale");
    }
    #[tokio::test]
    async fn roster_is_extensible_and_reading_does_not_reserve_work() {
        let (_dir, mut app) = crate::app::tests::fixture();
        let mut custom = app.config.providers[0].clone();
        custom.id = "custom".into();
        custom.enabled = false;
        custom.command = "/must-not-run".into();
        app.config.providers.push(custom);
        let before = serde_json::to_value(&app.core.lock().await.state).unwrap();
        let result = app.account_usage().await;
        assert_eq!(result["agents"].as_array().unwrap().len(), 5);
        assert_eq!(result["agents"][4]["enabled"], false);
        assert_eq!(result["agents"][4]["status"], "unavailable");
        assert_eq!(
            serde_json::to_value(&app.core.lock().await.state).unwrap(),
            before
        );
    }
}
