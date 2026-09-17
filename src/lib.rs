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
pub mod core;
pub mod device;
#[cfg(feature = "gui")]
mod export;
#[cfg(all(not(target_arch = "wasm32"), feature = "server"))]
pub mod server;
#[cfg(feature = "gui")]
mod session;
#[cfg(feature = "gui")]
mod ui;
#[cfg(feature = "gui")]
pub use app::MainApp;
