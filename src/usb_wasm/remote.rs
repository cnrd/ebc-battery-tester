use crate::backend::{
    BackendCapabilities, BackendCommand, BackendConnectionStatus, BackendEvent, BackendEventSender,
    BackendState, DiagnosticDirection, DiagnosticEvent, remote_api_commands,
};
use crate::core::{ApiCommand, AuthoritativeSnapshot, SnapshotUpdate, WebSocketEvent};
use futures::channel::mpsc::UnboundedReceiver;
use futures::{FutureExt as _, SinkExt as _, StreamExt as _};
use gloo_net::http::Request;
use gloo_net::websocket::{Message, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;

const MAX_RECONNECT_DELAY_MS: u32 = 15_000;
const COMMAND_HEADER: &str = "X-EBC-Command";

#[expect(clippy::too_many_lines)]
pub(super) async fn remote_task(
    mut command_rx: UnboundedReceiver<BackendCommand>,
    event_tx: BackendEventSender,
) {
    let mut reconnect_delay_ms = 1_000;
    loop {
        send_connection(
            &event_tx,
            if reconnect_delay_ms == 1_000 {
                BackendConnectionStatus::Connecting
            } else {
                BackendConnectionStatus::Reconnecting
            },
        );
        let url = match websocket_url() {
            Ok(url) => url,
            Err(error) => {
                send_connection(&event_tx, BackendConnectionStatus::Error(error));
                return;
            }
        };
        match WebSocket::open(&url) {
            Ok(socket) => {
                let (mut writer, mut reader) = socket.split();
                let mut received_snapshot = false;
                loop {
                    let message = reader.next().fuse();
                    let command = command_rx.next().fuse();
                    futures::pin_mut!(message, command);
                    futures::select! {
                        incoming = message => match incoming {
                            Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                                Ok(event) => {
                                    if !received_snapshot {
                                        received_snapshot = true;
                                        reconnect_delay_ms = 1_000;
                                        send_connection(&event_tx, BackendConnectionStatus::Connected);
                                    }
                                    publish_websocket(event, &event_tx);
                                }
                                Err(error) => log::error!("invalid server websocket event: {error}"),
                            },
                            Some(Ok(Message::Bytes(_))) => {}
                            Some(Err(error)) => {
                                log::warn!("server websocket failed: {error}");
                                break;
                            }
                            None => break,
                        },
                        command = command => {
                            let Some(command) = command else {
                                let _closed = writer.close().await;
                                return;
                            };
                            if matches!(command, BackendCommand::Shutdown) {
                                let _closed = writer.close().await;
                                return;
                            }
                            for command in remote_api_commands(command) {
                                event_tx.send(BackendEvent::Diagnostic(DiagnosticEvent {
                                    direction: DiagnosticDirection::Out,
                                    label: format!("{command:?}"),
                                    raw_bytes: Vec::new(),
                                }));
                                match send_command(command).await {
                                    Ok(snapshot) => {
                                        event_tx.send(BackendEvent::CommandSucceeded);
                                        publish_update(SnapshotUpdate::from(&snapshot), &event_tx);
                                    }
                                    Err(error) => {
                                        log::error!("remote command failed: {error}");
                                        event_tx.send(BackendEvent::CommandError(error));
                                    }
                                }
                            }
                        },
                    }
                }
            }
            Err(error) => log::warn!("failed to open server websocket: {error}"),
        }
        send_connection(&event_tx, BackendConnectionStatus::Reconnecting);
        let timeout = TimeoutFuture::new(reconnect_delay_ms).fuse();
        futures::pin_mut!(timeout);
        loop {
            let command = command_rx.next().fuse();
            futures::pin_mut!(command);
            futures::select_biased! {
                command = command => match command {
                    Some(BackendCommand::Shutdown) | None => return,
                    Some(_) => event_tx.send(BackendEvent::CommandError(
                        "browser is disconnected; command was not sent".to_owned(),
                    )),
                },
                () = timeout => break,
            }
        }
        while let Ok(command) = command_rx.try_recv() {
            if matches!(command, BackendCommand::Shutdown) {
                return;
            }
            event_tx.send(BackendEvent::CommandError(
                "browser is disconnected; command was not sent".to_owned(),
            ));
        }
        reconnect_delay_ms = (reconnect_delay_ms * 2).min(MAX_RECONNECT_DELAY_MS);
    }
}

fn publish_websocket(event: WebSocketEvent, event_tx: &BackendEventSender) {
    match event {
        WebSocketEvent::Snapshot(snapshot) => {
            let update = SnapshotUpdate::from(&snapshot);
            event_tx.send(BackendEvent::Snapshot {
                capabilities: BackendCapabilities::from_remote(&update),
                snapshot,
            });
        }
        WebSocketEvent::Update(update) => publish_update(update, event_tx),
        WebSocketEvent::Sample(sample) => event_tx.send(BackendEvent::Sample(sample)),
    }
}

fn publish_update(update: SnapshotUpdate, event_tx: &BackendEventSender) {
    event_tx.send(BackendEvent::Update(BackendState {
        capabilities: BackendCapabilities::from_remote(&update),
        update,
    }));
}

fn send_connection(event_tx: &BackendEventSender, status: BackendConnectionStatus) {
    event_tx.send(BackendEvent::BackendConnectionChanged(status));
}

fn websocket_url() -> Result<String, String> {
    let window = web_sys::window().ok_or_else(|| "browser window is unavailable".to_owned())?;
    let location = window.location();
    let protocol = location
        .protocol()
        .map_err(|error| format!("failed to read page protocol: {error:?}"))?;
    let host = location
        .host()
        .map_err(|error| format!("failed to read page host: {error:?}"))?;
    let websocket_protocol = if protocol == "https:" { "wss" } else { "ws" };
    Ok(format!("{websocket_protocol}://{host}/api/ws"))
}

async fn send_command(command: ApiCommand) -> Result<AuthoritativeSnapshot, String> {
    let response = match command {
        ApiCommand::Connect => {
            Request::post("/api/connect")
                .header(COMMAND_HEADER, "1")
                .send()
                .await
        }
        ApiCommand::Disconnect => {
            Request::post("/api/disconnect")
                .header(COMMAND_HEADER, "1")
                .send()
                .await
        }
        ApiCommand::Start(config) => {
            Request::post("/api/test/start")
                .header(COMMAND_HEADER, "1")
                .json(&config)
                .map_err(|error| error.to_string())?
                .send()
                .await
        }
        ApiCommand::Adjust(config) => {
            Request::post("/api/test/adjust")
                .header(COMMAND_HEADER, "1")
                .json(&config)
                .map_err(|error| error.to_string())?
                .send()
                .await
        }
        ApiCommand::Stop => {
            Request::post("/api/test/stop")
                .header(COMMAND_HEADER, "1")
                .send()
                .await
        }
        ApiCommand::Resume => {
            Request::post("/api/test/resume")
                .header(COMMAND_HEADER, "1")
                .send()
                .await
        }
        ApiCommand::Calibration(calibration) => {
            Request::post("/api/calibration")
                .header(COMMAND_HEADER, "1")
                .json(&calibration)
                .map_err(|error| error.to_string())?
                .send()
                .await
        }
    }
    .map_err(|error| error.to_string())?;
    if !response.ok() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body}"));
    }
    response.json().await.map_err(|error| error.to_string())
}
