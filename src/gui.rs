use std::time::Duration;
use std::{fs, path::Path};

use anyhow::Result;
use eframe::egui;

use crate::pump_api::{
    AppState, CycleStep, MeasurementPoint, PumpClient, PumpCmd, PumpRecipe, RecipeKind, TabView,
    auto_detect_serial_port, clamp_backsteps, clamp_measurement_interval_ms, clamp_speed_step,
    export_measurements_csv, load_recipe, save_recipe,
};

pub fn run_gui() -> Result<()> {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Ismatec Pump Controller",
        options,
        Box::new(|_cc| Ok(Box::<PumpGuiApp>::default())),
    )
    .map_err(|e| anyhow::anyhow!("GUI failed: {e}"))
}

#[derive(Default)]
struct PumpGuiApp {
    state: AppState,
    client: PumpClient,
    ui_settings: UiSettings,
    active_theme: VisualTheme,
    measurement_plot: MeasurementPlotSettings,
    measurement_interval_input: String,
    show_plot_export_modal: bool,
    recipe_files: Vec<String>,
    selected_recipe_idx: usize,
    file_confirm: Option<FileConfirmDialog>,
    ui_loaded_from_recipe: bool,
    startup_automation_done: bool,
    pending_auto_measure_after_connect: bool,
}

#[derive(Clone, Debug)]
enum FileAction {
    SaveRecipe { path: String },
    ExportCsv { path: String },
    ExportPlotSvg { path: String },
    DeleteRecipe { path: String },
}

#[derive(Clone, Debug)]
struct FileConfirmDialog {
    title: String,
    body: String,
    action: FileAction,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum VisualTheme {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Debug)]
struct UiSettings {
    density: f32,
    font_scale: f32,
    show_sidebar_on_wide: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            density: 1.0,
            font_scale: 1.0,
            show_sidebar_on_wide: true,
        }
    }
}

#[derive(Clone, Debug)]
struct MeasurementPlotSettings {
    title: String,
    show_legend: bool,
    show_flow: bool,
    show_setpoint: bool,
    show_theoretical: bool,
    show_estimated: bool,
    show_grid: bool,
    show_points: bool,
    auto_scale_y: bool,
    y_min: f64,
    y_max: f64,
    line_width: f32,
    max_points: usize,
    svg_export_path: String,
}

impl Default for MeasurementPlotSettings {
    fn default() -> Self {
        Self {
            title: "Measurement Plot".to_string(),
            show_legend: true,
            show_flow: true,
            show_setpoint: true,
            show_theoretical: false,
            show_estimated: false,
            show_grid: true,
            show_points: false,
            auto_scale_y: true,
            y_min: 0.0,
            y_max: 1.5,
            line_width: 2.0,
            max_points: 600,
            svg_export_path: "measurement_plot.svg".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// GUI-to-pump cascade helpers
//
// Every panel follows the same pattern:
//   1. Render a widget editing a value in `self.state.recipe` or similar
//      local state.
//   2. Only when the user *commits* the edit (Enter / drag stopped / button
//      click) do we call `send_cmd(...)`, which:
//         - marks the command as pending (increments `state.pending`),
//         - logs a short description,
//         - emits the `PumpCmd` to the worker.
//   3. The GUI reacts to `PumpEvt::Response` / `PumpEvt::Error` by clearing
//      the pending counter in `AppState::apply_event`, which naturally
//      re-enables the "pending-aware" buttons drawn via `Self::button_busy`.
// ---------------------------------------------------------------------------

impl PumpGuiApp {
    fn ensure_path_defaults(&mut self) {
        if self.state.recipe.ui.recipe_folder.is_empty() {
            self.state.recipe.ui.recipe_folder = ".".to_string();
        }
        if self.state.recipe.ui.data_folder.is_empty() {
            self.state.recipe.ui.data_folder = ".".to_string();
        }
    }

    fn apply_recipe_ui_settings(&mut self) {
        let ui = &self.state.recipe.ui;
        self.ui_settings.density = ui.density;
        self.ui_settings.font_scale = ui.font_scale;
        self.ui_settings.show_sidebar_on_wide = ui.show_sidebar_on_wide;
        self.active_theme = if ui.theme.eq_ignore_ascii_case("light") {
            VisualTheme::Light
        } else {
            VisualTheme::Dark
        };
        self.measurement_plot.title = ui.plot.title.clone();
        self.measurement_plot.show_legend = ui.plot.show_legend;
        self.measurement_plot.show_flow = ui.plot.show_flow;
        self.measurement_plot.show_setpoint = ui.plot.show_setpoint;
        self.measurement_plot.show_theoretical = ui.plot.show_theoretical;
        self.measurement_plot.show_estimated = ui.plot.show_estimated;
        self.measurement_plot.show_grid = ui.plot.show_grid;
        self.measurement_plot.show_points = ui.plot.show_points;
        self.measurement_plot.auto_scale_y = ui.plot.auto_scale_y;
        self.measurement_plot.y_min = ui.plot.y_min;
        self.measurement_plot.y_max = ui.plot.y_max;
        self.measurement_plot.line_width = ui.plot.line_width;
        self.measurement_plot.max_points = ui.plot.max_points;
        self.measurement_plot.svg_export_path = ui.plot.svg_export_path.clone();
    }

    fn persist_ui_settings_to_recipe(&mut self) {
        self.state.recipe.ui.density = self.ui_settings.density;
        self.state.recipe.ui.font_scale = self.ui_settings.font_scale;
        self.state.recipe.ui.show_sidebar_on_wide = self.ui_settings.show_sidebar_on_wide;
        self.state.recipe.ui.theme = if self.active_theme == VisualTheme::Light {
            "light".to_string()
        } else {
            "dark".to_string()
        };
        self.state.recipe.ui.plot.title = self.measurement_plot.title.clone();
        self.state.recipe.ui.plot.show_legend = self.measurement_plot.show_legend;
        self.state.recipe.ui.plot.show_flow = self.measurement_plot.show_flow;
        self.state.recipe.ui.plot.show_setpoint = self.measurement_plot.show_setpoint;
        self.state.recipe.ui.plot.show_theoretical = self.measurement_plot.show_theoretical;
        self.state.recipe.ui.plot.show_estimated = self.measurement_plot.show_estimated;
        self.state.recipe.ui.plot.show_grid = self.measurement_plot.show_grid;
        self.state.recipe.ui.plot.show_points = self.measurement_plot.show_points;
        self.state.recipe.ui.plot.auto_scale_y = self.measurement_plot.auto_scale_y;
        self.state.recipe.ui.plot.y_min = self.measurement_plot.y_min;
        self.state.recipe.ui.plot.y_max = self.measurement_plot.y_max;
        self.state.recipe.ui.plot.line_width = self.measurement_plot.line_width;
        self.state.recipe.ui.plot.max_points = self.measurement_plot.max_points;
        self.state.recipe.ui.plot.svg_export_path = self.measurement_plot.svg_export_path.clone();
    }

    fn maybe_run_startup_automation(&mut self) {
        if self.startup_automation_done {
            return;
        }
        self.startup_automation_done = true;
        if self.state.recipe.ui.auto_connect_on_start && !self.state.connected {
            if let Some(port) = auto_detect_serial_port(&self.state.recipe.serial_port) {
                self.state.recipe.serial_port = port.clone();
                self.send_cmd(
                    format!("Auto-connect {port} (addr {})", self.state.recipe.pump_addr),
                    PumpCmd::Connect {
                        port,
                        addr: self.state.recipe.pump_addr,
                    },
                );
                self.pending_auto_measure_after_connect = self.state.recipe.ui.auto_measure_on_start;
            } else {
                self.state
                    .push_log("Auto-connect enabled but no serial ports were detected");
            }
        }
    }

    fn recipe_full_path(&self) -> String {
        let p = Path::new(&self.state.recipe_file_path);
        if p.is_absolute() {
            self.state.recipe_file_path.clone()
        } else {
            Path::new(&self.state.recipe.ui.recipe_folder)
                .join(&self.state.recipe_file_path)
                .to_string_lossy()
                .to_string()
        }
    }

    fn csv_full_path(&self) -> String {
        let p = Path::new(&self.state.recipe.csv_export_path);
        if p.is_absolute() {
            self.state.recipe.csv_export_path.clone()
        } else {
            Path::new(&self.state.recipe.ui.data_folder)
                .join(&self.state.recipe.csv_export_path)
                .to_string_lossy()
                .to_string()
        }
    }

    fn svg_full_path(&self) -> String {
        let p = Path::new(&self.measurement_plot.svg_export_path);
        if p.is_absolute() {
            self.measurement_plot.svg_export_path.clone()
        } else {
            Path::new(&self.state.recipe.ui.data_folder)
                .join(&self.measurement_plot.svg_export_path)
                .to_string_lossy()
                .to_string()
        }
    }

    fn refresh_recipe_files(&mut self) {
        let mut files = Vec::new();
        match fs::read_dir(&self.state.recipe.ui.recipe_folder) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let is_json = path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
                    if is_json
                        && let Some(name) = path.file_name().and_then(|v| v.to_str())
                    {
                        files.push(name.to_string());
                    }
                }
                files.sort();
                self.recipe_files = files;
                if self.selected_recipe_idx >= self.recipe_files.len() {
                    self.selected_recipe_idx = 0;
                }
            }
            Err(e) => self
                .state
                .push_log(format!(
                    "Cannot read recipe folder {}: {e}",
                    self.state.recipe.ui.recipe_folder
                )),
        }
    }

    fn prompt_overwrite_or_run(&mut self, path: &str, action: FileAction, title: &str) {
        if Path::new(path).exists() {
            self.file_confirm = Some(FileConfirmDialog {
                title: title.to_string(),
                body: format!("File exists and will be overwritten:\n{path}"),
                action,
            });
        } else {
            self.run_file_action(action);
        }
    }

    fn run_file_action(&mut self, action: FileAction) {
        match action {
            FileAction::SaveRecipe { path } => {
                self.persist_ui_settings_to_recipe();
                match save_recipe(&path, &self.state.recipe) {
                    Ok(_) => self.state.push_log(format!("Saved {path}")),
                    Err(e) => self.state.push_log(format!("Save failed: {e}")),
                }
            }
            FileAction::ExportCsv { path } => match export_measurements_csv(&path, &self.state.measurements)
            {
                Ok(_) => self
                    .state
                    .push_log(format!("CSV exported to {path} ({} samples)", self.state.measurements.len())),
                Err(e) => self.state.push_log(format!("CSV export failed: {e}")),
            },
            FileAction::ExportPlotSvg { path } => {
                match export_measurement_svg(&path, self.measurement_slice(), &self.measurement_plot) {
                    Ok(_) => {
                        self.state.push_log(format!("Plot exported to {path}"));
                        self.show_plot_export_modal = false;
                    }
                    Err(e) => self.state.push_log(format!("Plot export failed: {e}")),
                }
            }
            FileAction::DeleteRecipe { path } => match fs::remove_file(&path) {
                Ok(_) => {
                    self.state.push_log(format!("Deleted {path}"));
                    self.refresh_recipe_files();
                }
                Err(e) => self.state.push_log(format!("Delete failed: {e}")),
            },
        }
    }

    fn load_recipe_and_apply(&mut self, path: &str) {
        match load_recipe(path) {
            Ok(recipe) => {
                self.state.recipe = recipe.clone();
                self.apply_recipe_ui_settings();
                self.state.calibration_input_buffer =
                    format!("{:.3}", self.state.recipe.calibrated_max_flow_ml_min);
                self.state.push_log(format!("Loaded {path} — applying to pump"));
                self.send_cmd("Apply recipe", PumpCmd::ApplyRecipe(recipe));
            }
            Err(e) => self.state.push_log(format!("Load failed: {e}")),
        }
    }

    fn measurement_slice(&self) -> &[MeasurementPoint] {
        let total = self.state.measurements.len();
        let start = total.saturating_sub(self.measurement_plot.max_points);
        &self.state.measurements[start..]
    }

    fn measurement_y_range(&self, points: &[MeasurementPoint]) -> Option<(f64, f64)> {
        let mut min_y = f64::INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        let mut saw_series = false;
        for p in points {
            if self.measurement_plot.show_flow {
                min_y = min_y.min(p.flow_ml_min);
                max_y = max_y.max(p.flow_ml_min);
                saw_series = true;
            }
            if self.measurement_plot.show_setpoint {
                min_y = min_y.min(p.speed_setpoint_ml_min);
                max_y = max_y.max(p.speed_setpoint_ml_min);
                saw_series = true;
            }
            if self.measurement_plot.show_theoretical {
                min_y = min_y.min(p.flow_theoretical_ml_min);
                max_y = max_y.max(p.flow_theoretical_ml_min);
                saw_series = true;
            }
            if self.measurement_plot.show_estimated {
                min_y = min_y.min(p.flow_estimated_ml_min);
                max_y = max_y.max(p.flow_estimated_ml_min);
                saw_series = true;
            }
        }
        if !saw_series {
            return None;
        }
        if self.measurement_plot.auto_scale_y {
            let span = (max_y - min_y).max(0.001);
            let pad = span * 0.12;
            Some((min_y - pad, max_y + pad))
        } else {
            let y0 = self.measurement_plot.y_min;
            let mut y1 = self.measurement_plot.y_max;
            if y1 <= y0 {
                y1 = y0 + 0.5;
            }
            Some((y0, y1))
        }
    }

    fn draw_measurement_plot(&self, ui: &mut egui::Ui) {
        let points = self.measurement_slice();
        let desired_h = if ui.available_width() < 760.0 {
            220.0
        } else {
            300.0
        };
        let size = egui::vec2(ui.available_width().max(260.0), desired_h);
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        let painter = ui.painter_at(rect);

        let bg = if self.active_theme == VisualTheme::Dark {
            egui::Color32::from_rgb(20, 24, 34)
        } else {
            egui::Color32::from_rgb(244, 246, 250)
        };
        painter.rect_filled(rect, egui::CornerRadius::same(8), bg);

        if points.len() < 2 {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Collecting samples...",
                egui::TextStyle::Body.resolve(ui.style()),
                ui.style().visuals.text_color(),
            );
            return;
        }

        let x_min = points.first().map(|p| p.elapsed_s).unwrap_or(0.0);
        let mut x_max = points.last().map(|p| p.elapsed_s).unwrap_or(1.0);
        if x_max <= x_min {
            x_max = x_min + 1.0;
        }
        let Some((y_min, y_max)) = self.measurement_y_range(points) else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Enable at least one series",
                egui::TextStyle::Body.resolve(ui.style()),
                ui.style().visuals.text_color(),
            );
            return;
        };

        let margins = egui::Margin {
            left: 58,
            right: 22,
            top: 24,
            bottom: 42,
        };
        let plot_rect = rect - margins;
        let to_pos = |x: f64, y: f64| -> egui::Pos2 {
            let tx = ((x - x_min) / (x_max - x_min)).clamp(0.0, 1.0) as f32;
            let ty = ((y - y_min) / (y_max - y_min)).clamp(0.0, 1.0) as f32;
            egui::pos2(
                egui::lerp(plot_rect.x_range(), tx),
                egui::lerp(plot_rect.y_range(), 1.0 - ty),
            )
        };

        if self.measurement_plot.show_grid {
            let grid_color = if self.active_theme == VisualTheme::Dark {
                egui::Color32::from_gray(55)
            } else {
                egui::Color32::from_gray(205)
            };
            for i in 1..6 {
                let t = i as f32 / 6.0;
                let x = egui::lerp(plot_rect.x_range(), t);
                let y = egui::lerp(plot_rect.y_range(), t);
                painter.line_segment(
                    [
                        egui::pos2(x, plot_rect.top()),
                        egui::pos2(x, plot_rect.bottom()),
                    ],
                    egui::Stroke::new(1.0, grid_color),
                );
                painter.line_segment(
                    [
                        egui::pos2(plot_rect.left(), y),
                        egui::pos2(plot_rect.right(), y),
                    ],
                    egui::Stroke::new(1.0, grid_color),
                );
            }
        }

        let axis_color = if self.active_theme == VisualTheme::Dark {
            egui::Color32::from_gray(170)
        } else {
            egui::Color32::from_gray(90)
        };
        painter.line_segment(
            [
                egui::pos2(plot_rect.left(), plot_rect.bottom()),
                egui::pos2(plot_rect.right(), plot_rect.bottom()),
            ],
            egui::Stroke::new(1.4, axis_color),
        );
        painter.line_segment(
            [
                egui::pos2(plot_rect.left(), plot_rect.bottom()),
                egui::pos2(plot_rect.left(), plot_rect.top()),
            ],
            egui::Stroke::new(1.4, axis_color),
        );

        let label_color = ui.style().visuals.weak_text_color();
        painter.text(
            egui::pos2(plot_rect.left(), plot_rect.top() - 18.0),
            egui::Align2::LEFT_TOP,
            &self.measurement_plot.title,
            egui::TextStyle::Button.resolve(ui.style()),
            ui.style().visuals.text_color(),
        );
        let x_ticks = 6;
        for i in 0..=x_ticks {
            let t = i as f32 / x_ticks as f32;
            let x = egui::lerp(plot_rect.x_range(), t);
            painter.line_segment(
                [
                    egui::pos2(x, plot_rect.bottom()),
                    egui::pos2(x, plot_rect.bottom() + 5.0),
                ],
                egui::Stroke::new(1.0, axis_color),
            );
            let value = x_min + (x_max - x_min) * (i as f64 / x_ticks as f64);
            painter.text(
                egui::pos2(x, plot_rect.bottom() + 8.0),
                egui::Align2::CENTER_TOP,
                format!("{value:.1}"),
                egui::TextStyle::Small.resolve(ui.style()),
                label_color,
            );
        }
        let y_ticks = 6;
        for i in 0..=y_ticks {
            let t = i as f32 / y_ticks as f32;
            let y = egui::lerp(plot_rect.y_range(), 1.0 - t);
            painter.line_segment(
                [
                    egui::pos2(plot_rect.left() - 5.0, y),
                    egui::pos2(plot_rect.left(), y),
                ],
                egui::Stroke::new(1.0, axis_color),
            );
            let value = y_min + (y_max - y_min) * (i as f64 / y_ticks as f64);
            painter.text(
                egui::pos2(plot_rect.left() - 8.0, y),
                egui::Align2::RIGHT_CENTER,
                format!("{value:.2}"),
                egui::TextStyle::Small.resolve(ui.style()),
                label_color,
            );
        }

        let draw_series = |painter: &egui::Painter,
                           data: &[MeasurementPoint],
                           map_y: fn(&MeasurementPoint) -> f64,
                           visible: bool,
                           color: egui::Color32,
                           show_points: bool,
                           width: f32| {
            if !visible || data.len() < 2 {
                return;
            }
            let mut polyline = Vec::with_capacity(data.len());
            for p in data {
                polyline.push(to_pos(p.elapsed_s, map_y(p)));
            }
            painter.add(egui::Shape::line(polyline.clone(), egui::Stroke::new(width, color)));
            if show_points {
                for pt in polyline {
                    painter.circle_filled(pt, 2.0 + width * 0.35, color);
                }
            }
        };

        draw_series(
            &painter,
            points,
            |p| p.flow_ml_min,
            self.measurement_plot.show_flow,
            egui::Color32::from_rgb(95, 191, 255),
            self.measurement_plot.show_points,
            self.measurement_plot.line_width,
        );
        draw_series(
            &painter,
            points,
            |p| p.speed_setpoint_ml_min,
            self.measurement_plot.show_setpoint,
            egui::Color32::from_rgb(255, 192, 84),
            self.measurement_plot.show_points,
            self.measurement_plot.line_width,
        );
        draw_series(
            &painter,
            points,
            |p| p.flow_theoretical_ml_min,
            self.measurement_plot.show_theoretical,
            egui::Color32::from_rgb(170, 123, 255),
            self.measurement_plot.show_points,
            self.measurement_plot.line_width,
        );
        draw_series(
            &painter,
            points,
            |p| p.flow_estimated_ml_min,
            self.measurement_plot.show_estimated,
            egui::Color32::from_rgb(98, 221, 142),
            self.measurement_plot.show_points,
            self.measurement_plot.line_width,
        );

        painter.text(
            egui::pos2(plot_rect.left() - 42.0, plot_rect.top() - 8.0),
            egui::Align2::LEFT_TOP,
            "Flow (ml/min)",
            egui::TextStyle::Small.resolve(ui.style()),
            label_color,
        );
        painter.text(
            egui::pos2(plot_rect.left(), plot_rect.bottom() + 2.0),
            egui::Align2::LEFT_TOP,
            format!("t={x_min:.2}s"),
            egui::TextStyle::Small.resolve(ui.style()),
            label_color,
        );
        painter.text(
            egui::pos2(plot_rect.right(), plot_rect.bottom() + 2.0),
            egui::Align2::RIGHT_TOP,
            format!("t={x_max:.2}s"),
            egui::TextStyle::Small.resolve(ui.style()),
            label_color,
        );
        painter.text(
            egui::pos2(plot_rect.center().x, plot_rect.bottom() + 24.0),
            egui::Align2::CENTER_TOP,
            "Time (s)",
            egui::TextStyle::Small.resolve(ui.style()),
            label_color,
        );

        let legend_items = [
            (
                self.measurement_plot.show_flow,
                egui::Color32::from_rgb(95, 191, 255),
                "Flow",
            ),
            (
                self.measurement_plot.show_setpoint,
                egui::Color32::from_rgb(255, 192, 84),
                "Setpoint",
            ),
            (
                self.measurement_plot.show_theoretical,
                egui::Color32::from_rgb(170, 123, 255),
                "Theoretical",
            ),
            (
                self.measurement_plot.show_estimated,
                egui::Color32::from_rgb(98, 221, 142),
                "Estimated",
            ),
        ];
        let active_count = legend_items.iter().filter(|(on, _, _)| *on).count();
        if self.measurement_plot.show_legend && active_count > 0 {
            let legend_h = 8.0 + (active_count as f32 * 18.0);
            let legend_rect = egui::Rect::from_min_size(
                egui::pos2(plot_rect.right() - 138.0, plot_rect.top() + 8.0),
                egui::vec2(130.0, legend_h),
            );
            let legend_bg = if self.active_theme == VisualTheme::Dark {
                egui::Color32::from_rgba_premultiplied(10, 14, 20, 200)
            } else {
                egui::Color32::from_rgba_premultiplied(255, 255, 255, 210)
            };
            painter.rect_filled(legend_rect, egui::CornerRadius::same(6), legend_bg);

            let mut row = 0;
            for (on, color, name) in legend_items {
                if !on {
                    continue;
                }
                let y = legend_rect.top() + 7.0 + (row as f32 * 18.0);
                painter.line_segment(
                    [
                        egui::pos2(legend_rect.left() + 8.0, y + 5.0),
                        egui::pos2(legend_rect.left() + 24.0, y + 5.0),
                    ],
                    egui::Stroke::new(2.4, color),
                );
                painter.text(
                    egui::pos2(legend_rect.left() + 29.0, y),
                    egui::Align2::LEFT_TOP,
                    name,
                    egui::TextStyle::Small.resolve(ui.style()),
                    ui.style().visuals.text_color(),
                );
                row += 1;
            }
        }
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        let mut visuals = match self.active_theme {
            VisualTheme::Dark => egui::Visuals::dark(),
            VisualTheme::Light => egui::Visuals::light(),
        };

        visuals.widgets.active.corner_radius = egui::CornerRadius::same(7);
        visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(7);
        visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(7);
        visuals.window_corner_radius = egui::CornerRadius::same(10);
        visuals.panel_fill = visuals.extreme_bg_color;
        ctx.set_visuals(visuals);

        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0) * self.ui_settings.density;
        style.spacing.button_padding = egui::vec2(10.0, 7.0) * self.ui_settings.density;
        style.spacing.indent = 18.0 * self.ui_settings.density;
        style.spacing.slider_width = 180.0;
        style.spacing.text_edit_width = 220.0;

        let base = 14.0 * self.ui_settings.font_scale;
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(base));
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::proportional(base + 0.5),
        );
        style.text_styles.insert(
            egui::TextStyle::Heading,
            egui::FontId::proportional(base + 6.0),
        );
        style.text_styles.insert(
            egui::TextStyle::Monospace,
            egui::FontId::monospace(base - 1.0),
        );
        ctx.set_style(style);
    }

    fn tab_name(idx: usize) -> &'static str {
        const NAMES: [&str; TabView::COUNT] = [
            "Connection",
            "Manual",
            "Cycled",
            "Measurement",
            "Recipe",
            "Status",
        ];
        NAMES.get(idx).copied().unwrap_or("Unknown")
    }

    fn quick_action_menu(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Save recipe").clicked() {
                    let path = self.recipe_full_path();
                    self.prompt_overwrite_or_run(
                        &path,
                        FileAction::SaveRecipe { path: path.clone() },
                        "Overwrite recipe file?",
                    );
                    ui.close();
                }
                if ui.button("Load recipe").clicked() {
                    let path = self.recipe_full_path();
                    self.load_recipe_and_apply(&path);
                    ui.close();
                }
                if ui.button("Export measurements CSV").clicked() {
                    let path = self.csv_full_path();
                    self.prompt_overwrite_or_run(
                        &path,
                        FileAction::ExportCsv { path: path.clone() },
                        "Overwrite CSV file?",
                    );
                    ui.close();
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    let _ = frame;
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });

            ui.menu_button("Edit", |ui| {
                if ui.button("Apply current recipe to pump").clicked() {
                    let recipe: PumpRecipe = self.state.recipe.clone();
                    self.send_cmd("Apply recipe", PumpCmd::ApplyRecipe(recipe));
                    ui.close();
                }
                if ui.button("Clear status log").clicked() {
                    self.state.logs.clear();
                    ui.close();
                }
                if ui.button("Clear measurements").clicked() {
                    self.state.measurements.clear();
                    ui.close();
                }
            });

            ui.menu_button("View", |ui| {
                ui.horizontal(|ui| {
                    ui.label("Theme:");
                    ui.selectable_value(&mut self.active_theme, VisualTheme::Dark, "Dark");
                    ui.selectable_value(&mut self.active_theme, VisualTheme::Light, "Light");
                });
                ui.add(
                    egui::Slider::new(&mut self.ui_settings.font_scale, 0.85..=1.35)
                        .text("Font scale"),
                );
                ui.add(
                    egui::Slider::new(&mut self.ui_settings.density, 0.8..=1.2)
                        .text("Density"),
                );
                ui.checkbox(
                    &mut self.ui_settings.show_sidebar_on_wide,
                    "Sidebar navigation on wide windows",
                );
            });

            ui.menu_button("Pump", |ui| {
                if ui.button("Connect").clicked() {
                    let port = self.state.recipe.serial_port.clone();
                    let addr = self.state.recipe.pump_addr;
                    self.send_cmd(
                        format!("Connect {port} (addr {addr})"),
                        PumpCmd::Connect { port, addr },
                    );
                    ui.close();
                }
                if ui.button("Disconnect").clicked() {
                    self.send_cmd("Disconnect", PumpCmd::Disconnect);
                    ui.close();
                }
                ui.separator();
                if ui.button("Start").clicked() {
                    self.send_cmd("Start", PumpCmd::Start);
                    ui.close();
                }
                if ui.button("Stop").clicked() {
                    self.send_cmd("Stop", PumpCmd::Stop);
                    ui.close();
                }
            });
        });
    }

    fn poll_backend(&mut self) {
        for evt in self.client.poll_events() {
            self.state.apply_event(evt);
        }
    }

    /// Issue a pump command and record it as pending for UI feedback.
    fn send_cmd(&mut self, label: impl Into<String>, cmd: PumpCmd) {
        let label = label.into();
        self.state.push_log(format!("> {label}"));
        self.state.mark_pending(label);
        self.client.send(cmd);
    }

    /// A button that is only enabled when there are no pump commands
    /// in-flight (prevents accidental queuing during slow serial writes).
    fn button_busy(&self, ui: &mut egui::Ui, label: &str) -> egui::Response {
        ui.add_enabled(self.state.pending == 0, egui::Button::new(label))
    }

    /// A button that stays enabled even when commands are pending. Used for
    /// safety-critical actions like `Stop` which must always reach the pump.
    fn button_always(&self, ui: &mut egui::Ui, label: &str) -> egui::Response {
        ui.add(egui::Button::new(label))
    }

    fn status_chip(
        &self,
        ui: &mut egui::Ui,
        label: &str,
        active: bool,
        color_on: egui::Color32,
        color_off: egui::Color32,
    ) {
        let color = if active { color_on } else { color_off };
        let text = format!("{label}: {}", if active { "ON" } else { "OFF" });
        let rich = egui::RichText::new(text).strong().color(color);
        ui.add(
            egui::Label::new(rich)
                .sense(egui::Sense::hover())
                .selectable(false),
        );
    }

    fn header_health_status(&self) -> (&'static str, egui::Color32, String) {
        let is_error = self.state.last_response.to_ascii_lowercase().contains("error");
        if !self.state.connected || is_error {
            let reason = if !self.state.connected {
                "Pump disconnected".to_string()
            } else {
                "Recent command error".to_string()
            };
            ("CRITICAL", egui::Color32::from_rgb(222, 72, 72), reason)
        } else if self.state.pending > 0 {
            (
                "ATTENTION",
                egui::Color32::from_rgb(224, 177, 52),
                format!(
                    "Command pending ({}): {}",
                    self.state.pending, self.state.pending_label
                ),
            )
        } else {
            (
                "HEALTHY",
                egui::Color32::from_rgb(78, 191, 111),
                "System idle and responsive".to_string(),
            )
        }
    }

    fn header_status(&self, ui: &mut egui::Ui) {
        let (health_label, health_color, health_reason) = self.header_health_status();

        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Status: {health_label}"))
                            .strong()
                            .color(health_color)
                            .size(15.0),
                    );
                    ui.separator();
                    ui.label(egui::RichText::new(health_reason).italics());
                });

                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    self.status_chip(
                        ui,
                        "Connected",
                        self.state.connected,
                        egui::Color32::from_rgb(78, 191, 111),
                        egui::Color32::from_gray(145),
                    );
                    ui.separator();
                    self.status_chip(
                        ui,
                        "Running",
                        self.state.running,
                        egui::Color32::from_rgb(101, 187, 255),
                        egui::Color32::from_gray(145),
                    );
                    ui.separator();
                    self.status_chip(
                        ui,
                        "Cycle",
                        self.state.cycle_running,
                        egui::Color32::from_rgb(224, 177, 52),
                        egui::Color32::from_gray(145),
                    );
                    ui.separator();
                    self.status_chip(
                        ui,
                        "Measuring",
                        self.state.measuring,
                        egui::Color32::from_rgb(187, 127, 255),
                        egui::Color32::from_gray(145),
                    );
                });
            });
        if self.state.pending > 0 {
            ui.label(
                egui::RichText::new(format!(
                    "Pending: {} ({})",
                    self.state.pending, self.state.pending_label
                ))
                .color(egui::Color32::from_rgb(224, 177, 52))
                .strong(),
            );
        } else {
            ui.label(
                egui::RichText::new("Pending: none")
                    .color(egui::Color32::from_rgb(78, 191, 111)),
            );
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Last response:").strong());
            let is_error = self.state.last_response.to_ascii_lowercase().contains("error");
            let response_color = if is_error {
                egui::Color32::from_rgb(222, 72, 72)
            } else {
                ui.style().visuals.text_color()
            };
            ui.label(egui::RichText::new(self.state.last_response.clone()).color(response_color));
        });
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            for idx in 0..TabView::COUNT {
                let name = Self::tab_name(idx);
                if ui
                    .selectable_label(self.state.selected_tab == idx, name)
                    .clicked()
                {
                    self.state.selected_tab = idx;
                }
            }
        });
        ui.separator();
    }

    // ---------------------------------------------------------------
    // Connection tab
    // ---------------------------------------------------------------
    fn connection_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Connection");

        ui.horizontal(|ui| {
            ui.label("Port:");
            ui.text_edit_singleline(&mut self.state.recipe.serial_port);
            ui.label("Address:");
            let mut addr = self.state.recipe.pump_addr as i32;
            if ui
                .add(egui::DragValue::new(&mut addr).range(1..=8))
                .changed()
            {
                self.state.recipe.pump_addr = addr as u8;
            }
        });

        ui.horizontal(|ui| {
            if self.button_busy(ui, "Connect").clicked() {
                let port = self.state.recipe.serial_port.clone();
                let addr = self.state.recipe.pump_addr;
                self.send_cmd(
                    format!("Connect {port} (addr {addr})"),
                    PumpCmd::Connect { port, addr },
                );
            }
            if self.button_always(ui, "Disconnect").clicked() {
                // Disconnect is fire-and-forget; still tracked as pending so
                // the UI reflects the round-trip.
                self.send_cmd("Disconnect", PumpCmd::Disconnect);
            }
        });

        ui.separator();
        ui.heading("Tube & Calibration");
        ui.horizontal(|ui| {
            ui.label("Tube ID (mm):");
            let resp = ui.add(
                egui::DragValue::new(&mut self.state.recipe.tube_inner_diameter_mm)
                    .speed(0.01)
                    .range(0.1..=20.0),
            );
            if commit_numeric(&resp) {
                let v = self.state.recipe.tube_inner_diameter_mm;
                self.send_cmd(
                    format!("Set tube ID {v:.3} mm"),
                    PumpCmd::SetTubeDiameterMm(v),
                );
            }
        });

        ui.horizontal(|ui| {
            ui.label("Calibrated max flow (ml/min):");
            ui.text_edit_singleline(&mut self.state.calibration_input_buffer);
            if self.button_busy(ui, "Apply calibration").clicked() {
                match self.state.calibration_input_buffer.parse::<f32>() {
                    Ok(v) => {
                        let v = v.max(0.0);
                        self.state.recipe.calibrated_max_flow_ml_min = v;
                        self.state.calibration_input_buffer = format!("{v:.3}");
                        self.send_cmd(
                            format!("Set calibrated flow {v:.3} ml/min"),
                            PumpCmd::SetCalibratedFlowMlMin(v),
                        );
                    }
                    Err(_) => self.state.push_log("Invalid calibrated flow input"),
                }
            }
        });
    }

    // ---------------------------------------------------------------
    // Manual tab
    // ---------------------------------------------------------------
    fn manual_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Manual Control");

        ui.horizontal(|ui| {
            if self.button_busy(ui, "Start").clicked() {
                self.send_cmd("Start", PumpCmd::Start);
            }
            if self.button_always(ui, "Stop").clicked() {
                // Always-available safety button.
                self.send_cmd("Stop", PumpCmd::Stop);
            }
        });

        ui.horizontal(|ui| {
            if self.button_busy(ui, "-").clicked() {
                let v = (self.state.recipe.speed_ml_min - self.state.speed_step_ml_min).max(0.0);
                self.state.recipe.speed_ml_min = v;
                self.send_cmd(
                    format!(
                        "Step -{:.3} -> {:.3} ml/min",
                        self.state.speed_step_ml_min, v
                    ),
                    PumpCmd::SetSpeedMlMin(v),
                );
            }
            ui.label("Step:");
            let resp = ui.text_edit_singleline(&mut self.state.speed_step_input_buffer);
            if resp.lost_focus()
                && let Ok(step) = self.state.speed_step_input_buffer.parse::<f32>()
            {
                self.state.speed_step_ml_min = clamp_speed_step(step);
                self.state.speed_step_input_buffer = format!("{:.3}", self.state.speed_step_ml_min);
            }
            if self.button_busy(ui, "+").clicked() {
                let v = self.state.recipe.speed_ml_min + self.state.speed_step_ml_min;
                self.state.recipe.speed_ml_min = v;
                self.send_cmd(
                    format!(
                        "Step +{:.3} -> {:.3} ml/min",
                        self.state.speed_step_ml_min, v
                    ),
                    PumpCmd::SetSpeedMlMin(v),
                );
            }
            ui.label(format!(
                "Speed: {:.3} ml/min (step {:.3})",
                self.state.recipe.speed_ml_min, self.state.speed_step_ml_min
            ));
        });

        ui.horizontal(|ui| {
            ui.label("Set exact speed (ml/min):");
            ui.text_edit_singleline(&mut self.state.speed_input_buffer);
            if self.button_busy(ui, "Apply").clicked() {
                match self.state.speed_input_buffer.parse::<f32>() {
                    Ok(v) => {
                        let v = v.max(0.0);
                        self.state.recipe.speed_ml_min = v;
                        self.state.speed_input_buffer.clear();
                        self.send_cmd(
                            format!("Set speed {v:.3} ml/min"),
                            PumpCmd::SetSpeedMlMin(v),
                        );
                    }
                    Err(_) => self.state.push_log("Invalid speed input"),
                }
            }
        });

        ui.horizontal(|ui| {
            if self.button_busy(ui, "Backsteps -").clicked() {
                let v = self.state.recipe.backsteps.saturating_sub(1);
                self.state.recipe.backsteps = v;
                self.send_cmd(format!("Set backsteps {v}"), PumpCmd::SetBacksteps(v));
            }
            if self.button_busy(ui, "Backsteps +").clicked() {
                let v = clamp_backsteps(self.state.recipe.backsteps.saturating_add(1));
                self.state.recipe.backsteps = v;
                self.send_cmd(format!("Set backsteps {v}"), PumpCmd::SetBacksteps(v));
            }
            ui.label(format!("Backsteps: {}", self.state.recipe.backsteps));
        });
    }

    // ---------------------------------------------------------------
    // Cycled tab
    // ---------------------------------------------------------------
    fn cycled_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Cycled Control");

        ui.horizontal(|ui| {
            ui.label("Cycle count N (0 = infinite):");
            let resp = ui.add(egui::DragValue::new(&mut self.state.recipe.cycles).range(0..=9999));
            if commit_numeric(&resp) {
                let v = self.state.recipe.cycles;
                self.send_cmd(format!("Set cycles {v}"), PumpCmd::SetCycles(v));
            }
        });

        ui.checkbox(
            &mut self.state.recipe.linear_transition_between_steps,
            "Linear transition between steps",
        );

        ui.label("Cycle steps (duration_s, speed_ml_min):");
        for (i, step) in self.state.recipe.cycle_program.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("#{i}"));
                ui.add(
                    egui::DragValue::new(&mut step.duration_s)
                        .speed(0.1)
                        .range(0.1..=3600.0),
                );
                ui.label("s");
                ui.add(
                    egui::DragValue::new(&mut step.speed_ml_min)
                        .speed(f64::from(self.state.speed_step_ml_min))
                        .range(0.0..=500.0),
                );
                ui.label("ml/min");
            });
        }

        ui.horizontal(|ui| {
            if ui.button("Add step").clicked() {
                self.state.recipe.cycle_program.push(CycleStep {
                    duration_s: 2.0,
                    speed_ml_min: self.state.recipe.speed_ml_min,
                });
            }
            if ui.button("Remove last").clicked() {
                let _ = self.state.recipe.cycle_program.pop();
                if self.state.recipe.cycle_program.is_empty() {
                    self.state.recipe.cycle_program.push(CycleStep {
                        duration_s: 2.0,
                        speed_ml_min: self.state.recipe.speed_ml_min,
                    });
                }
            }
        });

        ui.horizontal(|ui| {
            if !self.state.cycle_running {
                if self.button_busy(ui, "Start cycle program").clicked() {
                    let steps = self.state.recipe.cycle_program.clone();
                    let linear = self.state.recipe.linear_transition_between_steps;
                    let repeat_cycles = self.state.recipe.cycles;
                    self.send_cmd(
                        format!("Start cycle program (N={repeat_cycles})"),
                        PumpCmd::StartCycleProgram {
                            steps,
                            linear,
                            repeat_cycles,
                        },
                    );
                }
            } else if self.button_always(ui, "Stop cycle program").clicked() {
                self.send_cmd("Stop cycle program", PumpCmd::StopCycleProgram);
            }
            ui.label(format!("Running: {}", self.state.cycle_running));
        });
    }

    // ---------------------------------------------------------------
    // Measurement tab
    // ---------------------------------------------------------------
    fn measurement_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Measurement");
        self.ensure_path_defaults();

        if self.measurement_interval_input.is_empty() {
            self.measurement_interval_input = self.state.recipe.measurement_interval_ms.to_string();
        }

        ui.horizontal(|ui| {
            if !self.state.measuring {
                if self.button_busy(ui, "Start measurement").clicked() {
                    self.send_cmd("Begin measurement", PumpCmd::BeginMeasurement);
                }
            } else if self.button_always(ui, "Stop measurement").clicked() {
                self.send_cmd("Stop measurement", PumpCmd::StopMeasurement);
            }
            if ui.button("Reset measurement").clicked() {
                self.state.measurements.clear();
                self.state.push_log("Measurement samples reset");
            }
            if ui.button("Export CSV").clicked() {
                let path = self.csv_full_path();
                self.prompt_overwrite_or_run(
                    &path,
                    FileAction::ExportCsv { path: path.clone() },
                    "Overwrite CSV file?",
                );
            }
            if ui.button("Export plot (SVG)").clicked() {
                self.show_plot_export_modal = true;
            }
        });

        ui.horizontal_wrapped(|ui| {
            ui.label("Sample interval (ms):");
            let response = ui.text_edit_singleline(&mut self.measurement_interval_input);
            let apply_clicked = self.button_busy(ui, "Apply interval").clicked();
            let apply_on_enter =
                response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if apply_clicked || apply_on_enter {
                match self.measurement_interval_input.trim().parse::<u64>() {
                    Ok(v) => {
                        let clamped = clamp_measurement_interval_ms(v);
                        self.state.recipe.measurement_interval_ms = clamped;
                        self.measurement_interval_input = clamped.to_string();
                        self.send_cmd(
                            format!("Set measurement interval {clamped} ms"),
                            PumpCmd::SetMeasurementIntervalMs(clamped),
                        );
                    }
                    Err(_) => self.state.push_log("Invalid measurement interval"),
                }
            }
        });

        ui.horizontal(|ui| {
            ui.label("CSV filename:");
            ui.text_edit_singleline(&mut self.state.recipe.csv_export_path);
        });

        ui.separator();
        ui.label("Live plot");
        self.draw_measurement_plot(ui);
        ui.collapsing("Plot settings", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Plot title:");
                ui.text_edit_singleline(&mut self.measurement_plot.title);
            });
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.measurement_plot.show_legend, "Legend");
                ui.checkbox(&mut self.measurement_plot.show_grid, "Grid");
                ui.checkbox(&mut self.measurement_plot.show_points, "Markers");
                ui.checkbox(&mut self.measurement_plot.auto_scale_y, "Auto-scale Y");
            });
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut self.measurement_plot.show_flow, "Flow");
                ui.checkbox(&mut self.measurement_plot.show_setpoint, "Setpoint");
                ui.checkbox(&mut self.measurement_plot.show_theoretical, "Theoretical");
                ui.checkbox(&mut self.measurement_plot.show_estimated, "Estimated");
            });
            ui.add(
                egui::Slider::new(&mut self.measurement_plot.line_width, 1.0..=4.0)
                    .text("Line width"),
            );

            let mut max_points = self.measurement_plot.max_points as i32;
            if ui
                .add(egui::DragValue::new(&mut max_points).range(40..=10_000))
                .changed()
            {
                self.measurement_plot.max_points = max_points.max(40) as usize;
            }
            ui.label("Visible samples");

            if !self.measurement_plot.auto_scale_y {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Y min:");
                    ui.add(egui::DragValue::new(&mut self.measurement_plot.y_min).speed(0.05));
                    ui.label("Y max:");
                    ui.add(egui::DragValue::new(&mut self.measurement_plot.y_max).speed(0.05));
                });
            }
        });

        ui.label(format!("Samples: {}", self.state.measurements.len()));
        egui::ScrollArea::vertical()
            .max_height(220.0)
            .show(ui, |ui| {
                for p in self.state.measurements.iter().rev().take(25) {
                    ui.label(format!(
                        "{:.3}s | flow {:.3} ml/min | set {:.3} ml/min | {:.3}% | theoretical speed {:.3}",
                        p.elapsed_s,
                        p.flow_ml_min,
                        p.speed_setpoint_ml_min,
                        p.speed_percent,
                        p.flow_theoretical_ml_min
                    ));
                }
            });
    }

    // ---------------------------------------------------------------
    // Recipe tab
    // ---------------------------------------------------------------
    fn recipe_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Recipe");
        self.ensure_path_defaults();

        ui.horizontal_wrapped(|ui| {
            ui.label("Recipe folder:");
            ui.text_edit_singleline(&mut self.state.recipe.ui.recipe_folder);
            if ui.button("Refresh JSON list").clicked() {
                self.refresh_recipe_files();
            }
        });

        ui.horizontal_wrapped(|ui| {
            ui.label("Data folder:");
            ui.text_edit_singleline(&mut self.state.recipe.ui.data_folder);
        });

        ui.horizontal_wrapped(|ui| {
            ui.label("Recipe kind:");
            ui.selectable_value(&mut self.state.recipe.recipe_kind, RecipeKind::Config, "Config");
            ui.selectable_value(
                &mut self.state.recipe.recipe_kind,
                RecipeKind::Executable,
                "Executable",
            );
            ui.checkbox(
                &mut self.state.recipe.ui.auto_connect_on_start,
                "Auto-connect on app start",
            );
            ui.checkbox(
                &mut self.state.recipe.ui.auto_measure_on_start,
                "Auto-measure on app start",
            );
        });

        if self.recipe_files.is_empty() && ui.button("Scan recipe folder").clicked() {
            self.refresh_recipe_files();
        }
        if !self.recipe_files.is_empty() {
            let selected_text = self
                .recipe_files
                .get(self.selected_recipe_idx)
                .cloned()
                .unwrap_or_else(|| "Select recipe".to_string());
            egui::ComboBox::from_label("Recipe JSON files")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for (idx, file) in self.recipe_files.iter().enumerate() {
                        if ui
                            .selectable_label(self.selected_recipe_idx == idx, file)
                            .clicked()
                        {
                            self.selected_recipe_idx = idx;
                            self.state.recipe_file_path = file.clone();
                        }
                    }
                });
        }

        ui.horizontal_wrapped(|ui| {
            ui.label("Recipe filename:");
            ui.text_edit_singleline(&mut self.state.recipe_file_path);
        });

        ui.horizontal_wrapped(|ui| {
            if ui.button("Save recipe").clicked() {
                let path = self.recipe_full_path();
                self.prompt_overwrite_or_run(
                    &path,
                    FileAction::SaveRecipe { path: path.clone() },
                    "Overwrite recipe file?",
                );
            }
            if self.button_busy(ui, "Load recipe").clicked() {
                let path = self.recipe_full_path();
                self.load_recipe_and_apply(&path);
            }
            if self.button_busy(ui, "Apply current").clicked() {
                let recipe: PumpRecipe = self.state.recipe.clone();
                self.send_cmd("Apply recipe", PumpCmd::ApplyRecipe(recipe));
            }
            if ui.button("Delete recipe file").clicked() {
                let path = self.recipe_full_path();
                if Path::new(&path).exists() {
                    self.file_confirm = Some(FileConfirmDialog {
                        title: "Delete recipe file?".to_string(),
                        body: format!("This will permanently delete:\n{path}"),
                        action: FileAction::DeleteRecipe { path },
                    });
                } else {
                    self.state.push_log(format!("File not found: {path}"));
                }
            }
        });

        ui.label(format!("Resolved recipe path: {}", self.recipe_full_path()));
        ui.label(format!(
            "Resolved data folder: {}",
            self.state.recipe.ui.data_folder
        ));
        ui.label(format!(
            "Schema version: {}",
            self.state.recipe.schema_version
        ));
    }

    // ---------------------------------------------------------------
    // Status tab
    // ---------------------------------------------------------------
    fn status_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Status Log");
        egui::ScrollArea::vertical().show(ui, |ui| {
            for line in self.state.logs.iter().take(200) {
                ui.label(line);
            }
        });
    }

    fn plot_export_modal(&mut self, ctx: &egui::Context) {
        if !self.show_plot_export_modal {
            return;
        }
        let mut keep_open = self.show_plot_export_modal;
        egui::Window::new("Export Plot (SVG)")
            .open(&mut keep_open)
            .resizable(true)
            .collapsible(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.label("Configure plot style and export options");
                ui.separator();

                ui.horizontal_wrapped(|ui| {
                    ui.label("SVG path:");
                    ui.text_edit_singleline(&mut self.measurement_plot.svg_export_path);
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Title:");
                    ui.text_edit_singleline(&mut self.measurement_plot.title);
                });

                ui.separator();
                ui.label("Series");
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut self.measurement_plot.show_flow, "Flow");
                    ui.checkbox(&mut self.measurement_plot.show_setpoint, "Setpoint");
                    ui.checkbox(&mut self.measurement_plot.show_theoretical, "Theoretical");
                    ui.checkbox(&mut self.measurement_plot.show_estimated, "Estimated");
                });

                ui.separator();
                ui.label("Style");
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut self.measurement_plot.show_legend, "Legend");
                    ui.checkbox(&mut self.measurement_plot.show_grid, "Grid");
                    ui.checkbox(&mut self.measurement_plot.show_points, "Markers");
                    ui.checkbox(&mut self.measurement_plot.auto_scale_y, "Auto-scale Y");
                });
                ui.add(
                    egui::Slider::new(&mut self.measurement_plot.line_width, 1.0..=4.0)
                        .text("Line width"),
                );

                let mut max_points = self.measurement_plot.max_points as i32;
                if ui
                    .add(egui::DragValue::new(&mut max_points).range(40..=10_000))
                    .changed()
                {
                    self.measurement_plot.max_points = max_points.max(40) as usize;
                }
                ui.label("Visible samples");

                if !self.measurement_plot.auto_scale_y {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Y min:");
                        ui.add(egui::DragValue::new(&mut self.measurement_plot.y_min).speed(0.05));
                        ui.label("Y max:");
                        ui.add(egui::DragValue::new(&mut self.measurement_plot.y_max).speed(0.05));
                    });
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Export now").clicked() {
                        let path = self.svg_full_path();
                        self.prompt_overwrite_or_run(
                            &path,
                            FileAction::ExportPlotSvg { path: path.clone() },
                            "Overwrite plot SVG file?",
                        );
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_plot_export_modal = false;
                    }
                });
            });
        self.show_plot_export_modal = keep_open && self.show_plot_export_modal;
    }

    fn file_confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(current) = self.file_confirm.clone() else {
            return;
        };
        let mut keep_open = true;
        egui::Window::new(current.title.clone())
            .open(&mut keep_open)
            .resizable(false)
            .collapsible(false)
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.label(current.body.clone());
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Confirm").clicked() {
                        self.run_file_action(current.action.clone());
                        self.file_confirm = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.file_confirm = None;
                    }
                });
            });
        if !keep_open {
            self.file_confirm = None;
        }
    }
}

/// A numeric widget is considered "committed" when the user either stopped a
/// drag interaction or left the field (focus lost). Used for every
/// `DragValue` that issues a pump command so that the serial port is not
/// hammered during scrubbing.
fn commit_numeric(resp: &egui::Response) -> bool {
    resp.drag_stopped() || resp.lost_focus()
}

fn export_measurement_svg(
    path: &str,
    points: &[MeasurementPoint],
    settings: &MeasurementPlotSettings,
) -> Result<()> {
    if points.len() < 2 {
        anyhow::bail!("Need at least 2 points to export a plot");
    }

    let x_min = points.first().map(|p| p.elapsed_s).unwrap_or(0.0);
    let mut x_max = points.last().map(|p| p.elapsed_s).unwrap_or(1.0);
    if x_max <= x_min {
        x_max = x_min + 1.0;
    }

    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut seen = false;
    for p in points {
        if settings.show_flow {
            min_y = min_y.min(p.flow_ml_min);
            max_y = max_y.max(p.flow_ml_min);
            seen = true;
        }
        if settings.show_setpoint {
            min_y = min_y.min(p.speed_setpoint_ml_min);
            max_y = max_y.max(p.speed_setpoint_ml_min);
            seen = true;
        }
        if settings.show_theoretical {
            min_y = min_y.min(p.flow_theoretical_ml_min);
            max_y = max_y.max(p.flow_theoretical_ml_min);
            seen = true;
        }
        if settings.show_estimated {
            min_y = min_y.min(p.flow_estimated_ml_min);
            max_y = max_y.max(p.flow_estimated_ml_min);
            seen = true;
        }
    }
    if !seen {
        anyhow::bail!("Enable at least one plot series before exporting");
    }
    let (y_min, y_max) = if settings.auto_scale_y {
        let span = (max_y - min_y).max(0.001);
        let pad = span * 0.12;
        (min_y - pad, max_y + pad)
    } else {
        let y0 = settings.y_min;
        let mut y1 = settings.y_max;
        if y1 <= y0 {
            y1 = y0 + 0.5;
        }
        (y0, y1)
    };

    let w = 1400.0;
    let h = 820.0;
    let pad = 68.0;
    let pw = w - (pad * 2.0);
    let ph = h - (pad * 2.0);
    let map = |x: f64, y: f64| -> (f64, f64) {
        let tx = ((x - x_min) / (x_max - x_min)).clamp(0.0, 1.0);
        let ty = ((y - y_min) / (y_max - y_min)).clamp(0.0, 1.0);
        (pad + tx * pw, pad + (1.0 - ty) * ph)
    };

    let mut svg = String::new();
    svg.push_str(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">"#
    ));
    svg.push_str(r##"<rect x="0" y="0" width="100%" height="100%" fill="#111723"/>"##);
    svg.push_str(&format!(
        r##"<rect x="{pad}" y="{pad}" width="{pw}" height="{ph}" fill="#172033" stroke="#2a3850" stroke-width="1"/>"##
    ));
    let x_ticks = 6;
    let y_ticks = 6;
    if settings.show_grid {
        for i in 1..x_ticks {
            let t = i as f64 / x_ticks as f64;
            let x = pad + t * pw;
            svg.push_str(&format!(
                r##"<line x1="{x}" y1="{pad}" x2="{x}" y2="{}" stroke="#27354a" stroke-width="1"/>"##,
                pad + ph
            ));
        }
        for i in 1..y_ticks {
            let t = i as f64 / y_ticks as f64;
            let y = pad + t * ph;
            svg.push_str(&format!(
                r##"<line x1="{pad}" y1="{y}" x2="{}" y2="{y}" stroke="#27354a" stroke-width="1"/>"##,
                pad + pw
            ));
        }
    }
    svg.push_str(&format!(
        r##"<line x1="{pad}" y1="{}" x2="{}" y2="{}" stroke="#91a9c8" stroke-width="1.5"/>"##,
        pad + ph,
        pad + pw,
        pad + ph
    ));
    svg.push_str(&format!(
        r##"<line x1="{pad}" y1="{pad}" x2="{pad}" y2="{}" stroke="#91a9c8" stroke-width="1.5"/>"##,
        pad + ph
    ));
    for i in 0..=x_ticks {
        let t = i as f64 / x_ticks as f64;
        let x = pad + t * pw;
        let xv = x_min + (x_max - x_min) * t;
        svg.push_str(&format!(
            r##"<line x1="{x}" y1="{}" x2="{x}" y2="{}" stroke="#91a9c8" stroke-width="1"/>"##,
            pad + ph,
            pad + ph + 7.0
        ));
        svg.push_str(&format!(
            r##"<text x="{x}" y="{}" fill="#9db1cd" font-size="14" text-anchor="middle" font-family="Arial">{xv:.1}</text>"##,
            pad + ph + 24.0
        ));
    }
    for i in 0..=y_ticks {
        let t = i as f64 / y_ticks as f64;
        let y = pad + (1.0 - t) * ph;
        let yv = y_min + (y_max - y_min) * t;
        svg.push_str(&format!(
            r##"<line x1="{}" y1="{y}" x2="{pad}" y2="{y}" stroke="#91a9c8" stroke-width="1"/>"##,
            pad - 7.0
        ));
        svg.push_str(&format!(
            r##"<text x="{}" y="{}" fill="#9db1cd" font-size="14" text-anchor="end" dominant-baseline="middle" font-family="Arial">{yv:.2}</text>"##,
            pad - 10.0,
            y
        ));
    }

    let series_to_svg = |svg: &mut String,
                         on: bool,
                         color: &str,
                         value_of: fn(&MeasurementPoint) -> f64| {
        if !on {
            return;
        }
        svg.push_str(&format!(
            r#"<polyline fill="none" stroke="{color}" stroke-width="{}" points=""#,
            settings.line_width
        ));
        for p in points {
            let (x, y) = map(p.elapsed_s, value_of(p));
            svg.push_str(&format!("{x:.2},{y:.2} "));
        }
        svg.push_str(r#"" />"#);
    };
    series_to_svg(&mut svg, settings.show_flow, "#5fbfff", |p| p.flow_ml_min);
    series_to_svg(&mut svg, settings.show_setpoint, "#ffc054", |p| p.speed_setpoint_ml_min);
    series_to_svg(&mut svg, settings.show_theoretical, "#aa7bff", |p| {
        p.flow_theoretical_ml_min
    });
    series_to_svg(&mut svg, settings.show_estimated, "#62dd8e", |p| {
        p.flow_estimated_ml_min
    });

    let title = if settings.title.trim().is_empty() {
        "Measurement Plot"
    } else {
        settings.title.as_str()
    };
    svg.push_str(&format!(
        r##"<text x="{pad}" y="40" fill="#d9e2ef" font-size="24" font-family="Arial">{title}</text>"##
    ));
    svg.push_str(&format!(
        r##"<text x="{pad}" y="{}" fill="#9db1cd" font-size="16" font-family="Arial">time {:.2}s to {:.2}s</text>"##,
        h - 24.0,
        x_min,
        x_max
    ));
    svg.push_str(&format!(
        r##"<text x="{}" y="{}" fill="#9db1cd" font-size="16" text-anchor="middle" font-family="Arial">Time (s)</text>"##,
        pad + (pw * 0.5),
        pad + ph + 46.0
    ));
    svg.push_str(&format!(
        r##"<text x="24" y="{}" fill="#9db1cd" font-size="16" font-family="Arial" transform="rotate(-90 24,{})">Flow (ml/min)</text>"##,
        pad + (ph * 0.5),
        pad + (ph * 0.5)
    ));

    let mut legend_row: usize = 0;
    let mut add_legend = |on: bool, color: &str, name: &str| {
        if !on {
            return;
        }
        let y = pad + 16.0 + (legend_row as f64 * 22.0);
        svg.push_str(&format!(
            r##"<line x1="{}" y1="{y}" x2="{}" y2="{y}" stroke="{color}" stroke-width="3"/>"##,
            pad + pw - 150.0,
            pad + pw - 124.0
        ));
        svg.push_str(&format!(
            r##"<text x="{}" y="{}" fill="#d9e2ef" font-size="14" font-family="Arial">{name}</text>"##,
            pad + pw - 116.0,
            y + 5.0
        ));
        legend_row += 1;
    };
    if settings.show_legend {
        add_legend(settings.show_flow, "#5fbfff", "Flow");
        add_legend(settings.show_setpoint, "#ffc054", "Setpoint");
        add_legend(settings.show_theoretical, "#aa7bff", "Theoretical");
        add_legend(settings.show_estimated, "#62dd8e", "Estimated");
    }
    svg.push_str("</svg>");
    std::fs::write(path, svg)?;
    Ok(())
}

impl eframe::App for PumpGuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_backend();
        if !self.ui_loaded_from_recipe {
            self.apply_recipe_ui_settings();
            self.ui_loaded_from_recipe = true;
        }
        self.ensure_path_defaults();
        self.maybe_run_startup_automation();
        if self.state.connected && self.pending_auto_measure_after_connect {
            self.send_cmd("Auto measurement start", PumpCmd::BeginMeasurement);
            self.pending_auto_measure_after_connect = false;
        }
        self.apply_theme(ctx);

        egui::TopBottomPanel::top("main_menu").show(ctx, |ui| {
            self.quick_action_menu(ui, _frame);
        });

        let width = ctx.input(|i| i.content_rect().width());
        let use_sidebar = self.ui_settings.show_sidebar_on_wide && width > 980.0;

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            if !use_sidebar {
                self.tab_bar(ui);
            }
            self.header_status(ui);
        });

        if use_sidebar {
            egui::SidePanel::left("nav_sidebar")
                .resizable(false)
                .default_width(190.0)
                .show(ctx, |ui| {
                    ui.heading("Views");
                    ui.separator();
                    for idx in 0..TabView::COUNT {
                        if ui
                            .selectable_label(self.state.selected_tab == idx, Self::tab_name(idx))
                            .clicked()
                        {
                            self.state.selected_tab = idx;
                        }
                    }
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::Frame::group(ui.style()).show(ui, |ui| match self.state.current_tab() {
                TabView::Connection => self.connection_panel(ui),
                TabView::Manual => self.manual_panel(ui),
                TabView::Cycled => self.cycled_panel(ui),
                TabView::Measurement => self.measurement_panel(ui),
                TabView::Recipe => self.recipe_panel(ui),
                TabView::Status => self.status_panel(ui),
            });
        });
        self.plot_export_modal(ctx);
        self.file_confirm_modal(ctx);
        self.persist_ui_settings_to_recipe();

        // Keep the UI responsive so pending/measurement updates appear without
        // requiring user interaction.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}
