//! Presentation-only views of bounded telemetry. Final run metrics come from `RunSummary`.
use crate::core::{CycleSample, Sample, TestConfiguration, power_microwatts};
use crate::session::DeviceSession;
use crate::ui::{format_cycle_state, format_duration, format_test_state};
use egui_plot::{AxisHints, HLine, Legend, Line, Plot, PlotPoint};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum PlotMetric {
    #[default]
    Voltage,
    Current,
    Power,
}

impl PlotMetric {
    fn label(self) -> &'static str {
        match self {
            Self::Voltage => "Voltage (V)",
            Self::Current => "Current (A)",
            Self::Power => "Power (W)",
        }
    }
    fn value(self, voltage_mv: u16, current_ma: u16) -> f64 {
        match self {
            Self::Voltage => f64::from(voltage_mv) / 1000.0,
            Self::Current => f64::from(current_ma) / 1000.0,
            Self::Power => power_w(voltage_mv, current_ma),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum PhysicalXAxis {
    #[default]
    Time,
    Capacity,
    Energy,
}

impl PhysicalXAxis {
    fn label(self) -> &'static str {
        match self {
            Self::Time => "Time",
            Self::Capacity => "Capacity (mAh)",
            Self::Energy => "Energy (Wh)",
        }
    }
    fn value(self, sample: &Sample) -> f64 {
        match self {
            Self::Time => sample.elapsed_seconds as f64,
            Self::Capacity => sample.capacity_mah as f64,
            Self::Energy => sample.energy_wh,
        }
    }
}

pub(crate) fn power_w(voltage_mv: u16, current_ma: u16) -> f64 {
    power_microwatts(voltage_mv, current_ma) as f64 / 1_000_000.0
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PlotOptions {
    pub metric: PlotMetric,
    pub x_axis: PhysicalXAxis,
}

pub(crate) fn metric_controls(options: &mut PlotOptions, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.label("Metric:");
        for (metric, label) in [
            (PlotMetric::Voltage, "Voltage"),
            (PlotMetric::Current, "Current"),
            (PlotMetric::Power, "Power"),
        ] {
            ui.selectable_value(&mut options.metric, metric, label);
        }
    });
}

pub(crate) fn physical_controls(options: &mut PlotOptions, ui: &mut egui::Ui) {
    metric_controls(options, ui);
    ui.horizontal_wrapped(|ui| {
        ui.label("X axis:");
        for (axis, label) in [
            (PhysicalXAxis::Time, "Time"),
            (PhysicalXAxis::Capacity, "Capacity"),
            (PhysicalXAxis::Energy, "Energy"),
        ] {
            ui.selectable_value(&mut options.x_axis, axis, label);
        }
    });
}

pub(crate) fn ui(session: &DeviceSession, ui: &mut egui::Ui) {
    let selection_id = ui.make_persistent_id("telemetry_plot_cycle");
    let mut show_cycle = ui.data_mut(|data| {
        data.get_persisted::<bool>(selection_id)
            .unwrap_or(!session.cycle_samples.is_empty())
    });
    if !session.cycle_samples.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label("Scope:");
            ui.selectable_value(&mut show_cycle, false, "Physical run");
            ui.selectable_value(&mut show_cycle, true, "Whole cycle");
        });
        ui.data_mut(|data| data.insert_persisted(selection_id, show_cycle));
    } else {
        show_cycle = false;
    }
    let options_id = ui.make_persistent_id("live_plot_options");
    let mut options =
        ui.data_mut(|data| data.get_temp::<PlotOptions>(options_id).unwrap_or_default());
    if show_cycle {
        metric_controls(&mut options, ui);
        ui.label("X axis: Time");
        if session.cycle_samples.is_empty() {
            ui.weak("No whole-cycle telemetry samples are available.");
        } else {
            cycle_samples_plot(
                (
                    "live_cycle",
                    session.cycle.execution_id.as_deref(),
                    options.metric,
                ),
                &session.cycle_samples,
                options.metric,
                ui,
            );
        }
    } else {
        physical_controls(&mut options, ui);
        if session.samples.is_empty() {
            ui.weak("No physical-run telemetry samples are available yet.");
        } else {
            physical_samples_plot(
                (
                    "live_run",
                    session.samples.last().map(|sample| sample.run_id.as_str()),
                    options.metric,
                    options.x_axis,
                ),
                &session.samples,
                session.current_test_config,
                options,
                ui,
            );
        }
    }
    ui.data_mut(|data| data.insert_temp(options_id, options));
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReferenceLine {
    pub label: &'static str,
    pub value: f64,
}

pub(crate) fn reference_lines(config: TestConfiguration, metric: PlotMetric) -> Vec<ReferenceLine> {
    use TestConfiguration as C;
    match (config, metric) {
        (
            C::DischargeConstantCurrent {
                cutoff_voltage_mv, ..
            }
            | C::DischargeConstantPower {
                cutoff_voltage_mv, ..
            },
            PlotMetric::Voltage,
        ) => vec![ReferenceLine {
            label: "Cutoff voltage",
            value: f64::from(cutoff_voltage_mv) / 1000.0,
        }],
        (C::DischargeConstantCurrent { current_ma, .. }, PlotMetric::Current) => {
            vec![ReferenceLine {
                label: "Configured current",
                value: f64::from(current_ma) / 1000.0,
            }]
        }
        (C::DischargeConstantPower { power_w, .. }, PlotMetric::Power) => vec![ReferenceLine {
            label: "Configured power",
            value: f64::from(power_w),
        }],
        (C::ChargeConstantVoltage { voltage_mv, .. }, PlotMetric::Voltage) => vec![ReferenceLine {
            label: "Target voltage",
            value: f64::from(voltage_mv) / 1000.0,
        }],
        (
            C::ChargeConstantVoltage {
                current_ma,
                cutoff_current_ma,
                ..
            },
            PlotMetric::Current,
        ) => vec![
            ReferenceLine {
                label: "Configured current",
                value: f64::from(current_ma) / 1000.0,
            },
            ReferenceLine {
                label: "Cutoff current",
                value: f64::from(cutoff_current_ma) / 1000.0,
            },
        ],
        _ => Vec::new(),
    }
}

pub(crate) fn physical_points(samples: &[Sample], options: PlotOptions) -> Vec<[f64; 2]> {
    samples
        .iter()
        .map(|s| {
            [
                options.x_axis.value(s),
                options.metric.value(s.voltage_mv, s.current_ma),
            ]
        })
        .collect()
}

fn physical_tooltip(sample: &Sample) -> String {
    format!(
        "{}\n{:.3} V\n{:.3} A\n{:.3} W\n{} mAh\n{:.3} Wh",
        format_duration(sample.elapsed_seconds as f64),
        f64::from(sample.voltage_mv) / 1000.0,
        f64::from(sample.current_ma) / 1000.0,
        power_w(sample.voltage_mv, sample.current_ma),
        sample.capacity_mah,
        sample.energy_wh
    )
}

pub(crate) fn physical_samples_plot(
    id: impl std::hash::Hash,
    samples: &[Sample],
    config: Option<TestConfiguration>,
    options: PlotOptions,
    ui: &mut egui::Ui,
) {
    if samples.is_empty() {
        ui.weak("No telemetry samples are available for this run.");
        return;
    }
    let points = physical_points(samples, options);
    let label_formatter = |_name: &str, point: &PlotPoint| {
        samples
            .iter()
            .min_by(|a, b| {
                let distance = |s: &Sample| {
                    let dx = (options.x_axis.value(s) - point.x).abs();
                    let dy = (options.metric.value(s.voltage_mv, s.current_ma) - point.y).abs();
                    (dx, dy)
                };
                distance(a)
                    .partial_cmp(&distance(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map_or_else(String::new, physical_tooltip)
    };
    let references = config.map_or_else(Vec::new, |c| reference_lines(c, options.metric));
    graph(
        (id, options.metric, options.x_axis),
        options.x_axis.label(),
        options.metric.label(),
        options.x_axis == PhysicalXAxis::Time,
        label_formatter,
        !references.is_empty(),
        ui,
        |plot_ui| {
            plot_ui.line(Line::new("run", points).name(options.metric.label()));
            for reference in references {
                plot_ui.hline(
                    HLine::new(reference.label, reference.value)
                        .name(reference.label)
                        .style(egui_plot::LineStyle::Dashed { length: 6.0 }),
                );
            }
        },
    );
}

pub(crate) fn cycle_points(samples: &[CycleSample], metric: PlotMetric) -> Vec<[f64; 2]> {
    samples
        .iter()
        .map(|s| {
            [
                s.elapsed_milliseconds as f64 / 1000.0,
                metric.value(s.voltage_mv, s.current_ma),
            ]
        })
        .collect()
}

pub(crate) fn cycle_samples_plot(
    id: impl std::hash::Hash,
    samples: &[CycleSample],
    metric: PlotMetric,
    ui: &mut egui::Ui,
) {
    if samples.is_empty() {
        ui.weak("No whole-cycle telemetry samples are available.");
        return;
    }
    let points = cycle_points(samples, metric);
    let label_formatter = |_name: &str, point: &PlotPoint| {
        samples
            .iter()
            .min_by(|a, b| {
                let distance = |s: &CycleSample| {
                    (
                        (s.elapsed_milliseconds as f64 / 1000.0 - point.x).abs(),
                        (metric.value(s.voltage_mv, s.current_ma) - point.y).abs(),
                    )
                };
                distance(a)
                    .partial_cmp(&distance(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map_or_else(String::new, |s| {
                format!(
                    "{}\n{:.3} V\n{:.3} A\n{:.3} W\nRepeat {} / Step {}\n{} / {}{}{}",
                    format_duration(s.elapsed_milliseconds as f64 / 1000.0),
                    f64::from(s.voltage_mv) / 1000.0,
                    f64::from(s.current_ma) / 1000.0,
                    power_w(s.voltage_mv, s.current_ma),
                    s.repeat_index + 1,
                    s.step_index + 1,
                    format_cycle_state(s.cycle_state),
                    format_test_state(&s.test_state),
                    s.test_capacity_mah
                        .map_or_else(String::new, |v| format!("\n{v} mAh")),
                    if s.test_capacity_mah.is_some() {
                        format!("\n{:.3} Wh", s.test_energy_wh)
                    } else {
                        String::new()
                    }
                )
            })
    };
    graph(
        (id, metric),
        "Time",
        metric.label(),
        true,
        label_formatter,
        false,
        ui,
        |plot_ui| plot_ui.line(Line::new("cycle", points).name(metric.label())),
    );
}

pub(crate) struct ComparisonCurve<'a> {
    pub key: &'a str,
    pub label: &'a str,
    pub samples: &'a [Sample],
}

pub(crate) fn comparison_plot(
    id: impl std::hash::Hash,
    runs: &[ComparisonCurve<'_>],
    options: PlotOptions,
    ui: &mut egui::Ui,
) {
    let label_formatter = |name: &str, point: &PlotPoint| {
        runs.iter()
            .find(|run| run.label == name || run.key == name)
            .and_then(|run| {
                run.samples
                    .iter()
                    .min_by(|a, b| {
                        let distance = |s: &Sample| {
                            (
                                (options.x_axis.value(s) - point.x).abs(),
                                (options.metric.value(s.voltage_mv, s.current_ma) - point.y).abs(),
                            )
                        };
                        distance(a)
                            .partial_cmp(&distance(b))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|sample| (run, sample))
            })
            .map_or_else(String::new, |(run, sample)| {
                format!("{}\n{}\n{}", run.label, run.key, physical_tooltip(sample))
            })
    };
    graph(
        (id, options.metric, options.x_axis),
        options.x_axis.label(),
        options.metric.label(),
        options.x_axis == PhysicalXAxis::Time,
        label_formatter,
        true,
        ui,
        |plot_ui| {
            for run in runs {
                plot_ui.line(
                    Line::new(run.key, physical_points(run.samples, options)).name(run.label),
                );
            }
        },
    );
}

#[expect(
    clippy::too_many_arguments,
    reason = "plot axis, legend, and drawing inputs are explicit"
)]
fn graph(
    id: impl std::hash::Hash,
    x_label: &str,
    y_label: &str,
    time_axis: bool,
    label_formatter: impl Fn(&str, &PlotPoint) -> String,
    show_legend: bool,
    ui: &mut egui::Ui,
    draw: impl FnOnce(&mut egui_plot::PlotUi<'_>),
) {
    let mut x = AxisHints::new_x().label(x_label);
    if time_axis {
        x = x.formatter(|mark, _| format_duration(mark.value));
    }
    let mut plot = Plot::new(id)
        .label_formatter(label_formatter)
        .custom_x_axes(vec![x])
        .custom_y_axes(vec![AxisHints::new_y().label(y_label)]);
    if show_legend {
        plot = plot.legend(Legend::default());
    }
    plot.show(ui, draw);
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "plot fixture must deserialize")]
mod tests {
    use super::*;

    fn sample() -> Sample {
        serde_json::from_value(serde_json::json!({"timestamp_utc":"now","elapsed_seconds":83,"voltage_mv":4000,"current_ma":2000,"capacity_mah":123,"energy_wh":0.456,"mode":"DischargeConstantCurrent"})).expect("plot sample")
    }

    #[test]
    fn physical_axes_and_metrics_use_sample_units() {
        let sample = sample();
        for (axis, x) in [
            (PhysicalXAxis::Time, 83.0),
            (PhysicalXAxis::Capacity, 123.0),
            (PhysicalXAxis::Energy, 0.456),
        ] {
            for (metric, y) in [
                (PlotMetric::Voltage, 4.0),
                (PlotMetric::Current, 2.0),
                (PlotMetric::Power, 8.0),
            ] {
                assert_eq!(
                    physical_points(
                        std::slice::from_ref(&sample),
                        PlotOptions {
                            metric,
                            x_axis: axis
                        }
                    ),
                    vec![[x, y]]
                );
            }
        }
        let tooltip = physical_tooltip(&sample);
        for value in ["4.000 V", "2.000 A", "8.000 W", "123 mAh", "0.456 Wh"] {
            assert!(tooltip.contains(value));
        }
    }

    #[test]
    fn cycle_metric_uses_time_only_and_correct_units() {
        let sample = CycleSample {
            execution_id: "cycle".into(),
            sequence: 0,
            timestamp_utc: "now".into(),
            elapsed_milliseconds: 2500,
            repeat_index: 0,
            step_index: 0,
            cycle_state: crate::core::CycleState::RunningStep,
            test_state: crate::core::TestState::Running,
            mode: crate::device::DeviceMode::DischargeConstantCurrent,
            activity_known: true,
            active: true,
            voltage_mv: 4000,
            current_ma: 2000,
            device_capacity_mah: 10,
            test_capacity_mah: Some(10),
            test_energy_wh: 0.1,
        };
        for (metric, y) in [
            (PlotMetric::Voltage, 4.0),
            (PlotMetric::Current, 2.0),
            (PlotMetric::Power, 8.0),
        ] {
            assert_eq!(
                cycle_points(std::slice::from_ref(&sample), metric),
                vec![[2.5, y]]
            );
        }
    }
    #[test]
    fn reference_mapping_and_power() {
        let cc = TestConfiguration::DischargeConstantCurrent {
            current_ma: 2000,
            cutoff_voltage_mv: 3000,
            cutoff_time_min: 10,
        };
        let cp = TestConfiguration::DischargeConstantPower {
            power_w: 8,
            cutoff_voltage_mv: 3100,
            cutoff_time_min: 10,
        };
        let cv = TestConfiguration::ChargeConstantVoltage {
            current_ma: 3000,
            voltage_mv: 4200,
            cutoff_current_ma: 100,
        };
        assert_eq!(reference_lines(cc, PlotMetric::Voltage)[0].value, 3.0);
        assert_eq!(reference_lines(cc, PlotMetric::Current)[0].value, 2.0);
        assert!(reference_lines(cc, PlotMetric::Power).is_empty());
        assert_eq!(reference_lines(cp, PlotMetric::Voltage)[0].value, 3.1);
        assert_eq!(reference_lines(cp, PlotMetric::Power)[0].value, 8.0);
        assert!(reference_lines(cp, PlotMetric::Current).is_empty());
        assert_eq!(reference_lines(cv, PlotMetric::Voltage)[0].value, 4.2);
        assert_eq!(reference_lines(cv, PlotMetric::Current).len(), 2);
        assert!(reference_lines(cv, PlotMetric::Power).is_empty());
        assert_eq!(power_w(4000, 2000), 8.0);
        assert_eq!(power_w(u16::MAX, u16::MAX), 4294.836225);
    }
}
