use crate::device::{OUTBOUND_FRAME_SIZE, OutboundFrame};
use futures::FutureExt as _;

thread_local! {
    // Reopening a handle with unresolved close could let its late resource
    // release close a replacement session. Fail finitely instead of racing it.
    static UNRETIRED: std::cell::RefCell<Vec<web_sys::UsbDevice>> = const { std::cell::RefCell::new(Vec::new()) };
}
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

/// Wall time is never observation time. It supplies only a conservative veto
/// when the browser's monotonic clock demonstrably omitted host suspension (or
/// wall time jumped). Date's one-millisecond quantization cannot prove a gap
/// within that interval; capture bracketing accounts for runnable-clock jitter.
#[derive(Clone, Copy)]
pub(super) struct ClockSample {
    pub(super) before: web_time::Instant,
    after: web_time::Instant,
    wall_ms: f64,
}

impl ClockSample {
    pub(super) fn now() -> Self {
        let before = web_time::Instant::now();
        let wall_ms = js_sys::Date::now();
        let after = web_time::Instant::now();
        Self {
            before,
            after,
            wall_ms,
        }
    }

    pub(super) fn discontinuity_since(self, earlier: Self) -> bool {
        !self.wall_ms.is_finite()
            || !earlier.wall_ms.is_finite()
            || self.wall_ms + 1.0 < earlier.wall_ms
            || (self.wall_ms - earlier.wall_ms - 1.0).max(0.0)
                > self
                    .after
                    .saturating_duration_since(earlier.before)
                    .as_secs_f64()
                    * 1000.0
    }
}

/// A Promise result is provisional until the monotonic deadline is checked.
/// Polling also handles freeze/resume and deterministic clock advances without
/// granting a stale completion priority over an expired timeout.
async fn bounded<T: 'static + wasm_bindgen::convert::FromWasmAbi + Into<JsValue>>(
    promise: js_sys::Promise<T>,
    operation: &str,
) -> Result<JsValue, String> {
    let started = ClockSample::now();
    let deadline = started.before + crate::controller::REPORT_FRESHNESS_TIMEOUT;
    let result = JsFuture::from(promise).fuse();
    futures::pin_mut!(result);
    loop {
        let now = ClockSample::now();
        if now.before >= deadline || now.discontinuity_since(started) {
            return Err(format!(
                "{operation} timed out; transport outcome is uncertain"
            ));
        }
        let tick = gloo_timers::future::TimeoutFuture::new(100).fuse();
        futures::pin_mut!(tick);
        futures::select! {
            value = result => {
                let now = ClockSample::now();
                if now.before >= deadline || now.discontinuity_since(started) {
                    return Err(format!("{operation} timed out; late completion retired"));
                }
                return value.map(Into::into).map_err(|e| format!("{operation} failed: {e:?}"));
            },
            () = tick => {},
        }
    }
}

pub(super) async fn close(device: &web_sys::UsbDevice) -> Result<(), String> {
    let result = bounded(device.close(), "WebUSB close").await.map(|_| ());
    if result.is_err() {
        quarantine_handle(device);
    }
    result
}

fn quarantine_handle(device: &web_sys::UsbDevice) {
    UNRETIRED.with(|devices| {
        if !devices
            .borrow()
            .iter()
            .any(|old| js_sys::Object::is(old.as_ref(), device.as_ref()))
        {
            devices.borrow_mut().push(device.clone());
        }
    });
}

pub(super) struct UsbState {
    pub(super) device: web_sys::UsbDevice,
    pub(super) out_endpoint_num: u8,
    pub(super) in_endpoint_num: u8,
}

// Configures the CH340 USB-serial chip for 9600 baud, 8 data bits, odd parity, 1 stop bit.
// Sequence derived from https://github.com/selevo/WebUsbSerialTerminal/blob/main/serial.js
// Every CH340 control write carries a 1-byte null payload alongside the register/value in
// the SETUP packet wValue/wIndex fields.
async fn ch340_configure(device: &web_sys::UsbDevice) -> Result<(), JsValue> {
    // The CH340 requires a specific sequence of control transfers to initialize the serial port.
    ch340_control(device, 0xA1, 0xC29C, 0xB2B9).await?; // serial init
    ch340_control(device, 0xA4, 0x00DF, 0x0000).await?; // modem ctrl: DTR + RTS on
    ch340_control(device, 0xA4, 0x009F, 0x0000).await?; // modem ctrl: call mode
    ch340_control(device, 0x9A, 0x2727, 0x0000).await?; // reset control status
    ch340_control(device, 0x9A, 0x1312, 0xB282).await?; // baud factor: 9600
    ch340_control(device, 0x9A, 0x0F2C, 0x0008).await?; // baud offset: 9600
    ch340_control(device, 0x9A, 0x2518, 0x00CB).await?; // line control: 8O1
    ch340_control(device, 0x9A, 0x2727, 0x0000).await?; // control status
    ch340_control(device, 0x9A, 0x1312, 0xB282).await?; // baud factor: 9600 (final)
    ch340_control(device, 0x9A, 0x0F2C, 0x0008).await?; // baud offset: 9600 (final)
    ch340_control(device, 0x9A, 0x2727, 0x0000).await?; // control status (final)

    Ok(())
}

async fn ch340_control(
    device: &web_sys::UsbDevice,
    request: u8,
    value: u16,
    index: u16,
) -> Result<(), JsValue> {
    let mut data = [0u8; 1];
    let promise = device.control_transfer_out_with_u8_slice(
        &web_sys::UsbControlTransferParameters::new(
            index,
            web_sys::UsbRecipient::Device,
            request,
            web_sys::UsbRequestType::Vendor,
            value,
        ),
        &mut data,
    )?;
    let value = bounded(promise, "CH340 configuration")
        .await
        .map_err(JsValue::from)?;
    let result: web_sys::UsbOutTransferResult = value.unchecked_into();
    if result.status() != web_sys::UsbTransferStatus::Ok || result.bytes_written() != 1 {
        return Err(format!(
            "CH340 control transfer returned {:?} after writing {} of 1 bytes",
            result.status(),
            result.bytes_written()
        )
        .into());
    }
    Ok(())
}

pub(super) async fn connect(device_index: usize) -> Result<UsbState, String> {
    log::info!("Connecting to device at index {device_index}...");
    let window = web_sys::window().ok_or("No window context")?;
    let usb = window.navigator().usb();

    let value = bounded(usb.get_devices(), "USB device enumeration")
        .await
        .map_err(|e| format!("Failed to get USB devices: {e:?}"))?;
    let item = js_sys::Array::from(&value).get(device_index as u32);
    if item.is_undefined() {
        return Err(format!("No USB device at index {device_index}"));
    }
    let device: web_sys::UsbDevice = item.unchecked_into();
    if UNRETIRED.with(|devices| {
        devices
            .borrow()
            .iter()
            .any(|old| js_sys::Object::is(old.as_ref(), device.as_ref()))
    }) {
        return Err(
            "this USB handle has unresolved resource retirement; it cannot be reopened safely"
                .to_owned(),
        );
    }

    match open_configured(&device, device_index).await {
        Ok(state) => Ok(state),
        Err(error) => {
            if error.contains("timed out") {
                quarantine_handle(&device);
            }
            match close(&device).await {
                Ok(()) => Err(error),
                Err(close_error) => Err(format!(
                    "{error}; resource retirement failed: {close_error}"
                )),
            }
        }
    }
}

async fn open_configured(
    device: &web_sys::UsbDevice,
    device_index: usize,
) -> Result<UsbState, String> {
    bounded(device.open(), "USB open").await.map_err(|e| {
        // A late unresolved open can re-open the resource after even a
        // successful close. Never reopen this exact handle beneath it.
        quarantine_handle(device);
        format!("Failed to open device: {e:?}")
    })?;
    bounded(device.select_configuration(1), "USB configuration")
        .await
        .map_err(|e| format!("Failed to select configuration: {e:?}"))?;
    ch340_configure(device)
        .await
        .map_err(|e| format!("Failed to configure CH340: {e:?}"))?;

    // Find bulk IN and OUT endpoints from the active configuration.
    let config = device
        .configuration()
        .ok_or("USB device has no active configuration")?;
    let mut interface_num: Option<u8> = None;
    let mut out_endpoint_num: Option<u8> = None;
    let mut in_endpoint_num: Option<u8> = None;
    for iface_val in config.interfaces() {
        let iface: web_sys::UsbInterface = iface_val.unchecked_into();
        let interface_number = iface.interface_number();
        for ep_val in iface.alternate().endpoints() {
            let ep: web_sys::UsbEndpoint = ep_val.unchecked_into();
            if ep.type_() == web_sys::UsbEndpointType::Bulk {
                if ep.direction() == web_sys::UsbDirection::Out {
                    interface_num = Some(interface_number);
                    out_endpoint_num = Some(ep.endpoint_number());
                } else if ep.direction() == web_sys::UsbDirection::In {
                    in_endpoint_num = Some(ep.endpoint_number());
                }
            }
        }
    }
    let (Some(interface_num), Some(out_endpoint_num), Some(in_endpoint_num)) =
        (interface_num, out_endpoint_num, in_endpoint_num)
    else {
        return Err("No bulk endpoints found on USB device".to_owned());
    };
    log::info!(
        "Using interface {interface_num}, OUT endpoint {out_endpoint_num}, IN endpoint {in_endpoint_num}"
    );

    bounded(device.claim_interface(interface_num), "USB interface claim")
        .await
        .map_err(|e| format!("Failed to claim interface {interface_num}: {e:?}"))?;

    send_frame(
        device,
        out_endpoint_num,
        OutboundFrame::Connect(device_index),
    )
    .await
    .map_err(|error| format!("Connect command failed: {error}"))?;

    Ok(UsbState {
        device: device.clone(),
        out_endpoint_num,
        in_endpoint_num,
    })
}

pub(super) async fn disconnect(
    device: &web_sys::UsbDevice,
    out_endpoint_num: u8,
) -> Result<(), String> {
    log::info!("Disconnecting from device...");
    let protocol = send_frame(device, out_endpoint_num, OutboundFrame::Disconnect).await;
    let resource = close(device).await;
    match (protocol, resource) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(a), Err(b)) => Err(format!("{a}; {b}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

pub(super) async fn stop(device: &web_sys::UsbDevice, out_endpoint_num: u8) -> Result<(), String> {
    send_frame(device, out_endpoint_num, OutboundFrame::Stop).await
}

pub(super) async fn send_frame(
    device: &web_sys::UsbDevice,
    out_endpoint_num: u8,
    frame: OutboundFrame,
) -> Result<(), String> {
    let mut bytes: [u8; OUTBOUND_FRAME_SIZE] = frame.into();
    let promise = device
        .transfer_out_with_u8_slice(out_endpoint_num, &mut bytes)
        .map_err(|error| format!("Failed to start transfer: {error:?}"))?;
    let value = bounded(promise, "WebUSB frame write")
        .await
        .map_err(|error| format!("Frame send failed: {error:?}"))?;
    let result: web_sys::UsbOutTransferResult = value.unchecked_into();
    if result.status() != web_sys::UsbTransferStatus::Ok {
        return Err(format!("Frame send returned {:?}", result.status()));
    }
    if result.bytes_written() != OUTBOUND_FRAME_SIZE as u32 {
        return Err(format!(
            "Frame send wrote {} of {OUTBOUND_FRAME_SIZE} bytes",
            result.bytes_written()
        ));
    }
    Ok(())
}
