use crate::device::{ConnectionStatus, UsbDeviceInfo};
use crate::transport::{DeviceEvent, EventSender, TransportCommand};
use futures::channel::mpsc::UnboundedReceiver;
use wasm_bindgen::JsCast as _;
use wasm_bindgen_futures::JsFuture;

#[path = "usb_wasm/connection.rs"]
mod connection;
#[path = "usb_wasm/remote.rs"]
mod remote;
#[path = "usb_wasm/worker.rs"]
mod worker;

pub fn is_remote_transport() -> bool {
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

pub fn enumerate_devices(event_tx: EventSender) {
    if is_remote_transport() {
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        let Some(window) = web_sys::window() else {
            return;
        };
        let usb = window.navigator().usb();
        if usb.is_undefined() {
            log::error!("WebUSB API not supported in this browser");
            event_tx.send(DeviceEvent::StatusChanged(ConnectionStatus::Error(
                "WebUSB API not supported".to_owned(),
            )));
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
                event_tx.send(DeviceEvent::DevicesUpdated(devices));
            }
            Err(e) => {
                log::error!("Failed to enumerate USB devices: {e:?}");
                event_tx.send(DeviceEvent::StatusChanged(ConnectionStatus::Error(
                    format!("Failed to enumerate USB devices: {e:?}"),
                )));
            }
        }
    });
}

pub fn request_device(event_tx: EventSender) {
    if is_remote_transport() {
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        let Some(window) = web_sys::window() else {
            return;
        };
        let usb = window.navigator().usb();
        let filter = web_sys::UsbDeviceFilter::new();
        filter.set_vendor_id(crate::device::VENDOR_ID);
        let options = web_sys::UsbDeviceRequestOptions::new(&[filter]);
        match JsFuture::from(usb.request_device(&options)).await {
            Ok(_) => enumerate_devices(event_tx),
            Err(e) => {
                log::warn!("USB device request cancelled or denied: {e:?}");
            }
        }
    });
}

pub fn spawn_device_worker(cmd_rx: UnboundedReceiver<TransportCommand>, event_tx: EventSender) {
    if is_remote_transport() {
        wasm_bindgen_futures::spawn_local(remote::remote_task(cmd_rx, event_tx));
    } else {
        wasm_bindgen_futures::spawn_local(worker::device_task(cmd_rx, event_tx));
    }
}
