//! Deciding what success means, before deciding how to build it.
//!
//! Every other check in Firm asks whether a task was carried out. This asks the different
//! question: does the finished thing actually do what it was for. The handwriting trial is
//! the argument for separating them — a slant metric passed six tests and returned zero on
//! 92% of real images, because every check tested how it responded to a transform and none
//! tested what it said about the corpus.
//!
//! Two properties make it worth having. It runs **first**, so its criteria are written by
//! something that has never seen the decomposition and cannot fit them to it. And what it
//! produces is **binding**: the planner is then obliged to build a thing that satisfies
//! criteria it did not choose.
//!
//! It may read the documents the brief refers to — it asked to, on its first live call, and
//! it was right to. What it must not read is an implementation that already exists, which
//! would turn its criteria into a description of current behaviour.

use crate::{config::Config, worker};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// An executable question about the finished project, run against the integrated result.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Probe {
    pub id: String,
    /// What this establishes, in one sentence — the part a person reads.
    pub description: String,
    /// argv, run in the integration worktree. Exit 0 passes.
    pub command: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Validation {
    /// Things that can be run and can fail.
    #[serde(default)]
    pub probes: Vec<Probe>,
    /// Things that cannot be automated but that a person should weigh at the end. Recorded
    /// deliberately: half of validation is a judgement nobody should pretend to automate.
    #[serde(default)]
    pub criteria: Vec<String>,
}

impl Validation {
    pub fn is_empty(&self) -> bool {
        self.probes.is_empty() && self.criteria.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProbeResult {
    pub id: String,
    pub description: String,
    pub command: String,
    pub passed: bool,
    pub detail: String,
}

/// What the validation planner is asked, and the schema its wording assumes — both in
/// `prompts/validator.md`, so neither can be changed without the other in view.
fn prompt(brief: &str) -> Result<String> {
    crate::prompt::get("validator").render(&[("brief", brief)])
}

fn template() -> crate::prompt::Prompt {
    crate::prompt::get("validator")
}

/// Ask the validation planner what success means. One call per run, before anything else.
pub async fn plan(config: &Config, brief: &str, cancel: watch::Receiver<u64>) -> Result<(Validation, String)> {
    // The validator defaults to the planner's provider: what matters is that it runs on a
    // different context, not that it is a different model. A separate `validator` is there
    // for when a second opinion is worth paying for.
    let wanted = match config.validator.trim().is_empty() {
        true => config.planner.as_str(),
        false => config.validator.trim(),
    };
    let chosen = config
        .providers
        .iter()
        .find(|p| p.id == wanted && p.enabled)
        .cloned()
        .with_context(|| format!("Validation planner {wanted} is not configured or not enabled"))?;

    let mut provider = chosen.clone();
    // It answers from the brief alone. Given planning arguments it would explore a
    // workspace that has nothing in it yet and spend every turn doing so.
    // Deliberately not the reviewer's or observer's arguments: both pin a JSON schema for
    // their own job, and a CLI held to the wrong schema returns nothing rather than
    // improvising. Learned by watching grok answer this prompt with `structuredOutput:
    // null` and "model did not produce structured output".
    let Some(args) = provider
        .validator_args
        .clone()
        .or_else(|| provider.manager_args.clone())
    else {
        bail!("The validation planner needs validator_args or manager_args");
    };
    provider.args = args;

    let mut config = config.clone();
    config.verify_command.clear();
    config.allowances.idle_timeout_seconds = 0;

    let prepared = worker::prepare_role(
        &config,
        &provider,
        &template(),
        prompt(brief)?,
        config.workspace.clone(),
    )?;
    let result = worker::run(&config, &prepared, cancel).await?;
    if let Some(reason) = result.interruption {
        bail!("The validation planner was interrupted: {reason}");
    }
    let validation = parse(&result.output)
        .with_context(|| format!("The validation planner did not return a usable model.\n{}", tail(&result.output)))?;
    Ok((validation, result.output))
}

/// Pull a validation model out of a reply that may be wrapped in prose or a CLI envelope.
pub fn parse(reply: &str) -> Result<Validation> {
    if let Ok(validation) = serde_json::from_str::<Validation>(reply.trim())
        && !validation.is_empty()
    {
        return Ok(validation);
    }
    for candidate in super::plan::objects(reply) {
        if let Ok(validation) = serde_json::from_str::<Validation>(candidate)
            && !validation.is_empty()
        {
            return Ok(validation);
        }
        // Nested under a CLI's own result envelope.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate)
            && let Some(found) = find(&value)
        {
            return Ok(found);
        }
    }
    bail!("No validation model found in the reply")
}

fn find(value: &serde_json::Value) -> Option<Validation> {
    if let Some(object) = value.as_object() {
        if (object.contains_key("probes") || object.contains_key("criteria"))
            && let Ok(validation) = serde_json::from_value::<Validation>(value.clone())
            && !validation.is_empty()
        {
            return Some(validation);
        }
        for nested in object.values() {
            if let Some(found) = find(nested) {
                return Some(found);
            }
        }
    }
    None
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().rev().take(20).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// Run every probe against a tree, in order. Never cancels a run: this is a report.
pub async fn run(
    validation: &Validation,
    path: &std::path::Path,
    timeout_seconds: u64,
    cancel: &watch::Receiver<u64>,
) -> Vec<ProbeResult> {
    let mut results = Vec::new();
    for probe in &validation.probes {
        if probe.command.is_empty() {
            continue;
        }
        let outcome =
            worker::run_command_in(&probe.command, path, timeout_seconds, cancel.clone()).await;
        let (passed, detail) = match outcome {
            Ok(result) => (
                result.exit_code == Some(0) && result.interruption.is_none(),
                clip(&result.output, 4 * 1024),
            ),
            Err(error) => (false, error.to_string()),
        };
        results.push(ProbeResult {
            id: probe.id.clone(),
            description: probe.description.clone(),
            command: probe.command.join(" "),
            passed,
            detail,
        });
    }
    results
}

fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[probe output truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_is_read_from_json_however_the_cli_wrapped_it() {
        let plain = parse(
            r#"{"probes":[{"id":"spread","description":"not degenerate","command":["sh","-c","x"]}],"criteria":["read the output"]}"#,
        )
        .unwrap();
        assert_eq!(plain.probes.len(), 1);
        assert_eq!(plain.criteria.len(), 1);
        assert_eq!(plain.probes[0].id, "spread");

        let nested = parse(
            r#"{"type":"result","structuredOutput":{"probes":[],"criteria":["look at the report"]}}"#,
        )
        .unwrap();
        assert_eq!(nested.criteria, vec!["look at the report".to_string()]);

        // Text printed after the answer, as codex appends a token count.
        let trailing = parse("{\"probes\":[],\"criteria\":[\"a\"]}\ntokens used: 4211").unwrap();
        assert_eq!(trailing.criteria.len(), 1);
    }

    #[test]
    fn an_empty_or_unreadable_model_is_an_error_not_a_silent_pass() {
        // Silently accepting nothing would mean a run with no validation at all, reported
        // as though it had been validated.
        assert!(parse("{\"probes\":[],\"criteria\":[]}").is_err());
        assert!(parse("I could not do that.").is_err());
        assert!(parse("").is_err());
    }

    #[tokio::test]
    async fn probes_run_against_the_tree_and_report_both_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("present"), "x").unwrap();
        let validation = Validation {
            probes: vec![
                Probe {
                    id: "there".into(),
                    description: "the file exists".into(),
                    command: vec!["test".into(), "-f".into(), "present".into()],
                },
                Probe {
                    id: "missing".into(),
                    description: "the other file exists".into(),
                    command: vec!["test".into(), "-f".into(), "absent".into()],
                },
            ],
            criteria: vec!["someone should look at it".into()],
        };
        let (_tx, rx) = watch::channel(0);
        let results = run(&validation, dir.path(), 30, &rx).await;
        assert_eq!(results.len(), 2);
        assert!(results[0].passed, "runs in the given tree");
        assert!(!results[1].passed);
        assert_eq!(results[1].description, "the other file exists");
    }

    #[test]
    fn the_prompt_withholds_the_plan_and_asks_the_validation_question() {
        let text = crate::prompt::flatten(&prompt("Build a thing").unwrap());
        assert!(text.contains("Build a thing"));
        assert!(text.contains("you will not see the plan"));
        assert!(text.contains("what would make this thing useless"));
        // The constraint that keeps probes writable before the code exists.
        assert!(text.contains("interfaces the brief itself names"));
        // And the one that stops it emitting probes that pass on an empty repo.
        assert!(text.contains("able to fail"));
        // A live call returned the single probe "read DATA.md and README.md" with the
        // command `true` — it was asking for the documents the brief names. It may now
        // read them, but a step it wants to take is still not a probe.
        assert!(text.contains("may read what the brief refers to"));
        assert!(text.contains("not a probe"));
        // The one thing it must not look at, or its criteria describe what exists rather
        // than what the work is for.
        assert!(text.contains("do not read any existing"));
    }
}
