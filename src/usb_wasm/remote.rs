use crate::core::{ApiCommand, AuthoritativeSnapshot};
use crate::transport::{DeviceEvent, EventSender, RemoteConnectionStatus, TransportCommand};
use futures::channel::mpsc::UnboundedReceiver;
use futures::{FutureExt as _, SinkExt as _, StreamExt as _};
use gloo_net::http::Request;
use gloo_net::websocket::{Message, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;

const MAX_RECONNECT_DELAY_MS: u32 = 15_000;
const COMMAND_HEADER: &str = "X-EBC-Command";

pub(super) async fn remote_task(
    mut cmd_rx: UnboundedReceiver<TransportCommand>,
    event_tx: EventSender,
) {
    let mut reconnect_delay_ms = 1_000;
    loop {
        send_connection(
            &event_tx,
            if reconnect_delay_ms == 1_000 {
                RemoteConnectionStatus::Connecting
            } else {
                RemoteConnectionStatus::Reconnecting
            },
        );
        let url = match websocket_url() {
            Ok(url) => url,
            Err(error) => {
                send_connection(&event_tx, RemoteConnectionStatus::Error(error));
                return;
            }
        };
        match WebSocket::open(&url) {
            Ok(socket) => {
                let (mut writer, mut reader) = socket.split();
                let mut received_snapshot = false;
                loop {
                    let message = reader.next().fuse();
                    let command = cmd_rx.next().fuse();
                    futures::pin_mut!(message, command);
                    futures::select! {
                        incoming = message => match incoming {
                            Some(Ok(Message::Text(text))) => {
                                match serde_json::from_str(&text) {
                                    Ok(event) => {
                                        if !received_snapshot {
                                            received_snapshot = true;
                                            reconnect_delay_ms = 1_000;
                                            send_connection(
                                                &event_tx,
                                                RemoteConnectionStatus::Connected,
                                            );
                                        }
                                        event_tx.send(DeviceEvent::Remote(event));
                                    }
                                    Err(error) => log::error!("invalid server websocket event: {error}"),
                                }
                            }
                            Some(Ok(Message::Bytes(_))) => {}
                            Some(Err(error)) => {
                                log::warn!("server websocket failed: {error}");
                                break;
                            }
                            None => break,
                        },
                        outgoing = command => if let Some(TransportCommand::Remote(command)) = outgoing {
                                match send_command(command).await {
                                    Ok(snapshot) => {
                                        event_tx.send(DeviceEvent::RemoteCommandSucceeded);
                                        event_tx.send(DeviceEvent::Remote(
                                            crate::core::WebSocketEvent::Update(
                                                crate::core::SnapshotUpdate::from(&snapshot),
                                            ),
                                        ));
                                    }
                                    Err(error) => {
                                        log::error!("remote command failed: {error}");
                                        event_tx.send(DeviceEvent::RemoteCommandError(error));
                                    }
                                }
                        } else {
                            let _closed = writer.close().await;
                            return;
                        },
                    }
                }
            }
            Err(error) => log::warn!("failed to open server websocket: {error}"),
        }
        send_connection(&event_tx, RemoteConnectionStatus::Reconnecting);
        TimeoutFuture::new(reconnect_delay_ms).await;
        reconnect_delay_ms = (reconnect_delay_ms * 2).min(MAX_RECONNECT_DELAY_MS);
    }
}

fn send_connection(event_tx: &EventSender, status: RemoteConnectionStatus) {
    event_tx.send(DeviceEvent::RemoteConnectionChanged(status));
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
