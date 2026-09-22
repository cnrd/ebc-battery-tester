use crate::session::DeviceSession;
use crate::ui;

#[cfg(not(target_arch = "wasm32"))]
use crate::backend_client::BackendTarget;

const MOBILE_BREAKPOINT: f32 = 700.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MobileSection {
    Connection,
    LiveData,
    StartControls,
    StopControl,
    Plot,
    Settings,
}

fn is_mobile_layout(width: f32) -> bool {
    width < MOBILE_BREAKPOINT
}

fn mobile_sections(active: bool) -> [MobileSection; 5] {
    [
        MobileSection::Connection,
        MobileSection::LiveData,
        if active {
            MobileSection::StopControl
        } else {
            MobileSection::StartControls
        },
        MobileSection::Plot,
        MobileSection::Settings,
    ]
}

#[derive(serde::Deserialize, serde::Serialize, Default)]
#[serde(default)]
pub struct MainApp {
    control_panel: ui::control_panel::ControlPanel,
    recipe_panel: ui::recipe_panel::RecipePanel,
    #[cfg(not(target_arch = "wasm32"))]
    backend_target: BackendTarget,
    #[cfg(not(target_arch = "wasm32"))]
    #[serde(default = "default_remote_url")]
    remote_url: String,
    #[cfg(not(target_arch = "wasm32"))]
    #[serde(skip)]
    remote_url_draft: String,
    #[serde(skip)]
    session: DeviceSession,
    #[serde(skip)]
    calibrate_window: ui::calibrate_window::CalibrateWindow,
    #[serde(skip)]
    log_window: ui::log_window::LogWindow,
    #[serde(skip)]
    about_window: ui::about_window::AboutWindow,
}

impl MainApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app: Self = if let Some(storage) = cc.storage {
            eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default()
        } else {
            Default::default()
        };
        #[cfg(target_arch = "wasm32")]
        {
            app.session = DeviceSession::new(&cc.egui_ctx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if app.remote_url.trim().is_empty() {
                app.remote_url = default_remote_url();
            }
            app.remote_url_draft.clone_from(&app.remote_url);
            app.session =
                match DeviceSession::new(&cc.egui_ctx, app.backend_target, &app.remote_url) {
                    Ok(session) => session,
                    Err(error) => {
                        app.backend_target = BackendTarget::Local;
                        let mut session =
                            DeviceSession::new(&cc.egui_ctx, BackendTarget::Local, &app.remote_url)
                                .unwrap_or_default();
                        session.command_error = Some(error);
                        session
                    }
                };
        }
        app.about_window = ui::about_window::AboutWindow::new(&cc.egui_ctx);
        app
    }

    fn connection_ui(&mut self, ui: &mut egui::Ui) {
        #[cfg(not(target_arch = "wasm32"))]
        ui::usb_panel::backend_selector(
            &mut self.session,
            &mut self.backend_target,
            &mut self.remote_url,
            &mut self.remote_url_draft,
            ui,
        );
        ui::usb_panel::ui(&mut self.session, ui);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn default_remote_url() -> String {
    "http://127.0.0.1:8080".to_owned()
}

impl eframe::App for MainApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, self);
    }

    // Direct transports own the device and shut it down on exit. Remote browser
    // clients deliberately leave the independently running backend untouched.
    fn on_exit(&mut self) {
        self.session.shutdown();
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the top-level layout keeps desktop and mobile section ordering together"
    )]
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.about_window.poll();
        self.session.consume_events(ui.ctx());
        // Repaint periodically for presentation only; backend housekeeping runs
        // independently of egui updates.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));
        egui::Panel::top("top_panel").show_inside(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                egui::widgets::global_theme_preference_buttons(ui);
                ui.separator();
                if ui
                    .add_enabled(
                        self.session.can_control_device() && self.session.can_calibrate(),
                        egui::Button::new("Calibrate"),
                    )
                    .on_disabled_hover_text("Connect the device to a battery first")
                    .clicked()
                {
                    self.calibrate_window.open = true;
                }
                ui.separator();
                if ui.button("Log").clicked() {
                    self.log_window.open = !self.log_window.open;
                }
                ui.separator();
                if ui.button("About").clicked() {
                    self.about_window.open = true;
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    match &self.about_window.update_check_state {
                        crate::update_check::UpdateCheckState::Checking => {
                            ui.separator();
                            ui.spinner();
                            ui.weak("Checking for updates...");
                        }
                        crate::update_check::UpdateCheckState::UpToDate => {
                            ui.separator();
                            ui.label(format!("v{} (up to date)", env!("CARGO_PKG_VERSION")));
                        }
                        crate::update_check::UpdateCheckState::UpdateAvailable(tag) => {
                            ui.separator();
                            ui.colored_label(
                                ui.visuals().warn_fg_color,
                                format!("Update available: {tag}"),
                            );
                            ui.hyperlink_to("Download", crate::update_check::RELEASES_PAGE_URL);
                        }
                        crate::update_check::UpdateCheckState::Failed => {}
                    }
                }
            });
        });

        self.about_window.ui(ui);
        self.calibrate_window.ui(&mut self.session, ui);
        self.log_window.ui(&mut self.session, ui);

        if is_mobile_layout(ui.available_width()) {
            egui::ScrollArea::vertical().show(ui, |ui| {
                for section in mobile_sections(self.session.show_stop_control()) {
                    match section {
                        MobileSection::Connection => self.connection_ui(ui),
                        MobileSection::LiveData => {
                            if self.session.can_control_device() {
                                ui::live_data::ui(&self.session, ui);
                            }
                        }
                        MobileSection::StartControls | MobileSection::StopControl => {
                            if self.session.can_control_device() {
                                self.control_panel.ui_mobile_primary(&mut self.session, ui);
                            }
                        }
                        MobileSection::Plot => {
                            ui.separator();
                            ui.allocate_ui(egui::vec2(ui.available_width(), 280.0), |ui| {
                                ui::plot::ui(&self.session, ui);
                            });
                        }
                        MobileSection::Settings => {
                            if self.session.can_control_device() {
                                self.control_panel.ui_mobile_settings(ui);
                            }
                            self.recipe_panel.ui(&mut self.session, ui);
                        }
                    }
                }
            });
        } else {
            egui::Panel::left("left_panel").show_inside(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.connection_ui(ui);
                    ui.push_id("control_section", |ui| {
                        if self.session.can_control_device() {
                            ui::live_data::ui(&self.session, ui);
                            self.control_panel.ui(&mut self.session, ui);
                        }
                        self.recipe_panel.ui(&mut self.session, ui);
                    });
                });
            });

            egui::CentralPanel::default().show_inside(ui, |ui| {
                ui::plot::ui(&self.session, ui);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_phone_viewports_use_mobile_layout() {
        assert!(is_mobile_layout(320.0));
        assert!(is_mobile_layout(390.0));
        assert!(!is_mobile_layout(MOBILE_BREAKPOINT));
    }

    #[test]
    fn primary_action_precedes_plot_and_settings() {
        let idle = mobile_sections(false);
        let active = mobile_sections(true);
        assert_eq!(idle[2], MobileSection::StartControls);
        assert_eq!(active[2], MobileSection::StopControl);
        assert_eq!(idle[3], MobileSection::Plot);
        assert_eq!(active[3], MobileSection::Plot);
        assert_eq!(idle[4], MobileSection::Settings);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn unapplied_remote_url_draft_is_not_persisted() {
        let app = MainApp {
            backend_target: BackendTarget::Remote,
            remote_url: "http://active.example:8080".to_owned(),
            remote_url_draft: "not a valid url".to_owned(),
            ..MainApp::default()
        };
        let serialized = serde_json::to_string(&app)
            .unwrap_or_else(|error| panic!("failed to serialize app: {error}"));
        let restored: MainApp = serde_json::from_str(&serialized)
            .unwrap_or_else(|error| panic!("failed to restore app: {error}"));

        assert_eq!(restored.backend_target, BackendTarget::Remote);
        assert_eq!(restored.remote_url, "http://active.example:8080");
        assert!(restored.remote_url_draft.is_empty());
        assert!(!serialized.contains("not a valid url"));
    }
}
