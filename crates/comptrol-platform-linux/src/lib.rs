#![deny(unsafe_code)]

use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::proxy_ext::ProxyExt;
use atspi::{AccessibilityConnection, zbus};
use serde_json::{Value, json};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Press,
    SetValue,
}

#[derive(Clone, Debug)]
pub struct Request<'a> {
    pub process_id: u32,
    pub name: &'a str,
    pub role: Option<&'a str>,
    pub action: Action,
    pub value: Option<&'a str>,
    pub expected_attribute: Option<&'a str>,
    pub expected_value: Option<&'a str>,
    pub timeout: Duration,
}

#[derive(Clone, Debug)]
struct OwnedRequest {
    process_id: u32,
    name: String,
    role: Option<String>,
    action: Action,
    value: Option<String>,
    expected_attribute: Option<String>,
    expected_value: Option<String>,
    timeout: Duration,
}

impl<'a> From<Request<'a>> for OwnedRequest {
    fn from(request: Request<'a>) -> Self {
        Self {
            process_id: request.process_id,
            name: request.name.to_owned(),
            role: request.role.map(str::to_owned),
            action: request.action,
            value: request.value.map(str::to_owned),
            expected_attribute: request.expected_attribute.map(str::to_owned),
            expected_value: request.expected_value.map(str::to_owned),
            timeout: request.timeout,
        }
    }
}

impl OwnedRequest {
    fn as_request(&self) -> Request<'_> {
        Request {
            process_id: self.process_id,
            name: &self.name,
            role: self.role.as_deref(),
            action: self.action,
            value: self.value.as_deref(),
            expected_attribute: self.expected_attribute.as_deref(),
            expected_value: self.expected_value.as_deref(),
            timeout: self.timeout,
        }
    }
}

type WorkItem = (OwnedRequest, mpsc::Sender<Result<Value, String>>);
type WorkerSender = mpsc::Sender<WorkItem>;

static WORKER: OnceLock<WorkerSender> = OnceLock::new();

fn worker() -> &'static WorkerSender {
    WORKER.get_or_init(|| {
        let (requests, receiver) =
            mpsc::channel::<(OwnedRequest, mpsc::Sender<Result<Value, String>>)>();
        std::thread::Builder::new()
            .name("comptrol-linux-atspi".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                while let Ok((request, response)) = receiver.recv() {
                    let result = match &runtime {
                        Ok(runtime) => runtime.block_on(async {
                            tokio::time::timeout(
                                request.timeout,
                                execute_async(request.as_request()),
                            )
                            .await
                            .map_err(|_| "AT-SPI operation timed out".to_owned())?
                        }),
                        Err(error) => Err(format!("AT-SPI runtime unavailable: {error}")),
                    };
                    let _ = response.send(result);
                }
            })
            .expect("failed to start persistent Linux AT-SPI worker");
        requests
    })
}

pub fn execute(request: Request<'_>) -> Result<Value, String> {
    let (response, receiver) = mpsc::channel();
    worker()
        .send((request.into(), response))
        .map_err(|_| "Linux AT-SPI worker stopped".to_owned())?;
    receiver
        .recv()
        .map_err(|_| "Linux AT-SPI worker stopped before responding".to_owned())?
}

async fn execute_async(request: Request<'_>) -> Result<Value, String> {
    let connection = AccessibilityConnection::new()
        .await
        .map_err(|error| format!("AT-SPI connection unavailable: {error}"))?;
    let root = connection
        .root_accessible_on_registry()
        .await
        .map_err(|error| format!("AT-SPI registry unavailable: {error}"))?;
    let dbus = zbus::fdo::DBusProxy::new(connection.connection())
        .await
        .map_err(|error| format!("D-Bus identity proxy unavailable: {error}"))?;
    let matches = find_matches(&root, connection.connection(), &dbus, &request, 2048).await?;
    if matches.is_empty() {
        return Err("target_missing".to_owned());
    }
    if matches.len() > 1 {
        return Err("target_ambiguous".to_owned());
    }
    let target = &matches[0];
    let proxies = target
        .proxies()
        .await
        .map_err(|error| format!("AT-SPI interfaces unavailable: {error}"))?;
    match request.action {
        Action::Press => {
            let action = proxies
                .action()
                .await
                .map_err(|error| format!("AT-SPI action interface unavailable: {error}"))?;
            if !action
                .do_action(0)
                .await
                .map_err(|error| format!("AT-SPI action failed: {error}"))?
            {
                return Err("action_rejected".to_owned());
            }
        }
        Action::SetValue => {
            let value = request.value.ok_or("value_required")?;
            let editable = proxies
                .editable_text()
                .await
                .map_err(|error| format!("AT-SPI editable-text interface unavailable: {error}"))?;
            if !editable
                .set_text_contents(value)
                .await
                .map_err(|error| format!("AT-SPI set-value failed: {error}"))?
            {
                return Err("value_rejected".to_owned());
            }
        }
    }
    let verified = verify(target, &request).await?;
    Ok(json!({
        "verified": verified,
        "route": "linux_atspi_direct",
        "process_id": request.process_id,
        "bounded_nodes": 2048,
        "mouse": "untouched",
        "clipboard": "untouched"
    }))
}

async fn find_matches<'a>(
    root: &AccessibleProxy<'a>,
    connection: &'a zbus::Connection,
    dbus: &zbus::fdo::DBusProxy<'a>,
    request: &Request<'_>,
    limit: usize,
) -> Result<Vec<AccessibleProxy<'a>>, String> {
    let mut queue = vec![root.clone()];
    let mut matches = Vec::new();
    while let Some(node) = queue.pop() {
        if queue.len() + matches.len() >= limit {
            break;
        }
        if node_matches(&node, dbus, request).await? {
            matches.push(node.clone());
            if matches.len() > 1 {
                break;
            }
        }
        let children = node
            .get_children()
            .await
            .map_err(|error| format!("AT-SPI child traversal failed: {error}"))?;
        for child in children.into_iter().rev() {
            if child.is_null() {
                continue;
            }
            queue.push(
                child
                    .into_accessible_proxy(connection)
                    .await
                    .map_err(|error| format!("AT-SPI child proxy failed: {error}"))?,
            );
        }
    }
    Ok(matches)
}

async fn node_matches(
    node: &AccessibleProxy<'_>,
    dbus: &zbus::fdo::DBusProxy<'_>,
    request: &Request<'_>,
) -> Result<bool, String> {
    if node
        .name()
        .await
        .map_err(|error| format!("AT-SPI name read failed: {error}"))?
        != request.name
    {
        return Ok(false);
    }
    if let Some(role) = request.role
        && !node
            .get_role_name()
            .await
            .map_err(|error| format!("AT-SPI role read failed: {error}"))?
            .eq_ignore_ascii_case(role)
    {
        return Ok(false);
    }
    let application = node
        .get_application()
        .await
        .map_err(|error| format!("AT-SPI application identity failed: {error}"))?;
    let Some(bus_name) = application.name() else {
        return Ok(false);
    };
    let pid = dbus
        .get_connection_unix_process_id(zbus_names::BusName::Unique(bus_name.clone()))
        .await
        .map_err(|error| format!("AT-SPI process identity failed: {error}"))?;
    Ok(pid == request.process_id)
}

async fn verify(node: &AccessibleProxy<'_>, request: &Request<'_>) -> Result<bool, String> {
    match request.expected_attribute {
        None => Ok(false),
        Some("name") => Ok(node
            .name()
            .await
            .map_err(|error| format!("AT-SPI verification failed: {error}"))?
            == request.expected_value.unwrap_or_default()),
        Some("value") => {
            let proxies = node
                .proxies()
                .await
                .map_err(|error| format!("AT-SPI verification interfaces unavailable: {error}"))?;
            let text_proxy = proxies.text().await.map_err(|error| {
                format!("AT-SPI verification text interface unavailable: {error}")
            })?;
            let count = text_proxy
                .character_count()
                .await
                .map_err(|error| format!("AT-SPI value length verification failed: {error}"))?;
            let text = text_proxy
                .get_text(0, count)
                .await
                .map_err(|error| format!("AT-SPI value verification failed: {error}"))?;
            Ok(text == request.expected_value.unwrap_or_default())
        }
        Some(_) => Err("unsupported_verification_attribute".to_owned()),
    }
}
