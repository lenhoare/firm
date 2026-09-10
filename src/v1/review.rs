//! Judging work when no check can prove it was done.
//!
//! In trial mode a task carries a check that fails before the work and passes after, so a
//! merge is proof. Ordinary projects have no such thing: their test suite says only that
//! nothing broke, which a stub satisfies as well as a implementation does. Build mode
//! closes that gap with a roster model reading the diff against the task it was meant to
//! do — never the model that wrote it, and never with tools, since the diff is right there
//! in the prompt.
//!
//! This is judgement, not proof, and the ledger keeps it apart from the real thing.

use crate::{config::Provider, worker};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Review {
    pub passed: bool,
    pub reason: String,
}

/// What the reviewer is asked. The diff is supplied rather than fetched: a reviewer with
/// tools spends its turns exploring and never answers, which is what the forum observer
/// taught us the expensive way.
pub fn prompt(
    title: &str,
    brief: &str,
    acceptance: &[String],
    files: &[String],
    diff: &str,
) -> String {
    let acceptance = match acceptance.is_empty() {
        true => "  (none stated)".to_string(),
        false => acceptance
            .iter()
            .map(|a| format!("  - {a}"))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    format!(
        "You are reviewing one coding agent's finished work for a team. Decide the single \
         question: does this diff actually do the task, or only appear to?\n\n\
         Reject work that is pretending: a function that returns a constant to satisfy a \
         caller, a stub, a TODO left where the logic belongs, an empty except that swallows \
         the failure, a test weakened or deleted rather than satisfied, or a change that \
         addresses something other than the task. Reject work that is plainly incomplete \
         against the acceptance criteria.\n\n\
         Do not reject on style, naming, formatting, or how you would have done it, and do \
         not ask for extra work beyond the task. Imperfect code that genuinely does the job \
         passes. You are the last check before this is merged, so an honest pass matters as \
         much as an honest rejection.\n\n\
         TASK: {title}\n{brief}\n\nACCEPTANCE:\n{acceptance}\n\nFILES CHANGED: {}\n\n\
         DIFF:\n{diff}\n\n\
         Everything you need is in this prompt. Do not use tools, do not read files, do not \
         explore — reply immediately with JSON only, no prose and no markdown fence:\n\
         {{\"verdict\": \"pass\" or \"fail\", \"reason\": \"one sentence\"}}",
        files.join(", "),
    )
}

/// Read a verdict out of a reply that may be JSON, JSON wrapped in a CLI's own envelope,
/// or prose with the answer somewhere inside. A reviewer that cannot be understood is not
/// a rejection — the caller treats `None` as "no opinion" rather than as failure.
pub fn parse(reply: &str) -> Option<Review> {
    for candidate in objects(reply) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) else {
            continue;
        };
        if let Some(review) = from_value(&value) {
            return Some(review);
        }
    }
    // Some CLIs answer in prose however firmly they are asked not to. Take a clear verdict
    // word if the reply contains exactly one of them.
    let lower = reply.to_lowercase();
    let says_pass = lower.contains("\"pass\"") || lower.contains("verdict: pass");
    let says_fail = lower.contains("\"fail\"") || lower.contains("verdict: fail");
    match (says_pass, says_fail) {
        (true, false) => Some(Review {
            passed: true,
            reason: "Reviewer passed it in prose".into(),
        }),
        (false, true) => Some(Review {
            passed: false,
            reason: first_line(reply),
        }),
        _ => None,
    }
}

/// A verdict anywhere in a JSON value, however the CLI nested it.
fn from_value(value: &serde_json::Value) -> Option<Review> {
    if let Some(object) = value.as_object() {
        let verdict = object
            .get("verdict")
            .or_else(|| object.get("passed"))
            .or_else(|| object.get("pass"));
        if let Some(verdict) = verdict {
            let passed = match verdict {
                serde_json::Value::Bool(flag) => *flag,
                serde_json::Value::String(text) => {
                    let text = text.trim().to_lowercase();
                    match text.as_str() {
                        "pass" | "passed" | "accept" | "approved" | "true" | "yes" => true,
                        "fail" | "failed" | "reject" | "rejected" | "false" | "no" => false,
                        _ => return None,
                    }
                }
                _ => return None,
            };
            let reason = object
                .get("reason")
                .or_else(|| object.get("detail"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            return Some(Review { passed, reason });
        }
        for nested in object.values() {
            if let Some(review) = from_value(nested) {
                return Some(review);
            }
        }
    }
    None
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Reviewer rejected it")
        .chars()
        .take(300)
        .collect()
}

/// Every balanced `{...}` region, ignoring braces inside strings. Shared shape with the
/// planner's scanner, for the same reason: CLIs print things after the answer.
fn objects(reply: &str) -> Vec<&str> {
    let bytes = reply.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'{' {
            index += 1;
            continue;
        }
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut cursor = index;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            if in_string {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                }
            } else if byte == b'"' {
                in_string = true;
            } else if byte == b'{' {
                depth += 1;
            } else if byte == b'}' {
                depth -= 1;
                if depth == 0 {
                    found.push(&reply[index..=cursor]);
                    break;
                }
            }
            cursor += 1;
        }
        index += 1;
    }
    found
}

/// Ask one provider for a verdict. Returns `None` when the reviewer could not be reached
/// or could not be understood: a broken reviewer must not silently become a rejection.
pub async fn ask(
    config: &crate::config::Config,
    reviewer: &Provider,
    prompt: String,
    cancel: watch::Receiver<u64>,
) -> Result<Option<Review>> {
    let mut provider = reviewer.clone();
    let Some(args) = provider
        .reviewer_args
        .clone()
        .or_else(|| provider.observer_args.clone())
        .or_else(|| provider.manager_args.clone())
    else {
        return Ok(None);
    };
    provider.args = args;

    let mut config = config.clone();
    config.verify_command.clear();
    // A reviewer answers in one go, so silence is thinking rather than a hang.
    config.allowances.idle_timeout_seconds = 0;

    let prepared = worker::prepare_prompt_in(&config, &provider, prompt, config.workspace.clone())?;
    let result = worker::run(&config, &prepared, cancel).await?;
    if result.interruption.is_some() {
        return Ok(None);
    }
    Ok(parse(&result.output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_is_read_from_json_however_the_cli_wrapped_it() {
        let plain = parse(r#"{"verdict":"fail","reason":"returns a constant"}"#).unwrap();
        assert!(!plain.passed);
        assert_eq!(plain.reason, "returns a constant");

        // grok returns the model's answer nested under its own envelope.
        let nested = parse(r#"{"type":"result","structuredOutput":{"verdict":"pass","reason":"does the work"}}"#)
            .unwrap();
        assert!(nested.passed);

        // Booleans, alternative key names, and text printed after the answer.
        assert!(parse(r#"{"passed":true,"detail":"fine"}"#).unwrap().passed);
        assert!(!parse(r#"{"pass":"reject","reason":"stub"}"#).unwrap().passed);
        assert!(
            parse("here you go:\n{\"verdict\":\"pass\"}\ntokens used: 812")
                .unwrap()
                .passed
        );
    }

    #[test]
    fn prose_is_accepted_only_when_it_says_one_thing() {
        assert!(!parse("VERDICT: fail — the function is a stub").unwrap().passed);
        assert!(parse("VERDICT: pass").unwrap().passed);
        // Both words present, so no clear verdict: better to have no opinion than a coin toss.
        assert!(parse("it could pass or fail depending on your view").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn an_unreadable_reply_is_no_opinion_rather_than_a_rejection() {
        assert!(parse("{\"unrelated\":1}").is_none());
        assert!(parse("I was unable to complete the request.").is_none());
    }

    #[test]
    fn the_prompt_carries_the_diff_and_forbids_exploring() {
        let text = prompt(
            "Add a parser",
            "Parse the config file",
            &["handles comments".into()],
            &["src/parse.rs".into()],
            "@@ -1 +1 @@\n+fn parse() {}",
        );
        assert!(text.contains("Add a parser"));
        assert!(text.contains("handles comments"));
        assert!(text.contains("src/parse.rs"));
        assert!(text.contains("+fn parse() {}"));
        assert!(text.contains("Do not use tools"));
        // The reviewer must not be turned into a style critic; that would reject good work.
        assert!(text.contains("Do not reject on style"));
    }
}
