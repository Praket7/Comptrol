use crate::{BrowserConnection, BrowserError};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Owns the long-lived browser-level connections used by warm operations.
///
/// The key is the exact debugger WebSocket URL. Keeping endpoint ownership in
/// one place prevents a caller from accidentally creating one socket per tab
/// or per operation. Eviction is explicit because a disconnected connection
/// must not be silently reused after its generation has changed.
#[derive(Clone, Default)]
pub struct BrowserManager {
    connections: Arc<RwLock<HashMap<String, Arc<BrowserConnection>>>>,
}

impl BrowserManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn connect(&self, endpoint: &str) -> Result<Arc<BrowserConnection>, BrowserError> {
        if endpoint.is_empty() {
            return Err(BrowserError::Connection(
                "empty debugger endpoint".to_owned(),
            ));
        }
        if let Some(connection) = self.connections.read().await.get(endpoint).cloned()
            && !connection.is_closed()
        {
            return Ok(connection);
        }
        let connection = Arc::new(BrowserConnection::connect(endpoint).await?);
        connection.bootstrap().await?;
        attach_existing_targets(&connection).await?;
        connection.bootstrap_attached_targets().await?;
        let mut connections = self.connections.write().await;
        if let Some(existing) = connections.get(endpoint).cloned()
            && !existing.is_closed()
        {
            return Ok(existing);
        }
        connections.insert(endpoint.to_owned(), Arc::clone(&connection));
        Ok(connection)
    }

    pub async fn get(&self, endpoint: &str) -> Option<Arc<BrowserConnection>> {
        self.connections.read().await.get(endpoint).cloned()
    }

    pub async fn evict(&self, endpoint: &str) -> bool {
        self.connections.write().await.remove(endpoint).is_some()
    }

    pub async fn len(&self) -> usize {
        self.connections.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.connections.read().await.is_empty()
    }
}

/// `Target.setAutoAttach` covers targets created after the subscription is
/// installed, but Chrome may expose an already-open page before that command
/// is processed. Attach those existing page-like targets explicitly so all
/// target commands are routed through flattened sessions rather than the
/// browser-level socket.
pub(crate) async fn attach_existing_targets(
    connection: &BrowserConnection,
) -> Result<(), BrowserError> {
    let response = connection
        .command(None, "Target.getTargets", Value::Null)
        .await?;
    let Some(targets) = response.get("targetInfos").and_then(Value::as_array) else {
        return Ok(());
    };
    for info in targets {
        let target_type = info.get("type").and_then(Value::as_str).unwrap_or_default();
        // Chrome's `webview` targets do not expose the Page domain in the
        // same way as ordinary tabs. They remain discoverable, but are not
        // eligible for this page-domain bootstrap.
        if !matches!(target_type, "page" | "iframe") {
            continue;
        }
        let Some(target_id) = info.get("targetId").and_then(Value::as_str) else {
            continue;
        };
        let already_attached = {
            let graph = connection.targets.read().await;
            graph
                .targets
                .get(target_id)
                .is_some_and(|target| target.attached)
        };
        if already_attached {
            continue;
        }
        let result = connection
            .command(
                None,
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
            )
            .await;
        let response = match result {
            Ok(response) => response,
            Err(error) if error.to_string().contains("already attached") => continue,
            Err(error) => return Err(error),
        };
        if let Some(session_id) = response.get("sessionId").and_then(Value::as_str) {
            connection.targets.write().await.apply_event(&json!({
                "method": "Target.attachedToTarget",
                "params": {
                    "sessionId": session_id,
                    "targetInfo": info,
                }
            }));
        }
    }
    Ok(())
}
