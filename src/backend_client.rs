//! GUI integration adapter for semantic backend runtimes.

use futures::channel::mpsc;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};

#[cfg(not(target_arch = "wasm32"))]
use std::sync::{OnceLock, mpsc as std_mpsc};
#[cfg(not(target_arch = "wasm32"))]
use std::thread::JoinHandle;

use crate::backend::{BackendCommand, BackendEvent, BackendEventSender};
use crate::usb;

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) enum BackendTarget {
    #[default]
    Local,
    Remote,
}

pub(crate) struct BackendClient {
    command_tx: UnboundedSender<BackendCommand>,
    event_rx: UnboundedReceiver<BackendEvent>,
    #[cfg(target_arch = "wasm32")]
    event_tx: BackendEventSender,
    remote: bool,
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<JoinHandle<()>>,
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
            #[cfg(not(target_arch = "wasm32"))]
            worker: None,
        }
    }
}

impl BackendClient {
    #[cfg(target_arch = "wasm32")]
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

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn new(
        ctx: &egui::Context,
        target: BackendTarget,
        remote_url: &str,
    ) -> Result<Self, String> {
        let (command_tx, command_rx) = mpsc::unbounded();
        let (event_tx, event_rx) = mpsc::unbounded();
        let event_tx = BackendEventSender::new(event_tx, {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        });
        let (worker, remote) = match target {
            BackendTarget::Local => (usb::spawn_backend(command_rx, event_tx)?, false),
            BackendTarget::Remote => {
                let urls = crate::remote_backend::RemoteUrls::parse(remote_url)?;
                (
                    crate::remote_native::spawn_backend(urls, command_rx, event_tx)?,
                    true,
                )
            }
        };
        Ok(Self {
            command_tx,
            event_rx,
            remote,
            worker: Some(worker),
        })
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

impl Drop for BackendClient {
    fn drop(&mut self) {
        self.shutdown();
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(worker) = self.worker.take() {
            retire_worker(worker);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn retire_worker(worker: JoinHandle<()>) {
    static REAPER: OnceLock<Option<std_mpsc::Sender<JoinHandle<()>>>> = OnceLock::new();
    let reaper = REAPER.get_or_init(|| {
        let (tx, rx) = std_mpsc::channel::<JoinHandle<()>>();
        if let Err(error) = std::thread::Builder::new()
            .name("ebc-backend-reaper".to_owned())
            .spawn(move || {
                for worker in rx {
                    if worker.join().is_err() {
                        log::error!("backend worker panicked during shutdown");
                    }
                }
            })
        {
            log::error!("failed to spawn backend reaper thread: {error}");
            return None;
        }
        Some(tx)
    });
    let Some(reaper) = reaper else {
        return;
    };
    if reaper.send(worker).is_err() {
        log::error!("backend reaper stopped before worker could be joined");
    }
}
