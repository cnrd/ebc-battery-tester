use crate::backend::BackendConnectionStatus;
use crate::device::ConnectionStatus;
use crate::session::DeviceSession;

#[cfg(not(target_arch = "wasm32"))]
use crate::backend_client::BackendTarget;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn backend_selector(
    session: &mut DeviceSession,
    active_target: &mut BackendTarget,
    active_remote_url: &mut String,
    remote_url_draft: &mut String,
    remote_instance_id: &mut Option<String>,
    discovery: &crate::mdns::DiscoverySnapshot,
    ui: &mut egui::Ui,
) {
    ui.heading("Backend");
    ui.horizontal_wrapped(|ui| {
        if ui
            .selectable_label(*active_target == BackendTarget::Local, "Local USB")
            .clicked()
            && *active_target != BackendTarget::Local
        {
            match session.switch_backend(ui.ctx(), BackendTarget::Local, active_remote_url, None) {
                Ok(()) => *active_target = BackendTarget::Local,
                Err(error) => session.command_error = Some(error),
            }
        }
        if ui
            .selectable_label(*active_target == BackendTarget::Remote, "Remote server")
            .clicked()
            && *active_target != BackendTarget::Remote
        {
            // Switching back uses the last applied target, not an unapplied draft.
            match session.switch_backend(
                ui.ctx(),
                BackendTarget::Remote,
                active_remote_url,
                remote_instance_id.as_deref(),
            ) {
                Ok(()) => *active_target = BackendTarget::Remote,
                Err(error) => session.command_error = Some(error),
            }
        }
    });
    ui.label("Remote server URL");
    ui.horizontal(|ui| {
        ui.text_edit_singleline(remote_url_draft);
        if ui.button("Apply").clicked() {
            apply_remote(
                session,
                active_target,
                active_remote_url,
                remote_url_draft,
                remote_instance_id,
                None,
                ui.ctx(),
            );
        }
    });
    if let Some(warning) = &discovery.warning {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!("Discovery unavailable: {warning}"),
        );
    }
    discovered_servers_ui(
        session,
        active_target,
        active_remote_url,
        remote_url_draft,
        remote_instance_id,
        discovery,
        ui,
    );
    ui.separator();
}

#[cfg(not(target_arch = "wasm32"))]
fn discovered_servers_ui(
    session: &mut DeviceSession,
    active_target: &mut BackendTarget,
    active_remote_url: &mut String,
    remote_url_draft: &mut String,
    remote_instance_id: &mut Option<String>,
    discovery: &crate::mdns::DiscoverySnapshot,
    ui: &mut egui::Ui,
) {
    egui::CollapsingHeader::new("Discovered servers")
        .default_open(true)
        .show(ui, |ui| {
            if discovery.servers.is_empty() {
                ui.weak("No EBC servers discovered.");
                ui.weak("You can still enter a remote server URL manually.");
            }
            for server in &discovery.servers {
                ui.group(|ui| {
                    ui.label(&server.instance_name).on_hover_text(format!(
                        "Instance ID: {}\nHostname: {}",
                        server.instance_id, server.hostname
                    ));
                    ui.label(format!("API v{} (discovery hint)", server.api_version_hint));
                    if let Some(endpoint) = server.endpoints.first() {
                        ui.label(endpoint)
                            .on_hover_text(server.endpoints.join("\n"));
                        if ui.button("Connect").clicked() {
                            // Discovery changes alone never switch a backend.
                            let mut selected_url = endpoint.clone();
                            if apply_remote(
                                session,
                                active_target,
                                active_remote_url,
                                &mut selected_url,
                                remote_instance_id,
                                Some(&server.instance_id),
                                ui.ctx(),
                            ) {
                                remote_url_draft.clone_from(active_remote_url);
                            }
                        }
                    } else {
                        ui.weak("No connectable address (scoped IPv6 is unsupported).");
                        for address in &server.link_local_ipv6 {
                            ui.weak(format!(
                                "{}%{} (interface {})",
                                address.address, address.interface_name, address.interface_index
                            ));
                        }
                    }
                });
            }
        });
}

#[cfg(not(target_arch = "wasm32"))]
fn apply_remote(
    session: &mut DeviceSession,
    active_target: &mut BackendTarget,
    active_remote_url: &mut String,
    remote_url_draft: &mut String,
    remote_instance_id: &mut Option<String>,
    expected_instance_id: Option<&str>,
    ctx: &egui::Context,
) -> bool {
    let urls = match crate::remote_backend::RemoteUrls::parse(remote_url_draft) {
        Ok(urls) => urls,
        Err(error) => {
            session.command_error = Some(error);
            return false;
        }
    };
    match session.switch_backend(ctx, BackendTarget::Remote, &urls.base, expected_instance_id) {
        Ok(()) => {
            remember_remote_target(
                active_remote_url,
                remote_instance_id,
                &urls.base,
                expected_instance_id,
            );
            remote_url_draft.clone_from(&urls.base);
            *active_target = BackendTarget::Remote;
            true
        }
        Err(error) => {
            session.command_error = Some(error);
            false
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn remember_remote_target(
    url: &mut String,
    id: &mut Option<String>,
    applied_url: &str,
    expected_id: Option<&str>,
) {
    *url = applied_url.to_owned();
    *id = expected_id.map(str::to_owned);
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod discovery_target_tests {
    use super::remember_remote_target;

    #[test]
    fn discovered_apply_binds_identity_and_manual_apply_clears_it() {
        let mut url = String::new();
        let mut id = None;
        remember_remote_target(&mut url, &mut id, "http://192.168.1.20:8080", Some("uuid"));
        assert_eq!(url, "http://192.168.1.20:8080");
        assert_eq!(id.as_deref(), Some("uuid"));
        remember_remote_target(&mut url, &mut id, "http://manual.example:8080", None);
        assert_eq!(url, "http://manual.example:8080");
        assert_eq!(id, None);
    }
}

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
                session.request_device_access();
            }
            if ui.button("Refresh").clicked() {
                session.refresh_devices();
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
                    session.connect(idx);
                }
            }
            ConnectionStatus::Connecting => {
                ui.spinner();
                ui.label("Connecting...");
            }
            ConnectionStatus::Connected => {
                if ui.button("Disconnect").clicked() {
                    session.disconnect_device();
                }
            }
            ConnectionStatus::Error(_) => {
                if let Some(idx) = session.selected_device_index
                    && ui.button("Retry").clicked()
                {
                    session.connect(idx);
                }
            }
        });
    });
    if let ConnectionStatus::Error(msg) = &session.status {
        ui.colored_label(egui::Color32::RED, format!("Error: {msg}"));
    }
    if let Some(error) = &session.command_error {
        ui.colored_label(
            ui.visuals().error_fg_color,
            format!("Command failed: {error}"),
        );
    }
}

fn remote_ui(session: &DeviceSession, ui: &mut egui::Ui) {
    ui.heading("Remote Server");
    let browser_connected = session.remote_status == BackendConnectionStatus::Connected;
    egui::Grid::new("remote_connection_status").show(ui, |ui| {
        ui.label("Server:");
        match &session.remote_status {
            BackendConnectionStatus::NotUsed => {
                ui.label("--");
            }
            BackendConnectionStatus::Connecting => {
                ui.spinner();
                ui.label("Connecting");
            }
            BackendConnectionStatus::Connected => {
                ui.colored_label(egui::Color32::GREEN, "Connected");
            }
            BackendConnectionStatus::Reconnecting => {
                ui.spinner();
                ui.label("Reconnecting");
            }
            BackendConnectionStatus::Error(error) => {
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
                    session.connect(0);
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
                    session.disconnect_device();
                }
            }
            ConnectionStatus::Error(error) => {
                ui.colored_label(ui.visuals().error_fg_color, error);
                if ui
                    .add_enabled(browser_connected, egui::Button::new("Retry device"))
                    .clicked()
                {
                    session.connect(0);
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
