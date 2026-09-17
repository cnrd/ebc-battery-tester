use crate::session::DeviceSession;
use crate::ui;

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
        app.session = DeviceSession::new(&cc.egui_ctx);
        app.about_window = ui::about_window::AboutWindow::new(&cc.egui_ctx);
        app
    }
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

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.about_window.poll();
        self.session.consume_events(ui.ctx());
        self.session.send_timer_sync_if_needed(ui.ctx());
        // Request a repaint every second to update the timer. This is needed so
        // that the clock is updated every second. Not when something happens.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(1));
        egui::Panel::top("top_panel").show_inside(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                egui::widgets::global_theme_preference_buttons(ui);
                ui.separator();
                if ui
                    .add_enabled(
                        self.session.can_control_device() && self.session.has_live_voltage(),
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
                for section in mobile_sections(self.session.mode_on) {
                    match section {
                        MobileSection::Connection => ui::usb_panel::ui(&mut self.session, ui),
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
                        }
                    }
                }
            });
        } else {
            egui::Panel::left("left_panel").show_inside(ui, |ui| {
                ui::usb_panel::ui(&mut self.session, ui);
                ui.push_id("control_section", |ui| {
                    if self.session.can_control_device() {
                        ui::live_data::ui(&self.session, ui);
                        self.control_panel.ui(&mut self.session, ui);
                    }
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
}
