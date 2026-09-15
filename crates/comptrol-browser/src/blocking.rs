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
                    while let Ok(Request::Command {
                        endpoint,
                        method,
                        params,
                        response,
                    }) = receiver.recv()
                    {
                        let result = async {
                            let connection = manager.connect(&endpoint).await?;
                            connection.command(None, method, params).await
                        }
                        .await;
                        let _ = response.send(result);
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
}

impl Default for BlockingBrowserManager {
    fn default() -> Self {
        Self::new()
    }
}
