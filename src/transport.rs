//! GUI-neutral commands and events exchanged with client transport workers.

use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;
use serde::{Deserialize, Serialize};

use crate::core::{ApiCommand, WebSocketEvent};
use crate::device::{ConnectionStatus, InboundFrame, OutboundFrame, UsbDeviceInfo};

#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(target_arch = "wasm32"),
    expect(
        dead_code,
        reason = "remote transport commands are only consumed by WASM builds"
    )
)]
pub(crate) enum TransportCommand {
    Connect(usize),
    Disconnect,
    Protocol(OutboundFrame),
    Remote(ApiCommand),
}

#[cfg_attr(
    not(target_arch = "wasm32"),
    expect(
        dead_code,
        reason = "remote transport events are only produced by WASM builds"
    )
)]
pub(crate) enum DeviceEvent {
    StatusChanged(ConnectionStatus),
    DevicesUpdated(Vec<UsbDeviceInfo>),
    Frame(InboundFrame, Vec<u8>),
    RemoteConnectionChanged(RemoteConnectionStatus),
    Remote(WebSocketEvent),
    RemoteCommandSucceeded,
    RemoteCommandError(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteConnectionStatus {
    #[default]
    NotUsed,
    Connecting,
    Connected,
    Reconnecting,
    Error(String),
}

impl std::fmt::Debug for DeviceEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StatusChanged(status) => f.debug_tuple("StatusChanged").field(status).finish(),
            Self::DevicesUpdated(devices) => {
                f.debug_tuple("DevicesUpdated").field(devices).finish()
            }
            Self::Frame(frame, _) => write!(f, "Frame({frame:?})"),
            Self::RemoteConnectionChanged(status) => f
                .debug_tuple("RemoteConnectionChanged")
                .field(status)
                .finish(),
            Self::Remote(event) => f.debug_tuple("Remote").field(event).finish(),
            Self::RemoteCommandSucceeded => write!(f, "RemoteCommandSucceeded"),
            Self::RemoteCommandError(error) => {
                f.debug_tuple("RemoteCommandError").field(error).finish()
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct EventSender {
    sender: UnboundedSender<DeviceEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl EventSender {
    pub(crate) fn new(
        sender: UnboundedSender<DeviceEvent>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            sender,
            wake: Arc::new(wake),
        }
    }

    pub(crate) fn send(&self, event: DeviceEvent) {
        self.sender.unbounded_send(event).ok();
        (self.wake)();
    }
}
