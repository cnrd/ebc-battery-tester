#[cfg(test)]
use std::io::Read as _;
#[cfg(test)]
use std::io::Write as _;

use crate::backend::{
    BackendCommand, BackendEvent, BackendEventSender, DiagnosticDirection, DiagnosticEvent,
};
use crate::device::{OUTBOUND_FRAME_SIZE, OutboundFrame, UsbDeviceInfo};
use crate::local_backend::{LocalBackend, LocalOutput};
use futures::channel::mpsc::UnboundedReceiver;
use serialport::SerialPortType;

const BAUD_RATE: u32 = 9600;
const SLEEP_DURATION: std::time::Duration = std::time::Duration::from_millis(10);

fn available_devices() -> Vec<UsbDeviceInfo> {
    let all_ports = serialport::available_ports().unwrap_or_default();
    log::debug!("All serial ports: {all_ports:?}");
    all_ports
        .into_iter()
        .filter_map(|p| {
            if let SerialPortType::UsbPort(usb) = p.port_type
                && usb.vid == crate::device::VENDOR_ID
            {
                return Some(UsbDeviceInfo {
                    product_name: usb.product.unwrap_or_default(),
                    manufacturer_name: usb.manufacturer.unwrap_or_default(),
                    vendor_id: usb.vid,
                    product_id: usb.pid,
                });
            }
            None
        })
        .collect()
}

pub fn spawn_backend(
    command_rx: UnboundedReceiver<BackendCommand>,
    event_tx: BackendEventSender,
) -> Result<std::thread::JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("ebc-local-backend".to_owned())
        .spawn(move || backend_thread(command_rx, event_tx))
        .map_err(|error| format!("failed to spawn local backend thread: {error}"))
}

fn find_ch340_port(idx: usize) -> Option<String> {
    serialport::available_ports()
        .ok()?
        .into_iter()
        .filter(|p| matches!(&p.port_type, SerialPortType::UsbPort(usb) if usb.vid == crate::device::VENDOR_ID))
        .nth(idx)
        .map(|p| p.port_name)
}

fn connect(idx: usize) -> Result<Box<dyn serialport::SerialPort>, String> {
    let name = find_ch340_port(idx).ok_or_else(|| format!("No CH340 device at index {idx}"))?;
    let mut port = serialport::new(&name, BAUD_RATE)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::Odd)
        .stop_bits(serialport::StopBits::One)
        .timeout(std::time::Duration::from_millis(10))
        .open()
        .map_err(|e| format!("Failed to open {name}: {e}"))?;
    crate::transport_time::write_frame(port.as_mut(), OutboundFrame::Connect(idx))?;
    Ok(port)
}

fn backend_thread(command_rx: UnboundedReceiver<BackendCommand>, event_tx: BackendEventSender) {
    backend_runtime(command_rx, event_tx, None);
}

#[expect(clippy::needless_pass_by_value)]
#[expect(
    clippy::too_many_lines,
    reason = "single native executor owns physical command and receive boundaries"
)]
fn backend_runtime(
    mut command_rx: UnboundedReceiver<BackendCommand>,
    event_tx: BackendEventSender,
    mut port: Option<Box<dyn serialport::SerialPort>>,
) {
    let mut backend = LocalBackend::default();
    let mut ingress = crate::device::SerialIngress::default();
    let mut ingress_context = (0, None);
    // A supplied port is a software-test seam for opening, not an alternate
    // controller/queue/receive implementation. Production starts with None.
    if port.is_some() {
        publish(
            backend.begin_connection(),
            &mut port,
            &event_tx,
            &mut backend,
        );
        publish(
            backend.connection_established(),
            &mut port,
            &event_tx,
            &mut backend,
        );
    }

    'runtime: loop {
        let mut publish = |output,
                           port: &mut Option<Box<dyn serialport::SerialPort>>,
                           events: &BackendEventSender,
                           backend: &mut LocalBackend| {
            publish_serial(
                output,
                port,
                events,
                backend,
                &mut ingress,
                &mut ingress_context,
            );
        };
        publish(LocalOutput::default(), &mut port, &event_tx, &mut backend);
        loop {
            let command = command_rx.try_recv();
            if command.as_ref().is_ok_and(|command| {
                !matches!(
                    command,
                    BackendCommand::Api(crate::core::ApiCommand::Stop)
                        | BackendCommand::StopCycle
                        | BackendCommand::Disconnect
                        | BackendCommand::Shutdown
                )
            }) {
                publish(LocalOutput::default(), &mut port, &event_tx, &mut backend);
            }
            match command {
                Ok(BackendCommand::RefreshDevices) => {
                    event_tx.send(BackendEvent::DevicesUpdated(available_devices()));
                }
                Ok(BackendCommand::Connect(idx)) => {
                    retire_existing_port(&mut backend, &mut port, &event_tx);
                    publish(
                        backend.begin_connection(),
                        &mut port,
                        &event_tx,
                        &mut backend,
                    );
                    event_tx.send(outgoing(OutboundFrame::Connect(idx)));
                    match connect(idx) {
                        Ok(p) => {
                            port = Some(p);
                            publish(
                                backend.connection_established(),
                                &mut port,
                                &event_tx,
                                &mut backend,
                            );
                        }
                        Err(e) => {
                            log::error!("Failed to connect: {e}");
                            publish(
                                backend.connection_failed(e),
                                &mut port,
                                &event_tx,
                                &mut backend,
                            );
                        }
                    }
                }
                Ok(BackendCommand::Disconnect) => {
                    publish(
                        backend.request_disconnect(),
                        &mut port,
                        &event_tx,
                        &mut backend,
                    );
                    port = None;
                    publish(backend.disconnected(), &mut port, &event_tx, &mut backend);
                }
                Ok(BackendCommand::Api(command)) => {
                    publish(backend.command(command), &mut port, &event_tx, &mut backend);
                }
                Ok(BackendCommand::StartTest(request)) => {
                    publish(
                        backend.start_test(request),
                        &mut port,
                        &event_tx,
                        &mut backend,
                    );
                }
                Ok(BackendCommand::Resume(config)) => {
                    publish(backend.resume(config), &mut port, &event_tx, &mut backend);
                }
                Ok(BackendCommand::StartCycle(request)) => {
                    publish(
                        backend.start_cycle(request),
                        &mut port,
                        &event_tx,
                        &mut backend,
                    );
                }
                Ok(BackendCommand::StartSavedRecipeSnapshot {
                    recipe,
                    reference,
                    execution_name,
                }) => {
                    publish(
                        backend.start_saved_recipe(recipe, reference, execution_name),
                        &mut port,
                        &event_tx,
                        &mut backend,
                    );
                }
                Ok(
                    BackendCommand::History(_)
                    | BackendCommand::StartSavedRecipe { .. }
                    | BackendCommand::RefreshRecipes
                    | BackendCommand::CreateSavedRecipe(_)
                    | BackendCommand::UpdateSavedRecipe { .. }
                    | BackendCommand::DeleteSavedRecipe { .. }
                    | BackendCommand::ImportRecipe(_)
                    | BackendCommand::ExportRecipe { .. },
                ) => event_tx.send(BackendEvent::CommandError(
                    "remote recipe command sent to local backend".to_owned(),
                )),
                Ok(BackendCommand::RenameRun { run_id, request }) => {
                    publish(
                        backend.rename_run(&run_id, request),
                        &mut port,
                        &event_tx,
                        &mut backend,
                    );
                }
                Ok(BackendCommand::RenameCycle {
                    execution_id,
                    request,
                }) => {
                    publish(
                        backend.rename_cycle(&execution_id, request),
                        &mut port,
                        &event_tx,
                        &mut backend,
                    );
                }
                Ok(BackendCommand::StopCycle) => {
                    publish(backend.stop_cycle(), &mut port, &event_tx, &mut backend);
                }
                Ok(BackendCommand::Shutdown) => {
                    publish(backend.shutdown(), &mut port, &event_tx, &mut backend);
                    break 'runtime;
                }
                Err(futures::channel::mpsc::TryRecvError::Empty) => break,
                Err(futures::channel::mpsc::TryRecvError::Closed) => break 'runtime,
            }
        }

        std::thread::sleep(SLEEP_DURATION);
        tick_serial(
            &mut port,
            &event_tx,
            &mut backend,
            &mut ingress,
            &mut ingress_context,
        );
    }
}

fn tick_serial(
    port: &mut Option<Box<dyn serialport::SerialPort>>,
    event_tx: &BackendEventSender,
    backend: &mut LocalBackend,
    ingress: &mut crate::device::SerialIngress,
    context: &mut (u64, Option<web_time::Instant>),
) {
    publish_serial(
        LocalOutput::default(),
        port,
        event_tx,
        backend,
        ingress,
        context,
    );
    publish_serial(backend.tick(), port, event_tx, backend, ingress, context);
}

fn publish_serial(
    output: LocalOutput,
    port: &mut Option<Box<dyn serialport::SerialPort>>,
    event_tx: &BackendEventSender,
    backend: &mut LocalBackend,
    ingress: &mut crate::device::SerialIngress,
    context: &mut (u64, Option<web_time::Instant>),
) {
    publish_inner(output, port, event_tx, backend, Some((ingress, context)));
}

fn reconcile_serial(
    port: &mut Option<Box<dyn serialport::SerialPort>>,
    backend: &mut LocalBackend,
    ingress: &mut crate::device::SerialIngress,
    context: &mut (u64, Option<web_time::Instant>),
    receive: bool,
) -> LocalOutput {
    let mut output = LocalOutput::default();
    if let Some(current) = port.as_mut() {
        let new_context = backend.ingress_context();
        if *context != new_context {
            if let Err(error) = ingress.quarantine(current.as_mut()) {
                *port = None;
                return backend.connection_failed(error);
            }
            *context = new_context;
        }
        if receive {
            let prefix = match ingress.receive_prefix(current.as_mut()) {
                Ok(prefix) => prefix,
                Err(error) => {
                    *port = None;
                    return backend.connection_failed(error);
                }
            };
            if !prefix.is_empty() {
                backend.begin_receive_prefix();
                for item in prefix {
                    let received =
                        backend.frame_received_at(item.frame, item.raw, item.received_at);
                    output.extend(received);
                }
                let reconciled = backend.finish_receive_prefix();
                output.extend(reconciled);
            }
        }
    }
    output
}

fn retire_existing_port(
    backend: &mut LocalBackend,
    port: &mut Option<Box<dyn serialport::SerialPort>>,
    event_tx: &BackendEventSender,
) {
    if port.is_some() {
        publish(backend.request_disconnect(), port, event_tx, backend);
        *port = None;
    }
}

fn publish(
    output: LocalOutput,
    port: &mut Option<Box<dyn serialport::SerialPort>>,
    event_tx: &BackendEventSender,
    backend: &mut LocalBackend,
) {
    publish_inner(output, port, event_tx, backend, None);
}

fn publish_inner(
    mut output: LocalOutput,
    port: &mut Option<Box<dyn serialport::SerialPort>>,
    event_tx: &BackendEventSender,
    backend: &mut LocalBackend,
    mut ingress: Option<(
        &mut crate::device::SerialIngress,
        &mut (u64, Option<web_time::Instant>),
    )>,
) {
    // Empty publication is the runnable receive service path. Actual Stop sends
    // bypass ordinary input work, but still synchronize connection provenance.
    if let Some((buffer, context)) = ingress.as_mut() {
        let receive = output.sends.is_empty();
        output.extend(reconcile_serial(port, backend, buffer, context, receive));
    }
    for event in output.events {
        event_tx.send(event);
    }
    let mut sends = std::collections::VecDeque::from(output.sends);
    while let Some(send) = sends.pop_front() {
        if !matches!(send.frame(), OutboundFrame::Stop)
            && let Some((buffer, context)) = ingress.as_mut()
        {
            let available = reconcile_serial(port, backend, buffer, context, true);
            for event in available.events {
                event_tx.send(event);
            }
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
        let result = if let Some(port) = port {
            crate::transport_time::write_frame(port.as_mut(), frame)
        } else {
            Err("serial port is not open".to_owned())
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
            && let Some((buffer, context)) = ingress.as_mut()
        {
            let available = reconcile_serial(port, backend, buffer, context, true);
            for event in available.events {
                event_tx.send(event);
            }
            sends.extend(available.sends);
        }
        let completion = backend.finish_send(send, result);
        for event in completion.events {
            event_tx.send(event);
        }
        if failed {
            *port = None;
        }
        // Lifecycle fences also quarantine driver and partial input immediately,
        // before the next queued command can read across that boundary.
        if let Some((buffer, context)) = ingress.as_mut() {
            let quarantined = reconcile_serial(port, backend, buffer, context, false);
            for event in quarantined.events {
                event_tx.send(event);
            }
        }
    }
}

fn outgoing(frame: OutboundFrame) -> BackendEvent {
    BackendEvent::Diagnostic(DiagnosticEvent {
        direction: DiagnosticDirection::Out,
        label: format!("{frame:?}"),
        raw_bytes: <[u8; OUTBOUND_FRAME_SIZE]>::from(frame).to_vec(),
    })
}

#[cfg(all(test, unix))]
#[expect(clippy::expect_used, reason = "PTY boundary tests should fail fast")]
mod tests {
    use super::*;
    use crate::core::{
        ApiCommand, CycleRecipe, CycleStep, CycleStepCompletion, StartCycleRequest,
        StartTestRequest, TestConfiguration, TestState,
    };
    use futures::channel::mpsc;
    use serialport::SerialPort as _;

    fn raw_report(state: u8, capacity: u16) -> Vec<u8> {
        let mut bytes = vec![
            0xfa,
            state,
            0,
            0,
            0x10,
            0xa0,
            (capacity / 240) as u8,
            (capacity % 240) as u8,
            0,
            0,
            0,
            10,
            1,
            0x3c,
            0,
            0,
            9,
            0,
            0xf8,
        ];
        bytes[17] = bytes[1..17].iter().fold(0, |sum, byte| sum ^ byte);
        bytes
    }

    fn wait_update(
        rx: &mut futures::channel::mpsc::UnboundedReceiver<BackendEvent>,
        predicate: impl Fn(&crate::core::SnapshotUpdate) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            while let Ok(event) = rx.try_recv() {
                let update = match event {
                    BackendEvent::Update(state) => state.update,
                    BackendEvent::Snapshot(snapshot) => {
                        crate::core::SnapshotUpdate::from(&snapshot)
                    }
                    _ => continue,
                };
                if predicate(&update) {
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("native runtime did not publish expected state");
    }

    #[test]
    fn native_terminal_rest_tick_reconciles_available_active_before_completion() {
        let (mut backend, mut port, mut peer, events, mut rx) = fixture();
        let mut ingress = crate::device::SerialIngress::default();
        let mut context = (0, None);
        publish_serial(
            LocalOutput::default(),
            &mut port,
            &events,
            &mut backend,
            &mut ingress,
            &mut context,
        );
        peer.write_all(&raw_report(0, 0)).expect("Idle input");
        std::thread::sleep(std::time::Duration::from_millis(5));
        publish_serial(
            LocalOutput::default(),
            &mut port,
            &events,
            &mut backend,
            &mut ingress,
            &mut context,
        );
        let output = backend.start_cycle(StartCycleRequest {
            recipe: CycleRecipe {
                steps: vec![CycleStep::Rest {
                    duration_seconds: 1,
                }],
                repeat_count: 1,
            },
            name: None,
        });
        publish_serial(
            output,
            &mut port,
            &events,
            &mut backend,
            &mut ingress,
            &mut context,
        );
        while rx.try_recv().is_ok() {}
        std::thread::sleep(std::time::Duration::from_millis(1010));
        peer.write_all(&raw_report(10, 50))
            .expect("Active available before due tick");
        std::thread::sleep(std::time::Duration::from_millis(5));
        tick_serial(&mut port, &events, &mut backend, &mut ingress, &mut context);
        let mut interrupted = false;
        while let Ok(event) = rx.try_recv() {
            if let BackendEvent::Update(state) = event {
                assert_ne!(
                    state.update.cycle.state,
                    crate::core::CycleState::Completed,
                    "autonomous completion cannot precede available contradictory evidence"
                );
                interrupted |= state.update.cycle.state == crate::core::CycleState::Interrupted;
            }
        }
        assert!(interrupted);
        let mut wire = [0; OUTBOUND_FRAME_SIZE];
        assert!(
            peer.read(&mut wire).is_err(),
            "Rest contradiction emits no physical action"
        );
    }

    #[test]
    fn native_runtime_queue_and_serial_fragments_obey_start_and_stop_fences() {
        let (port, mut peer) = serialport::TTYPort::pair().expect("software PTY");
        peer.set_timeout(std::time::Duration::from_millis(200))
            .expect("timeout");
        let (tx, commands) = mpsc::unbounded();
        let (events, mut rx) = mpsc::unbounded();
        let event_sender = BackendEventSender::new(events, || {});
        let thread = std::thread::spawn(move || {
            backend_runtime(commands, event_sender, Some(Box::new(port)));
        });
        wait_update(&mut rx, |s| {
            s.connection == crate::core::ServerConnectionState::Connected
        });
        peer.write_all(&raw_report(0, 0)).expect("Idle input");
        wait_update(&mut rx, |s| s.device.activity_known && !s.device.active);
        let partial = raw_report(10, 50);
        peer.write_all(&partial[..18]).expect("old prefix");
        std::thread::sleep(std::time::Duration::from_millis(30));
        let config = TestConfiguration::DischargeConstantCurrent {
            current_ma: 100,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        };
        tx.unbounded_send(BackendCommand::StartTest(StartTestRequest {
            config,
            name: None,
        }))
        .expect("public command queue Start");
        read_frame(
            &mut peer,
            OutboundFrame::StartConstantCurrentDischarge(100, 3000, 0),
        );
        peer.write_all(&partial[18..])
            .expect("old suffix after Start");
        wait_update(&mut rx, |s| {
            s.test.state == TestState::Starting && !s.device.activity_known
        });
        std::thread::sleep(std::time::Duration::from_millis(30));
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(event, BackendEvent::Sample(_)));
        }
        peer.write_all(&raw_report(10, 1))
            .expect("new zero-current Active");
        wait_update(&mut rx, |s| {
            s.test.state == TestState::Running && s.device.current_ma == Some(0)
        });
        peer.write_all(&partial[..18]).expect("prefix before Stop");
        std::thread::sleep(std::time::Duration::from_millis(30));
        tx.unbounded_send(BackendCommand::Api(ApiCommand::Stop))
            .expect("public Stop queue");
        read_frame(&mut peer, OutboundFrame::Stop);
        peer.write_all(&partial[18..])
            .expect("old suffix after Stop");
        wait_update(&mut rx, |s| {
            s.test.state == TestState::Stopping && s.test.capacity_mah == Some(1)
        });
        tx.unbounded_send(BackendCommand::Shutdown)
            .expect("shutdown queue");
        thread.join().expect("native runtime retired");
    }

    fn fixture() -> (
        LocalBackend,
        Option<Box<dyn serialport::SerialPort>>,
        serialport::TTYPort,
        BackendEventSender,
        futures::channel::mpsc::UnboundedReceiver<BackendEvent>,
    ) {
        let (write, mut read) = serialport::TTYPort::pair().expect("software PTY pair");
        read.set_timeout(std::time::Duration::from_millis(50))
            .expect("timeout");
        let (tx, rx) = mpsc::unbounded();
        let events = BackendEventSender::new(tx, || {});
        let mut backend = LocalBackend::default();
        backend.begin_connection();
        backend.connection_established();
        (backend, Some(Box::new(write)), read, events, rx)
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "paired PTY schedules exercise the full completion boundary"
    )]
    fn native_serial_completion_reconciles_adjust_and_calibration_input() {
        for command in [
            ApiCommand::Adjust(TestConfiguration::DischargeConstantCurrent {
                current_ma: 100,
                cutoff_voltage_mv: 3000,
                cutoff_time_min: 0,
            }),
            ApiCommand::Calibration(crate::core::CalibrationCommand::VoltageLow(4000)),
        ] {
            let (mut backend, mut port, mut peer, events, mut rx) = fixture();
            let mut ingress = crate::device::SerialIngress::default();
            let mut context = (0, None);
            publish_serial(
                LocalOutput::default(),
                &mut port,
                &events,
                &mut backend,
                &mut ingress,
                &mut context,
            );
            peer.write_all(&raw_report(0, 0)).expect("Idle input");
            std::thread::sleep(std::time::Duration::from_millis(5));
            publish_serial(
                LocalOutput::default(),
                &mut port,
                &events,
                &mut backend,
                &mut ingress,
                &mut context,
            );
            let config = TestConfiguration::DischargeConstantCurrent {
                current_ma: 100,
                cutoff_voltage_mv: 3000,
                cutoff_time_min: 0,
            };
            let output = backend.start_test(StartTestRequest { config, name: None });
            assert_eq!(output.sends.len(), 1, "fixture must authorize Start");
            publish_serial(
                output,
                &mut port,
                &events,
                &mut backend,
                &mut ingress,
                &mut context,
            );
            read_frame(
                &mut peer,
                OutboundFrame::StartConstantCurrentDischarge(100, 3000, 0),
            );
            peer.write_all(&raw_report(10, 1)).expect("Active input");
            std::thread::sleep(std::time::Duration::from_millis(5));
            publish_serial(
                LocalOutput::default(),
                &mut port,
                &events,
                &mut backend,
                &mut ingress,
                &mut context,
            );
            while rx.try_recv().is_ok() {}
            let mut input = peer.try_clone().expect("PTY input clone");
            port = Some(Box::new(crate::transport_time::fixture::WriteHook {
                port: port.take().expect("open port"),
                hook: Box::new(move || {
                    input
                        .write_all(&raw_report(11, 50))
                        .expect("CP during write");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }),
            }));
            let output = backend.command(command);
            assert_eq!(output.sends.len(), 1);
            let frame = output.sends[0].frame();
            publish_serial(
                output,
                &mut port,
                &events,
                &mut backend,
                &mut ingress,
                &mut context,
            );
            read_frame(&mut peer, frame);
            let mut observed = false;
            while let Ok(event) = rx.try_recv() {
                assert!(
                    !matches!(
                        event,
                        BackendEvent::CommandSucceeded | BackendEvent::Sample(_)
                    ),
                    "contradictory write cannot claim continuing owned success"
                );
                if let BackendEvent::Update(state) = event {
                    observed |= state.update.device.active
                        && state.update.device.mode
                            == Some(crate::device::DeviceMode::DischargeConstantPower)
                        && state.update.test.state == TestState::RecoveredUncertain;
                }
            }
            assert!(
                observed,
                "completion must preserve independently fresh contradictory knowledge"
            );
        }
    }

    fn read_frame(read: &mut serialport::TTYPort, expected: OutboundFrame) {
        let mut bytes = [0; OUTBOUND_FRAME_SIZE];
        read.read_exact(&mut bytes)
            .expect("real native transport write");
        assert_eq!(
            bytes,
            <[u8; OUTBOUND_FRAME_SIZE]>::from(expected),
            "native wire frame must match semantic intent"
        );
    }

    fn report(backend: &mut LocalBackend, state: u8) -> LocalOutput {
        let mut bytes = vec![
            0xfa, state, 0, 10, 0x10, 0xa0, 0, 30, 0, 0, 0, 10, 1, 0x3c, 0, 0, 9, 0, 0xf8,
        ];
        if state < 10 {
            bytes[3] = 0;
        }
        bytes[17] = bytes[1..17].iter().fold(0, |sum, byte| sum ^ byte);
        let received_at = web_time::Instant::now();
        let (frame, raw) = crate::device::process_buffer(&mut bytes)
            .pop()
            .expect("valid fixture frame");
        backend.frame_received_at(frame, raw, received_at)
    }

    #[test]
    fn native_unknown_stop_retries_write_real_frames_and_old_connection_stop_cannot_write() {
        let (mut backend, mut port, mut read, events, mut rx) = fixture();
        for _ in 0..2 {
            let output = backend.command(ApiCommand::Stop);
            publish(output, &mut port, &events, &mut backend);
            read_frame(&mut read, OutboundFrame::Stop);
        }
        std::thread::sleep(crate::controller::REPORT_FRESHNESS_TIMEOUT);
        publish(backend.tick(), &mut port, &events, &mut backend);
        publish(
            backend.command(ApiCommand::Stop),
            &mut port,
            &events,
            &mut backend,
        );
        read_frame(&mut read, OutboundFrame::Stop);
        let old = backend.command(ApiCommand::Stop);
        backend.begin_connection();
        backend.connection_established();
        publish(old, &mut port, &events, &mut backend);
        assert!(read.read(&mut [0; OUTBOUND_FRAME_SIZE]).is_err());
        while let Ok(event) = rx.try_recv() {
            if let BackendEvent::Update(state) = event {
                assert!(!state.update.device.activity_known);
                assert_ne!(state.update.test.state, TestState::Running);
                assert!(state.update.current_run.id.is_none());
            }
        }
        publish(
            backend.request_disconnect(),
            &mut port,
            &events,
            &mut backend,
        );
        read_frame(&mut read, OutboundFrame::Stop);
        read_frame(&mut read, OutboundFrame::Disconnect);
    }

    #[test]
    fn native_parsed_mode_contradiction_revokes_manual_and_cycle_control_before_samples() {
        let config = TestConfiguration::DischargeConstantCurrent {
            current_ma: 100,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        };
        for cycle in [false, true] {
            for firmware in [false, true] {
                let (mut backend, mut port, mut read, events, mut rx) = fixture();
                publish(report(&mut backend, 0), &mut port, &events, &mut backend);
                let start = if cycle {
                    backend.start_cycle(StartCycleRequest {
                        recipe: CycleRecipe {
                            steps: vec![CycleStep::Device {
                                config,
                                completion: CycleStepCompletion::Hardware,
                            }],
                            repeat_count: 1,
                        },
                        name: None,
                    })
                } else {
                    backend.start_test(StartTestRequest { config, name: None })
                };
                publish(start, &mut port, &events, &mut backend);
                read_frame(
                    &mut read,
                    OutboundFrame::StartConstantCurrentDischarge(100, 3000, 0),
                );
                if firmware {
                    publish(report(&mut backend, 10), &mut port, &events, &mut backend);
                }
                while rx.try_recv().is_ok() {}
                publish(
                    report(&mut backend, if firmware { 0x6f } else { 11 }),
                    &mut port,
                    &events,
                    &mut backend,
                );
                let mut revoked = false;
                while let Ok(event) = rx.try_recv() {
                    assert!(!matches!(event, BackendEvent::Sample(_)));
                    if let BackendEvent::Update(state) = event {
                        revoked = state.update.test.state == TestState::RecoveredUncertain;
                        assert!(state.update.device.active);
                        assert!(!state.update.capabilities.adjust);
                        if cycle {
                            assert_eq!(
                                state.update.cycle.state,
                                crate::core::CycleState::Interrupted
                            );
                        }
                    }
                }
                assert!(revoked);
                publish(
                    backend.command(ApiCommand::Adjust(config)),
                    &mut port,
                    &events,
                    &mut backend,
                );
                assert!(read.read(&mut [0; OUTBOUND_FRAME_SIZE]).is_err());
                publish(
                    backend.command(ApiCommand::Stop),
                    &mut port,
                    &events,
                    &mut backend,
                );
                read_frame(&mut read, OutboundFrame::Stop);
            }
        }
    }

    #[test]
    fn native_full_receive_prefix_prevents_next_start_and_preserves_fragments() {
        let (mut backend, mut port, mut read, events, mut rx) = fixture();
        let config = TestConfiguration::DischargeConstantCurrent {
            current_ma: 100,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 0,
        };
        publish(report(&mut backend, 0), &mut port, &events, &mut backend);
        let step = CycleStep::Device {
            config,
            completion: CycleStepCompletion::Hardware,
        };
        let start = backend.start_cycle(StartCycleRequest {
            recipe: CycleRecipe {
                steps: vec![step.clone(), step],
                repeat_count: 1,
            },
            name: None,
        });
        publish(start, &mut port, &events, &mut backend);
        read_frame(
            &mut read,
            OutboundFrame::StartConstantCurrentDischarge(100, 3000, 0),
        );
        publish(report(&mut backend, 10), &mut port, &events, &mut backend);
        while rx.try_recv().is_ok() {}
        let mut ingress = crate::device::SerialIngress::default();
        ingress
            .quarantine(port.as_mut().expect("port").as_mut())
            .expect("input quarantine");
        let mut context = backend.ingress_context();
        let mut batch = Vec::new();
        for state in [20, 0, 10] {
            let mut bytes = vec![
                0xfa, state, 0, 0, 0x10, 0xa0, 0, 1, 0, 0, 0, 10, 1, 0x3c, 0, 0, 9, 0, 0xf8,
            ];
            bytes[17] = bytes[1..17].iter().fold(0, |sum, byte| sum ^ byte);
            batch.extend(bytes);
        }
        // Actual PTY input, shared production SerialIngress, production
        // reconciliation/authorization/write/completion path, not report injection.
        read.write_all(&batch[..18]).expect("fragment prefix");
        std::thread::sleep(std::time::Duration::from_millis(20));
        publish_serial(
            LocalOutput::default(),
            &mut port,
            &events,
            &mut backend,
            &mut ingress,
            &mut context,
        );
        read.write_all(&batch[18..])
            .expect("split/coalesced suffix");
        std::thread::sleep(std::time::Duration::from_millis(20));
        publish_serial(
            LocalOutput::default(),
            &mut port,
            &events,
            &mut backend,
            &mut ingress,
            &mut context,
        );
        assert!(
            read.read(&mut [0; OUTBOUND_FRAME_SIZE]).is_err(),
            "queued Active forbids next Start"
        );
        let mut interrupted = false;
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, BackendEvent::Sample(_)),
                "pre-command Active cannot sample next child"
            );
            if let BackendEvent::Update(state) = event {
                interrupted |= state.update.cycle.state == crate::core::CycleState::Interrupted;
            }
        }
        assert!(
            interrupted,
            "intermediate contradiction must interrupt before any next Start"
        );
    }
}
