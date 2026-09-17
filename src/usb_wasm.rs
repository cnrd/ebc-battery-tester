use crate::backend::{BackendCommand, BackendEvent, BackendEventSender};
use crate::device::UsbDeviceInfo;
use futures::channel::mpsc::UnboundedReceiver;
use wasm_bindgen::JsCast as _;
use wasm_bindgen_futures::JsFuture;

#[path = "usb_wasm/connection.rs"]
mod connection;
#[path = "usb_wasm/remote.rs"]
mod remote;
#[path = "usb_wasm/worker.rs"]
mod worker;

pub fn is_remote_backend() -> bool {
    let Some(window) = web_sys::window() else {
        return crate::session::select_wasm_transport(
            option_env!("EBC_WASM_DEFAULT_TRANSPORT").unwrap_or("webusb"),
            "",
        ) == crate::session::TransportMode::Remote;
    };
    let location = window.location();
    let switches = format!(
        "{}&{}",
        location.search().unwrap_or_default(),
        location.hash().unwrap_or_default()
    );
    crate::session::select_wasm_transport(
        option_env!("EBC_WASM_DEFAULT_TRANSPORT").unwrap_or("webusb"),
        &switches,
    ) == crate::session::TransportMode::Remote
}

pub(super) async fn enumerate_devices(event_tx: &BackendEventSender) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let usb = window.navigator().usb();
    if usb.is_undefined() {
        event_tx.send(BackendEvent::CommandError(
            "WebUSB API not supported".to_owned(),
        ));
        return;
    }
    match JsFuture::from(usb.get_devices()).await {
        Ok(value) => {
            let mut devices = Vec::new();
            for item in js_sys::Array::from(&value) {
                let device: web_sys::UsbDevice = item.unchecked_into();
                devices.push(UsbDeviceInfo {
                    product_name: device.product_name().unwrap_or_default(),
                    manufacturer_name: device.manufacturer_name().unwrap_or_default(),
                    vendor_id: device.vendor_id(),
                    product_id: device.product_id(),
                });
            }
            event_tx.send(BackendEvent::DevicesUpdated(devices));
        }
        Err(error) => {
            log::error!("Failed to enumerate USB devices: {error:?}");
            event_tx.send(BackendEvent::CommandError(format!(
                "Failed to enumerate USB devices: {error:?}"
            )));
        }
    }
}

pub fn request_device_access(event_tx: BackendEventSender) {
    if is_remote_backend() {
        return;
    }
    let Some(window) = web_sys::window() else {
        return;
    };
    let usb = window.navigator().usb();
    if usb.is_undefined() {
        event_tx.send(BackendEvent::CommandError(
            "WebUSB API not supported".to_owned(),
        ));
        return;
    }
    let filter = web_sys::UsbDeviceFilter::new();
    filter.set_vendor_id(crate::device::VENDOR_ID);
    let options = web_sys::UsbDeviceRequestOptions::new(&[filter]);
    // Invoke requestDevice synchronously while the click's user activation is live.
    let promise = usb.request_device(&options);
    wasm_bindgen_futures::spawn_local(async move {
        match JsFuture::from(promise).await {
            Ok(_) => enumerate_devices(&event_tx).await,
            Err(error) => {
                log::warn!("USB device request cancelled or denied: {error:?}");
                event_tx.send(BackendEvent::CommandError(
                    "USB device request was cancelled or denied".to_owned(),
                ));
            }
        }
    });
}

pub fn spawn_backend(command_rx: UnboundedReceiver<BackendCommand>, event_tx: BackendEventSender) {
    if is_remote_backend() {
        wasm_bindgen_futures::spawn_local(remote::remote_task(command_rx, event_tx));
    } else {
        wasm_bindgen_futures::spawn_local(worker::local_backend_task(command_rx, event_tx));
    }
}
