//! The shared forum: what agents working in parallel learn from each other.
//!
//! Two sources, deliberately. The **controller** writes entries from evidence it already
//! holds — outcomes, failures, timeouts, conflicts — which costs nothing and cannot be
//! skipped. An **observer** model reads a finished attempt's event stream and writes the
//! things only the agent knew: what it tried and abandoned, constraints it discovered.
//!
//! Asking workers to self-report was rejected: it contradicts the "change only this file"
//! instruction, a shared file in the worktree would be the most merge-conflict-prone thing
//! in the repository, and unrewarded side-work is the first thing a cheap model drops.
//!
//! Entries are untrusted text written by agents. They are rendered into later prompts as
//! attributed, quoted data — never as instructions.

use crate::state::now;
use anyhow::{Result, bail, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

/// Highest-value kinds sort first into a prompt slice. Dead ends lead because they are the
/// entries that most reliably save another agent a wasted run, and the ones least
/// recoverable from a diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    DeadEnd,
    Blocker,
    ApiFact,
    Convention,
    Finding,
    Decision,
    Outcome,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeadEnd => "dead_end",
            Self::Blocker => "blocker",
            Self::ApiFact => "api_fact",
            Self::Convention => "convention",
            Self::Finding => "finding",
            Self::Decision => "decision",
            Self::Outcome => "outcome",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value.trim().to_ascii_lowercase().as_str() {
            "dead_end" | "deadend" | "dead-end" => Self::DeadEnd,
            "blocker" => Self::Blocker,
            "api_fact" | "apifact" | "api-fact" => Self::ApiFact,
            "convention" => Self::Convention,
            "finding" => Self::Finding,
            "decision" => Self::Decision,
            "outcome" => Self::Outcome,
            other => bail!("Unknown forum entry kind: {other}"),
        })
    }
    /// Ordering used when filling a bounded prompt slice.
    fn rank(self) -> u8 {
        match self {
            Self::DeadEnd => 0,
            Self::Blocker => 1,
            Self::ApiFact => 2,
            Self::Convention => 3,
            Self::Finding => 4,
            Self::Decision => 5,
            Self::Outcome => 6,
        }
    }
}

pub const MAX_TITLE: usize = 200;
pub const MAX_BODY: usize = 2000;

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub id: String,
    pub run_id: String,
    pub task_id: Option<String>,
    pub author: String,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub created_at: u64,
}

/// What an observer is asked to return. Kept small and flat so a cheap model can produce
/// it reliably, and lenient on the kind so a near-miss is not discarded.
#[derive(Clone, Debug, Deserialize)]
pub struct Draft {
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
}

pub struct Forum<'a> {
    conn: &'a Connection,
}

impl<'a> Forum<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn initialize(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS forum (id TEXT PRIMARY KEY, run_id TEXT NOT NULL, task_id TEXT, author TEXT NOT NULL, kind TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL, created_at INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS forum_run ON forum(run_id, created_at);",
        )?;
        Ok(())
    }

    /// Publish one entry. Titles and bodies are clipped rather than rejected: a
    /// slightly-too-long observation is still worth keeping.
    pub fn publish(
        &self,
        run_id: &str,
        task_id: Option<&str>,
        author: &str,
        kind: Kind,
        title: &str,
        body: &str,
    ) -> Result<String> {
        let title = clip(title.trim(), MAX_TITLE);
        ensure!(!title.is_empty(), "A forum entry needs a title");
        let body = clip(body.trim(), MAX_BODY);
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO forum(id,run_id,task_id,author,kind,title,body,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![id, run_id, task_id, author, kind.as_str(), title, body, now()],
        )?;
        Ok(id)
    }

    pub fn entries(&self, run_id: &str) -> Result<Vec<Entry>> {
        let mut query = self.conn.prepare(
            "SELECT id,run_id,task_id,author,kind,title,body,created_at FROM forum WHERE run_id=?1 ORDER BY created_at",
        )?;
        let rows = query.query_map([run_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, u64>(7)?,
            ))
        })?;
        rows.map(|row| {
            let row = row?;
            Ok(Entry {
                id: row.0,
                run_id: row.1,
                task_id: row.2,
                author: row.3,
                kind: Kind::parse(&row.4)?,
                title: row.5,
                body: row.6,
                created_at: row.7,
            })
        })
        .collect()
    }

    /// The slice of the forum shown to an agent starting work, under a hard byte budget.
    ///
    /// An unbounded forum poisons every prompt, so this is a filter, not a dump: entries
    /// from the agent's own task are dropped (it is about to do that work), the most
    /// useful kinds lead, and newest wins within a kind.
    pub fn slice_for(&self, run_id: &str, task_id: &str, budget: usize) -> Result<String> {
        let mut entries: Vec<Entry> = self
            .entries(run_id)?
            .into_iter()
            .filter(|e| e.task_id.as_deref() != Some(task_id))
            .collect();
        if entries.is_empty() {
            return Ok(String::new());
        }
        entries.sort_by_key(|e| (e.kind.rank(), std::cmp::Reverse(e.created_at)));

        let mut out = String::new();
        for entry in entries {
            let rendered = format!(
                "- [{}] {} (by {})\n  {}\n",
                entry.kind.as_str(),
                entry.title,
                entry.author,
                entry.body.replace('\n', "\n  ")
            );
            if out.len() + rendered.len() > budget {
                continue;
            }
            out.push_str(&rendered);
        }
        Ok(out)
    }
}

/// Extract entries from an observer's reply. Models emit imperfect JSON, so this is
/// deliberately forgiving: every line that parses as an entry is kept, everything else is
/// ignored rather than failing the whole batch.
pub fn parse_drafts(reply: &str) -> Vec<Draft> {
    let mut drafts = Vec::new();
    for line in reply.lines() {
        let line = line.trim().trim_start_matches("```json").trim_matches('`');
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(draft) = serde_json::from_str::<Draft>(line)
            && !draft.title.trim().is_empty()
        {
            drafts.push(draft);
        }
    }
    // A reply wrapped in one array, or in an object with an `entries` list — the latter is
    // what a schema-constrained model returns, and it may be pretty-printed across lines.
    if drafts.is_empty()
        && let Some(start) = reply.find('[')
        && let Some(end) = reply.rfind(']')
        && start < end
        && let Ok(batch) = serde_json::from_str::<Vec<Draft>>(&reply[start..=end])
    {
        drafts.extend(batch.into_iter().filter(|d| !d.title.trim().is_empty()));
    }
    if drafts.is_empty()
        && let Some(start) = reply.find('{')
        && let Some(end) = reply.rfind('}')
        && start < end
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&reply[start..=end])
    {
        // Accept an `entries` array wherever it appears, since CLIs wrap replies in their
        // own envelopes.
        drafts.extend(find_entries(&value));
    }
    drafts.truncate(8);
    drafts
}

/// Reduce a raw agent event stream to the parts an observer can actually use.
///
/// A few minutes of work produces tens of kilobytes of NDJSON, nearly all of it lifecycle
/// bookkeeping. Sending that whole blob is wasteful and actively harmful: at least one CLI
/// silently offloads an oversized prompt to disk and expects the model to read it back,
/// which an observer with no tools cannot do. So keep the human-meaningful strings — what
/// the agent said, its reasoning summaries, status messages and errors — and drop the rest.
pub fn distil_stream(output: &str, budget: usize) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0usize;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            // Not an event: plain output, which is exactly the readable part.
            push_unique(&mut kept, &mut total, trimmed, budget);
            continue;
        };
        for text in meaningful_strings(&value) {
            push_unique(&mut kept, &mut total, &text, budget);
        }
        if total >= budget {
            break;
        }
    }
    kept.join("\n")
}

/// Human-readable strings from an event, ignoring identifiers and structural noise.
fn meaningful_strings(value: &serde_json::Value) -> Vec<String> {
    let mut found = Vec::new();
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if matches!(key.as_str(), "text" | "message" | "content" | "summary_text")
                    && let Some(text) = child.as_str()
                {
                    let text = text.trim();
                    // Skip the prompt echo and anything too long to be a useful line.
                    if !text.is_empty() && text.len() < 600 {
                        found.push(text.to_string());
                    }
                } else {
                    found.extend(meaningful_strings(child));
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                found.extend(meaningful_strings(item));
            }
        }
        _ => {}
    }
    found
}

fn push_unique(kept: &mut Vec<String>, total: &mut usize, text: &str, budget: usize) {
    if *total + text.len() > budget || kept.last().map(String::as_str) == Some(text) {
        return;
    }
    *total += text.len() + 1;
    kept.push(text.to_string());
}

/// Search a JSON document for an `entries` array of drafts, at any depth.
fn find_entries(value: &serde_json::Value) -> Vec<Draft> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(entries) = map.get("entries")
                && let Ok(drafts) = serde_json::from_value::<Vec<Draft>>(entries.clone())
            {
                return drafts
                    .into_iter()
                    .filter(|d| !d.title.trim().is_empty())
                    .collect();
            }
            map.values().flat_map(find_entries).collect()
        }
        serde_json::Value::Array(items) => items.iter().flat_map(find_entries).collect(),
        _ => Vec::new(),
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
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forum() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        Forum::initialize(&conn).unwrap();
        conn
    }

    #[test]
    fn a_slice_leads_with_dead_ends_and_respects_its_budget() {
        let conn = forum();
        let forum = Forum::new(&conn);
        forum.publish("r", Some("a"), "controller", Kind::Outcome, "a merged", "fine").unwrap();
        forum.publish("r", Some("b"), "grok", Kind::Finding, "a finding", "detail").unwrap();
        forum.publish("r", Some("c"), "grok", Kind::DeadEnd, "regex approach fails", "it backtracks").unwrap();

        let slice = forum.slice_for("r", "z", 4096).unwrap();
        let dead = slice.find("dead_end").unwrap();
        let finding = slice.find("finding").unwrap();
        assert!(dead < finding, "dead ends lead:\n{slice}");
        assert!(slice.contains("(by grok)"), "entries are attributed");

        // An agent is not shown notes about the task it is about to do.
        let own = forum.slice_for("r", "c", 4096).unwrap();
        assert!(!own.contains("regex approach fails"), "{own}");

        // The budget is a hard cap.
        let tight = forum.slice_for("r", "z", 60).unwrap();
        assert!(tight.len() <= 60, "{} bytes", tight.len());
    }

    #[test]
    fn oversized_entries_are_clipped_rather_than_rejected() {
        let conn = forum();
        let forum = Forum::new(&conn);
        forum
            .publish("r", None, "grok", Kind::Finding, &"t".repeat(500), &"b".repeat(9000))
            .unwrap();
        let entries = forum.entries("r").unwrap();
        assert!(entries[0].title.len() <= MAX_TITLE + 4);
        assert!(entries[0].body.len() <= MAX_BODY + 4);
        assert!(forum.publish("r", None, "grok", Kind::Finding, "  ", "x").is_err());
    }

    #[test]
    fn distilling_keeps_what_an_agent_said_and_drops_bookkeeping() {
        let stream = concat!(
            r#"{"payload_type":"task.lifecycle.scheduled","payload":{"task_id":"8e37","kind":"scheduled"}}"#, "\n",
            r#"{"payload_type":"run.output.delta","payload":{"text":"Implemented align in src/columns.rs."}}"#, "\n",
            r#"{"payload_type":"task.lifecycle.status","payload":{"event":{"message":"opening model stream attempt 1/10"}}}"#, "\n",
            r#"{"payload_type":"task.lifecycle.scheduled","payload":{"task_id":"7084","kind":"scheduled"}}"#, "\n",
            "plain trailing line\n",
        );
        let distilled = distil_stream(stream, 4096);
        assert!(distilled.contains("Implemented align"), "keeps what the agent said");
        assert!(distilled.contains("opening model stream"), "keeps status messages");
        assert!(distilled.contains("plain trailing line"), "keeps non-JSON output");
        assert!(!distilled.contains("scheduled"), "drops lifecycle noise: {distilled}");
        assert!(distilled.len() < stream.len() / 2, "substantially smaller");

        // The budget is honoured even when every line is meaningful.
        let big: String = (0..500)
            .map(|i| format!("{{\"text\":\"line {i} of chatter\"}}\n"))
            .collect();
        assert!(distil_stream(&big, 300).len() <= 300);
    }

    #[test]
    fn observer_replies_are_parsed_leniently() {
        // JSONL with prose and a fenced block around it, which is what models actually do.
        let reply = "Here is what I found:\n```json\n\
            {\"kind\":\"dead_end\",\"title\":\"char_indices is wrong here\",\"body\":\"it splits graphemes\"}\n\
            not json at all\n\
            {\"kind\":\"nonsense\",\"title\":\"still kept\",\"body\":\"\"}\n```";
        let drafts = parse_drafts(reply);
        assert_eq!(drafts.len(), 2, "junk lines are skipped, valid ones kept");
        assert_eq!(drafts[0].title, "char_indices is wrong here");
        assert!(Kind::parse(&drafts[0].kind).is_ok());
        assert!(Kind::parse(&drafts[1].kind).is_err(), "caller decides on a bad kind");

        // A single array is also accepted.
        let array = "[{\"kind\":\"finding\",\"title\":\"one\",\"body\":\"b\"}]";
        assert_eq!(parse_drafts(array).len(), 1);
        assert!(parse_drafts("no entries here").is_empty());
    }
}
