use crate::session::DeviceSession;
use crate::ui::format_duration;
use egui_plot::{AxisHints, HPlacement, Legend, Line, Plot, PlotPoint, VPlacement};

pub(crate) fn ui(session: &DeviceSession, ui: &mut egui::Ui) {
    let selection_id = ui.make_persistent_id("telemetry_plot_cycle");
    let mut show_cycle = ui.data_mut(|data| {
        data.get_persisted::<bool>(selection_id)
            .unwrap_or(!session.cycle_samples.is_empty())
    });
    if !session.cycle_samples.is_empty() {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut show_cycle, false, "Physical run");
            ui.selectable_value(&mut show_cycle, true, "Whole cycle");
        });
        ui.data_mut(|data| data.insert_persisted(selection_id, show_cycle));
    } else {
        show_cycle = false;
    }

    if show_cycle {
        cycle_plot(session, ui);
    } else {
        physical_plot(session, ui);
    }
}

fn physical_plot(session: &DeviceSession, ui: &mut egui::Ui) {
    let voltage_points: Vec<[f64; 2]> = session
        .samples
        .iter()
        .map(|sample| {
            [
                sample.elapsed_seconds as f64,
                sample.voltage_mv as f64 / 1000.0,
            ]
        })
        .collect();
    let current_points: Vec<[f64; 2]> = session
        .samples
        .iter()
        .map(|sample| {
            [
                sample.elapsed_seconds as f64,
                sample.current_ma as f64 / 1000.0,
            ]
        })
        .collect();
    let label_formatter = |_name: &str, point: &PlotPoint| {
        let sample = session.samples.iter().min_by(|left, right| {
            (left.elapsed_seconds as f64 - point.x)
                .abs()
                .total_cmp(&(right.elapsed_seconds as f64 - point.x).abs())
        });
        sample.map_or_else(
            || format_duration(point.x),
            |sample| {
                format!(
                    "{}\n{:.3} V\n{:.3} A",
                    format_duration(sample.elapsed_seconds as f64),
                    sample.voltage_mv as f64 / 1000.0,
                    sample.current_ma as f64 / 1000.0,
                )
            },
        )
    };
    show_plot(
        "live_data_plot",
        voltage_points,
        current_points,
        label_formatter,
        ui,
    );
}

fn cycle_plot(session: &DeviceSession, ui: &mut egui::Ui) {
    let voltage_points: Vec<[f64; 2]> = session
        .cycle_samples
        .iter()
        .map(|sample| {
            [
                sample.elapsed_milliseconds as f64 / 1000.0,
                sample.voltage_mv as f64 / 1000.0,
            ]
        })
        .collect();
    let current_points: Vec<[f64; 2]> = session
        .cycle_samples
        .iter()
        .map(|sample| {
            [
                sample.elapsed_milliseconds as f64 / 1000.0,
                sample.current_ma as f64 / 1000.0,
            ]
        })
        .collect();
    let label_formatter = |_name: &str, point: &PlotPoint| {
        let sample = session.cycle_samples.iter().min_by(|left, right| {
            (left.elapsed_milliseconds as f64 / 1000.0 - point.x)
                .abs()
                .total_cmp(&(right.elapsed_milliseconds as f64 / 1000.0 - point.x).abs())
        });
        sample.map_or_else(
            || format_duration(point.x),
            |sample| {
                format!(
                    "{}\n{:.3} V / {:.3} A\nRepeat {} / Step {}\n{:?} / {:?}",
                    format_duration(sample.elapsed_milliseconds as f64 / 1000.0),
                    sample.voltage_mv as f64 / 1000.0,
                    sample.current_ma as f64 / 1000.0,
                    sample.repeat_index + 1,
                    sample.step_index + 1,
                    sample.cycle_state,
                    sample.test_state,
                )
            },
        )
    };
    show_plot(
        "cycle_data_plot",
        voltage_points,
        current_points,
        label_formatter,
        ui,
    );
}

fn show_plot(
    id: &'static str,
    voltage_points: Vec<[f64; 2]>,
    current_points: Vec<[f64; 2]>,
    label_formatter: impl Fn(&str, &PlotPoint) -> String,
    ui: &mut egui::Ui,
) {
    let time_axis_formatter =
        |mark: egui_plot::GridMark, _range: &std::ops::RangeInclusive<f64>| {
            format_duration(mark.value)
        };

    Plot::new(id)
        .legend(Legend::default())
        .label_formatter(label_formatter)
        .custom_x_axes(vec![
            AxisHints::new_x()
                .label("Time")
                .formatter(time_axis_formatter),
            AxisHints::new_x()
                .label("Time")
                .placement(VPlacement::Top)
                .formatter(time_axis_formatter),
        ])
        .custom_y_axes(vec![
            AxisHints::new_y()
                .label("Voltage (V) / Current (A)")
                .placement(HPlacement::Left),
            AxisHints::new_y()
                .label("Voltage (V) / Current (A)")
                .placement(HPlacement::Right),
        ])
        .show(ui, |plot_ui| {
            plot_ui.line(Line::new("Voltage", voltage_points).name("Voltage"));
            plot_ui.line(Line::new("Current", current_points).name("Current"));
        });
}
