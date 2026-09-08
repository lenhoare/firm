//! Shared, bounded side discussions. No implementation assignments or automatic retries.
use crate::{
    app::App,
    state::{State, Store, now},
    worker,
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::watch;

pub const CONTEXT_BYTES: usize = 48 * 1024;
const EXCERPT_BYTES: usize = 4096;
const REPLY_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub round: String,
    pub speaker: String,
    pub status: String,
    pub text: String,
    pub time: u64,
    pub prompt: Option<String>,
    pub thread_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meeting {
    pub id: String,
    pub title: String,
    pub notes: String,
    pub created_at: u64,
    pub status: String,
    pub messages: Vec<Message>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub question: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub include_project: bool,
}

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS meetings (id TEXT PRIMARY KEY, created_at INTEGER NOT NULL, data TEXT NOT NULL);")?;
    let mut query = conn.prepare("SELECT data FROM meetings")?;
    let rows = query
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in rows {
        let mut meeting: Meeting = serde_json::from_str(&row)?;
        if meeting.status == "running" {
            meeting.status = "interrupted".into();
            for message in &mut meeting.messages {
                if ["queued", "running"].contains(&message.status.as_str()) {
                    message.status = "interrupted".into();
                    message.text = "Interrupted by controller restart; not retried. Inspect any uncertain Codex turn before continuing.".into();
                }
            }
            conn.execute(
                "UPDATE meetings SET data=?1 WHERE id=?2",
                params![serde_json::to_string(&meeting)?, meeting.id],
            )?;
        }
    }
    Ok(())
}
impl Store {
    pub fn meeting(&self, id: &str) -> Result<Meeting> {
        let text: String = self
            .conn
            .query_row("SELECT data FROM meetings WHERE id=?1", [id], |r| r.get(0))
            .context("Meeting not found")?;
        Ok(serde_json::from_str(&text)?)
    }
    pub fn save_meeting(&self, meeting: &Meeting) -> Result<()> {
        self.conn.execute("INSERT INTO meetings(id,created_at,data) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
            params![meeting.id, meeting.created_at, serde_json::to_string(meeting)?])?;
        Ok(())
    }
    pub fn meetings(&self) -> Result<Vec<Value>> {
        let mut query = self
            .conn
            .prepare("SELECT data FROM meetings ORDER BY created_at DESC, id")?;
        query.query_map([], |r| r.get::<_, String>(0))?.map(|row| {
            let m: Meeting = serde_json::from_str(&row?)?;
            Ok(json!({"id":m.id,"title":m.title,"created_at":m.created_at,"status":m.status,"messages":m.messages.len()}))
        }).collect()
    }
}

pub fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let marker = "\n[Excerpt truncated]";
    let mut end = limit.saturating_sub(marker.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &text[..end])
}

fn project_brief(state: &State) -> String {
    let mut text = format!(
        "Objective: {}\nStage: {}\nPhases: {:?}\n",
        clip(&state.objective, 1500),
        state.stage,
        state.phases
    );
    for task in state.tasks.iter().rev().take(3) {
        text.push_str(&format!(
            "\n{} ({}, {}): {}\n",
            task.assignment.title,
            task.assignment.provider,
            task.status,
            clip(&task.output, 650)
        ));
    }
    if let Some(d) = state.decisions.last() {
        text.push_str(&format!(
            "\nLatest manager review: {}\nDiscoveries: {:?}",
            d.summary, d.observations
        ));
    }
    clip(&text, EXCERPT_BYTES)
}

pub fn context(meeting: &Meeting, index: usize, brief: &str) -> Result<String> {
    let current = &meeting.messages[index];
    let render = |m: &Message| {
        format!(
            "\n--- {} [{}] ---\n{}\n",
            m.speaker,
            m.status,
            clip(&m.text, EXCERPT_BYTES)
        )
    };
    let prefix = format!(
        "{}\nYou are {}.\nPinned notes:\n{}\nProject brief (captured at round start):\n{}\n",
        include_str!("../prompts/meeting.md"),
        current.speaker,
        meeting.notes,
        brief
    );
    let this_round: String = meeting.messages[..index]
        .iter()
        .filter(|m| m.round == current.round)
        .map(render)
        .collect();
    let older: Vec<_> = meeting.messages[..index]
        .iter()
        .filter(|m| m.round != current.round)
        .collect();
    let mut retained = Vec::new();
    let mut used = prefix.len() + this_round.len() + 200;
    ensure!(
        used <= CONTEXT_BYTES,
        "Current round exceeds the meeting context budget"
    );
    for m in older.iter().rev().take(12) {
        let rendered = render(m);
        if used + rendered.len() > CONTEXT_BYTES {
            break;
        }
        used += rendered.len();
        retained.push(rendered);
    }
    let omitted = older.len() - retained.len();
    retained.reverse();
    Ok(format!(
        "{prefix}\nEarlier discussion ({omitted} older messages omitted):\n{}\nCurrent round — question and previous participants:\n{this_round}",
        retained.concat()
    ))
}

impl App {
    fn meeting_roster(&self, state: &State) -> Vec<Value> {
        let providers = state.providers(&self.config.providers);
        self.config.meeting_order.iter().map(|id| {
            match providers.iter().find(|p| &p.id == id) {
                Some(p) if id == "codex" && p.enabled => json!({"id":id,"name":p.name,"included":true}),
                Some(p) if p.enabled && p.meeting_args.is_some() => json!({"id":id,"name":p.name,"included":true}),
                Some(p) => json!({"id":id,"name":p.name,"included":false,"reason":if p.enabled { "No discussion-only CLI configuration" } else { "Provider disabled" }}),
                None => json!({"id":id,"name":id,"included":false,"reason":"Provider not configured"}),
            }
        }).collect()
    }
    fn meeting_gate(&self, state: &State) -> Option<String> {
        if !state.paused {
            return Some("Pause background work before starting a meeting round".into());
        }
        if state.active_turn.is_some() {
            return Some("Reconcile the uncertain Codex turn first".into());
        }
        let mut check = state.clone();
        check.paused = false;
        if let Some(reason) = check.gate(true, now()) {
            return Some(reason);
        }
        let roster = self.meeting_roster(state);
        let count = roster
            .iter()
            .filter(|p| p["included"] == true && p["id"] != "codex")
            .count();
        if state
            .recent(&state.worker_starts, now())
            .saturating_add(count)
            > state.allowances.worker_runs
        {
            return Some(format!(
                "This round needs {count} worker runs; the shared worker allowance is too small or exhausted"
            ));
        }
        for p in state.providers(&self.config.providers) {
            if roster
                .iter()
                .any(|r| r["id"] == p.id && r["included"] == true)
                && let Some(reason) = state.provider_gate(&p, now())
            {
                return Some(reason);
            }
        }
        None
    }
    pub async fn meeting_overview(&self) -> Result<Value> {
        let core = self.core.lock().await;
        Ok(
            json!({"meetings":core.store.meetings()?,"active":core.meeting_active,"demo":core.state.demo,
            "roster":self.meeting_roster(&core.state),"blocked":if core.active { Some("Wait for active work to finish".into()) } else { self.meeting_gate(&core.state) },
            "uncertain_turn":core.state.active_turn,"context_bytes":CONTEXT_BYTES}),
        )
    }
    pub async fn new_meeting(&self, title: String) -> Result<String> {
        ensure!(
            !title.trim().is_empty() && title.len() <= 200,
            "Meeting title must be 1–200 bytes"
        );
        let core = self.core.lock().await;
        ensure!(!core.active, "Wait for active work to finish");
        ensure!(
            core.store.meetings()?.len() < 20,
            "This prototype retains at most 20 meetings"
        );
        let meeting = Meeting {
            id: uuid::Uuid::new_v4().to_string(),
            title,
            notes: String::new(),
            created_at: now(),
            status: "idle".into(),
            messages: vec![],
        };
        core.store.save_meeting(&meeting)?;
        Ok(meeting.id)
    }
    pub async fn ask_meeting(&self, id: &str, input: Question) -> Result<()> {
        ensure!(
            !input.question.trim().is_empty()
                && input.question.len() <= 4000
                && input.notes.len() <= 4000,
            "Question and pinned notes must each fit in 4000 bytes"
        );
        self.refresh_usage().await;
        let (meeting, start, brief, cancel) = {
            let mut core = self.core.lock().await;
            ensure!(!core.active, "Wait for active work to finish");
            if let Some(reason) = self.meeting_gate(&core.state) {
                bail!(reason);
            }
            let mut meeting = core.store.meeting(id)?;
            ensure!(meeting.status != "running", "Meeting is already running");
            ensure!(
                meeting.messages.len() + self.config.meeting_order.len() < 100,
                "Start a new meeting after 100 messages"
            );
            let round = uuid::Uuid::new_v4().to_string();
            meeting.notes = input.notes;
            meeting.status = "running".into();
            meeting.messages.push(Message {
                id: uuid::Uuid::new_v4().to_string(),
                round: round.clone(),
                speaker: "Len".into(),
                status: "sent".into(),
                text: input.question,
                time: now(),
                prompt: None,
                thread_id: None,
            });
            let start = meeting.messages.len();
            for p in self.meeting_roster(&core.state) {
                meeting.messages.push(Message {
                    id: uuid::Uuid::new_v4().to_string(),
                    round: round.clone(),
                    speaker: p["id"].as_str().unwrap().into(),
                    status: if p["included"] == true {
                        "queued"
                    } else {
                        "skipped"
                    }
                    .into(),
                    text: p["reason"].as_str().unwrap_or("").into(),
                    time: now(),
                    prompt: None,
                    thread_id: None,
                });
            }
            let brief = if input.include_project {
                project_brief(&core.state)
            } else {
                "Project context not included by Len.".into()
            };
            core.store.save_meeting(&meeting)?;
            core.state.event(
                "meeting",
                format!("Meeting round queued: {}", meeting.title),
            );
            core.save()?;
            core.active = true;
            core.meeting_active = Some(id.into());
            (meeting, start, brief, self.cancel.subscribe())
        };
        let app = self.clone();
        tokio::spawn(async move {
            let mut meeting = meeting;
            let result = app.meeting_round(&mut meeting, start, &brief, cancel).await;
            let mut core = app.core.lock().await;
            meeting.status = if result.is_ok() {
                "complete"
            } else {
                "interrupted"
            }
            .into();
            if let Err(error) = result {
                for m in &mut meeting.messages[start..] {
                    if ["queued", "running"].contains(&m.status.as_str()) {
                        m.status = "interrupted".into();
                        m.text = error.to_string();
                    }
                }
                core.state
                    .event("meeting", format!("Meeting interrupted: {error}"));
            }
            if let Err(error) = core.store.save_meeting(&meeting) {
                core.state
                    .event("error", format!("Meeting persistence failed: {error}"));
            }
            if core.state.active_turn.is_none() {
                core.state.side_thread_id = None;
            }
            core.state.event(
                "meeting",
                format!("Meeting {}: {}", meeting.title, meeting.status),
            );
            let _ = core.save();
            core.meeting_active = None;
            core.active = false;
        });
        Ok(())
    }
    pub async fn stop_meeting(&self, id: &str) -> Result<()> {
        let core = self.core.lock().await;
        ensure!(
            core.meeting_active.as_deref() == Some(id),
            "This meeting is not active"
        );
        self.cancel.send_modify(|v| *v += 1);
        Ok(())
    }
    async fn meeting_round(
        &self,
        meeting: &mut Meeting,
        start: usize,
        brief: &str,
        cancel: watch::Receiver<u64>,
    ) -> Result<()> {
        for index in start..meeting.messages.len() {
            ensure!(
                !cancel.has_changed()?,
                "Meeting stopped; no later participant was started"
            );
            if meeting.messages[index].status == "skipped" {
                continue;
            }
            let speaker = meeting.messages[index].speaker.clone();
            let prompt = context(meeting, index, brief)?;
            meeting.messages[index].prompt = Some(prompt.clone());
            if speaker == "codex" {
                self.refresh_usage().await;
            }
            let snapshot = {
                let mut core = self.core.lock().await;
                let mut check = core.state.clone();
                check.paused = false;
                if let Some(reason) = check.gate(speaker == "codex", now()) {
                    bail!(reason);
                }
                if speaker == "codex" {
                    core.state.manager_starts.push(now());
                } else {
                    let providers = core.state.providers(&self.config.providers);
                    let p = providers
                        .iter()
                        .find(|p| p.id == speaker)
                        .context("Meeting provider removed")?;
                    if let Some(reason) = core.state.provider_gate(p, now()) {
                        bail!(reason);
                    }
                    core.state.worker_starts.push(now());
                }
                core.state
                    .provider_usage
                    .entry(speaker.clone())
                    .or_default()
                    .starts
                    .push(now());
                core.state
                    .event("meeting", format!("Reserved {speaker} meeting reply"));
                core.save()?; // Shared, durable budget reservation before any external call.
                meeting.messages[index].status = "running".into();
                core.store.save_meeting(meeting)?;
                core.state.clone()
            };
            let (result, thread) = self
                .meeting_reply(&snapshot, &speaker, &prompt, cancel.clone())
                .await?;
            let message = &mut meeting.messages[index];
            message.thread_id = thread;
            match result {
                Ok(text) => {
                    message.text = clip(&text, REPLY_BYTES);
                    message.status = "answered".into();
                }
                Err(error) => {
                    message.text = error.to_string();
                    message.status = "failed".into();
                }
            }
            {
                let mut core = self.core.lock().await;
                if worker::looks_rate_limited(&message.text) {
                    let until =
                        now().saturating_add(core.state.allowances.provider_cooldown_seconds);
                    core.state
                        .provider_usage
                        .entry(speaker.clone())
                        .or_default()
                        .cooldown_until = until;
                }
                core.store.save_meeting(meeting)?;
                core.save()?;
            }
            self.capture_record(
                "result",
                "Meeting reply",
                json!({"meeting_id":meeting.id,"message":meeting.messages[index]}),
            )
            .await?;
            ensure!(
                self.core.lock().await.state.active_turn.is_none(),
                "Codex turn uncertain; reconcile before continuing"
            );
        }
        Ok(())
    }
    async fn meeting_reply(
        &self,
        snapshot: &State,
        speaker: &str,
        prompt: &str,
        mut cancel: watch::Receiver<u64>,
    ) -> Result<(Result<String>, Option<String>)> {
        // Empty working directory avoids loading project hooks/rules or changing the worktree.
        // CLI permission modes still matter; this is not an OS isolation boundary for custom CLIs.
        let workspace = tempfile::Builder::new().prefix("firm-meeting-").tempdir()?;
        let mut config = self.config.clone();
        config.workspace = workspace.path().into();
        config.allowances = snapshot.allowances.clone();
        if snapshot.demo {
            self.capture_record(
                "input",
                "Demo meeting input",
                json!({"demo":true,"speaker":speaker,"prompt":prompt}),
            )
            .await?;
            tokio::select! { _ = cancel.changed() => bail!("Meeting cancelled"), _ = tokio::time::sleep(Duration::from_millis(250)) => {} }
            return Ok((
                Ok(format!(
                    "DEMO · {speaker}\nI received the shared question and {} earlier participant replies in this round. {} No live model call was made.",
                    prompt
                        .split("Current round —")
                        .last()
                        .unwrap_or("")
                        .matches("[answered]")
                        .count(),
                    if speaker == "codex" {
                        "Synthesis: compare the suggestions with evidence before implementing anything."
                    } else {
                        "My suggestion: try one small experiment and record what we learn."
                    }
                )),
                None,
            ));
        }
        if speaker == "codex" {
            ensure!(
                !cancel.has_changed()?,
                "Meeting cancelled before Codex start"
            );
            let rpc = self.rpc.as_ref().context("Codex connection missing")?;
            let response = rpc.call("thread/start", json!({"model":config.codex_model,"cwd":config.workspace,"approvalPolicy":"never","sandbox":"read-only","developerInstructions":include_str!("../prompts/meeting.md")})).await?;
            let thread = response
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .context("Missing meeting thread ID")?
                .to_string();
            {
                let mut core = self.core.lock().await;
                core.state.side_thread_id = Some(thread.clone());
                core.save()?;
            }
            let params = json!({"threadId":thread,"model":config.codex_model,"input":[{"type":"text","text":prompt}],"approvalPolicy":"never","sandboxPolicy":{"type":"readOnly"}});
            return Ok((
                self.codex_turn(snapshot, thread.clone(), params, cancel)
                    .await,
                Some(thread),
            ));
        }
        let mut provider = snapshot
            .providers(&config.providers)
            .into_iter()
            .find(|p| p.id == speaker)
            .context("Meeting provider missing")?;
        provider.args = provider
            .meeting_args
            .clone()
            .context("No discussion-only CLI arguments")?;
        let prepared = worker::prepare_prompt(&config, &provider, prompt.into())?;
        self.capture_record(
            "input",
            "Meeting CLI input",
            prepared.request(&config, false),
        )
        .await?;
        let result = worker::run(&config, &prepared, cancel)
            .await
            .and_then(|result| {
                if let Some(reason) = result.interruption {
                    bail!("{reason}\n{}", clip(&result.output, REPLY_BYTES));
                }
                ensure!(
                    result.exit_code == Some(0),
                    "CLI exited {:?}: {}",
                    result.exit_code,
                    clip(&result.output, REPLY_BYTES)
                );
                ensure!(!result.output.trim().is_empty(), "CLI returned no reply");
                let reply = result
                    .output
                    .split("\n\nWorker stderr:\n")
                    .next()
                    .unwrap_or("")
                    .trim();
                ensure!(!reply.is_empty(), "CLI returned no reply");
                Ok(reply.into())
            });
        Ok((result, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ProviderSettings;
    #[tokio::test]
    async fn codex_meeting_uses_separate_readonly_thread_and_plain_response() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::{accept_async, tungstenite::Message as WsMessage};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(socket).await.unwrap();
            while let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                if request.get("id").is_none() {
                    continue;
                }
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => json!({}),
                    "thread/start" => {
                        assert_eq!(request["params"]["sandbox"], "read-only");
                        assert_eq!(request["params"]["approvalPolicy"], "never");
                        assert!(
                            request["params"]["cwd"]
                                .as_str()
                                .unwrap()
                                .contains("firm-meeting-")
                        );
                        assert!(
                            request["params"]["developerInstructions"]
                                .as_str()
                                .unwrap()
                                .contains("not an implementation worker")
                        );
                        json!({"thread":{"id":"meeting-thread"}})
                    }
                    "thread/read" => {
                        assert_eq!(request["params"]["threadId"], "meeting-thread");
                        json!({"thread":{"id":"meeting-thread","turns":[],"status":{"type":"idle"}}})
                    }
                    "turn/start" => {
                        assert!(request["params"].get("outputSchema").is_none());
                        assert_eq!(request["params"]["threadId"], "meeting-thread");
                        for event in [
                            json!({"method":"item/completed","params":{"threadId":"meeting-thread","turnId":"meeting-turn","item":{"type":"agentMessage","phase":"final_answer","text":"An independent view, then a synthesis."}}}),
                            json!({"method":"turn/completed","params":{"threadId":"meeting-thread","turn":{"id":"meeting-turn","status":"completed"}}}),
                        ] {
                            ws.send(WsMessage::Text(event.to_string().into()))
                                .await
                                .unwrap();
                        }
                        json!({"turn":{"id":"meeting-turn"}})
                    }
                    method => panic!("Unexpected method {method}"),
                };
                ws.send(WsMessage::Text(
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
            crate::codex::Codex::connect(&format!("ws://{addr}"))
                .await
                .unwrap(),
        );
        let snapshot = {
            let mut core = app.core.lock().await;
            core.state.demo = false;
            core.state.thread_id = Some("implementation-thread".into());
            core.state.clone()
        };
        let (reply, thread) = app
            .meeting_reply(
                &snapshot,
                "codex",
                "Shared meeting prompt",
                app.cancel.subscribe(),
            )
            .await
            .unwrap();
        assert_eq!(reply.unwrap(), "An independent view, then a synthesis.");
        assert_eq!(thread.as_deref(), Some("meeting-thread"));
        let core = app.core.lock().await;
        assert_eq!(
            core.state.thread_id.as_deref(),
            Some("implementation-thread")
        );
        assert!(core.state.active_turn.is_none());
        server.abort();
    }
    async fn finished(app: &App) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while app.core.lock().await.active {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }
    fn question(text: &str) -> Question {
        Question {
            question: text.into(),
            notes: "Preserve interactive capacity".into(),
            include_project: true,
        }
    }
    #[tokio::test]
    async fn ordered_round_shares_context_persists_and_keeps_work_separate() {
        let (dir, app) = crate::app::tests::fixture();
        let id = app.new_meeting("Architecture".into()).await.unwrap();
        app.ask_meeting(&id, question("What have we missed?"))
            .await
            .unwrap();
        assert!(app.ask_meeting(&id, question("duplicate")).await.is_err());
        assert!(app.resume().await.is_err());
        finished(&app).await;
        let meeting = app.core.lock().await.store.meeting(&id).unwrap();
        assert_eq!(meeting.status, "complete");
        assert_eq!(
            meeting
                .messages
                .iter()
                .map(|m| m.speaker.as_str())
                .collect::<Vec<_>>(),
            ["Len", "grok", "muse", "qwen", "codex"]
        );
        for (index, message) in meeting.messages.iter().enumerate().skip(1) {
            assert_eq!(message.status, "answered");
            let prompt = message.prompt.as_ref().unwrap();
            assert!(prompt.contains("What have we missed?"));
            assert!(prompt.contains("Preserve interactive capacity"));
            assert_eq!(prompt.matches("[answered]").count(), index - 1);
            assert!(prompt.len() <= CONTEXT_BYTES);
        }
        {
            let core = app.core.lock().await;
            assert_eq!(core.state.stage, "idle");
            assert!(
                core.state.paused && core.state.tasks.is_empty() && core.state.thread_id.is_none()
            );
            assert_eq!(core.state.worker_starts.len(), 3);
            assert_eq!(core.state.manager_starts.len(), 1);
            assert_eq!(core.state.provider_usage["codex"].starts.len(), 1);
        }
        assert!(
            app.ask_meeting(&id, question("Another round"))
                .await
                .unwrap_err()
                .to_string()
                .contains("allowance")
        );
        assert_eq!(
            app.core
                .lock()
                .await
                .store
                .meeting(&id)
                .unwrap()
                .messages
                .len(),
            5
        );
        let (store, state) = Store::open(
            &dir.path().join("test.db"),
            true,
            app.config.allowances.clone(),
        )
        .unwrap();
        assert_eq!(store.meeting(&id).unwrap().messages.len(), 5);
        assert_eq!(state.worker_starts.len(), 3);
        let records = app.archive_action(|archive| archive.list()).await.unwrap();
        assert_eq!(records.iter().filter(|r| r["kind"] == "input").count(), 4);
        assert_eq!(records.iter().filter(|r| r["kind"] == "result").count(), 4);
    }
    #[tokio::test]
    async fn disabled_providers_are_skipped_and_followups_see_previous_round() {
        let (_dir, app) = crate::app::tests::fixture();
        for id in ["muse", "qwen"] {
            app.provider_settings(
                id,
                ProviderSettings {
                    enabled: false,
                    max_runs: 3,
                },
            )
            .await
            .unwrap();
        }
        let id = app.new_meeting("Followup".into()).await.unwrap();
        app.ask_meeting(&id, question("First question"))
            .await
            .unwrap();
        finished(&app).await;
        app.ask_meeting(&id, question("Second question"))
            .await
            .unwrap();
        finished(&app).await;
        let core = app.core.lock().await;
        let meeting = core.store.meeting(&id).unwrap();
        assert_eq!(meeting.messages[2].status, "skipped");
        assert_eq!(meeting.messages[3].status, "skipped");
        assert!(
            meeting.messages[6]
                .prompt
                .as_ref()
                .unwrap()
                .contains("First question")
        );
        assert_eq!(core.state.worker_starts.len(), 2);
        assert!(!core.state.provider_usage.contains_key("muse"));
    }
    #[tokio::test]
    async fn meeting_stop_does_not_cancel_the_workshop_and_restart_does_not_retry() {
        let (dir, app) = crate::app::tests::fixture();
        let id = app.new_meeting("Stop".into()).await.unwrap();
        app.ask_meeting(&id, question("Question")).await.unwrap();
        app.stop_meeting(&id).await.unwrap();
        finished(&app).await;
        let mut meeting = app.core.lock().await.store.meeting(&id).unwrap();
        assert_eq!(meeting.status, "interrupted");
        assert_eq!(app.core.lock().await.state.stage, "idle");
        assert!(app.core.lock().await.state.worker_starts.len() <= 1);
        meeting.status = "running".into();
        meeting.messages[1].status = "running".into();
        app.core.lock().await.store.save_meeting(&meeting).unwrap();
        let (store, _) = Store::open(
            &dir.path().join("test.db"),
            true,
            app.config.allowances.clone(),
        )
        .unwrap();
        assert_eq!(
            store.meeting(&id).unwrap().messages[1].status,
            "interrupted"
        );
    }
    #[test]
    fn long_unicode_context_is_bounded_and_current_round_is_kept() {
        let mut meeting = Meeting {
            id: "test".into(),
            title: "Test".into(),
            notes: "Pinned facts".into(),
            created_at: 0,
            status: "running".into(),
            messages: vec![],
        };
        for i in 0..80 {
            meeting.messages.push(Message {
                id: i.to_string(),
                round: if i < 76 { "old" } else { "current" }.into(),
                speaker: if i == 76 { "Len" } else { "agent" }.into(),
                status: "answered".into(),
                text: "🙂".repeat(10000),
                time: 0,
                prompt: None,
                thread_id: None,
            });
        }
        meeting.messages[76].text = "CURRENT QUESTION".into();
        let prompt = context(&meeting, 79, "Project brief").unwrap();
        assert!(prompt.len() <= CONTEXT_BYTES);
        assert!(
            prompt.contains("CURRENT QUESTION")
                && prompt.contains("Pinned facts")
                && prompt.contains("older messages omitted")
                && prompt.contains("Excerpt truncated")
        );
        assert!(clip("🙂".repeat(100).as_str(), 50).len() <= 50);
    }
}
