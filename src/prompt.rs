//! Prompts as files rather than string literals.
//!
//! Most of what makes this system work is prose, and prose spliced through Rust `format!`
//! literals with backslash continuations is hard to read, harder to review, and produces
//! diffs nobody can follow. It also kept the wrong things apart: a prompt asking for
//! `{verdict, reason}` lived in Rust while the JSON schema pinning a CLI to that shape sat
//! in a provider's arguments in TOML. That separation is why the validation planner was
//! once handed the reviewer's schema and answered with nothing at all.
//!
//! So a prompt is a markdown file with frontmatter: the invocation settings that belong to
//! it — schema, turn budget, tools — sit directly above the words that assume them.
//!
//! ```text
//! ---
//! max_turns: 8
//! tools: list_dir,read_file,grep
//! schema: {"type":"object", ...}
//! ---
//! You are reviewing one coding agent's finished work...
//! ```
//!
//! Substitution is `{name}` and deliberately not a template language: no conditionals, no
//! loops, no inheritance. Anything that needs a decision is decided in Rust and passed in
//! as a value. The one risk this introduces is a placeholder nobody fills — `format!`
//! catches that at compile time and a runtime replace would not — so rendering fails loudly
//! on any `{placeholder}` left behind, and a test renders every shipped prompt to prove it.

use anyhow::{Result, bail};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct Prompt {
    /// Frontmatter keys, in file order.
    settings: BTreeMap<String, String>,
    body: String,
}

impl Prompt {
    /// Parse a prompt file. Frontmatter is optional: a file with no `---` fence is all body,
    /// which keeps the older prompts working unchanged.
    pub fn parse(text: &str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let Some(rest) = text.strip_prefix("---\n") else {
            return Self {
                settings: BTreeMap::new(),
                body: text.trim_start_matches('\n').to_string(),
            };
        };
        let Some(end) = rest.find("\n---") else {
            return Self {
                settings: BTreeMap::new(),
                body: text.to_string(),
            };
        };
        let mut settings = BTreeMap::new();
        for line in rest[..end].lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once(':') {
                settings.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
        let body = rest[end + 4..].trim_start_matches('\n').to_string();
        Self { settings, body }
    }

    pub fn setting(&self, key: &str) -> Option<&str> {
        self.settings.get(key).map(String::as_str)
    }

    /// The JSON schema this prompt's wording assumes, if it declares one. Kept here so a
    /// CLI can be held to the shape the prose actually asks for.
    pub fn schema(&self) -> Option<&str> {
        self.setting("schema")
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    /// Fill in `{name}` placeholders. Fails if any is left over, because an unfilled
    /// placeholder is sent verbatim to a model and reads as gibberish to it.
    pub fn render(&self, values: &[(&str, &str)]) -> Result<String> {
        let mut text = self.body.clone();
        for (name, value) in values {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        if let Some(missing) = leftover(&text) {
            bail!("Prompt has an unfilled placeholder: {{{missing}}}");
        }
        Ok(text)
    }
}

/// The first `{placeholder}` still present: a short run of letters, digits and underscores
/// inside braces. JSON examples in a prompt use `{{` and `}}`-free shapes like
/// `{"verdict": ...}`, which this deliberately does not match.
fn leftover(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'{' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
        {
            end += 1;
        }
        if end > start && end < bytes.len() && bytes[end] == b'}' {
            return Some(text[start..end].to_string());
        }
        index += 1;
    }
    None
}

/// Every prompt compiled into the binary, by name. Compile-time rather than read from disk:
/// a missing file becomes a build error instead of a failed run, and `snapshots.rs` already
/// hashes these into its reproducibility recipe.
pub fn shipped() -> Vec<(&'static str, Prompt)> {
    SOURCES
        .iter()
        .map(|(name, text)| (*name, Prompt::parse(text)))
        .collect()
}

// One entry per file. `get` is how callers reach them.
const SOURCES: &[(&str, &str)] = &[
    ("worker", include_str!("../prompts/worker.md")),
    ("manager", include_str!("../prompts/manager.md")),
    ("meeting", include_str!("../prompts/meeting.md")),
    ("planner", include_str!("../prompts/planner.md")),
    // The planner's one conditional rule, as two files rather than a branch in a string.
    ("planner_verify_trial", include_str!("../prompts/planner_verify_trial.md")),
    ("planner_verify_build", include_str!("../prompts/planner_verify_build.md")),
    ("validator", include_str!("../prompts/validator.md")),
    ("reviewer", include_str!("../prompts/reviewer.md")),
    ("observer", include_str!("../prompts/observer.md")),
    ("objective", include_str!("../prompts/objective.md")),
    ("notes", include_str!("../prompts/notes.md")),
    ("task", include_str!("../prompts/task.md")),
];

/// Look one up by name. Panics if absent, which can only happen if a caller and `SOURCES`
/// disagree — a programming error, caught by the test below rather than in the field.
pub fn get(name: &str) -> Prompt {
    let text = SOURCES
        .iter()
        .find(|(id, _)| *id == name)
        .unwrap_or_else(|| panic!("No prompt named {name}"))
        .1;
    Prompt::parse(text)
}

/// Collapse runs of whitespace, so a test can assert on a phrase without caring where the
/// markdown happens to wrap. Prompts are prose and get re-wrapped constantly; assertions
/// that break on a line ending would make editing them a chore.
#[cfg(test)]
pub fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_is_split_from_the_prose_that_assumes_it() {
        let prompt = Prompt::parse(
            "---\nmax_turns: 8\nschema: {\"type\":\"object\"}\n# a comment\n---\nHello {name}.\n",
        );
        assert_eq!(prompt.setting("max_turns"), Some("8"));
        assert_eq!(prompt.schema(), Some("{\"type\":\"object\"}"));
        assert_eq!(prompt.body().trim(), "Hello {name}.");
        assert_eq!(prompt.render(&[("name", "world")]).unwrap().trim(), "Hello world.");
    }

    #[test]
    fn a_file_without_frontmatter_is_all_prose() {
        let prompt = Prompt::parse("Just the words.\n");
        assert!(prompt.schema().is_none());
        assert_eq!(prompt.body().trim(), "Just the words.");
    }

    #[test]
    fn an_unfilled_placeholder_is_an_error_rather_than_gibberish_sent_to_a_model() {
        // What `format!` caught at compile time and a runtime replace would not.
        let prompt = Prompt::parse("Do {this} with {that}.");
        let error = prompt.render(&[("this", "something")]).unwrap_err().to_string();
        assert!(error.contains("{that}"), "{error}");
        assert!(prompt.render(&[("this", "a"), ("that", "b")]).is_ok());
    }

    #[test]
    fn json_examples_in_a_prompt_are_not_mistaken_for_placeholders() {
        // Prompts show CLIs the shape to reply in, and that shape is full of braces.
        let prompt = Prompt::parse("Reply with {\"verdict\": \"pass\", \"reason\": \"...\"}");
        assert!(prompt.render(&[]).is_ok());
    }

    /// The test that exists because three prompt bugs in a row were only found by paying a
    /// model to fail. Rendering every shipped prompt with placeholder values proves the
    /// files parse, that nothing is left unfilled, and that any declared schema is real JSON.
    #[test]
    fn every_shipped_prompt_renders_and_declares_a_usable_schema() {
        let filler = [
            ("workspace", "/tmp/workspace"),
            ("providers", "grok, muse"),
            ("verify_rule", "- a rule"),
            ("success", "how it will be judged"),
            ("brief", "a brief"),
            ("notes", "some notes"),
            ("objective", "an objective"),
            ("title", "a task"),
            ("acceptance", "- criteria"),
            ("files", "src/lib.rs"),
            ("diff", "@@ -1 +1 @@"),
            ("check", "cargo test"),
            ("task", "a task"),
            ("preamble", "a preamble"),
            ("notes_path", "/tmp/notes.jsonl"),
            ("stream", "an event stream"),
            ("provider", "grok"),
            ("outcome", "verified"),
            ("kinds", "finding, blocker"),
            ("bigger_picture", "the point"),
            ("previous", "what happened before"),
            ("forum", "what others found"),
        ];
        for (name, prompt) in shipped() {
            let rendered = prompt
                .render(&filler)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(
                rendered.len() > 40,
                "{name} rendered to almost nothing; is the body empty?"
            );
            if let Some(schema) = prompt.schema() {
                serde_json::from_str::<serde_json::Value>(schema)
                    .unwrap_or_else(|error| panic!("{name} declares invalid JSON schema: {error}"));
            }
        }
    }
}
