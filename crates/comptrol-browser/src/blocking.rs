use crate::{BrowserError, BrowserManager};
use serde_json::Value;
use std::sync::mpsc;
use std::thread;

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
    NextEvent {
        endpoint: String,
        timeout_ms: u64,
        response: mpsc::Sender<Result<Value, BrowserError>>,
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
                                        let (current_generation, current_revision, current_url) = {
                                            let graph = connection.targets.read().await;
                                            let target =
                                                graph.targets.get(&target_id).ok_or_else(|| {
                                                    BrowserError::StaleReference(target_id.clone())
                                                })?;
                                            (
                                                graph.generation,
                                                target.revision.clone(),
                                                target.url.clone(),
                                            )
                                        };
                                        if revision.starts_with("url:")
                                            && current_url.as_deref()
                                                != Some(revision.trim_start_matches("url:"))
                                        {
                                            return Err(BrowserError::StaleReference(
                                                target_id.clone(),
                                            ));
                                        }
                                        return connection
                                            .target_command(
                                                &target_id,
                                                current_generation,
                                                &current_revision,
                                                method,
                                                params,
                                            )
                                            .await;
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
}

impl Default for BlockingBrowserManager {
    fn default() -> Self {
        Self::new()
    }
}
