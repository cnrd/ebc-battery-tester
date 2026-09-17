use super::connection;
use crate::backend::{
    BackendCommand, BackendEvent, BackendEventSender, DiagnosticDirection, DiagnosticEvent,
};
use crate::device::{InboundFrame, OUTBOUND_FRAME_SIZE, OutboundFrame};
use crate::local_backend::{LocalBackend, LocalOutput};
use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use gloo_timers::future::TimeoutFuture;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

const INBOUND_BUFFER_SIZE: u32 = 64;
const BACKEND_TICK_MS: u32 = 100;

enum InputEvent {
    Frame {
        generation: u64,
        frame: InboundFrame,
        raw: Vec<u8>,
    },
    Error {
        generation: u64,
        message: String,
    },
}

#[expect(clippy::too_many_lines)]
pub(super) async fn local_backend_task(
    mut command_rx: UnboundedReceiver<BackendCommand>,
    event_tx: BackendEventSender,
) {
    let mut backend = LocalBackend::default();
    let (input_tx, mut input_rx) = mpsc::unbounded();
    let mut stop_reading_tx: Option<oneshot::Sender<()>> = None;
    let mut device: Option<web_sys::UsbDevice> = None;
    let mut out_endpoint_num: Option<u8> = None;
    let mut generation = 0_u64;

    loop {
        let command = command_rx.next().fuse();
        let input = input_rx.next().fuse();
        let tick = TimeoutFuture::new(BACKEND_TICK_MS).fuse();
        futures::pin_mut!(command, input, tick);
        futures::select! {
            command = command => {
                let Some(command) = command else { break };
                match command {
                    BackendCommand::RefreshDevices => super::enumerate_devices(&event_tx).await,
                    BackendCommand::Connect(index) => {
                        generation = generation.wrapping_add(1);
                        if device.is_some() {
                            disconnect_device(
                                LocalBackend::safe_disconnect(),
                                &mut device,
                                &mut out_endpoint_num,
                                &mut stop_reading_tx,
                                &event_tx,
                            ).await;
                        }
                        publish(backend.begin_connection(), device.as_ref(), out_endpoint_num, &event_tx).await;
                        event_tx.send(outgoing(OutboundFrame::Connect(index)));
                        match connection::connect(index).await {
                            Ok(state) => {
                                let dev = state.device;
                                out_endpoint_num = Some(state.out_endpoint_num);
                                let (stop_tx, stop_rx) = oneshot::channel();
                                stop_reading_tx = Some(stop_tx);
                                wasm_bindgen_futures::spawn_local(reading_task(
                                    dev.clone(),
                                    state.in_endpoint_num,
                                    input_tx.clone(),
                                    stop_rx,
                                    generation,
                                ));
                                device = Some(dev);
                                publish(backend.connection_established(), device.as_ref(), out_endpoint_num, &event_tx).await;
                            }
                            Err(error) => {
                                publish(backend.connection_failed(error), device.as_ref(), out_endpoint_num, &event_tx).await;
                            }
                        }
                    }
                    BackendCommand::Disconnect => {
                        generation = generation.wrapping_add(1);
                        disconnect_device(
                            LocalBackend::safe_disconnect(),
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                        publish(backend.disconnected(), None, None, &event_tx).await;
                    }
                    BackendCommand::Api(command) => {
                        publish(backend.command(command), device.as_ref(), out_endpoint_num, &event_tx).await;
                    }
                    BackendCommand::Resume(config) => {
                        publish(backend.resume(config), device.as_ref(), out_endpoint_num, &event_tx).await;
                    }
                    BackendCommand::Shutdown => {
                        disconnect_device(
                            backend.shutdown(),
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                        break;
                    }
                }
            }
            input = input => match input {
                Some(InputEvent::Frame { generation: event_generation, frame, raw })
                    if event_generation == generation => {
                    publish(backend.frame(frame, raw), device.as_ref(), out_endpoint_num, &event_tx).await;
                }
                Some(InputEvent::Error { generation: event_generation, message })
                    if event_generation == generation => {
                    if let Some(current) = device.take()
                        && let Err(error) = JsFuture::from(current.close()).await
                    {
                        log::error!("Failed to close WebUSB device after read error: {error:?}");
                    }
                    out_endpoint_num = None;
                    stop_reading_tx = None;
                    publish(backend.connection_failed(message), None, None, &event_tx).await;
                }
                Some(InputEvent::Frame { .. } | InputEvent::Error { .. }) => {}
                None => break,
            },
            () = tick => {
                publish(backend.tick(), device.as_ref(), out_endpoint_num, &event_tx).await;
            }
        }
    }
}

async fn disconnect_device(
    output: LocalOutput,
    device: &mut Option<web_sys::UsbDevice>,
    out_endpoint_num: &mut Option<u8>,
    stop_reading_tx: &mut Option<oneshot::Sender<()>>,
    event_tx: &BackendEventSender,
) {
    for frame in output.frames {
        event_tx.send(outgoing(frame));
        if let (Some(current), Some(endpoint)) = (device.as_ref(), *out_endpoint_num) {
            let result = match frame {
                OutboundFrame::Stop => connection::stop(current, endpoint).await,
                OutboundFrame::Disconnect => {
                    if let Some(stop_tx) = stop_reading_tx.take() {
                        let _stopped = stop_tx.send(());
                    }
                    connection::disconnect(current, endpoint).await
                }
                _ => send_frame(current, endpoint, frame).await,
            };
            if let Err(error) = result {
                log::error!("Failed to send {frame:?}: {error:?}");
            }
        }
    }
    for event in output.events {
        event_tx.send(event);
    }
    *device = None;
    *out_endpoint_num = None;
}

async fn publish(
    output: LocalOutput,
    device: Option<&web_sys::UsbDevice>,
    out_endpoint_num: Option<u8>,
    event_tx: &BackendEventSender,
) {
    for frame in output.frames {
        event_tx.send(outgoing(frame));
        if let (Some(device), Some(endpoint)) = (device, out_endpoint_num)
            && let Err(error) = send_frame(device, endpoint, frame).await
        {
            log::error!("Failed to send {frame:?}: {error:?}");
        }
    }
    for event in output.events {
        event_tx.send(event);
    }
}

fn outgoing(frame: OutboundFrame) -> BackendEvent {
    BackendEvent::Diagnostic(DiagnosticEvent {
        direction: DiagnosticDirection::Out,
        label: format!("{frame:?}"),
        raw_bytes: <[u8; OUTBOUND_FRAME_SIZE]>::from(frame).to_vec(),
    })
}

async fn send_frame(
    device: &web_sys::UsbDevice,
    out_endpoint_num: u8,
    frame: OutboundFrame,
) -> Result<(), JsValue> {
    let mut bytes: [u8; OUTBOUND_FRAME_SIZE] = frame.into();
    let promise = device
        .transfer_out_with_u8_slice(out_endpoint_num, &mut bytes)
        .map_err(|error| format!("Failed to start transfer: {error:?}"))?;
    JsFuture::from(promise)
        .await
        .map_err(|error| format!("Frame send failed: {error:?}"))?;
    Ok(())
}

async fn reading_task(
    device: web_sys::UsbDevice,
    in_endpoint: u8,
    input_tx: UnboundedSender<InputEvent>,
    mut stop_reading_rx: oneshot::Receiver<()>,
    generation: u64,
) {
    let mut buffer = Vec::new();
    loop {
        let transfer = JsFuture::from(device.transfer_in(in_endpoint, INBOUND_BUFFER_SIZE)).fuse();
        futures::pin_mut!(transfer);
        futures::select! {
            result = transfer => match result {
                Ok(value) => {
                    let result: web_sys::UsbInTransferResult = value.unchecked_into();
                    if let Some(data) = result.data() {
                        buffer.extend_from_slice(&js_sys::Uint8Array::new(&data.buffer()).to_vec());
                        for (frame, raw) in crate::device::process_buffer(&mut buffer) {
                            input_tx.unbounded_send(InputEvent::Frame {
                                generation,
                                frame,
                                raw,
                            }).ok();
                        }
                    }
                }
                Err(error) => {
                    log::error!("Bulk IN transfer failed: {error:?}");
                    input_tx.unbounded_send(InputEvent::Error {
                        generation,
                        message: "Read error: connection lost".to_owned(),
                    }).ok();
                    return;
                }
            },
            _ = stop_reading_rx => return,
        }
    }
}
