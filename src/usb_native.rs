use std::io::{Read as _, Write as _};

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
    let bytes: [u8; OUTBOUND_FRAME_SIZE] = OutboundFrame::Connect(idx).into();
    port.write_all(&bytes)
        .map_err(|e| format!("Failed to send connect command: {e}"))?;
    Ok(port)
}

#[expect(clippy::needless_pass_by_value)]
#[expect(
    clippy::too_many_lines,
    reason = "the native transport loop handles all backend command variants"
)]
fn backend_thread(mut command_rx: UnboundedReceiver<BackendCommand>, event_tx: BackendEventSender) {
    let mut backend = LocalBackend::default();
    let mut port: Option<Box<dyn serialport::SerialPort>> = None;
    let mut buffer: Vec<u8> = Vec::new();

    'runtime: loop {
        loop {
            match command_rx.try_recv() {
                Ok(BackendCommand::RefreshDevices) => {
                    event_tx.send(BackendEvent::DevicesUpdated(available_devices()));
                }
                Ok(BackendCommand::Connect(idx)) => {
                    retire_existing_port(&mut backend, &mut port, &event_tx);
                    buffer.clear();
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
                    buffer.clear();
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

        if let Some(ref mut p) = port {
            let mut temp_buffer = [0u8; 64];
            match p.read(&mut temp_buffer) {
                Ok(n) if n > 0 => {
                    let received_at = web_time::Instant::now();
                    buffer.extend_from_slice(&temp_buffer[..n]);
                    for (frame, raw) in crate::device::process_buffer(&mut buffer) {
                        publish(
                            backend.frame_received_at(frame, raw, received_at),
                            &mut port,
                            &event_tx,
                            &mut backend,
                        );
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    log::error!("Serial read error: {e}");
                    port = None;
                    buffer.clear();
                    publish(
                        backend.connection_failed("Read error: connection lost".to_owned()),
                        &mut port,
                        &event_tx,
                        &mut backend,
                    );
                }
            }
        } else {
            std::thread::sleep(SLEEP_DURATION);
        }
        publish(backend.tick(), &mut port, &event_tx, &mut backend);
    }
}

fn retire_existing_port(
    backend: &mut LocalBackend,
    port: &mut Option<Box<dyn serialport::SerialPort>>,
    event_tx: &BackendEventSender,
) {
    if port.is_some() {
        publish(LocalBackend::safe_disconnect(), port, event_tx, backend);
        *port = None;
    }
}

fn publish(
    output: LocalOutput,
    port: &mut Option<Box<dyn serialport::SerialPort>>,
    event_tx: &BackendEventSender,
    backend: &mut LocalBackend,
) {
    for event in output.events {
        event_tx.send(event);
    }
    for send in output.sends {
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
            let bytes: [u8; OUTBOUND_FRAME_SIZE] = frame.into();
            port.write_all(&bytes)
                .map_err(|error| format!("serial write failed: {error}"))
        } else {
            Err("serial port is not open".to_owned())
        };
        if let Err(error) = &result {
            log::error!("Failed to send {frame:?}: {error}");
        }
        let failed = result.is_err();
        let completion = backend.finish_send(send, result);
        for event in completion.events {
            event_tx.send(event);
        }
        if failed {
            *port = None;
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
    fn native_batch_received_before_next_child_start_cannot_acknowledge_that_start() {
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
        let mut batch = Vec::new();
        for state in [20, 0, 10] {
            let mut bytes = vec![
                0xfa, state, 0, 0, 0x10, 0xa0, 0, 1, 0, 0, 0, 10, 1, 0x3c, 0, 0, 9, 0, 0xf8,
            ];
            bytes[17] = bytes[1..17].iter().fold(0, |sum, byte| sum ^ byte);
            batch.extend(bytes);
        }
        let received_at = web_time::Instant::now();
        for (frame, raw) in crate::device::process_buffer(&mut batch) {
            publish(
                backend.frame_received_at(frame, raw, received_at),
                &mut port,
                &events,
                &mut backend,
            );
        }
        read_frame(
            &mut read,
            OutboundFrame::StartConstantCurrentDischarge(100, 3000, 0),
        );
        let mut starting = false;
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, BackendEvent::Sample(_)),
                "pre-command Active cannot sample next child"
            );
            if let BackendEvent::Snapshot(snapshot) = event {
                starting =
                    snapshot.test.state == TestState::Starting && !snapshot.device.activity_known;
            }
        }
        assert!(
            starting,
            "next child must still await a new physical observation"
        );
    }
}
