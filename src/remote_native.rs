//! Native HTTP/WebSocket remote backend worker.

use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs as _};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::channel::mpsc::{TryRecvError, UnboundedReceiver};
use tungstenite::client::IntoClientRequest as _;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::backend::{
    BackendCommand, BackendConnectionStatus, BackendEvent, BackendEventSender, DiagnosticDirection,
    DiagnosticEvent, remote_api_commands,
};
use crate::core::{ApiCommand, AuthoritativeSnapshot, RecipeExport, SavedRecipe, WebSocketEvent};
use crate::remote_backend::{
    COMMAND_HEADER, INITIAL_RECONNECT_DELAY_MS, MAX_RECONNECT_DELAY_MS, RemoteUrls,
    publish_websocket,
};

const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(100);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn spawn_backend(
    urls: RemoteUrls,
    command_rx: UnboundedReceiver<BackendCommand>,
    event_tx: BackendEventSender,
) -> Result<JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("ebc-remote-backend".to_owned())
        .spawn(move || remote_thread(urls, command_rx, event_tx))
        .map_err(|error| format!("failed to spawn remote backend thread: {error}"))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the worker owns its backend configuration and event sender"
)]
#[expect(
    clippy::too_many_lines,
    reason = "the reconnect loop keeps socket, command, and recipe resynchronization ordering together"
)]
fn remote_thread(
    urls: RemoteUrls,
    mut command_rx: UnboundedReceiver<BackendCommand>,
    event_tx: BackendEventSender,
) {
    let agent = ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build();
    let mut reconnect_delay_ms = INITIAL_RECONNECT_DELAY_MS;
    let mut connected_once = false;
    let mut attempted_connection = false;

    loop {
        if reject_queued_commands(&mut command_rx, &event_tx) {
            return;
        }
        event_tx.send(BackendEvent::BackendConnectionChanged(
            if connected_once || attempted_connection {
                BackendConnectionStatus::Reconnecting
            } else {
                BackendConnectionStatus::Connecting
            },
        ));
        attempted_connection = true;
        match connect_websocket(&urls) {
            Ok(mut socket) => {
                if let Err(error) = set_read_timeout(&mut socket, SOCKET_POLL_INTERVAL) {
                    publish_network_error(&event_tx, &error);
                } else {
                    let mut ready = false;
                    loop {
                        if ready {
                            match command_rx.try_recv() {
                                Ok(BackendCommand::Shutdown) | Err(TryRecvError::Closed) => {
                                    let _closed = socket.close(None);
                                    return;
                                }
                                Ok(command) => {
                                    send_backend_command(command, &urls, &agent, &event_tx);
                                }
                                Err(TryRecvError::Empty) => {}
                            }
                        } else if reject_queued_commands(&mut command_rx, &event_tx) {
                            let _closed = socket.close(None);
                            return;
                        }

                        match socket.read() {
                            Ok(Message::Text(text)) => {
                                match serde_json::from_str::<WebSocketEvent>(text.as_str()) {
                                    Ok(WebSocketEvent::RecipeLibrary(subscription_library)) => {
                                        let recipes = match fetch_recipes(&urls, &agent) {
                                            Ok(recipes) => recipes,
                                            Err(error) => {
                                                publish_network_error(
                                                    &event_tx,
                                                    &format!("failed to refresh recipes: {error}"),
                                                );
                                                subscription_library
                                            }
                                        };
                                        event_tx.send(BackendEvent::RecipeLibrary(recipes));
                                        ready = true;
                                        connected_once = true;
                                        reconnect_delay_ms = INITIAL_RECONNECT_DELAY_MS;
                                        event_tx.send(BackendEvent::BackendConnectionChanged(
                                            BackendConnectionStatus::Connected,
                                        ));
                                    }
                                    Ok(event) => {
                                        if matches!(event, WebSocketEvent::Snapshot(_)) {
                                            connected_once = true;
                                        }
                                        publish_websocket(event, &event_tx);
                                    }
                                    Err(error) => {
                                        log::error!("invalid server websocket event: {error}");
                                    }
                                }
                            }
                            Ok(Message::Close(_)) => break,
                            Ok(
                                Message::Ping(_)
                                | Message::Pong(_)
                                | Message::Binary(_)
                                | Message::Frame(_),
                            ) => {}
                            Err(tungstenite::Error::Io(error))
                                if matches!(
                                    error.kind(),
                                    ErrorKind::WouldBlock | ErrorKind::TimedOut
                                ) => {}
                            Err(error) => {
                                publish_network_error(
                                    &event_tx,
                                    &format!("server WebSocket failed: {error}"),
                                );
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) => publish_network_error(&event_tx, &error),
        }

        event_tx.send(BackendEvent::BackendConnectionChanged(
            BackendConnectionStatus::Reconnecting,
        ));
        if wait_for_reconnect(
            &mut command_rx,
            Duration::from_millis(reconnect_delay_ms),
            &event_tx,
        ) {
            return;
        }
        reconnect_delay_ms = (reconnect_delay_ms * 2).min(MAX_RECONNECT_DELAY_MS);
    }
}

fn connect_websocket(urls: &RemoteUrls) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, String> {
    let parsed = url::Url::parse(&urls.websocket)
        .map_err(|error| format!("invalid WebSocket URL: {error}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "WebSocket URL is missing a host".to_owned())?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| "WebSocket URL is missing a port".to_owned())?;
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("failed to resolve remote server: {error}"))?;
    let mut last_error = None;
    let mut stream = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let stream = stream.ok_or_else(|| {
        last_error.map_or_else(
            || "remote server did not resolve to an address".to_owned(),
            |error| format!("failed to connect remote server: {error}"),
        )
    })?;
    stream
        .set_read_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|error| format!("failed to configure WebSocket handshake: {error}"))?;
    stream
        .set_write_timeout(Some(HTTP_TIMEOUT))
        .map_err(|error| format!("failed to configure WebSocket writes: {error}"))?;
    let mut request = urls
        .websocket
        .as_str()
        .into_client_request()
        .map_err(|error| format!("invalid WebSocket request: {error}"))?;
    request.headers_mut().insert(
        "Origin",
        urls.origin
            .parse()
            .map_err(|error| format!("invalid remote Origin header: {error}"))?,
    );
    tungstenite::client_tls(request, stream)
        .map(|(socket, _response)| socket)
        .map_err(|error| format!("failed to connect server WebSocket: {error}"))
}

fn set_read_timeout(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Duration,
) -> Result<(), String> {
    let result = match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(Some(timeout)),
        MaybeTlsStream::Rustls(stream) => stream.sock.set_read_timeout(Some(timeout)),
        _ => return Err("unsupported native WebSocket TLS stream".to_owned()),
    };
    result.map_err(|error| format!("failed to configure WebSocket polling: {error}"))
}

#[expect(
    clippy::too_many_lines,
    reason = "the exhaustive semantic command-to-HTTP mapping is clearest in one dispatcher"
)]
fn send_backend_command(
    command: BackendCommand,
    urls: &RemoteUrls,
    agent: &ureq::Agent,
    event_tx: &BackendEventSender,
) {
    match &command {
        BackendCommand::History(request) => {
            let result = fetch_history(request, urls, agent);
            event_tx.send(BackendEvent::HistoryResult {
                request: request.clone(),
                result,
            });
            return;
        }
        BackendCommand::StartTest(request) => {
            event_tx.send(BackendEvent::Diagnostic(DiagnosticEvent {
                direction: DiagnosticDirection::Out,
                label: "StartTest".to_owned(),
                raw_bytes: Vec::new(),
            }));
            let result = send_json(&format!("{}/api/test/start", urls.base), request, agent);
            publish_command_result(result, event_tx);
            return;
        }
        BackendCommand::StartCycle(request) => {
            event_tx.send(BackendEvent::Diagnostic(DiagnosticEvent {
                direction: DiagnosticDirection::Out,
                label: "StartCycle".to_owned(),
                raw_bytes: Vec::new(),
            }));
            let result = send_json(&format!("{}/api/cycle/start", urls.base), request, agent);
            publish_command_result(result, event_tx);
            return;
        }
        BackendCommand::StartSavedRecipe { recipe_id, request } => {
            let result = send_json::<_, AuthoritativeSnapshot>(
                &format!("{}/api/recipes/{recipe_id}/start", urls.base),
                request,
                agent,
            );
            publish_command_result(result, event_tx);
            return;
        }
        BackendCommand::RefreshRecipes => {
            match fetch_recipes(urls, agent) {
                Ok(recipes) => {
                    event_tx.send(BackendEvent::RecipeLibrary(recipes));
                    event_tx.send(BackendEvent::CommandSucceeded);
                }
                Err(error) => event_tx.send(BackendEvent::CommandError(error)),
            }
            return;
        }
        BackendCommand::CreateSavedRecipe(request) => {
            publish_created_recipe_result(
                send_json(&format!("{}/api/recipes", urls.base), request, agent),
                event_tx,
            );
            return;
        }
        BackendCommand::UpdateSavedRecipe { recipe_id, request } => {
            let result = agent
                .put(&format!("{}/api/recipes/{recipe_id}", urls.base))
                .set(COMMAND_HEADER, "1")
                .send_json(request)
                .map_err(format_http_error)
                .and_then(|response| {
                    response
                        .into_json::<SavedRecipe>()
                        .map_err(|error| format!("invalid server response: {error}"))
                });
            if result
                .as_ref()
                .is_err_and(|error| error.contains("HTTP 409"))
                && let Ok(recipes) = fetch_recipes(urls, agent)
            {
                event_tx.send(BackendEvent::RecipeLibrary(recipes));
            }
            publish_recipe_result(result, event_tx);
            return;
        }
        BackendCommand::DeleteSavedRecipe { recipe_id, request } => {
            let result = agent
                .delete(&format!("{}/api/recipes/{recipe_id}", urls.base))
                .set(COMMAND_HEADER, "1")
                .send_json(request)
                .map_err(format_http_error)
                .and_then(|response| {
                    response
                        .into_json::<SavedRecipe>()
                        .map_err(|error| format!("invalid server response: {error}"))
                });
            match result {
                Ok(recipe) => {
                    event_tx.send(BackendEvent::RecipeDeleted(recipe.id));
                    event_tx.send(BackendEvent::CommandSucceeded);
                }
                Err(error) => {
                    if error.contains("HTTP 409")
                        && let Ok(recipes) = fetch_recipes(urls, agent)
                    {
                        event_tx.send(BackendEvent::RecipeLibrary(recipes));
                    }
                    event_tx.send(BackendEvent::CommandError(error));
                }
            }
            return;
        }
        BackendCommand::ImportRecipe(export) => {
            publish_created_recipe_result(
                send_json(&format!("{}/api/recipes/import", urls.base), export, agent),
                event_tx,
            );
            return;
        }
        BackendCommand::ExportRecipe { recipe_id } => {
            match get_json::<RecipeExport>(
                &format!("{}/api/recipes/{recipe_id}/export", urls.base),
                agent,
            ) {
                Ok(export) => {
                    event_tx.send(BackendEvent::RecipeExported(export));
                    event_tx.send(BackendEvent::CommandSucceeded);
                }
                Err(error) => event_tx.send(BackendEvent::CommandError(error)),
            }
            return;
        }
        BackendCommand::StartSavedRecipeSnapshot { .. } => {
            event_tx.send(BackendEvent::CommandError(
                "local saved recipe snapshot sent to remote backend".to_owned(),
            ));
            return;
        }
        BackendCommand::RenameRun { run_id, request } => {
            let result = send_json(
                &format!("{}/api/runs/{run_id}/name", urls.base),
                request,
                agent,
            );
            if result.is_ok() {
                event_tx.send(BackendEvent::HistoryRenamed {
                    id: run_id.clone(),
                    cycle: false,
                    name: crate::core::normalize_optional_name(request.name.as_deref())
                        .unwrap_or_default(),
                });
            }
            publish_command_result(result, event_tx);
            return;
        }
        BackendCommand::RenameCycle {
            execution_id,
            request,
        } => {
            let result = send_json(
                &format!("{}/api/cycles/{execution_id}/name", urls.base),
                request,
                agent,
            );
            if result.is_ok() {
                event_tx.send(BackendEvent::HistoryRenamed {
                    id: execution_id.clone(),
                    cycle: true,
                    name: crate::core::normalize_optional_name(request.name.as_deref())
                        .unwrap_or_default(),
                });
            }
            publish_command_result(result, event_tx);
            return;
        }
        BackendCommand::StopCycle => {
            event_tx.send(BackendEvent::Diagnostic(DiagnosticEvent {
                direction: DiagnosticDirection::Out,
                label: "StopCycle".to_owned(),
                raw_bytes: Vec::new(),
            }));
            let result = agent
                .post(&format!("{}/api/cycle/stop", urls.base))
                .set(COMMAND_HEADER, "1")
                .call()
                .map_err(format_http_error)
                .and_then(|response| {
                    response
                        .into_json::<AuthoritativeSnapshot>()
                        .map_err(|error| format!("invalid server response: {error}"))
                });
            publish_command_result(result, event_tx);
            return;
        }
        _ => {}
    }
    for command in remote_api_commands(command) {
        event_tx.send(BackendEvent::Diagnostic(DiagnosticEvent {
            direction: DiagnosticDirection::Out,
            label: format!("{command:?}"),
            raw_bytes: Vec::new(),
        }));
        match send_command(command, urls, agent) {
            Ok(_snapshot) => {
                event_tx.send(BackendEvent::CommandSucceeded);
            }
            Err(error) => {
                log::error!("remote command failed: {error}");
                event_tx.send(BackendEvent::CommandError(error));
                break;
            }
        }
    }
}

fn fetch_history(
    request: &crate::backend::HistoryRequest,
    urls: &RemoteUrls,
    agent: &ureq::Agent,
) -> Result<crate::backend::HistoryEvent, String> {
    use std::io::Read as _;
    let response = agent
        .get(&format!("{}{}", urls.base, request.path()))
        .call()
        .map_err(format_http_error)?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    request.decode(bytes)
}

fn publish_command_result(
    result: Result<AuthoritativeSnapshot, String>,
    event_tx: &BackendEventSender,
) {
    match result {
        Ok(_snapshot) => event_tx.send(BackendEvent::CommandSucceeded),
        Err(error) => {
            log::error!("remote command failed: {error}");
            event_tx.send(BackendEvent::CommandError(error));
        }
    }
}

fn send_json<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    url: &str,
    body: &T,
    agent: &ureq::Agent,
) -> Result<R, String> {
    agent
        .post(url)
        .set(COMMAND_HEADER, "1")
        .send_json(body)
        .map_err(format_http_error)?
        .into_json()
        .map_err(|error| format!("invalid server response: {error}"))
}

fn get_json<T: serde::de::DeserializeOwned>(url: &str, agent: &ureq::Agent) -> Result<T, String> {
    agent
        .get(url)
        .call()
        .map_err(format_http_error)?
        .into_json()
        .map_err(|error| format!("invalid server response: {error}"))
}

fn fetch_recipes(urls: &RemoteUrls, agent: &ureq::Agent) -> Result<Vec<SavedRecipe>, String> {
    get_json(&format!("{}/api/recipes", urls.base), agent)
}

fn publish_recipe_result(result: Result<SavedRecipe, String>, event_tx: &BackendEventSender) {
    match result {
        Ok(recipe) => {
            event_tx.send(BackendEvent::RecipeUpsert(recipe));
            event_tx.send(BackendEvent::CommandSucceeded);
        }
        Err(error) => event_tx.send(BackendEvent::CommandError(error)),
    }
}

fn publish_created_recipe_result(
    result: Result<SavedRecipe, String>,
    event_tx: &BackendEventSender,
) {
    match result {
        Ok(recipe) => {
            event_tx.send(BackendEvent::RecipeCreated(recipe));
            event_tx.send(BackendEvent::CommandSucceeded);
        }
        Err(error) => event_tx.send(BackendEvent::CommandError(error)),
    }
}

fn send_command(
    command: ApiCommand,
    urls: &RemoteUrls,
    agent: &ureq::Agent,
) -> Result<AuthoritativeSnapshot, String> {
    if matches!(command, ApiCommand::Start(_)) {
        return Err(
            "ApiCommand::Start cannot be sent remotely; use BackendCommand::StartTest".to_owned(),
        );
    }
    let request = agent
        .post(&urls.endpoint(command)?)
        .set(COMMAND_HEADER, "1");
    let response = match command {
        ApiCommand::Adjust(config) => request.send_json(config),
        ApiCommand::Calibration(calibration) => request.send_json(calibration),
        ApiCommand::Connect | ApiCommand::Disconnect | ApiCommand::Stop | ApiCommand::Resume => {
            request.call()
        }
        ApiCommand::Start(_) => unreachable!("start was rejected above"),
    }
    .map_err(format_http_error)?;
    response
        .into_json()
        .map_err(|error| format!("invalid server response: {error}"))
}

fn format_http_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, response) => {
            let body = response.into_string().unwrap_or_default();
            format!("HTTP {status}: {body}")
        }
        ureq::Error::Transport(error) => error.to_string(),
    }
}

fn wait_for_reconnect(
    command_rx: &mut UnboundedReceiver<BackendCommand>,
    delay: Duration,
    event_tx: &BackendEventSender,
) -> bool {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if reject_queued_commands(command_rx, event_tx) {
            return true;
        }
        std::thread::sleep(SOCKET_POLL_INTERVAL);
    }
    false
}

fn reject_queued_commands(
    command_rx: &mut UnboundedReceiver<BackendCommand>,
    event_tx: &BackendEventSender,
) -> bool {
    loop {
        match command_rx.try_recv() {
            Ok(BackendCommand::Shutdown) | Err(TryRecvError::Closed) => return true,
            Ok(_) => reject_disconnected_command(event_tx),
            Err(TryRecvError::Empty) => return false,
        }
    }
}

fn reject_disconnected_command(event_tx: &BackendEventSender) {
    event_tx.send(BackendEvent::CommandError(
        "remote client is disconnected; command was not sent".to_owned(),
    ));
}

fn publish_network_error(event_tx: &BackendEventSender, error: &str) {
    log::warn!("{error}");
    event_tx.send(BackendEvent::CommandError(error.to_owned()));
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "transport tests should fail fast")]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    use futures::channel::mpsc;

    use super::*;
    use crate::core::{
        CreateSavedRecipeRequest, CycleRecipe, DeleteSavedRecipeRequest, RECIPE_EXPORT_FORMAT,
        RECIPE_EXPORT_VERSION, RenameRequest, StartCycleRequest, StartSavedRecipeRequest,
        StartTestRequest, TestConfiguration, UpdateSavedRecipeRequest,
    };

    fn config() -> TestConfiguration {
        TestConfiguration::DischargeConstantCurrent {
            current_ma: 1000,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        }
    }

    fn capture_backend_request(command: BackendCommand) -> String {
        let response_body = match &command {
            BackendCommand::RefreshRecipes => serde_json::json!([]),
            BackendCommand::CreateSavedRecipe(_)
            | BackendCommand::UpdateSavedRecipe { .. }
            | BackendCommand::DeleteSavedRecipe { .. }
            | BackendCommand::ImportRecipe(_) => {
                serde_json::to_value(saved_recipe("recipe-1", "Recipe", 2))
                    .expect("serialize recipe response")
            }
            BackendCommand::ExportRecipe { .. } => {
                serde_json::to_value(recipe_export()).expect("serialize recipe export response")
            }
            _ => serde_json::to_value(AuthoritativeSnapshot::default())
                .expect("serialize response snapshot"),
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP test server");
        let address = listener.local_addr().expect("HTTP test server address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept HTTP command");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set HTTP read timeout");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            let expected_len = loop {
                let read = stream.read(&mut chunk).expect("read HTTP command");
                assert_ne!(read, 0, "HTTP command ended before its body");
                request.extend_from_slice(&chunk[..read]);
                let Some(headers_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..headers_end]).to_ascii_lowercase();
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                break headers_end + 4 + content_length;
            };
            while request.len() < expected_len {
                let read = stream.read(&mut chunk).expect("read HTTP body");
                assert_ne!(read, 0, "HTTP command body ended early");
                request.extend_from_slice(&chunk[..read]);
            }

            let body = serde_json::to_string(&response_body).expect("serialize HTTP response");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write HTTP response");
            String::from_utf8(request).expect("HTTP request is UTF-8")
        });

        let urls = RemoteUrls::parse(&format!("http://{address}")).expect("remote URLs");
        let (event_tx, _event_rx) = mpsc::unbounded();
        let event_tx = BackendEventSender::new(event_tx, || {});
        let agent = ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build();
        send_backend_command(command, &urls, &agent, &event_tx);
        server.join().expect("HTTP test server")
    }

    fn request_body(request: &str) -> serde_json::Value {
        let (_, body) = request.split_once("\r\n\r\n").expect("HTTP body");
        serde_json::from_str(body).expect("JSON request body")
    }

    fn saved_recipe(id: &str, name: &str, revision: u64) -> SavedRecipe {
        SavedRecipe {
            id: id.to_owned(),
            name: name.to_owned(),
            recipe: CycleRecipe {
                steps: Vec::new(),
                repeat_count: 1,
            },
            revision,
            created_at_utc: "created".to_owned(),
            updated_at_utc: "updated".to_owned(),
        }
    }

    fn recipe_export() -> RecipeExport {
        RecipeExport {
            format: RECIPE_EXPORT_FORMAT.to_owned(),
            version: RECIPE_EXPORT_VERSION,
            name: "Imported".to_owned(),
            recipe: CycleRecipe {
                steps: Vec::new(),
                repeat_count: 2,
            },
        }
    }

    #[test]
    fn naming_commands_use_canonical_routes_and_envelopes() {
        let start_test = StartTestRequest {
            config: config(),
            name: Some("run name".to_owned()),
        };
        let request = capture_backend_request(BackendCommand::StartTest(start_test.clone()));
        assert!(request.starts_with("POST /api/test/start HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-ebc-command: 1\r\n")
        );
        assert_eq!(
            request_body(&request),
            serde_json::to_value(start_test).expect("serialize start test")
        );

        let start_cycle = StartCycleRequest {
            recipe: CycleRecipe {
                steps: Vec::new(),
                repeat_count: 1,
            },
            name: Some("cycle name".to_owned()),
        };
        let request = capture_backend_request(BackendCommand::StartCycle(start_cycle.clone()));
        assert!(request.starts_with("POST /api/cycle/start HTTP/1.1\r\n"));
        assert_eq!(
            request_body(&request),
            serde_json::to_value(start_cycle).expect("serialize start cycle")
        );

        let rename = RenameRequest {
            name: Some("renamed".to_owned()),
        };
        let request = capture_backend_request(BackendCommand::RenameRun {
            run_id: "run-1".to_owned(),
            request: rename.clone(),
        });
        assert!(request.starts_with("POST /api/runs/run-1/name HTTP/1.1\r\n"));
        assert_eq!(
            request_body(&request),
            serde_json::to_value(&rename).expect("serialize run rename")
        );

        let request = capture_backend_request(BackendCommand::RenameCycle {
            execution_id: "cycle-1".to_owned(),
            request: rename.clone(),
        });
        assert!(request.starts_with("POST /api/cycles/cycle-1/name HTTP/1.1\r\n"));
        assert_eq!(
            request_body(&request),
            serde_json::to_value(rename).expect("serialize cycle rename")
        );
    }

    #[test]
    fn recipe_commands_use_canonical_methods_routes_and_bodies() {
        let request = capture_backend_request(BackendCommand::RefreshRecipes);
        assert!(request.starts_with("GET /api/recipes HTTP/1.1\r\n"));
        assert_eq!(request.split_once("\r\n\r\n").expect("HTTP headers").1, "");

        let create = CreateSavedRecipeRequest {
            name: "Created".to_owned(),
            recipe: recipe_export().recipe,
        };
        let request = capture_backend_request(BackendCommand::CreateSavedRecipe(create.clone()));
        assert!(request.starts_with("POST /api/recipes HTTP/1.1\r\n"));
        assert_eq!(
            request_body(&request),
            serde_json::to_value(create).expect("create body")
        );

        let update = UpdateSavedRecipeRequest {
            name: "Updated".to_owned(),
            recipe: recipe_export().recipe,
            expected_revision: 1,
        };
        let request = capture_backend_request(BackendCommand::UpdateSavedRecipe {
            recipe_id: "recipe-1".to_owned(),
            request: update.clone(),
        });
        assert!(request.starts_with("PUT /api/recipes/recipe-1 HTTP/1.1\r\n"));
        assert_eq!(
            request_body(&request),
            serde_json::to_value(update).expect("update body")
        );

        let delete = DeleteSavedRecipeRequest {
            expected_revision: 2,
        };
        let request = capture_backend_request(BackendCommand::DeleteSavedRecipe {
            recipe_id: "recipe-1".to_owned(),
            request: delete,
        });
        assert!(request.starts_with("DELETE /api/recipes/recipe-1 HTTP/1.1\r\n"));
        assert_eq!(
            request_body(&request),
            serde_json::json!({ "expected_revision": 2 })
        );

        let start = StartSavedRecipeRequest {
            execution_name: Some("Execution".to_owned()),
        };
        let request = capture_backend_request(BackendCommand::StartSavedRecipe {
            recipe_id: "recipe-1".to_owned(),
            request: start.clone(),
        });
        assert!(request.starts_with("POST /api/recipes/recipe-1/start HTTP/1.1\r\n"));
        assert_eq!(
            request_body(&request),
            serde_json::to_value(start).expect("start body")
        );

        let export = recipe_export();
        let request = capture_backend_request(BackendCommand::ImportRecipe(export.clone()));
        assert!(request.starts_with("POST /api/recipes/import HTTP/1.1\r\n"));
        assert_eq!(
            request_body(&request),
            serde_json::to_value(export).expect("import body")
        );

        let request = capture_backend_request(BackendCommand::ExportRecipe {
            recipe_id: "recipe-1".to_owned(),
        });
        assert!(request.starts_with("GET /api/recipes/recipe-1/export HTTP/1.1\r\n"));
        assert_eq!(request.split_once("\r\n\r\n").expect("HTTP headers").1, "");
    }

    #[test]
    fn disconnected_commands_are_rejected_and_never_retained() {
        let (command_tx, mut command_rx) = mpsc::unbounded();
        let (event_tx, mut event_rx) = mpsc::unbounded();
        let event_tx = BackendEventSender::new(event_tx, || {});
        assert!(
            command_tx
                .unbounded_send(BackendCommand::Connect(0))
                .is_ok()
        );
        assert!(
            command_tx
                .unbounded_send(BackendCommand::Api(ApiCommand::Stop))
                .is_ok()
        );

        assert!(!reject_queued_commands(&mut command_rx, &event_tx));
        assert!(matches!(command_rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(BackendEvent::CommandError(_))
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(BackendEvent::CommandError(_))
        ));
    }

    #[test]
    fn shutdown_interrupts_disconnected_waits() {
        let (command_tx, mut command_rx) = mpsc::unbounded();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let event_tx = BackendEventSender::new(event_tx, || {});
        assert!(command_tx.unbounded_send(BackendCommand::Shutdown).is_ok());

        assert!(reject_queued_commands(&mut command_rx, &event_tx));
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the integration fixture serves WebSocket, initial recipe refresh, and command HTTP"
    )]
    fn native_transport_receives_snapshot_and_sends_semantic_command() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|error| panic!("failed to bind test server: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("failed to read test server address: {error}"));
        let snapshot = AuthoritativeSnapshot::default();
        let response_snapshot = snapshot.clone();
        let server = std::thread::spawn(move || {
            let (websocket_stream, _) = listener
                .accept()
                .unwrap_or_else(|error| panic!("failed to accept WebSocket: {error}"));
            let mut websocket = tungstenite::accept(websocket_stream)
                .unwrap_or_else(|error| panic!("failed WebSocket handshake: {error}"));
            let event = WebSocketEvent::Snapshot(snapshot);
            let text = serde_json::to_string(&event)
                .unwrap_or_else(|error| panic!("failed to serialize Snapshot: {error}"));
            websocket
                .send(Message::Text(text.into()))
                .unwrap_or_else(|error| panic!("failed to send Snapshot: {error}"));
            let library = serde_json::to_string(&WebSocketEvent::RecipeLibrary(Vec::new()))
                .unwrap_or_else(|error| panic!("failed to serialize recipe library: {error}"));
            websocket
                .send(Message::Text(library.into()))
                .unwrap_or_else(|error| panic!("failed to send recipe library: {error}"));

            let (mut recipe_http, _) = listener
                .accept()
                .unwrap_or_else(|error| panic!("failed to accept recipe refresh: {error}"));
            recipe_http
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap_or_else(|error| panic!("failed to set HTTP timeout: {error}"));
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = recipe_http
                    .read(&mut chunk)
                    .unwrap_or_else(|error| panic!("failed to read recipe refresh: {error}"));
                assert_ne!(read, 0, "recipe refresh ended before headers");
                request.extend_from_slice(&chunk[..read]);
            }
            let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(request.starts_with("get /api/recipes http/1.1\r\n"));
            recipe_http
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]",
                )
                .unwrap_or_else(|error| panic!("failed to send recipe library: {error}"));

            let (mut http, _) = listener
                .accept()
                .unwrap_or_else(|error| panic!("failed to accept HTTP command: {error}"));
            http.set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap_or_else(|error| panic!("failed to set HTTP timeout: {error}"));
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = http
                    .read(&mut chunk)
                    .unwrap_or_else(|error| panic!("failed to read HTTP command: {error}"));
                assert_ne!(read, 0, "HTTP command ended before headers");
                request.extend_from_slice(&chunk[..read]);
            }
            let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(request.starts_with("post /api/connect http/1.1\r\n"));
            assert!(request.contains("x-ebc-command: 1\r\n"));

            let body = serde_json::to_string(&response_snapshot)
                .unwrap_or_else(|error| panic!("failed to serialize response: {error}"));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            http.write_all(response.as_bytes())
                .unwrap_or_else(|error| panic!("failed to send HTTP response: {error}"));
            let _closed = websocket.close(None);
        });

        let urls = RemoteUrls::parse(&format!("http://{address}"))
            .unwrap_or_else(|error| panic!("failed to build test URLs: {error}"));
        let (command_tx, command_rx) = mpsc::unbounded();
        let (event_tx, mut event_rx) = mpsc::unbounded();
        let event_tx = BackendEventSender::new(event_tx, || {});
        let worker = spawn_backend(urls, command_rx, event_tx)
            .unwrap_or_else(|error| panic!("failed to start remote worker: {error}"));

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut connected = false;
        let mut received_snapshot = false;
        while Instant::now() < deadline && !(connected && received_snapshot) {
            match event_rx.try_recv() {
                Ok(BackendEvent::BackendConnectionChanged(BackendConnectionStatus::Connected)) => {
                    connected = true;
                }
                Ok(BackendEvent::Snapshot(_)) => received_snapshot = true,
                Ok(_) | Err(TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(TryRecvError::Closed) => break,
            }
        }
        assert!(
            connected && received_snapshot,
            "initial Snapshot was not received"
        );
        assert!(
            command_tx
                .unbounded_send(BackendCommand::Connect(0))
                .is_ok()
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut succeeded = false;
        while Instant::now() < deadline && !succeeded {
            match event_rx.try_recv() {
                Ok(BackendEvent::CommandSucceeded) => succeeded = true,
                Ok(_) | Err(TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(TryRecvError::Closed) => break,
            }
        }
        assert!(succeeded, "semantic HTTP command did not succeed");
        assert!(command_tx.unbounded_send(BackendCommand::Shutdown).is_ok());
        assert!(worker.join().is_ok(), "remote worker panicked");
        assert!(server.join().is_ok(), "test server panicked");
    }
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "exercise every canonical history route in one fixture"
    )]
    fn history_transport_reads_canonical_routes_without_mutation_headers_and_decodes_responses() {
        use crate::backend::{HistoryEvent, HistoryRequest};
        let run = serde_json::json!({"id":"run-1", "archived_at_utc":"now", "state":"completed", "elapsed_seconds":1, "sample_count":0});
        let cycle =
            serde_json::json!({"execution_id":"cycle-1", "sample_count":0, "child_run_count":0});
        let cases = [
            (
                HistoryRequest::RefreshRuns,
                "/api/runs",
                serde_json::to_vec(&vec![run.clone()]).expect("runs"),
            ),
            (
                HistoryRequest::LoadRun("run-1".to_owned()),
                "/api/runs/run-1",
                serde_json::to_vec(&serde_json::json!({"summary":run,"samples":[]})).expect("run"),
            ),
            (
                HistoryRequest::ExportRunCsv("run-1".to_owned()),
                "/api/runs/run-1/history.csv",
                b"original,run,csv\n".to_vec(),
            ),
            (
                HistoryRequest::RefreshCycles,
                "/api/cycles",
                serde_json::to_vec(&vec![cycle.clone()]).expect("cycles"),
            ),
            (
                HistoryRequest::LoadCycle("cycle-1".to_owned()),
                "/api/cycles/cycle-1",
                serde_json::to_vec(
                    &serde_json::json!({"summary":cycle,"samples":[],"child_runs":[]}),
                )
                .expect("cycle"),
            ),
            (
                HistoryRequest::ExportCycleCsv("cycle-1".to_owned()),
                "/api/cycles/cycle-1/history.csv",
                b"original,cycle,csv\n".to_vec(),
            ),
        ];
        for (request, path, body) in cases {
            let listener = TcpListener::bind("127.0.0.1:0").expect("mock HTTP listener");
            let address = listener.local_addr().expect("address");
            let original = body.clone();
            let worker = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept GET");
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("timeout");
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).expect("read GET");
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .expect("headers");
                stream.write_all(&body).expect("response body");
                String::from_utf8(request).expect("request text")
            });
            let urls = RemoteUrls::parse(&format!("http://{address}")).expect("URLs");
            let (tx, mut rx) = mpsc::unbounded();
            let sender = BackendEventSender::new(tx, || {});
            send_backend_command(
                BackendCommand::History(request.clone()),
                &urls,
                &ureq::agent(),
                &sender,
            );
            let event = rx.try_recv().expect("history event");
            match event {
                BackendEvent::HistoryResult {
                    request: response_request,
                    result,
                } if response_request == request => match result.expect("history result") {
                    HistoryEvent::Runs(runs) => assert_eq!(runs[0].id, "run-1"),
                    HistoryEvent::RunLoaded(history) => {
                        assert_eq!(history.summary.id, "run-1");
                    }
                    HistoryEvent::Cycles(cycles) => {
                        assert_eq!(cycles[0].execution_id, "cycle-1");
                    }
                    HistoryEvent::CycleLoaded(history) => {
                        assert_eq!(history.summary.execution_id, "cycle-1");
                    }
                    HistoryEvent::FileExported(file) => {
                        assert_eq!(file.bytes, original);
                        assert!(matches!(
                            file.filename.as_str(),
                            "run-1.csv" | "cycle-1.csv"
                        ));
                        assert_eq!(file.content_type, "text/csv");
                    }
                },
                other => panic!("unexpected history response: {other:?}"),
            }
            let raw_request = worker.join().expect("HTTP worker");
            assert!(raw_request.starts_with(&format!("GET {path} HTTP/1.1\r\n")));
            assert!(!raw_request.to_lowercase().contains("x-ebc-command"));
        }
    }
}
