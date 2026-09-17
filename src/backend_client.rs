//! GUI integration adapter for semantic backend runtimes.

use futures::channel::mpsc;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::backend::{BackendCommand, BackendEvent, BackendEventSender};
use crate::usb;

pub(crate) struct BackendClient {
    command_tx: UnboundedSender<BackendCommand>,
    event_rx: UnboundedReceiver<BackendEvent>,
    #[cfg(target_arch = "wasm32")]
    event_tx: BackendEventSender,
    remote: bool,
}

impl Default for BackendClient {
    fn default() -> Self {
        let (command_tx, _) = mpsc::unbounded();
        let (_event_tx, event_rx) = mpsc::unbounded();
        Self {
            command_tx,
            event_rx,
            #[cfg(target_arch = "wasm32")]
            event_tx: BackendEventSender::new(_event_tx, || {}),
            remote: false,
        }
    }
}

impl BackendClient {
    pub(crate) fn new(ctx: &egui::Context) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded();
        let (event_tx, event_rx) = mpsc::unbounded();
        let event_tx = BackendEventSender::new(event_tx, {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        });
        let remote = usb::is_remote_backend();
        usb::spawn_backend(command_rx, event_tx.clone());
        Self {
            command_tx,
            event_rx,
            #[cfg(target_arch = "wasm32")]
            event_tx,
            remote,
        }
    }

    pub(crate) fn is_remote(&self) -> bool {
        self.remote
    }

    pub(crate) fn command(&self, command: BackendCommand) {
        self.command_tx.unbounded_send(command).ok();
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn request_device_access(&self) {
        usb::request_device_access(self.event_tx.clone());
    }

    pub(crate) fn try_event(&mut self) -> Option<BackendEvent> {
        self.event_rx.try_recv().ok()
    }

    pub(crate) fn shutdown(&self) {
        self.command(BackendCommand::Shutdown);
    }
}
