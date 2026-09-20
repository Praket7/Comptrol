use crate::{BrowserError, BrowserManager, TargetRecord};
use serde_json::{Value, json};
use std::sync::{Arc, mpsc};
use std::thread;

/// Result of an event-driven wait on the live target graph: the generation
/// observed at resolution plus every page target known at that moment.
#[derive(Clone, Debug)]
pub struct WaitGraphSnapshot {
    pub generation: u64,
    pub targets: Vec<TargetRecord>,
}

enum Request {
    Command {
        endpoint: String,
        method: String,
        params: Value,
        response: mpsc::Sender<Result<Value, BrowserError>>,
    },
    TargetCommand {
        endpoint: String,
        target_id: String,
        generation: u64,
        revision: String,
        method: String,
        params: Value,
        response: mpsc::Sender<Result<Value, BrowserError>>,
    },
    FrameCommand {
        endpoint: String,
        frame_id: String,
        generation: u64,
        revision: u64,
        method: String,
        params: Value,
        response: mpsc::Sender<Result<Value, BrowserError>>,
    },
    NextEvent {
        endpoint: String,
        timeout_ms: u64,
        response: mpsc::Sender<Result<Value, BrowserError>>,
    },
    WaitTargetGraph {
        endpoint: String,
        timeout_ms: u64,
        predicate: Arc<dyn Fn(&TargetRecord) -> bool + Send + Sync>,
        min_count: usize,
        response: mpsc::Sender<Result<WaitGraphSnapshot, BrowserError>>,
    },
}

/// Synchronous compatibility bridge for callers that cannot yet be async.
///
/// The runtime and `BrowserManager` live for the lifetime of the bridge. A
/// request therefore reuses the browser-level flattened-session connection;
/// it never creates a Tokio runtime or a WebSocket per operation.
#[derive(Clone)]
pub struct BlockingBrowserManager {
    requests: mpsc::Sender<Request>,
}

impl BlockingBrowserManager {
    pub fn new() -> Self {
        let (requests, receiver) = mpsc::channel::<Request>();
        thread::Builder::new()
            .name("comptrol-browser-runtime".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("browser runtime must initialize");
                runtime.block_on(async move {
                    let manager = BrowserManager::new();
                    while let Ok(request) = receiver.recv() {
                        match request {
                            Request::Command {
                                endpoint,
                                method,
                                params,
                                response,
                            } => {
                                let result = async {
                                    let connection = manager.connect(&endpoint).await?;
                                    connection.command(None, method, params).await
                                }
                                .await;
                                let _ = response.send(result);
                                continue;
                            }
                            Request::TargetCommand {
                                endpoint,
                                target_id,
                                generation,
                                revision,
                                method,
                                params,
                                response,
                            } => {
                                let result = async {
                                    let connection = manager.connect(&endpoint).await?;
                                    if generation == u64::MAX {
                                        for attempt in 0..2 {
                                            let snapshot = {
                                                let graph = connection.targets.read().await;
                                                graph.targets.get(&target_id).map(|target| {
                                                    (
                                                        graph.generation,
                                                        target.revision.clone(),
                                                        target.url.clone(),
                                                    )
                                                })
                                            };
                                            let Some((
                                                current_generation,
                                                current_revision,
                                                current_url,
                                            )) = snapshot
                                            else {
                                                crate::manager::attach_existing_targets(
                                                    &connection,
                                                )
                                                .await?;
                                                connection.bootstrap_attached_targets().await?;
                                                continue;
                                            };
                                            if revision.starts_with("url:")
                                                && current_url.as_deref()
                                                    != Some(revision.trim_start_matches("url:"))
                                            {
                                                // The graph may lag a navigation
                                                // whose targetInfoChanged event
                                                // has not arrived yet. Refresh
                                                // this target's record from the
                                                // authoritative target list once
                                                // before declaring the reference
                                                // stale.
                                                let refreshed = connection
                                                    .command(None, "Target.getTargets", Value::Null)
                                                    .await?;
                                                if let Some(infos) = refreshed
                                                    .get("targetInfos")
                                                    .and_then(Value::as_array)
                                                {
                                                    for info in infos {
                                                        if info
                                                            .get("targetId")
                                                            .and_then(Value::as_str)
                                                            != Some(target_id.as_str())
                                                        {
                                                            continue;
                                                        }
                                                        connection
                                                            .targets
                                                            .write()
                                                            .await
                                                            .apply_event(&json!({
                                                                "method":
                                                                    "Target.targetInfoChanged",
                                                                "params": {
                                                                    "targetInfo": info
                                                                }
                                                            }));
                                                    }
                                                }
                                                let retry_snapshot = {
                                                    let graph = connection.targets.read().await;
                                                    graph.targets.get(&target_id).map(|target| {
                                                        (
                                                            graph.generation,
                                                            target.revision.clone(),
                                                            target.url.clone(),
                                                        )
                                                    })
                                                };
                                                let Some((
                                                    retry_generation,
                                                    retry_revision,
                                                    retry_url,
                                                )) = retry_snapshot
                                                else {
                                                    return Err(BrowserError::StaleReference(
                                                        target_id.clone(),
                                                    ));
                                                };
                                                if retry_url.as_deref()
                                                    != Some(revision.trim_start_matches("url:"))
                                                {
                                                    return Err(BrowserError::StaleReference(
                                                        target_id.clone(),
                                                    ));
                                                }
                                                let value = connection
                                                    .target_command(
                                                        &target_id,
                                                        retry_generation,
                                                        &retry_revision,
                                                        method.clone(),
                                                        params.clone(),
                                                    )
                                                    .await;
                                                match value {
                                                    Ok(value) => return Ok(value),
                                                    Err(error)
                                                        if attempt == 0
                                                            && matches!(
                                                                error,
                                                                BrowserError::StaleReference(_)
                                                            ) =>
                                                    {
                                                        crate::manager::attach_existing_targets(
                                                            &connection,
                                                        )
                                                        .await?;
                                                        connection
                                                            .bootstrap_attached_targets()
                                                            .await?;
                                                    }
                                                    Err(error) => return Err(error),
                                                }
                                                continue;
                                            }
                                            match connection
                                                .target_command(
                                                    &target_id,
                                                    current_generation,
                                                    &current_revision,
                                                    method.clone(),
                                                    params.clone(),
                                                )
                                                .await
                                            {
                                                Ok(value) => return Ok(value),
                                                Err(error)
                                                    if attempt == 0
                                                        && matches!(
                                                            error,
                                                            BrowserError::StaleReference(_)
                                                        ) =>
                                                {
                                                    crate::manager::attach_existing_targets(
                                                        &connection,
                                                    )
                                                    .await?;
                                                    connection.bootstrap_attached_targets().await?;
                                                }
                                                Err(error) => return Err(error),
                                            }
                                        }
                                        return Err(BrowserError::StaleReference(
                                            target_id.clone(),
                                        ));
                                    }
                                    connection
                                        .target_command(
                                            &target_id, generation, &revision, method, params,
                                        )
                                        .await
                                }
                                .await;
                                let _ = response.send(result);
                                continue;
                            }
                            Request::NextEvent {
                                endpoint,
                                timeout_ms,
                                response,
                            } => {
                                let result = async {
                                    let connection = manager.connect(&endpoint).await?;
                                    connection
                                        .next_event(std::time::Duration::from_millis(timeout_ms))
                                        .await
                                }
                                .await;
                                let _ = response.send(result);
                            }
                            Request::WaitTargetGraph {
                                endpoint,
                                timeout_ms,
                                predicate,
                                min_count,
                                response,
                            } => {
                                let result = wait_for_graph_records(
                                    &manager,
                                    &endpoint,
                                    timeout_ms,
                                    predicate.as_ref(),
                                    min_count,
                                )
                                .await;
                                let _ = response.send(result);
                            }
                            Request::FrameCommand {
                                endpoint,
                                frame_id,
                                generation,
                                revision,
                                method,
                                params,
                                response,
                            } => {
                                let result = async {
                                    let connection = manager.connect(&endpoint).await?;
                                    connection
                                        .frame_command(
                                            &frame_id, generation, revision, method, params,
                                        )
                                        .await
                                }
                                .await;
                                let _ = response.send(result);
                            }
                        }
                    }
                });
            })
            .expect("browser runtime thread must start");
        Self { requests }
    }

    pub fn command(
        &self,
        endpoint: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, BrowserError> {
        let (response, receiver) = mpsc::channel();
        self.requests
            .send(Request::Command {
                endpoint: endpoint.to_owned(),
                method: method.to_owned(),
                params,
                response,
            })
            .map_err(|_| BrowserError::Closed)?;
        receiver.recv().map_err(|_| BrowserError::Closed)?
    }

    pub fn target_command(
        &self,
        endpoint: &str,
        target_id: &str,
        generation: u64,
        revision: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, BrowserError> {
        let (response, receiver) = mpsc::channel();
        self.requests
            .send(Request::TargetCommand {
                endpoint: endpoint.to_owned(),
                target_id: target_id.to_owned(),
                generation,
                revision: revision.to_owned(),
                method: method.to_owned(),
                params,
                response,
            })
            .map_err(|_| BrowserError::Closed)?;
        receiver.recv().map_err(|_| BrowserError::Closed)?
    }

    /// Compatibility target command for callers that still hold the legacy
    /// `/json/list` revision. The live graph supplies the current generation
    /// and session; a URL revision is checked before dispatch so a navigation
    /// cannot silently retarget the operation.
    pub fn target_command_legacy_revision(
        &self,
        endpoint: &str,
        target_id: &str,
        legacy_revision: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, BrowserError> {
        let (response, receiver) = mpsc::channel();
        self.requests
            .send(Request::TargetCommand {
                endpoint: endpoint.to_owned(),
                target_id: target_id.to_owned(),
                generation: u64::MAX,
                revision: legacy_revision.unwrap_or_default().to_owned(),
                method: method.to_owned(),
                params,
                response,
            })
            .map_err(|_| BrowserError::Closed)?;
        receiver.recv().map_err(|_| BrowserError::Closed)?
    }

    /// Wait until the persistent connection's live target graph satisfies the
    /// predicate for at least `min_count` page targets, or until `timeout_ms`
    /// elapses. This is event-driven: it never calls `/json/list` and never
    /// sleeps on a fixed interval; it reacts to the graph updates the
    /// connection already receives from target lifecycle events.
    pub fn wait_target_graph(
        &self,
        endpoint: &str,
        timeout_ms: u64,
        predicate: Arc<dyn Fn(&TargetRecord) -> bool + Send + Sync>,
        min_count: usize,
    ) -> Result<WaitGraphSnapshot, BrowserError> {
        let (response, receiver) = mpsc::channel();
        self.requests
            .send(Request::WaitTargetGraph {
                endpoint: endpoint.to_owned(),
                timeout_ms,
                predicate,
                min_count,
                response,
            })
            .map_err(|_| BrowserError::Closed)?;
        receiver.recv().map_err(|_| BrowserError::Closed)?
    }

    pub fn next_event(&self, endpoint: &str, timeout_ms: u64) -> Result<Value, BrowserError> {
        let (response, receiver) = mpsc::channel();
        self.requests
            .send(Request::NextEvent {
                endpoint: endpoint.to_owned(),
                timeout_ms,
                response,
            })
            .map_err(|_| BrowserError::Closed)?;
        receiver.recv().map_err(|_| BrowserError::Closed)?
    }

    pub fn frame_command(
        &self,
        endpoint: &str,
        frame_id: &str,
        generation: u64,
        revision: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, BrowserError> {
        let (response, receiver) = mpsc::channel();
        self.requests
            .send(Request::FrameCommand {
                endpoint: endpoint.to_owned(),
                frame_id: frame_id.to_owned(),
                generation,
                revision,
                method: method.to_owned(),
                params,
                response,
            })
            .map_err(|_| BrowserError::Closed)?;
        receiver.recv().map_err(|_| BrowserError::Closed)?
    }
}

/// Event-driven wait over the persistent connection's live target graph.
///
/// The graph is maintained by the connection's reader task from target
/// lifecycle events, so this loop never issues discovery requests and never
/// sleeps on a fixed interval: it wakes on each graph-changing event,
/// evaluates the predicate, and returns as soon as the requested membership
/// is observable. The deadline remains a bounded safety net.
async fn wait_for_graph_records(
    manager: &BrowserManager,
    endpoint: &str,
    timeout_ms: u64,
    predicate: &(dyn Fn(&TargetRecord) -> bool + Send + Sync),
    min_count: usize,
) -> Result<WaitGraphSnapshot, BrowserError> {
    let connection = manager.connect(endpoint).await?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let (generation, records) = connection.target_snapshot().await;
        let matched = records
            .iter()
            .filter(|record| predicate(record))
            .cloned()
            .collect::<Vec<_>>();
        if matched.len() >= min_count {
            return Ok(WaitGraphSnapshot {
                generation,
                targets: matched,
            });
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(BrowserError::Timeout);
        }
        // Drain the next graph-changing event with the remaining budget;
        // `next_event` already wakes early when an event arrives.
        connection
            .next_event(std::time::Duration::min(
                remaining,
                std::time::Duration::from_millis(250),
            ))
            .await
            .ok();
    }
}

impl Default for BlockingBrowserManager {
    fn default() -> Self {
        Self::new()
    }
}
