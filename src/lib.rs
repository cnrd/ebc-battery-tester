//! Shared protocol types plus the optional GUI and headless server implementations.

#[cfg(all(feature = "gui", target_arch = "wasm32"))]
#[path = "usb_wasm.rs"]
mod usb;

#[cfg(all(feature = "gui", not(target_arch = "wasm32")))]
#[path = "usb_native.rs"]
mod usb;

#[cfg(all(feature = "gui", not(target_arch = "wasm32")))]
mod update_check;

#[cfg(feature = "gui")]
mod app;
#[cfg(feature = "gui")]
mod backend;
#[cfg(feature = "gui")]
mod backend_client;
pub mod controller;
pub mod core;
pub mod cycle;
pub mod device;
#[cfg(feature = "gui")]
mod export;
#[cfg(feature = "gui")]
mod local_backend;
#[cfg(feature = "gui")]
mod remote_backend;
#[cfg(all(feature = "gui", not(target_arch = "wasm32")))]
mod remote_native;
#[cfg(all(not(target_arch = "wasm32"), feature = "server"))]
pub mod server;
#[cfg(feature = "gui")]
mod session;
#[cfg(feature = "gui")]
mod ui;
#[cfg(feature = "gui")]
pub use app::MainApp;
