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

pub const fn is_remote_backend() -> bool {
    false
}

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

pub fn spawn_backend(command_rx: UnboundedReceiver<BackendCommand>, event_tx: BackendEventSender) {
    if let Err(error) = std::thread::Builder::new()
        .name("ebc-local-backend".to_owned())
        .spawn(move || backend_thread(command_rx, event_tx))
    {
        log::error!("failed to spawn local backend thread: {error}");
    }
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
                        LocalBackend::safe_disconnect(),
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
                Ok(BackendCommand::Resume(config)) => {
                    publish(backend.resume(config), &mut port, &event_tx, &mut backend);
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
                    buffer.extend_from_slice(&temp_buffer[..n]);
                    for (frame, raw) in crate::device::process_buffer(&mut buffer) {
                        publish(
                            backend.frame(frame, raw),
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

fn publish(
    output: LocalOutput,
    port: &mut Option<Box<dyn serialport::SerialPort>>,
    event_tx: &BackendEventSender,
    backend: &mut LocalBackend,
) {
    for send in output.sends {
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
