#![deny(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("browser connection failed: {0}")]
    Connection(String),
    #[error("browser command channel closed")]
    Closed,
    #[error("browser response was invalid: {0}")]
    InvalidResponse(String),
    #[error("browser command cancelled")]
    Cancelled,
}

#[derive(Debug)]
struct OutgoingCommand {
    id: u64,
    session_id: Option<String>,
    method: String,
    params: Value,
    response: oneshot::Sender<Result<Value, BrowserError>>,
}

type PendingCommands = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, BrowserError>>>>>;

/// One browser-level WebSocket with a dedicated writer and reader.
/// Commands are correlated by id and never hold a global lock during I/O.
#[derive(Clone)]
pub struct BrowserConnection {
    outgoing: mpsc::Sender<OutgoingCommand>,
    pub targets: Arc<RwLock<TargetGraph>>,
    pub frames: Arc<RwLock<FrameGraph>>,
    next_command_id: Arc<AtomicU64>,
    generation: Arc<AtomicU64>,
    cancellation: CancellationToken,
}

impl BrowserConnection {
    pub async fn connect(url: &str) -> Result<Self, BrowserError> {
        use futures_util::{SinkExt, StreamExt};
        let (socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|error| BrowserError::Connection(error.to_string()))?;
        let (mut writer, mut reader) = socket.split();
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<OutgoingCommand>(128);
        let pending: PendingCommands = Arc::new(Mutex::new(HashMap::new()));
        let pending_for_reader = Arc::clone(&pending);
        let targets = Arc::new(RwLock::new(TargetGraph::default()));
        let frames = Arc::new(RwLock::new(FrameGraph::default()));
        let targets_for_reader = Arc::clone(&targets);
        let frames_for_reader = Arc::clone(&frames);
        let generation_for_reader = Arc::new(AtomicU64::new(0));
        let generation_for_disconnect = Arc::clone(&generation_for_reader);
        let cancellation = CancellationToken::new();
        let cancellation_for_tasks = cancellation.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancellation_for_tasks.cancelled() => break,
                    outgoing = outgoing_rx.recv() => {
                        let Some(command) = outgoing else { break };
                        let mut message = serde_json::json!({
                            "id": command.id,
                            "method": command.method,
                            "params": command.params,
                        });
                        if let Some(session_id) = command.session_id {
                            message["sessionId"] = Value::String(session_id);
                        }
                        pending_for_reader.lock().await.insert(command.id, command.response);
                        if writer.send(tokio_tungstenite::tungstenite::Message::Text(message.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let pending_for_responses = Arc::clone(&pending);
        let cancellation_for_reader = cancellation.clone();
        tokio::spawn(async move {
            while let Some(Ok(message)) = reader.next().await {
                let text = match message {
                    tokio_tungstenite::tungstenite::Message::Text(text) => text,
                    tokio_tungstenite::tungstenite::Message::Binary(bytes) => {
                        match String::from_utf8(bytes.to_vec()) {
                            Ok(text) => text.into(),
                            Err(_) => continue,
                        }
                    }
                    _ => continue,
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let Some(id) = value.get("id").and_then(Value::as_u64) else {
                    targets_for_reader.write().await.apply_event(&value);
                    frames_for_reader.write().await.apply_event(&value);
                    continue;
                };
                if let Some(sender) = pending_for_responses.lock().await.remove(&id) {
                    let result = if let Some(error) = value.get("error") {
                        Err(BrowserError::InvalidResponse(error.to_string()))
                    } else {
                        Ok(value.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = sender.send(result);
                }
            }
            cancellation_for_reader.cancel();
            let _generation = generation_for_disconnect.fetch_add(1, Ordering::AcqRel) + 1;
            targets_for_reader.write().await.reconnect();
            frames_for_reader.write().await.frames.clear();
            let mut pending = pending_for_responses.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(BrowserError::Closed));
            }
        });
        Ok(Self {
            outgoing: outgoing_tx,
            targets,
            frames,
            next_command_id: Arc::new(AtomicU64::new(1)),
            generation: generation_for_reader,
            cancellation,
        })
    }

    pub async fn command(
        &self,
        session_id: Option<String>,
        method: impl Into<String>,
        params: Value,
    ) -> Result<Value, BrowserError> {
        if self.cancellation.is_cancelled() {
            return Err(BrowserError::Closed);
        }
        let id = self.next_command_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        self.outgoing
            .send(OutgoingCommand {
                id,
                session_id,
                method: method.into(),
                params,
                response: response_tx,
            })
            .await
            .map_err(|_| BrowserError::Closed)?;
        response_rx.await.map_err(|_| BrowserError::Cancelled)?
    }

    pub async fn bootstrap(&self) -> Result<(), BrowserError> {
        self.command(
            None,
            "Target.setDiscoverTargets",
            serde_json::json!({"discover": true}),
        )
        .await?;
        self.command(
            None,
            "Target.setAutoAttach",
            serde_json::json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": false,
                "flatten": true
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn bootstrap_target(
        &self,
        session_id: impl Into<String>,
    ) -> Result<(), BrowserError> {
        let session_id = session_id.into();
        for method in [
            "Page.enable",
            "Runtime.enable",
            "DOM.enable",
            "Network.enable",
            "Accessibility.enable",
        ] {
            self.command(Some(session_id.clone()), method, Value::Null)
                .await?;
        }
        Ok(())
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    pub async fn reconnect_generation(&self) -> u64 {
        let next = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.targets.write().await.reconnect();
        next
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TargetRecord {
    pub id: String,
    pub target_type: String,
    pub browser_context_id: Option<String>,
    pub session_id: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub opener_id: Option<String>,
    pub attached: bool,
    pub generation: u64,
    pub revision: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct TargetGraph {
    pub generation: u64,
    pub targets: BTreeMap<String, TargetRecord>,
}

impl TargetGraph {
    pub fn apply_created(&mut self, target: TargetRecord) {
        self.targets.insert(target.id.clone(), target);
    }

    pub fn apply_changed(&mut self, id: &str, url: Option<String>, title: Option<String>) {
        if let Some(target) = self.targets.get_mut(id) {
            target.url = url;
            target.title = title;
            target.revision = format!("generation:{}:target:{}", self.generation, id);
        }
    }

    pub fn apply_destroyed(&mut self, id: &str) {
        self.targets.remove(id);
    }

    pub fn reconnect(&mut self) {
        self.generation = self.generation.saturating_add(1);
        for target in self.targets.values_mut() {
            target.attached = false;
            target.session_id = None;
            target.generation = self.generation;
            target.revision = format!("generation:{}:target:{}", self.generation, target.id);
        }
    }

    pub fn apply_event(&mut self, event: &Value) {
        match event.get("method").and_then(Value::as_str) {
            Some("Target.targetCreated") => {
                if let Some(info) = event
                    .get("params")
                    .and_then(|params| params.get("targetInfo"))
                {
                    let id = info
                        .get("targetId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !id.is_empty() {
                        self.apply_created(TargetRecord {
                            id: id.to_owned(),
                            target_type: info
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            browser_context_id: info
                                .get("browserContextId")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            session_id: None,
                            url: info.get("url").and_then(Value::as_str).map(str::to_owned),
                            title: info.get("title").and_then(Value::as_str).map(str::to_owned),
                            opener_id: info
                                .get("openerId")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            attached: false,
                            generation: self.generation,
                            revision: format!("generation:{}:target:{}", self.generation, id),
                        });
                    }
                }
            }
            Some("Target.targetInfoChanged") => {
                if let Some(info) = event
                    .get("params")
                    .and_then(|params| params.get("targetInfo"))
                {
                    let id = info
                        .get("targetId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !id.is_empty() {
                        self.apply_changed(
                            id,
                            info.get("url").and_then(Value::as_str).map(str::to_owned),
                            info.get("title").and_then(Value::as_str).map(str::to_owned),
                        );
                    }
                }
            }
            Some("Target.targetDestroyed") => {
                if let Some(id) = event
                    .get("params")
                    .and_then(|params| params.get("targetId"))
                    .and_then(Value::as_str)
                {
                    self.apply_destroyed(id);
                }
            }
            Some("Target.attachedToTarget") => {
                if let Some(params) = event.get("params") {
                    let target_id = params
                        .get("targetInfo")
                        .and_then(|info| info.get("targetId"))
                        .and_then(Value::as_str);
                    let target_id =
                        target_id.or_else(|| params.get("targetId").and_then(Value::as_str));
                    if let (Some(target_id), Some(session_id)) =
                        (target_id, params.get("sessionId").and_then(Value::as_str))
                        && let Some(target) = self.targets.get_mut(target_id)
                    {
                        target.session_id = Some(session_id.to_owned());
                        target.attached = true;
                        target.generation = self.generation;
                        target.revision =
                            format!("generation:{}:target:{}", self.generation, target_id);
                    }
                }
            }
            Some("Target.detachedFromTarget") => {
                if let Some(params) = event.get("params") {
                    let target_id = params.get("targetId").and_then(Value::as_str);
                    if let Some(target) = target_id.and_then(|id| self.targets.get_mut(id)) {
                        target.session_id = None;
                        target.attached = false;
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FrameRecord {
    pub id: String,
    pub parent_id: Option<String>,
    pub target_id: String,
    pub loader_id: Option<String>,
    pub execution_context_ids: Vec<u64>,
    pub generation: u64,
    pub revision: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct FrameGraph {
    pub frames: BTreeMap<String, FrameRecord>,
}

impl FrameGraph {
    pub fn upsert(&mut self, frame: FrameRecord) {
        self.frames.insert(frame.id.clone(), frame);
    }

    pub fn remove(&mut self, id: &str) {
        self.frames
            .retain(|frame_id, frame| frame_id != id && frame.parent_id.as_deref() != Some(id));
    }

    pub fn apply_event(&mut self, event: &Value) {
        match event.get("method").and_then(Value::as_str) {
            Some("Page.frameAttached") => {
                if let Some(params) = event.get("params")
                    && let Some(id) = params.get("frameId").and_then(Value::as_str)
                {
                    self.upsert(FrameRecord {
                        id: id.to_owned(),
                        parent_id: params
                            .get("parentFrameId")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        target_id: params
                            .get("targetId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        loader_id: None,
                        execution_context_ids: Vec::new(),
                        generation: 0,
                        revision: 0,
                    });
                }
            }
            Some("Page.frameDetached") => {
                if let Some(id) = event
                    .get("params")
                    .and_then(|params| params.get("frameId"))
                    .and_then(Value::as_str)
                {
                    self.remove(id);
                }
            }
            Some("Runtime.executionContextCreated") => {
                if let Some(context) = event.get("params").and_then(|params| params.get("context"))
                    && let (Some(frame_id), Some(context_id)) = (
                        context
                            .get("auxData")
                            .and_then(|data| data.get("frameId"))
                            .and_then(Value::as_str),
                        context.get("id").and_then(Value::as_u64),
                    )
                    && let Some(frame) = self.frames.get_mut(frame_id)
                    && !frame.execution_context_ids.contains(&context_id)
                {
                    frame.execution_context_ids.push(context_id);
                    frame.revision = frame.revision.saturating_add(1);
                }
            }
            Some("Runtime.executionContextDestroyed") => {
                if let Some(context_id) = event
                    .get("params")
                    .and_then(|params| params.get("executionContextId"))
                    .and_then(Value::as_u64)
                {
                    for frame in self.frames.values_mut() {
                        frame.execution_context_ids.retain(|id| *id != context_id);
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BrowserCommand {
    pub id: u64,
    pub session_id: Option<String>,
    pub method: String,
    pub params: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;

    fn target() -> TargetRecord {
        TargetRecord {
            id: "tab".to_owned(),
            target_type: "page".to_owned(),
            browser_context_id: Some("default".to_owned()),
            session_id: Some("session".to_owned()),
            url: Some("https://example.test".to_owned()),
            title: Some("Example".to_owned()),
            opener_id: None,
            attached: true,
            generation: 0,
            revision: "generation:0:target:tab".to_owned(),
        }
    }

    #[test]
    fn reconnect_invalidates_sessions_and_increments_generation() {
        let mut graph = TargetGraph::default();
        graph.apply_created(target());
        graph.reconnect();
        let target = graph.targets.get("tab").unwrap();
        assert_eq!(graph.generation, 1);
        assert_eq!(target.session_id, None);
        assert!(!target.attached);
        assert_eq!(target.generation, 1);
    }

    #[tokio::test]
    async fn multiplexes_out_of_order_responses_on_one_socket() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut requests = Vec::new();
            while requests.len() < 2 {
                if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) =
                    socket.next().await
                {
                    requests.push(serde_json::from_str::<Value>(&text).unwrap());
                }
            }
            socket.send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::json!({"method":"Target.targetCreated","params":{"targetInfo":{"targetId":"event-tab","type":"page","url":"https://event.test","title":"Event tab"}}}).to_string().into()
            )).await.unwrap();
            for request in requests.into_iter().rev() {
                socket.send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::json!({"id": request["id"], "result": {"method": request["method"]}}).to_string().into()
                )).await.unwrap();
            }
        });
        let connection = BrowserConnection::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        let first = connection.command(Some("session-a".to_owned()), "Runtime.enable", Value::Null);
        let second = connection.command(Some("session-b".to_owned()), "Page.enable", Value::Null);
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.unwrap()["method"], "Runtime.enable");
        assert_eq!(second.unwrap()["method"], "Page.enable");
        let targets = connection.targets.read().await;
        assert_eq!(
            targets.targets["event-tab"].title.as_deref(),
            Some("Event tab")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn bootstrap_enables_discovery_and_flattened_auto_attach() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut methods = Vec::new();
            while methods.len() < 2 {
                if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) =
                    socket.next().await
                {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    methods.push(request["method"].as_str().unwrap().to_owned());
                    socket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            serde_json::json!({"id":request["id"],"result":{}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
            }
            methods
        });
        let connection = BrowserConnection::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        connection.bootstrap().await.unwrap();
        assert_eq!(
            server.await.unwrap(),
            vec!["Target.setDiscoverTargets", "Target.setAutoAttach"]
        );
    }

    #[tokio::test]
    async fn target_bootstrap_enables_required_domains_on_flattened_session() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut methods = Vec::new();
            while methods.len() < 5 {
                if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) =
                    socket.next().await
                {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(request["sessionId"], "session-1");
                    methods.push(request["method"].as_str().unwrap().to_owned());
                    socket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            serde_json::json!({"id":request["id"],"result":{}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
            }
            methods
        });
        let connection = BrowserConnection::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        connection.bootstrap_target("session-1").await.unwrap();
        assert_eq!(
            server.await.unwrap(),
            vec![
                "Page.enable",
                "Runtime.enable",
                "DOM.enable",
                "Network.enable",
                "Accessibility.enable"
            ]
        );
    }

    #[test]
    fn applies_target_and_frame_lifecycle_events() {
        let mut targets = TargetGraph::default();
        targets.apply_event(&serde_json::json!({"method":"Target.targetCreated","params":{"targetInfo":{"targetId":"tab-1","type":"page","url":"https://example.test","title":"Example"}}}));
        assert_eq!(targets.targets["tab-1"].title.as_deref(), Some("Example"));
        targets.apply_event(&serde_json::json!({"method":"Target.targetInfoChanged","params":{"targetInfo":{"targetId":"tab-1","url":"https://example.test/next","title":"Next"}}}));
        assert_eq!(
            targets.targets["tab-1"].url.as_deref(),
            Some("https://example.test/next")
        );
        targets.apply_event(&serde_json::json!({"method":"Target.attachedToTarget","params":{"sessionId":"session-1","targetInfo":{"targetId":"tab-1"}}}));
        assert_eq!(
            targets.targets["tab-1"].session_id.as_deref(),
            Some("session-1")
        );
        assert!(targets.targets["tab-1"].attached);
        targets.apply_event(&serde_json::json!({"method":"Target.detachedFromTarget","params":{"targetId":"tab-1","sessionId":"session-1"}}));
        assert!(!targets.targets["tab-1"].attached);
        assert!(targets.targets["tab-1"].session_id.is_none());

        let mut frames = FrameGraph::default();
        frames.apply_event(&serde_json::json!({"method":"Page.frameAttached","params":{"frameId":"frame-1","parentFrameId":"root","targetId":"tab-1"}}));
        frames.apply_event(&serde_json::json!({"method":"Runtime.executionContextCreated","params":{"context":{"id":7,"auxData":{"frameId":"frame-1"}}}}));
        assert_eq!(frames.frames["frame-1"].execution_context_ids, vec![7]);
        frames.apply_event(
            &serde_json::json!({"method":"Page.frameDetached","params":{"frameId":"frame-1"}}),
        );
        assert!(frames.frames.is_empty());
    }
}
