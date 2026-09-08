//! The scorer is the backbone: merge, retry and (later) ranking in compete mode all key
//! off it. It is always run by the controller inside the attempt's own worktree, never by
//! the agent — an agent's claim of success is evidence, not a result.

use crate::worker;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::sync::watch;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Verdict {
    pub passed: bool,
    /// Optional in partition mode; required by compete mode to rank attempts.
    pub score: Option<f64>,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub enum Scorer {
    /// A configured command run in the attempt's worktree. Exit 0 passes.
    Command {
        command: Vec<String>,
        timeout_seconds: u64,
    },
    // Planned (milestone 5): Human — parks the attempt for Len's verdict in the web app;
    // Agent — a roster member invoked read-only, never the provider that did the work.
}

impl Scorer {
    pub async fn score(&self, cwd: &Path, cancel: watch::Receiver<u64>) -> Result<Verdict> {
        match self {
            Self::Command {
                command,
                timeout_seconds,
            } => {
                let result =
                    worker::run_command_in(command, cwd, *timeout_seconds, cancel).await?;
                let passed = result.exit_code == Some(0) && result.interruption.is_none();
                let detail = match &result.interruption {
                    Some(reason) => format!("{reason}\n{}", result.output),
                    None => result.output.clone(),
                };
                Ok(Verdict {
                    passed,
                    score: None,
                    detail: clip(&detail, 8 * 1024),
                })
            }
        }
    }
}

fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[scorer output truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scorer(command: &[&str]) -> Scorer {
        Scorer::Command {
            command: command.iter().map(|s| (*s).to_string()).collect(),
            timeout_seconds: 30,
        }
    }

    #[tokio::test]
    async fn a_command_scorer_reports_pass_fail_and_runs_in_the_given_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), "here").unwrap();
        let (_tx, rx) = watch::channel(0);

        let verdict = scorer(&["test", "-f", "marker"]).score(dir.path(), rx.clone()).await.unwrap();
        assert!(verdict.passed, "the scorer runs inside the attempt worktree");

        let verdict = scorer(&["test", "-f", "absent"]).score(dir.path(), rx.clone()).await.unwrap();
        assert!(!verdict.passed);

        let verdict = scorer(&["sh", "-c", "echo detail-line; exit 3"])
            .score(dir.path(), rx)
            .await
            .unwrap();
        assert!(!verdict.passed);
        assert!(verdict.detail.contains("detail-line"), "failure detail is retained");
    }

    #[tokio::test]
    async fn a_hanging_scorer_times_out_rather_than_stalling_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = watch::channel(0);
        let verdict = Scorer::Command {
            command: vec!["sleep".into(), "30".into()],
            timeout_seconds: 1,
        }
        .score(dir.path(), rx)
        .await
        .unwrap();
        assert!(!verdict.passed);
        assert!(verdict.detail.contains("timed out"), "{}", verdict.detail);
    }

    #[test]
    fn long_scorer_output_is_clipped_on_a_character_boundary() {
        let text = "é".repeat(9000);
        let clipped = clip(&text, 8 * 1024);
        assert!(clipped.len() < text.len());
        assert!(clipped.contains("truncated"));
    }
}
