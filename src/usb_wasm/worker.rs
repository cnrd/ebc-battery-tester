use super::connection;
use crate::device::{ConnectionStatus, OUTBOUND_FRAME_SIZE, OutboundFrame};
use crate::transport::{DeviceEvent, EventSender, TransportCommand};
use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::oneshot;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

const INBOUND_BUFFER_SIZE: u32 = 64;

pub(super) async fn device_task(
    mut cmd_rx: UnboundedReceiver<TransportCommand>,
    event_tx: EventSender,
) {
    let mut stop_reading_tx: Option<oneshot::Sender<()>> = None;
    let mut device: Option<web_sys::UsbDevice> = None;
    let mut out_endpoint_num: Option<u8> = None;
    loop {
        match cmd_rx.next().await {
            Some(TransportCommand::Connect(idx)) => {
                event_tx.send(DeviceEvent::StatusChanged(ConnectionStatus::Connecting));
                match connection::connect(idx).await {
                    Ok(state) => {
                        let dev = state.device;
                        out_endpoint_num = Some(state.out_endpoint_num);
                        let (stop_tx, stop_rx) = oneshot::channel();
                        stop_reading_tx = Some(stop_tx);
                        event_tx.send(DeviceEvent::StatusChanged(ConnectionStatus::Connected));
                        wasm_bindgen_futures::spawn_local(reading_task(
                            dev.clone(),
                            state.in_endpoint_num,
                            event_tx.clone(),
                            stop_rx,
                        ));
                        device = Some(dev);
                    }
                    Err(e) => {
                        log::error!("Failed to connect: {e}");
                        event_tx.send(DeviceEvent::StatusChanged(ConnectionStatus::Error(e)));
                    }
                }
            }
            Some(TransportCommand::Disconnect) => {
                if let (Some(device), Some(ep)) = (&device, out_endpoint_num) {
                    if let Some(stop_tx) = stop_reading_tx.take() {
                        let result = stop_tx.send(());
                        if let Err(e) = result {
                            log::error!("Failed to stop reading task: {e:?}");
                        }
                    }
                    let result = connection::disconnect(device, ep).await;
                    if let Err(e) = result {
                        log::error!("Failed to disconnect: {e:?}");
                    }
                }
                device = None;
                out_endpoint_num = None;
                event_tx.send(DeviceEvent::StatusChanged(ConnectionStatus::Disconnected));
            }
            Some(TransportCommand::Protocol(OutboundFrame::Stop)) => {
                if let (Some(device), Some(ep)) = (&device, out_endpoint_num)
                    && let Err(e) = connection::stop(device, ep).await
                {
                    log::error!("Failed to send stop command: {e:?}");
                }
            }
            // Every other frame (discharge/charge start/adjust/continue, timer
            // sync, calibration) is a plain "encode and transfer" command with
            // no extra connection-state bookkeeping, so they all funnel through
            // send_frame.
            Some(TransportCommand::Protocol(frame)) => {
                if let (Some(device), Some(ep)) = (&device, out_endpoint_num)
                    && let Err(e) = send_frame(device, ep, frame).await
                {
                    log::error!("Failed to send {frame:?}: {e:?}");
                }
            }
            Some(TransportCommand::Remote(_)) => {}
            None => break,
        }
    }
}

async fn send_frame(
    device: &web_sys::UsbDevice,
    out_endpoint_num: u8,
    frame: OutboundFrame,
) -> Result<(), JsValue> {
    let mut bytes: [u8; OUTBOUND_FRAME_SIZE] = frame.into();
    let promise = device
        .transfer_out_with_u8_slice(out_endpoint_num, &mut bytes)
        .map_err(|e| format!("Failed to start transfer: {e:?}"))?;
    JsFuture::from(promise)
        .await
        .map_err(|e| format!("Frame send failed: {e:?}"))?;
    Ok(())
}

async fn reading_task(
    device: web_sys::UsbDevice,
    in_endpoint: u8,
    event_tx: EventSender,
    mut stop_reading_rx: oneshot::Receiver<()>,
) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let transfer = JsFuture::from(device.transfer_in(in_endpoint, INBOUND_BUFFER_SIZE)).fuse();
        futures::pin_mut!(transfer);
        futures::select! {
            result = transfer => match result {
                Ok(value) => {
                    let result: web_sys::UsbInTransferResult = value.unchecked_into();
                    if let Some(data) = result.data() {
                        buf.extend_from_slice(&js_sys::Uint8Array::new(&data.buffer()).to_vec());
                        for (frame, raw) in crate::device::process_buffer(&mut buf) {
                            event_tx.send(DeviceEvent::Frame(frame, raw));
                        }
                    }
                }
                Err(e) => {
                    log::error!("Bulk IN transfer failed: {e:?}");
                    event_tx.send(DeviceEvent::StatusChanged(
                        ConnectionStatus::Error("Read error: connection lost".to_owned()),
                    ));
                    return;
                }
            },
            _ = stop_reading_rx => return,
        }
    }
}
