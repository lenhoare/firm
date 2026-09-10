//! Turning a written brief into a task graph.
//!
//! One manager call at the start of a run, not one per unit of work — that spacing is the
//! whole cost argument. The result is written out as the same JSON `--tasks` already
//! accepts, so a person reads it before anything runs.
//!
//! A proposed graph is validated before it is offered, and validation **runs each proposed
//! check**. A manager can invent a `verify` command that does not execute, or one that
//! passes before any work is done, and either is worse than no check at all: the first
//! fails every attempt, the second merges everything unconditionally.

use super::board::{RunSpec, validate_graph};
use crate::{config::Config, worker};
use anyhow::{Context, Result, bail};
use std::path::Path;
use tokio::sync::watch;

/// What checking one proposed task's command established.
#[derive(Debug)]
pub struct CheckReport {
    pub task: String,
    pub command: String,
    /// Whether the command could be executed at all.
    pub ran: bool,
    pub exit: Option<i32>,
    /// A check that already passes before the work is done proves nothing about the work.
    pub vacuous: bool,
    pub detail: String,
}

impl CheckReport {
    pub fn sound(&self) -> bool {
        self.ran && !self.vacuous
    }
}

/// Guards a plan leaves switched off. A task with no declared `files` can change anything,
/// and a test-writing task with no `must_fail` could write a test that asserts nothing —
/// neither is an error, but neither should be silent.
pub fn ungarded(spec: &RunSpec) -> Vec<String> {
    let mut warnings = Vec::new();
    for task in &spec.tasks {
        if task.files.is_empty() {
            warnings.push(format!(
                "{} declares no file scope, so it may change anything",
                task.id
            ));
        }
        if task.class == "test" && task.must_fail.as_ref().is_none_or(|c| c.is_empty()) {
            warnings.push(format!(
                "{} writes tests but gives no must_fail, so a test that asserts nothing would pass",
                task.id
            ));
        }
    }
    warnings
}

/// Ask the planner to decompose a brief into a graph. Returns the parsed graph and the
/// raw reply, which is kept so a refusal or a malformed answer can be inspected.
pub async fn decompose(
    config: &Config,
    brief: &str,
    notes: &str,
    validation: &super::validate::Validation,
    cancel: watch::Receiver<u64>,
) -> Result<(RunSpec, String)> {
    let planner = config
        .providers
        .iter()
        .find(|p| p.id == config.planner && p.enabled)
        .cloned()
        .with_context(|| format!("Planner {} is not configured or not enabled", config.planner))?;
    let mut provider = planner.clone();
    // Planning reads the workspace to see what is actually there, so it uses the read-only
    // planning invocation — tools available, no writing.
    provider.args = provider
        .planner_args
        .clone()
        .or_else(|| provider.manager_args.clone())
        .context("The planner needs planner_args or manager_args")?;

    let mut config = config.clone();
    config.verify_command.clear();
    // Idle detection assumes a streaming event log. A planning invocation emits plain text
    // and says nothing until it has finished thinking, so silence here is work, not a hang.
    config.allowances.idle_timeout_seconds = 0;
    let prompt = prompt(&config, brief, notes, validation);
    let prepared =
        worker::prepare_prompt_in(&config, &provider, prompt, config.workspace.clone())?;
    let result = worker::run(&config, &prepared, cancel).await?;
    let raw = result.output;
    if let Some(reason) = result.interruption {
        bail!("The planner was interrupted: {reason}\n{}", tail(&raw));
    }
    // Show what it actually said. "No JSON object" on its own is undebuggable.
    let mut spec = parse(&raw)
        .with_context(|| format!("The planner did not return a usable task graph.\n{}", tail(&raw)))?;
    validate_graph(&spec)?;
    // The model of success travels with the plan: it was written first and the plan has to
    // answer to it, not the other way round.
    spec.validation = validation.clone();
    Ok((spec, raw))
}

fn prompt(
    config: &Config,
    brief: &str,
    notes: &str,
    validation: &super::validate::Validation,
) -> String {
    // Written before this call, by something that never saw a plan. Handing it over makes
    // the criteria binding on the decomposition rather than a document nobody reads.
    let success = if validation.is_empty() {
        String::new()
    } else {
        let probes = validation
            .probes
            .iter()
            .map(|p| format!("  - {}: `{}`", p.description, p.command.join(" ")))
            .collect::<Vec<_>>()
            .join("\n");
        let criteria = validation
            .criteria
            .iter()
            .map(|c| format!("  - {c}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\n\nHOW THIS WILL BE JUDGED WHEN IT IS FINISHED. These were written before \
             you saw the brief and are not yours to change; plan work that satisfies them. \
             They are checked against the assembled project at the end, not task by task, \
             so make sure something in your graph is responsible for each.\n\
             Probes that will be run:\n{probes}\n\
             Judgements a person will make:\n{criteria}"
        )
    };
    let providers: Vec<&str> = config
        .providers
        .iter()
        .filter(|p| p.enabled)
        .map(|p| p.id.as_str())
        .collect();
    // What a check can mean depends on the regime. In trial mode it is proof; in build
    // mode most tasks have none, and demanding one produces invented checks that pass
    // vacuously — worse than none, because they look like evidence.
    let verify_rule = if config.mode == "build" {
        "- A `verify` command is optional here, and worth giving only where a real one \
           exists: a check that genuinely fails now and passes once the task is done. Do \
           not invent one to fill the field. Where there is none, the project's own tests \
           act as a guard against breakage and a reviewer reads the diff, so what matters \
           is that `acceptance` states plainly what done looks like — specific enough that \
           someone reading the diff could tell whether it was achieved.\n"
    } else {
        "- Each task needs a `verify` command: the exact argv the controller will run to \
           decide whether that task's work is acceptable. It must be scoped to that task, \
           because while other tasks are unfinished a whole-project check necessarily \
           fails. It must fail now and pass once the task is done — a check that already \
           passes proves nothing.\n"
    };
    format!(
        "You are planning work for a team of coding agents that run **in parallel**, each \
         in its own isolated git worktree, on the project at {workspace}. Read the \
         workspace to see what is actually there. Do not change any files.\n\n\
         Decompose the brief below into a task graph.\n\n\
         Rules that matter:\n\
         - Prefer tasks that touch **disjoint files**. Agents work simultaneously and their \
           work is merged, so two tasks editing the same file will conflict.\n\
         - Add a dependency only where one task genuinely needs another's merged result. \
           Every unnecessary dependency removes parallelism.\n\
         {verify_rule}\
         - `verify` is an argv array, executed directly with no shell: \
           [\"cargo\", \"test\", \"--offline\", \"--test\", \"parser\"], never a single string.\n\
         - `files` is required on every task: exactly the files it may modify. Anything else it changes \
           is rejected, which is what stops one task quietly editing another's work — or a \
           task editing the test it is supposed to satisfy.\n\
         - A task that writes a test must also give `must_fail`: a command that must still \
           fail once the test exists, because the code it tests has not been written. A new \
           test that passes against unimplemented code asserts nothing.\n\
         - Do not assign providers. Routing chooses from {providers:?}.\n\
         - If the project cannot be split this way — one monolithic test target, shared \
           fixtures, or no tests at all — say so by making the first task the one that \
           creates the seam: add the focused test target that later tasks can be judged \
           against. A graph of one huge task is a worse answer than a graph that starts by \
           making decomposition possible.\n\n\
         Reply with JSON only, no prose and no markdown fence:\n\
         {{\"objective\": \"...\", \"tasks\": [{{\"id\": \"short-slug\", \"title\": \"...\", \
         \"brief\": \"what to do, which files, and any constraint\", \
         \"acceptance\": [\"...\"], \"verify\": [\"...\"], \"files\": [\"src/thing.rs\"], \
         \"depends_on\": [], \"class\": \"implement\"}}]}}\n\
         A test-writing task adds \"must_fail\": [\"cargo\", \"test\", \"--test\", \"thing\"].\n\n\
         Ids are lowercase slugs, unique, and referenced by depends_on. Class is one of \
         design, implement, test, review, docs, integrate.\n\n\
         --- brief ---\n{brief}\n--- end brief ---{success}{notes}",
        workspace = config.workspace.display(),
        providers = providers,
        verify_rule = verify_rule,
        success = success,
    )
}

/// Pull a run specification out of a reply that may be wrapped in prose, a fence, or a
/// CLI's own result envelope — grok returns the model's answer under `structuredOutput`.
fn parse(reply: &str) -> Result<RunSpec> {
    if let Ok(spec) = serde_json::from_str::<RunSpec>(reply.trim()) {
        return Ok(spec);
    }
    // Take each balanced object in turn rather than assuming the reply is one object.
    // CLIs print things after the answer — codex appends a token count — and spanning from
    // the first brace to the last swallows whatever followed.
    let mut last_error = None;
    for candidate in objects(reply) {
        if let Ok(spec) = serde_json::from_str::<RunSpec>(candidate) {
            return Ok(spec);
        }
        match serde_json::from_str::<serde_json::Value>(candidate) {
            Ok(value) => {
                if let Some(spec) = find_graph(&value) {
                    return Ok(spec);
                }
            }
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => bail!("Reply was not valid JSON: {error}"),
        None => bail!("No task graph found in the reply"),
    }
}

/// Every balanced `{...}` region in the text, outermost first, ignoring braces inside
/// strings so an escaped quote or a brace in prose cannot throw off the count.
pub fn objects(reply: &str) -> Vec<&str> {
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
                    if let Some(slice) = reply.get(index..=cursor) {
                        found.push(slice);
                    }
                    break;
                }
            }
            cursor += 1;
        }
        index += 1;
    }
    found
}

/// Search a JSON document for the graph, wherever a CLI chose to put it.
fn find_graph(value: &serde_json::Value) -> Option<RunSpec> {
    if let serde_json::Value::Object(map) = value {
        if map.contains_key("objective") && map.get("tasks").is_some_and(serde_json::Value::is_array)
            && let Ok(spec) = serde_json::from_value::<RunSpec>(value.clone())
        {
            return Some(spec);
        }
        return map.values().find_map(find_graph);
    }
    if let serde_json::Value::Array(items) = value {
        return items.iter().find_map(find_graph);
    }
    None
}

/// Run every proposed check in the workspace as it is now. They should execute, and they
/// should fail: the work has not been done yet.
pub async fn check_proposals(
    config: &Config,
    spec: &RunSpec,
    cancel: &watch::Receiver<u64>,
) -> Vec<CheckReport> {
    let mut reports = Vec::new();
    for task in &spec.tasks {
        let Some(command) = task.verify.clone().filter(|c| !c.is_empty()) else {
            reports.push(CheckReport {
                task: task.id.clone(),
                command: String::new(),
                ran: false,
                exit: None,
                vacuous: false,
                detail: "No verify command proposed; the task cannot be judged".into(),
            });
            continue;
        };
        let outcome =
            worker::run_command_in(&command, &config.workspace, 120, cancel.clone()).await;
        let report = match outcome {
            Ok(result) => CheckReport {
                task: task.id.clone(),
                command: command.join(" "),
                ran: result.interruption.is_none(),
                exit: result.exit_code,
                // Passing before the work exists means it is not measuring the work.
                vacuous: result.exit_code == Some(0),
                detail: result
                    .interruption
                    .unwrap_or_else(|| super::dispatch::clip(result.output.trim(), 300)),
            },
            Err(error) => CheckReport {
                task: task.id.clone(),
                command: command.join(" "),
                ran: false,
                exit: None,
                vacuous: false,
                detail: error.to_string(),
            },
        };
        reports.push(report);
    }
    reports
}

/// The end of a reply, which is where a model puts its answer.
fn tail(reply: &str) -> String {
    let trimmed = reply.trim();
    let start = trimmed.len().saturating_sub(1200);
    let mut start = start;
    while start > 0 && !trimmed.is_char_boundary(start) {
        start -= 1;
    }
    format!("--- planner said ---\n{}", &trimmed[start..])
}

/// Read a brief, rejecting an empty one early rather than paying for a planning call.
pub fn read_brief(path: &Path) -> Result<String> {
    let brief = std::fs::read_to_string(path)
        .with_context(|| format!("Could not read the brief at {}", path.display()))?;
    if brief.trim().is_empty() {
        bail!("The brief at {} is empty", path.display());
    }
    Ok(brief)
}

/// Render the graph in the format `--tasks` accepts, so planning and running share a file
/// a person can read and edit in between.
pub fn to_tasks_json(spec: &RunSpec) -> Result<String> {
    Ok(serde_json::to_string_pretty(spec)? + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::board::TaskSpec;

    fn spec(json: &str) -> Result<RunSpec> {
        parse(json)
    }

    #[test]
    fn a_graph_is_recovered_from_an_envelope() {
        // grok returns the model's answer nested under its own result envelope.
        let enveloped = r#"{"usage":{"modelCalls":1},"structuredOutput":{"objective":"o",
            "tasks":[{"id":"a","title":"A","brief":"b","verify":["cargo","test"]}]}}"#;
        let spec = parse(enveloped).unwrap();
        assert_eq!(spec.objective, "o");
        assert_eq!(spec.tasks[0].verify.as_ref().unwrap(), &["cargo", "test"]);
        assert!(parse(r#"{"usage":{"modelCalls":1}}"#).is_err(), "an envelope with no graph");

        // codex prints a token count after its answer; the graph must still be found.
        let trailing = r#"{"objective":"o","tasks":[{"id":"a","title":"A","brief":"b"}]}
tokens used
10,445"#;
        assert_eq!(parse(trailing).unwrap().objective, "o");

        // A brace inside a string must not confuse the scan.
        let braced = r#"prose { not json
{"objective":"use {} carefully","tasks":[{"id":"a","title":"A","brief":"b"}]}"#;
        assert_eq!(parse(braced).unwrap().objective, "use {} carefully");
    }

    #[test]
    fn a_graph_is_recovered_from_prose_and_fences() {
        let bare = r#"{"objective":"o","tasks":[{"id":"a","title":"A","brief":"b"}]}"#;
        assert_eq!(spec(bare).unwrap().tasks.len(), 1);

        let wrapped = format!("Here is the plan:\n```json\n{bare}\n```\nHope that helps.");
        assert_eq!(spec(&wrapped).unwrap().objective, "o");

        assert!(spec("I cannot help with that.").is_err());
        assert!(spec("{not json}").is_err());
    }

    #[tokio::test]
    async fn proposed_checks_are_run_and_vacuous_ones_are_caught() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::read(Path::new("firm.toml")).unwrap();
        config.workspace = dir.path().to_path_buf();
        let task = |id: &str, verify: Option<Vec<&str>>| TaskSpec {
            id: id.into(),
            title: id.into(),
            brief: "b".into(),
            acceptance: vec![],
            depends_on: vec![],
            class: "implement".into(),
            provider: None,
            verify: verify.map(|v| v.iter().map(|s| (*s).to_string()).collect()),
            files: Vec::new(),
            must_fail: None,
        };
        let spec = RunSpec {
            brief: String::new(),
            validation: Default::default(),
            objective: "o".into(),
            tasks: vec![
                task("honest", Some(vec!["test", "-f", "not-yet-written"])),
                task("vacuous", Some(vec!["true"])),
                task("broken", Some(vec!["definitely-not-a-command"])),
                task("missing", None),
            ],
        };
        let (_tx, rx) = watch::channel(0);
        let reports = check_proposals(&config, &spec, &rx).await;

        let by = |id: &str| reports.iter().find(|r| r.task == id).unwrap();
        assert!(by("honest").sound(), "fails now, will pass when done: {:?}", by("honest"));
        assert!(by("vacuous").ran, "it does execute");
        assert!(!by("vacuous").sound(), "but it passes before any work exists");
        assert!(!by("broken").ran, "a command that cannot start is not a check");
        assert!(!by("missing").ran, "a task with no check cannot be judged");
    }
}
