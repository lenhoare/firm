use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub state_dir: PathBuf,
    pub workspace: PathBuf,
    pub codex_url: String,
    pub codex_model: String,
    /// Accepted only for migration of pre-roster config files and snapshot recipes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qwen_command: Option<String>,
    #[serde(default = "legacy_providers")]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub verify_command: Vec<String>,
    pub allowances: Allowances,
    #[serde(default = "meeting_order")]
    pub meeting_order: Vec<String>,
}

fn meeting_order() -> Vec<String> {
    ["grok", "muse", "qwen", "codex"].map(String::from).to_vec()
}

fn one() -> usize {
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_config_and_extensible_roster_are_validated() {
        let config = Config::read(Path::new("firm.toml")).unwrap();
        assert_eq!(
            config
                .providers
                .iter()
                .map(|p| p.id.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            ["codex", "qwen", "muse", "grok"].into_iter().collect()
        );
        let mut old = serde_json::to_value(&config).unwrap();
        old.as_object_mut().unwrap().remove("providers");
        old["qwen_command"] = serde_json::json!("old-qwen-wrapper");
        let mut legacy: Config = serde_json::from_value(old).unwrap();
        legacy.normalize_providers().unwrap();
        assert_eq!(legacy.providers.len(), 1);
        assert_eq!(legacy.providers[0].command, "old-qwen-wrapper");
        let mut custom = config.providers[0].clone();
        custom.id = "fourth-worker".into();
        legacy.providers.push(custom.clone());
        legacy.normalize_providers().unwrap();
        legacy.providers.push(custom);
        assert!(
            legacy
                .normalize_providers()
                .unwrap_err()
                .to_string()
                .contains("Duplicate")
        );
        legacy.providers.clear();
        legacy.normalize_providers().unwrap(); // Explicitly empty roster is supported.
        let mut invalid = config;
        invalid
            .providers
            .iter_mut()
            .find(|p| p.id == "muse")
            .unwrap()
            .input = PromptInput::Stdin;
        assert!(invalid.normalize_providers().is_err());
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub input: PromptInput,
    pub enabled: bool,
    pub max_runs: usize,
    /// Cost band used by v1 routing: 0 cheap, 1 mid, 2 premium. Prefer the lowest tier
    /// plausibly capable of a task class; escalate only on repeated failure.
    #[serde(default)]
    pub tier: u8,
    /// How many attempts by this provider may run at once, under the global cap.
    #[serde(default = "one")]
    pub max_concurrent: usize,
    #[serde(default)]
    pub description: String,
    /// Separate discussion-only invocation; absent means not available for meetings.
    #[serde(default)]
    pub meeting_args: Option<Vec<String>>,
    /// Planning/review invocation. Never fall back to the implementation arguments.
    #[serde(default)]
    pub manager_args: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PromptInput {
    #[default]
    Stdin,
    PromptFile,
}

fn legacy_providers() -> Vec<Provider> {
    vec![Provider {
        id: "qwen".into(),
        name: "Qwen".into(),
        command: "qwen".into(),
        args: [
            "--prompt",
            "Carry out the assignment supplied on stdin.",
            "--output-format",
            "text",
            "--approval-mode",
            "auto-edit",
            "--max-session-turns",
            "{max_turns}",
            "--max-tool-calls",
            "{max_tool_calls}",
            "--max-wall-time",
            "{timeout_seconds}s",
        ]
        .map(String::from)
        .to_vec(),
        input: PromptInput::Stdin,
        enabled: true,
        max_runs: 6,
        tier: 0,
        max_concurrent: one(),
        description: "General-purpose coding worker".into(),
        meeting_args: None,
        manager_args: None,
    }]
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Allowances {
    pub manager_turns: usize,
    pub worker_runs: usize,
    pub window_seconds: u64,
    pub manager_interval_seconds: u64,
    pub max_turn_seconds: u64,
    pub worker_timeout_seconds: u64,
    pub worker_max_turns: u32,
    pub worker_max_tool_calls: u32,
    pub max_used_percent: f64,
    pub usage_max_age_seconds: u64,
    pub provider_cooldown_seconds: u64,
}

impl Allowances {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.window_seconds > 0 && self.max_turn_seconds > 0 && self.worker_timeout_seconds > 0,
            "Durations must be positive"
        );
        ensure!(
            self.worker_max_turns > 0 && self.worker_max_tool_calls > 0,
            "Worker limits must be positive"
        );
        ensure!(
            self.usage_max_age_seconds > 0
                && self.max_used_percent > 0.0
                && self.max_used_percent <= 100.0,
            "Invalid usage threshold or freshness limit"
        );
        Ok(())
    }
}

impl Config {
    pub fn normalize_providers(&mut self) -> Result<()> {
        if let Some(command) = self.qwen_command.take() {
            let qwen = self
                .providers
                .iter_mut()
                .find(|p| p.id == "qwen")
                .ok_or_else(|| anyhow::anyhow!("Legacy qwen_command requires a qwen provider"))?;
            qwen.command = command;
        }
        let mut ids = std::collections::BTreeSet::new();
        ensure!(
            !self.meeting_order.is_empty()
                && self.meeting_order.len() <= 8
                && self.meeting_order.last().is_some_and(|p| p == "codex")
                && self
                    .meeting_order
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == self.meeting_order.len(),
            "Meeting order must contain at most 8 unique participants, ending with codex"
        );
        ensure!(self.providers.len() <= 64, "At most 64 providers");
        for provider in &self.providers {
            ensure!(
                !provider.id.is_empty()
                    && provider.id.len() <= 64
                    && provider.id.bytes().all(|b| b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b == b'-'
                        || b == b'_'),
                "Invalid provider ID: {}",
                provider.id
            );
            ensure!(
                ids.insert(&provider.id),
                "Duplicate provider ID: {}",
                provider.id
            );
            ensure!(
                !provider.name.trim().is_empty()
                    && !provider.command.trim().is_empty()
                    && provider.name.len() <= 128
                    && provider.description.len() <= 4000,
                "Invalid provider name, command or description: {}",
                provider.id
            );
            ensure!(
                provider.max_concurrent >= 1 && provider.max_concurrent <= 16,
                "Provider {} must allow 1-16 concurrent attempts",
                provider.id
            );
            ensure!(
                provider.args.len() <= 100
                    && provider
                        .args
                        .iter()
                        .all(|a| a.len() <= 16000 && !a.contains('\0'))
                    && !provider.command.contains('\0'),
                "Invalid command arguments: {}",
                provider.id
            );
            let has_file = provider.args.iter().any(|a| a.contains("{prompt_file}"));
            for args in [&provider.meeting_args, &provider.manager_args]
                .into_iter()
                .flatten()
            {
                ensure!(
                    args.len() <= 100 && args.iter().all(|a| a.len() <= 16000 && !a.contains('\0')),
                    "Invalid discussion/manager arguments: {}",
                    provider.id
                );
                ensure!(
                    args.iter().any(|a| a.contains("{prompt_file}"))
                        == (provider.input == PromptInput::PromptFile),
                    "Discussion/manager prompt transport must match provider input: {}",
                    provider.id
                );
            }
            ensure!(
                has_file == (provider.input == PromptInput::PromptFile),
                "Provider {}: prompt_file input requires a {{prompt_file}} argument; stdin must not have one",
                provider.id
            );
        }
        Ok(())
    }
    pub fn read(path: &Path) -> Result<Self> {
        let mut config: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        ensure!(
            config.listen.ip().is_loopback(),
            "Dashboard must bind to a loopback address"
        );
        ensure!(
            config.codex_url.starts_with("ws://127.0.0.1:")
                || config.codex_url.starts_with("ws://localhost:"),
            "This prototype requires a local Codex app-server"
        );
        config.allowances.validate()?;
        config.normalize_providers()?;
        let base = path.canonicalize()?.parent().unwrap().to_path_buf();
        config.workspace = base.join(&config.workspace).canonicalize()?;
        config.state_dir = base.join(&config.state_dir);
        Ok(config)
    }
}
