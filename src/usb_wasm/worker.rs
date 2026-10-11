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
use wasm_bindgen_futures::JsFuture;

const INBOUND_BUFFER_SIZE: u32 = 64;
const BACKEND_TICK_MS: u32 = 100;

fn advance_generation(generation: &mut u64) {
    *generation = generation
        .checked_add(1)
        .unwrap_or_else(|| panic!("WebUSB connection identities exhausted"));
}

enum InputEvent {
    Frame {
        generation: u64,
        received_at: web_time::Instant,
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
    let mut last_service = connection::ClockSample::now();

    loop {
        let mut available = check_clock(&mut last_service, &mut backend);
        available.extend(reconcile_prefix(
            &mut input_rx,
            None,
            &mut backend,
            generation,
        ));
        available.extend(check_clock(&mut last_service, &mut backend));
        publish(
            &mut input_rx,
            available,
            &mut backend,
            &mut generation,
            &mut device,
            &mut out_endpoint_num,
            &mut stop_reading_tx,
            &event_tx,
        )
        .await;
        let command = command_rx.next().fuse();
        let input = input_rx.next().fuse();
        let tick = TimeoutFuture::new(BACKEND_TICK_MS).fuse();
        futures::pin_mut!(command, input, tick);
        futures::select! {
            command = command => {
                let Some(command) = command else { break };
                TimeoutFuture::new(0).await;
                let mut available = check_clock(&mut last_service, &mut backend);
                available.extend(reconcile_prefix(&mut input_rx, None, &mut backend, generation));
                available.extend(check_clock(&mut last_service, &mut backend));
                publish(&mut input_rx, available, &mut backend, &mut generation, &mut device, &mut out_endpoint_num, &mut stop_reading_tx, &event_tx).await;
                match command {
                    BackendCommand::RefreshDevices => super::enumerate_devices(&event_tx).await,
                    BackendCommand::Connect(index) => {
                        advance_generation(&mut generation);
                        if device.is_some() {
                            let _retired = disconnect_device(
                                backend.request_disconnect(),
                                &mut backend,
                                &mut device,
                                &mut out_endpoint_num,
                                &mut stop_reading_tx,
                                &event_tx,
                            ).await;
                        }
                        publish(
                            &mut input_rx,
                            backend.begin_connection(),
                            &mut backend,
                            &mut generation,
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
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
                                publish(
                                    &mut input_rx,
                                    backend.connection_established(),
                                    &mut backend,
                                    &mut generation,
                                    &mut device,
                                    &mut out_endpoint_num,
                                    &mut stop_reading_tx,
                                    &event_tx,
                                ).await;
                            }
                            Err(error) => {
                                publish(
                                    &mut input_rx,
                                    backend.connection_failed(error),
                                    &mut backend,
                                    &mut generation,
                                    &mut device,
                                    &mut out_endpoint_num,
                                    &mut stop_reading_tx,
                                    &event_tx,
                                ).await;
                            }
                        }
                    }
                    BackendCommand::Disconnect => {
                        advance_generation(&mut generation);
                        let retirement = disconnect_device(
                            backend.request_disconnect(),
                            &mut backend,
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                        let output = match retirement {
                            Ok(()) => backend.disconnected(),
                            Err(error) => backend.connection_failed(error),
                        };
                        publish(
                            &mut input_rx,
                            output,
                            &mut backend,
                            &mut generation,
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                    }
                    BackendCommand::Api(command) => {
                        publish(
                            &mut input_rx,
                            backend.command(command),
                            &mut backend,
                            &mut generation,
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                    }
                    BackendCommand::StartTest(request) => {
                        publish(
                            &mut input_rx,
                            backend.start_test(request),
                            &mut backend,
                            &mut generation,
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                    }
                    BackendCommand::Resume(config) => {
                        publish(
                            &mut input_rx,
                            backend.resume(config),
                            &mut backend,
                            &mut generation,
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                    }
                    BackendCommand::StartCycle(request) => {
                        publish(
                            &mut input_rx,
                            backend.start_cycle(request),
                            &mut backend,
                            &mut generation,
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                    }
                    BackendCommand::StartSavedRecipeSnapshot {
                        recipe,
                        reference,
                        execution_name,
                    } => {
                        publish(
                            &mut input_rx,
                            backend.start_saved_recipe(recipe, reference, execution_name),
                            &mut backend,
                            &mut generation,
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                    }
                    BackendCommand::History(_)
                    | BackendCommand::StartSavedRecipe { .. }
                    | BackendCommand::RefreshRecipes
                    | BackendCommand::CreateSavedRecipe(_)
                    | BackendCommand::UpdateSavedRecipe { .. }
                    | BackendCommand::DeleteSavedRecipe { .. }
                    | BackendCommand::ImportRecipe(_)
                    | BackendCommand::ExportRecipe { .. } => {
                        event_tx.send(BackendEvent::CommandError(
                            "remote recipe command sent to local backend".to_owned(),
                        ));
                    }
                    BackendCommand::RenameRun { run_id, request } => {
                        publish(
                            &mut input_rx,
                            backend.rename_run(&run_id, request),
                            &mut backend,
                            &mut generation,
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                    }
                    BackendCommand::RenameCycle { execution_id, request } => {
                        publish(
                            &mut input_rx,
                            backend.rename_cycle(&execution_id, request),
                            &mut backend,
                            &mut generation,
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                    }
                    BackendCommand::StopCycle => {
                        publish(
                            &mut input_rx,
                            backend.stop_cycle(),
                            &mut backend,
                            &mut generation,
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                    }
                    BackendCommand::Shutdown => {
                        let _retired = disconnect_device(
                            backend.shutdown(),
                            &mut backend,
                            &mut device,
                            &mut out_endpoint_num,
                            &mut stop_reading_tx,
                            &event_tx,
                        ).await;
                        break;
                    }
                }
            }
            input = input => {
                if input.is_none() { break; }
                let mut available = check_clock(&mut last_service, &mut backend);
                available.extend(reconcile_prefix(&mut input_rx, input, &mut backend, generation));
                available.extend(check_clock(&mut last_service, &mut backend));
                publish(&mut input_rx, available, &mut backend, &mut generation, &mut device, &mut out_endpoint_num, &mut stop_reading_tx, &event_tx).await;
            },
            () = tick => {
                let mut available = check_clock(&mut last_service, &mut backend);
                available.extend(reconcile_prefix(&mut input_rx, None, &mut backend, generation));
                available.extend(check_clock(&mut last_service, &mut backend));
                publish(&mut input_rx, available, &mut backend, &mut generation, &mut device, &mut out_endpoint_num, &mut stop_reading_tx, &event_tx).await;
                publish(
                    &mut input_rx,
                    backend.tick(),
                    &mut backend,
                    &mut generation,
                    &mut device,
                    &mut out_endpoint_num,
                    &mut stop_reading_tx,
                    &event_tx,
                ).await;
            }
        }
    }
}

fn check_clock(last: &mut connection::ClockSample, backend: &mut LocalBackend) -> LocalOutput {
    let now = connection::ClockSample::now();
    let uncertain = now.discontinuity_since(*last);
    *last = now;
    if uncertain {
        backend.connection_failed(
            "WebUSB host-clock discontinuity; input provenance is uncertain".to_owned(),
        )
    } else {
        LocalOutput::default()
    }
}

fn reconcile_prefix(
    input_rx: &mut UnboundedReceiver<InputEvent>,
    first: Option<InputEvent>,
    backend: &mut LocalBackend,
    generation: u64,
) -> LocalOutput {
    // Establish the finite prefix before processing anything or awaiting output.
    let mut prefix: Vec<_> = first.into_iter().collect();
    while let Ok(input) = input_rx.try_recv() {
        prefix.push(input);
    }
    if prefix.is_empty() {
        return LocalOutput::default();
    }
    let mut output = LocalOutput::default();
    backend.begin_receive_prefix();
    for input in prefix {
        match input {
            InputEvent::Frame {
                generation: incoming,
                received_at,
                frame,
                raw,
            } if incoming == generation => {
                output.extend(backend.frame_received_at(frame, raw, received_at));
            }
            InputEvent::Error {
                generation: incoming,
                message,
            } if incoming == generation => {
                output.extend(backend.connection_failed(message));
            }
            _ => {}
        }
    }
    output.extend(backend.finish_receive_prefix());
    output
}

async fn disconnect_device(
    output: LocalOutput,
    backend: &mut LocalBackend,
    device: &mut Option<web_sys::UsbDevice>,
    out_endpoint_num: &mut Option<u8>,
    stop_reading_tx: &mut Option<oneshot::Sender<()>>,
    event_tx: &BackendEventSender,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for event in output.events {
        event_tx.send(event);
    }
    for send in output.sends {
        let (authorization, allowed) = backend.authorize_send(send);
        for event in authorization.events {
            event_tx.send(event);
        }
        if !allowed {
            errors.push("cleanup command was retired before the wire boundary".to_owned());
            continue;
        }
        let frame = send.frame();
        event_tx.send(outgoing(frame));
        let result = if let (Some(current), Some(endpoint)) = (device.as_ref(), *out_endpoint_num) {
            let result = match frame {
                OutboundFrame::Stop => connection::stop(current, endpoint).await,
                OutboundFrame::Disconnect => {
                    if let Some(stop_tx) = stop_reading_tx.take() {
                        let _stopped = stop_tx.send(());
                    }
                    connection::disconnect(current, endpoint).await
                }
                _ => connection::send_frame(current, endpoint, frame).await,
            };
            result.map_err(|error| format!("{error:?}"))
        } else {
            Err("WebUSB device is not open".to_owned())
        };
        if let Err(error) = &result {
            log::error!("Failed to send {frame:?}: {error}");
            errors.push(error.clone());
            if matches!(frame, OutboundFrame::Stop) {
                if let Some(current) = device.take()
                    && let Err(close_error) = connection::close(&current).await
                {
                    errors.push(close_error);
                }
                *out_endpoint_num = None;
            }
        }
        for event in backend.finish_send(send, result).events {
            event_tx.send(event);
        }
    }
    *device = None;
    *out_endpoint_num = None;
    if let Some(stop_tx) = stop_reading_tx.take() {
        let _stopped = stop_tx.send(());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "one worker owns input, output, retirement and backend state"
)]
async fn publish(
    input_rx: &mut UnboundedReceiver<InputEvent>,
    output: LocalOutput,
    backend: &mut LocalBackend,
    generation: &mut u64,
    device: &mut Option<web_sys::UsbDevice>,
    out_endpoint_num: &mut Option<u8>,
    stop_reading_tx: &mut Option<oneshot::Sender<()>>,
    event_tx: &BackendEventSender,
) {
    let mut last_service = connection::ClockSample::now();
    publish_events(
        output.events,
        generation,
        device,
        out_endpoint_num,
        stop_reading_tx,
        event_tx,
    )
    .await;
    let mut sends = std::collections::VecDeque::from(output.sends);
    while let Some(send) = sends.pop_front() {
        if !matches!(send.frame(), OutboundFrame::Stop) {
            // Let already-runnable input continuations reach the finite queue
            // watermark. This yields once, never waits for a future report.
            TimeoutFuture::new(0).await;
            let mut available = check_clock(&mut last_service, backend);
            available.extend(reconcile_prefix(input_rx, None, backend, *generation));
            available.extend(check_clock(&mut last_service, backend));
            publish_events(
                available.events,
                generation,
                device,
                out_endpoint_num,
                stop_reading_tx,
                event_tx,
            )
            .await;
            sends.extend(available.sends);
        }
        let (authorization, allowed) = backend.authorize_send(send);
        for event in authorization.events {
            event_tx.send(event);
        }
        if !allowed {
            continue;
        }
        let frame = send.frame();
        event_tx.send(outgoing(frame));
        let result = if let (Some(current), Some(endpoint)) = (device.as_ref(), *out_endpoint_num) {
            connection::send_frame(current, endpoint, frame)
                .await
                .map_err(|error| format!("{error:?}"))
        } else {
            Err("WebUSB device is not open".to_owned())
        };
        if let Err(error) = &result {
            log::error!("Failed to send {frame:?}: {error}");
        }
        let failed = result.is_err();
        if !failed
            && matches!(
                frame,
                OutboundFrame::AdjustConstantCurrentDischarge(..)
                    | OutboundFrame::CalibrateVoltageLow(_)
                    | OutboundFrame::CalibrateVoltageHigh(_)
                    | OutboundFrame::CalibrateCurrentLow(_)
                    | OutboundFrame::CalibrateCurrentHigh(_)
                    | OutboundFrame::CalibrateConfirm
                    | OutboundFrame::TimerSync(_)
            )
        {
            let available = reconcile_prefix(input_rx, None, backend, *generation);
            publish_events(
                available.events,
                generation,
                device,
                out_endpoint_num,
                stop_reading_tx,
                event_tx,
            )
            .await;
            sends.extend(available.sends);
        }
        for event in backend.finish_send(send, result).events {
            event_tx.send(event);
        }
        if failed {
            advance_generation(generation);
            if let Some(stop_tx) = stop_reading_tx.take() {
                let _stopped = stop_tx.send(());
            }
            if let Some(current) = device.take()
                && let Err(error) = connection::close(&current).await
            {
                log::error!("Failed to close WebUSB device after write error: {error:?}");
                for event in backend
                    .connection_failed(format!("WebUSB resource retirement failed: {error}"))
                    .events
                {
                    event_tx.send(event);
                }
            }
            *out_endpoint_num = None;
        }
    }
}

async fn publish_events(
    events: Vec<BackendEvent>,
    generation: &mut u64,
    device: &mut Option<web_sys::UsbDevice>,
    out_endpoint_num: &mut Option<u8>,
    stop_reading_tx: &mut Option<oneshot::Sender<()>>,
    event_tx: &BackendEventSender,
) {
    for event in events {
        if matches!(&event, BackendEvent::Update(state) if state.update.connection == crate::core::ServerConnectionState::Error)
        {
            advance_generation(generation);
            if let Some(stop_tx) = stop_reading_tx.take() {
                let _stopped = stop_tx.send(());
            }
            if let Some(current) = device.take()
                && let Err(error) = connection::close(&current).await
            {
                event_tx.send(BackendEvent::CommandError(format!(
                    "WebUSB resource retirement failed: {error}"
                )));
            }
            *out_endpoint_num = None;
        }
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

async fn reading_task(
    device: web_sys::UsbDevice,
    in_endpoint: u8,
    input_tx: UnboundedSender<InputEvent>,
    mut stop_reading_rx: oneshot::Receiver<()>,
    generation: u64,
) {
    let mut buffer = crate::device::ReceiveBuffer::default();
    loop {
        // transferIn has no device timestamp. Its issuance is an earlier,
        // conservative bound; a delayed continuation cannot mint a new receipt.
        let started = connection::ClockSample::now();
        let received_at = started.before;
        let transfer = JsFuture::from(device.transfer_in(in_endpoint, INBOUND_BUFFER_SIZE)).fuse();
        futures::pin_mut!(transfer);
        futures::select! {
            result = transfer => match result {
                Ok(value) => {
                    let result: web_sys::UsbInTransferResult = value.unchecked_into();
                    if result.status() != web_sys::UsbTransferStatus::Ok {
                        input_tx.unbounded_send(InputEvent::Error {
                            generation,
                            message: format!(
                                "WebUSB read returned {:?}: connection lost",
                                result.status()
                            ),
                        }).ok();
                        break;
                    }
                    if let Some(data) = result.data() {
                        let now = connection::ClockSample::now();
                        if now.before >= received_at + crate::controller::REPORT_FRESHNESS_TIMEOUT || now.discontinuity_since(started) {
                            input_tx.unbounded_send(InputEvent::Error { generation, message: "WebUSB receipt provenance uncertain after delayed input continuation".to_owned() }).ok();
                            return;
                        }
                        let bytes = js_sys::Uint8Array::new(&data.buffer()).subarray(data.byte_offset() as u32, (data.byte_offset() + data.byte_length()) as u32).to_vec();
                        for item in buffer.receive(&bytes, received_at) {
                            input_tx.unbounded_send(InputEvent::Frame {
                                generation,
                                received_at: item.received_at,
                                frame: item.frame,
                                raw: item.raw,
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
