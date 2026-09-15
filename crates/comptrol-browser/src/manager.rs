use crate::{BrowserConnection, BrowserError};
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
