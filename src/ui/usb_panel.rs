use crate::device::{ConnectionStatus, OutboundFrame, RemoteConnectionStatus};
use crate::session::DeviceSession;
use crate::usb;

pub(crate) fn ui(session: &mut DeviceSession, ui: &mut egui::Ui) {
    if session.is_remote() {
        remote_ui(session, ui);
        return;
    }
    ui.heading("USB Device");

    let device_labels: Vec<String> = session
        .available_devices
        .iter()
        .map(|d| d.to_string())
        .collect();

    let selected_text = session
        .selected_device_index
        .and_then(|i| device_labels.get(i))
        .map_or_else(|| "No device selected".to_owned(), Clone::clone);

    let compact = ui.available_width() < 700.0;
    ui.scope(|ui| {
        if compact {
            ui.spacing_mut().interact_size.y = 44.0;
        }
        ui.horizontal_wrapped(|ui| {
            #[cfg(target_arch = "wasm32")]
            if ui.button("Add USB device").clicked() {
                usb::request_device(session.event_tx.clone());
            }
            if ui.button("Refresh").clicked() {
                usb::enumerate_devices(session.event_tx.clone());
            }
        });
        ui.add_sized(
            [ui.available_width(), ui.spacing().interact_size.y],
            egui::Label::new("Selected device"),
        );
        egui::ComboBox::from_id_salt("usb_device_selector")
            .width(ui.available_width())
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                if device_labels.is_empty() {
                    ui.label("No devices found");
                }
                for (i, label) in device_labels.iter().enumerate() {
                    ui.selectable_value(&mut session.selected_device_index, Some(i), label);
                }
            });
        ui.horizontal_wrapped(|ui| match &session.status {
            ConnectionStatus::Disconnected => {
                if let Some(idx) = session.selected_device_index
                    && ui.button("Connect").clicked()
                {
                    session.send_cmd(OutboundFrame::Connect(idx), ui.ctx());
                }
            }
            ConnectionStatus::Connecting => {
                ui.spinner();
                ui.label("Connecting...");
            }
            ConnectionStatus::Connected => {
                if ui.button("Disconnect").clicked() {
                    session.send_cmd(OutboundFrame::Stop, ui.ctx());
                    session.send_cmd(OutboundFrame::Disconnect, ui.ctx());
                }
            }
            ConnectionStatus::Error(_) => {
                if let Some(idx) = session.selected_device_index
                    && ui.button("Retry").clicked()
                {
                    session.send_cmd(OutboundFrame::Connect(idx), ui.ctx());
                }
            }
        });
    });
    if let ConnectionStatus::Error(msg) = &session.status {
        ui.colored_label(egui::Color32::RED, format!("Error: {msg}"));
    }
}

fn remote_ui(session: &mut DeviceSession, ui: &mut egui::Ui) {
    ui.heading("Remote Server");
    let browser_connected = session.remote_status == RemoteConnectionStatus::Connected;
    egui::Grid::new("remote_connection_status").show(ui, |ui| {
        ui.label("Browser:");
        match &session.remote_status {
            RemoteConnectionStatus::NotUsed => {
                ui.label("--");
            }
            RemoteConnectionStatus::Connecting => {
                ui.spinner();
                ui.label("Connecting");
            }
            RemoteConnectionStatus::Connected => {
                ui.colored_label(egui::Color32::GREEN, "Connected");
            }
            RemoteConnectionStatus::Reconnecting => {
                ui.spinner();
                ui.label("Reconnecting");
            }
            RemoteConnectionStatus::Error(error) => {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
        }
        ui.end_row();

        ui.label("Device:");
        match &session.status {
            ConnectionStatus::Disconnected => {
                ui.label("Disconnected");
                if ui
                    .add_enabled(browser_connected, egui::Button::new("Connect device"))
                    .clicked()
                {
                    session.send_cmd(OutboundFrame::Connect(0), ui.ctx());
                }
            }
            ConnectionStatus::Connecting => {
                ui.spinner();
                ui.label("Connecting");
            }
            ConnectionStatus::Connected => {
                ui.colored_label(egui::Color32::GREEN, "Connected");
                if ui
                    .add_enabled(browser_connected, egui::Button::new("Disconnect device"))
                    .clicked()
                {
                    session.send_cmd(OutboundFrame::Stop, ui.ctx());
                    session.send_cmd(OutboundFrame::Disconnect, ui.ctx());
                }
            }
            ConnectionStatus::Error(error) => {
                ui.colored_label(ui.visuals().error_fg_color, error);
                if ui
                    .add_enabled(browser_connected, egui::Button::new("Retry device"))
                    .clicked()
                {
                    session.send_cmd(OutboundFrame::Connect(0), ui.ctx());
                }
            }
        }
        ui.end_row();
    });
    if let Some(error) = &session.command_error {
        ui.colored_label(
            ui.visuals().error_fg_color,
            format!("Command failed: {error}"),
        );
    }
}
