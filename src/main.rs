use anyhow::Result;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

mod gui;
mod pump_api;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(recipe_path) = args.first() {
        run_headless(recipe_path)
    } else {
        gui::run_gui()
    }
}

fn resolve_path(base_folder: &str, file_path: &str) -> String {
    let p = Path::new(file_path);
    if p.is_absolute() {
        file_path.to_string()
    } else {
        Path::new(base_folder).join(file_path).to_string_lossy().to_string()
    }
}

fn run_headless(recipe_path: &str) -> Result<()> {
    use anyhow::bail;
    use pump_api::{
        MeasurementPoint, PumpCmd, PumpClient, PumpEvt, RecipeKind, auto_detect_serial_port,
        export_measurements_csv, load_recipe,
    };

    let recipe = load_recipe(recipe_path)?;
    println!("Loaded recipe: {recipe_path}");
    println!("Recipe kind: {:?}", recipe.recipe_kind);

    if recipe.recipe_kind == RecipeKind::Config {
        println!(
            "Recipe kind is 'config': no pump execution performed in CLI mode.\n\
             Set `recipe_kind: \"executable\"` to run this recipe headlessly."
        );
        return Ok(());
    }

    if recipe.cycle_program.is_empty() {
        bail!("Executable recipe requires at least one cycle step");
    }
    if recipe.cycles == 0 {
        bail!("Executable CLI mode does not allow infinite cycles (cycles=0)");
    }

    let port = auto_detect_serial_port(&recipe.serial_port).unwrap_or(recipe.serial_port.clone());
    println!(
        "Execution plan:\n\
         - connect to port {port} (addr {})\n\
         - apply recipe settings\n\
         - start cycle program ({} cycles, {} steps, linear={})\n\
         - {}",
        recipe.pump_addr,
        recipe.cycles,
        recipe.cycle_program.len(),
        recipe.linear_transition_between_steps,
        if recipe.ui.auto_measure_on_start || recipe.measurement_enabled {
            "start measurement and export CSV at end if samples exist"
        } else {
            "no measurement (disabled)"
        }
    );

    let client = PumpClient::new();
    let mut captured_points: Vec<MeasurementPoint> = Vec::new();

    client.send(PumpCmd::Connect {
        port: port.clone(),
        addr: recipe.pump_addr,
    });
    let connect_deadline = Instant::now() + Duration::from_secs(10);
    let mut connected = false;
    while !connected {
        for evt in client.poll_events() {
            match evt {
                PumpEvt::Connected(msg) => {
                    println!("Connected: {msg}");
                    connected = true;
                    break;
                }
                PumpEvt::Error(err) => bail!("Connect failed: {err}"),
                _ => {}
            }
        }
        if Instant::now() >= connect_deadline {
            bail!("Timed out waiting for connection to {port}");
        }
        if !connected {
            thread::sleep(Duration::from_millis(40));
        }
    }

    client.send(PumpCmd::ApplyRecipe(recipe.clone()));
    let apply_deadline = Instant::now() + Duration::from_secs(12);
    let mut recipe_applied = false;
    while !recipe_applied {
        for evt in client.poll_events() {
            match evt {
                PumpEvt::Response(msg) if msg.contains("Recipe applied") => {
                    println!("{msg}");
                    recipe_applied = true;
                    break;
                }
                PumpEvt::Error(err) => bail!("Apply recipe failed: {err}"),
                _ => {}
            }
        }
        if Instant::now() >= apply_deadline {
            bail!("Timed out waiting for recipe apply response");
        }
        if !recipe_applied {
            thread::sleep(Duration::from_millis(40));
        }
    }

    let measuring = recipe.ui.auto_measure_on_start || recipe.measurement_enabled;
    if measuring {
        client.send(PumpCmd::BeginMeasurement);
    }

    client.send(PumpCmd::StartCycleProgram {
        steps: recipe.cycle_program.clone(),
        linear: recipe.linear_transition_between_steps,
        repeat_cycles: recipe.cycles,
    });

    let expected_cycle_s: f32 =
        recipe.cycle_program.iter().map(|s| s.duration_s.max(0.01)).sum::<f32>()
            * f32::from(recipe.cycles);
    let cycle_deadline = Instant::now() + Duration::from_secs_f32(expected_cycle_s + 20.0);
    let mut cycle_done = false;
    while !cycle_done {
        for evt in client.poll_events() {
            match evt {
                PumpEvt::MeasurementPoint(point) => captured_points.push(point),
                PumpEvt::CycleRunning(false) => {
                    cycle_done = true;
                    break;
                }
                PumpEvt::Error(err) => bail!("Execution failed: {err}"),
                _ => {}
            }
        }
        if Instant::now() >= cycle_deadline {
            bail!("Timed out waiting for cycle program completion");
        }
        if !cycle_done {
            thread::sleep(Duration::from_millis(50));
        }
    }

    if measuring {
        client.send(PumpCmd::StopMeasurement);
        thread::sleep(Duration::from_millis(200));
        for evt in client.poll_events() {
            if let PumpEvt::MeasurementPoint(point) = evt {
                captured_points.push(point);
            }
        }
    }

    if measuring && !captured_points.is_empty() {
        let csv_path = resolve_path(&recipe.ui.data_folder, &recipe.csv_export_path);
        export_measurements_csv(&csv_path, &captured_points)?;
        let svg_path = resolve_path(&recipe.ui.data_folder, &recipe.ui.plot.svg_export_path);
        export_measurement_svg_headless(&svg_path, &captured_points, &recipe.ui.plot)?;
        println!(
            "Headless run complete. Exported {} measurement samples to {} and plot to {}",
            captured_points.len(),
            csv_path,
            svg_path
        );
    } else {
        println!("Headless run complete.");
    }

    Ok(())
}

fn export_measurement_svg_headless(
    path: &str,
    points: &[pump_api::MeasurementPoint],
    settings: &pump_api::RecipePlotSettings,
) -> Result<()> {
    use anyhow::bail;
    if points.len() < 2 {
        bail!("Need at least 2 points to export a plot");
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
        bail!("Enable at least one plot series before exporting");
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
    if settings.show_grid {
        for i in 1..6 {
            let t = i as f64 / 6.0;
            let x = pad + t * pw;
            let y = pad + t * ph;
            svg.push_str(&format!(
                r##"<line x1="{x}" y1="{pad}" x2="{x}" y2="{}" stroke="#27354a" stroke-width="1"/>"##,
                pad + ph
            ));
            svg.push_str(&format!(
                r##"<line x1="{pad}" y1="{y}" x2="{}" y2="{y}" stroke="#27354a" stroke-width="1"/>"##,
                pad + pw
            ));
        }
    }
    let series_to_svg =
        |svg: &mut String, on: bool, color: &str, value_of: fn(&pump_api::MeasurementPoint) -> f64| {
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
    series_to_svg(&mut svg, settings.show_setpoint, "#ffc054", |p| {
        p.speed_setpoint_ml_min
    });
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
    svg.push_str("</svg>");
    std::fs::write(path, svg)?;
    Ok(())
}
