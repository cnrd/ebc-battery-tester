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
use crate::core::{ApiCommand, AuthoritativeSnapshot, WebSocketEvent};
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
                    let mut received_snapshot = false;
                    loop {
                        if received_snapshot {
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
                                    Ok(event) => {
                                        if matches!(event, WebSocketEvent::Snapshot(_)) {
                                            received_snapshot = true;
                                            connected_once = true;
                                            reconnect_delay_ms = INITIAL_RECONNECT_DELAY_MS;
                                            event_tx.send(BackendEvent::BackendConnectionChanged(
                                                BackendConnectionStatus::Connected,
                                            ));
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

fn send_backend_command(
    command: BackendCommand,
    urls: &RemoteUrls,
    agent: &ureq::Agent,
    event_tx: &BackendEventSender,
) {
    match &command {
        BackendCommand::StartCycle(recipe) => {
            event_tx.send(BackendEvent::Diagnostic(DiagnosticEvent {
                direction: DiagnosticDirection::Out,
                label: "StartCycle".to_owned(),
                raw_bytes: Vec::new(),
            }));
            let request = agent
                .post(&format!("{}/api/cycle/start", urls.base))
                .set(COMMAND_HEADER, "1");
            let result = request
                .send_json(recipe)
                .map_err(format_http_error)
                .and_then(|response| {
                    response
                        .into_json::<AuthoritativeSnapshot>()
                        .map_err(|error| format!("invalid server response: {error}"))
                });
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

fn send_command(
    command: ApiCommand,
    urls: &RemoteUrls,
    agent: &ureq::Agent,
) -> Result<AuthoritativeSnapshot, String> {
    let request = agent.post(&urls.endpoint(command)).set(COMMAND_HEADER, "1");
    let response = match command {
        ApiCommand::Start(config) | ApiCommand::Adjust(config) => request.send_json(config),
        ApiCommand::Calibration(calibration) => request.send_json(calibration),
        ApiCommand::Connect | ApiCommand::Disconnect | ApiCommand::Stop | ApiCommand::Resume => {
            request.call()
        }
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
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    use futures::channel::mpsc;

    use super::*;

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

            let (mut http, _) = listener
                .accept()
                .unwrap_or_else(|error| panic!("failed to accept HTTP command: {error}"));
            http.set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap_or_else(|error| panic!("failed to set HTTP timeout: {error}"));
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
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
}
