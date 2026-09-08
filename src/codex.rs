use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::{Mutex, broadcast, oneshot},
    time::timeout,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Writer = futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

#[derive(Clone)]
pub struct Codex {
    writer: Arc<Mutex<Writer>>,
    pending: Pending,
    next: Arc<AtomicU64>,
    pub events: broadcast::Sender<Value>,
}

impl Codex {
    pub async fn connect(url: &str) -> Result<Self> {
        let (ws, _) = timeout(Duration::from_secs(10), connect_async(url)).await??;
        let (writer, mut reader) = ws.split();
        let writer = Arc::new(Mutex::new(writer));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(2048);
        let rpc = Self {
            writer: writer.clone(),
            pending: pending.clone(),
            next: Arc::new(AtomicU64::new(1)),
            events: events.clone(),
        };
        tokio::spawn(async move {
            while let Some(message) = reader.next().await {
                let Ok(message) = message else {
                    break;
                };
                if let Message::Ping(data) = message {
                    if writer.lock().await.send(Message::Pong(data)).await.is_err() {
                        break;
                    }
                    continue;
                }
                let Message::Text(text) = message else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if v.get("method").is_some() {
                    if let Some(id) = v.get("id") {
                        // Unattended controller never grants new permissions or answers for Len.
                        let response = json!({"id":id,"error":{"code":-32601,"message":"Firm requires human control for interactive requests"}});
                        let _ = writer
                            .lock()
                            .await
                            .send(Message::Text(response.to_string().into()))
                            .await;
                    }
                    let _ = events.send(v);
                } else if let Some(id) = v.get("id").and_then(Value::as_u64)
                    && let Some(tx) = pending.lock().await.remove(&id)
                {
                    let result = if let Some(error) = v.get("error") {
                        Err(anyhow!("Codex request failed: {}", error))
                    } else {
                        Ok(v["result"].clone())
                    };
                    let _ = tx.send(result);
                }
            }
            for (_, tx) in pending.lock().await.drain() {
                let _ = tx.send(Err(anyhow!("Codex app-server disconnected")));
            }
            let _ = events.send(json!({"method":"firm/disconnected"}));
        });
        rpc.call("initialize", json!({"clientInfo":{"name":"firm","title":"Firm experimental controller","version":"0.1.0"}})).await?;
        rpc.send(json!({"method":"initialized","params":{}}))
            .await?;
        Ok(rpc)
    }
    async fn send(&self, value: Value) -> Result<()> {
        self.writer
            .lock()
            .await
            .send(Message::Text(value.to_string().into()))
            .await?;
        Ok(())
    }
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if let Err(error) = self
            .send(json!({"id":id,"method":method,"params":params}))
            .await
        {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        let result = timeout(Duration::from_secs(20), rx).await;
        self.pending.lock().await.remove(&id);
        result
            .context("Codex RPC timed out; check server state before retrying")?
            .context("Codex RPC closed")?
    }
    pub async fn require_idle(&self, thread: &str) -> Result<()> {
        let result = self
            .call(
                "thread/read",
                json!({"threadId":thread,"includeTurns":false}),
            )
            .await?;
        match result
            .pointer("/thread/status/type")
            .and_then(Value::as_str)
        {
            Some("idle" | "notLoaded") => Ok(()),
            other => bail!(
                "Manager thread is not idle ({other:?}); take control and inspect it in Codex"
            ),
        }
    }
}

pub fn decision_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["action","summary","phases","assignment","observations"],"properties":{
        "action":{"type":"string","enum":["delegate","complete","blocked"]},
        "summary":{"type":"string"},
        "phases":{"type":"array","items":{"type":"string"}},
        "observations":{"type":"array","items":{"type":"string"}},
        "assignment":{"anyOf":[{"type":"null"},{"type":"object","additionalProperties":false,"required":["provider","title","brief","acceptance","autonomy"],"properties":{
            "provider":{"type":"string"},
            "title":{"type":"string"},"brief":{"type":"string"},"acceptance":{"type":"array","items":{"type":"string"}},"autonomy":{"type":"string","enum":["guided","bounded","exploratory"]}
        }}]}
    }})
}
