use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use csv::Writer;
use serde::{Deserialize, Serialize};
use serialport::SerialPort;

// Serial / protocol timing.
const PUMP_BAUD: u32 = 9600;
const CMD_TIMEOUT: Duration = Duration::from_millis(1500);
const SERIAL_READ_POLL: Duration = Duration::from_millis(20);

// Worker internal scheduling.
const WORKER_POLL: Duration = Duration::from_millis(25);
const CYCLE_TICK_INTERVAL: Duration = Duration::from_millis(100);

// Ismatec framing characters.
const ACK: u8 = b'*';
const NACK: u8 = b'#';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabView {
    Connection,
    Manual,
    Cycled,
    Measurement,
    Recipe,
    Status,
}

impl TabView {
    pub const COUNT: usize = 6;

    pub fn from_index(i: usize) -> Self {
        match i {
            0 => Self::Connection,
            1 => Self::Manual,
            2 => Self::Cycled,
            3 => Self::Measurement,
            4 => Self::Recipe,
            _ => Self::Status,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CycleStep {
    pub duration_s: f32,
    pub speed_ml_min: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecipeKind {
    #[default]
    Config,
    Executable,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecipePlotSettings {
    pub title: String,
    pub show_legend: bool,
    pub show_flow: bool,
    pub show_setpoint: bool,
    pub show_theoretical: bool,
    pub show_estimated: bool,
    pub show_grid: bool,
    pub show_points: bool,
    pub auto_scale_y: bool,
    pub y_min: f64,
    pub y_max: f64,
    pub line_width: f32,
    pub max_points: usize,
    pub svg_export_path: String,
}

impl Default for RecipePlotSettings {
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecipeUiSettings {
    pub theme: String,
    pub density: f32,
    pub font_scale: f32,
    pub show_sidebar_on_wide: bool,
    pub auto_connect_on_start: bool,
    pub auto_measure_on_start: bool,
    pub recipe_folder: String,
    pub data_folder: String,
    pub plot: RecipePlotSettings,
}

impl Default for RecipeUiSettings {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            density: 1.0,
            font_scale: 1.0,
            show_sidebar_on_wide: true,
            auto_connect_on_start: false,
            auto_measure_on_start: false,
            recipe_folder: ".".to_string(),
            data_folder: ".".to_string(),
            plot: RecipePlotSettings::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PumpRecipe {
    pub schema_version: u32,
    #[serde(default)]
    pub recipe_kind: RecipeKind,
    #[serde(default)]
    pub ui: RecipeUiSettings,
    pub serial_port: String,
    pub pump_addr: u8,
    pub speed_ml_min: f32,
    pub dispense_duration_s: f32,
    pub pause_duration_s: f32,
    pub cycles: u16,
    pub backsteps: u8,
    pub measurement_enabled: bool,
    #[serde(default = "default_measurement_interval_ms")]
    pub measurement_interval_ms: u64,
    pub csv_export_path: String,
    pub tube_inner_diameter_mm: f32,
    pub rotor_rpm_max: f32,
    pub tube_advance_mm_per_rev: f32,
    pub linear_transition_between_steps: bool,
    pub cycle_program: Vec<CycleStep>,
    pub calibrated_max_flow_ml_min: f32,
}

impl Default for PumpRecipe {
    fn default() -> Self {
        Self {
            schema_version: 1,
            recipe_kind: RecipeKind::Config,
            ui: RecipeUiSettings::default(),
            serial_port: default_serial_port().to_string(),
            pump_addr: 1,
            speed_ml_min: 0.1,
            dispense_duration_s: 4.5,
            pause_duration_s: 2.0,
            cycles: 5,
            backsteps: 0,
            measurement_enabled: false,
            measurement_interval_ms: default_measurement_interval_ms(),
            csv_export_path: "flowrate_export.csv".to_string(),
            tube_inner_diameter_mm: 0.6,
            rotor_rpm_max: 45.0,
            tube_advance_mm_per_rev: 50.0,
            linear_transition_between_steps: false,
            cycle_program: vec![
                CycleStep {
                    duration_s: 2.0,
                    speed_ml_min: 0.01,
                },
                CycleStep {
                    duration_s: 4.0,
                    speed_ml_min: 0.1,
                },
            ],
            calibrated_max_flow_ml_min: 0.15,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MeasurementPoint {
    pub timestamp_iso: String,
    pub elapsed_s: f64,
    pub flow_ml_min: f64,
    pub speed_setpoint_ml_min: f64,
    pub speed_percent: f64,
    pub flow_theoretical_ml_min: f64,
    pub flow_estimated_ml_min: f64,
}

/// UI-side mirror of confirmed pump state plus view-only scratch buffers.
///
/// The worker thread is the single source of truth for hardware state; every
/// field below is updated exclusively from `apply_event` (driven by
/// [`PumpEvt`]) except for:
///   * pure UI scratch (`*_input_buffer`, `selected_tab`, `recipe_file_path`),
///   * the editable recipe draft (`recipe`), which is the user's desired
///     configuration; it only becomes pump state when committed via
///     [`PumpCmd`]s.
#[derive(Clone, Debug)]
pub struct AppState {
    pub selected_tab: usize,
    pub recipe: PumpRecipe,
    pub connected: bool,
    pub running: bool,
    pub measuring: bool,
    pub cycle_running: bool,
    pub last_response: String,
    pub logs: VecDeque<String>,
    pub measurements: Vec<MeasurementPoint>,
    pub speed_input_buffer: String,
    pub speed_step_ml_min: f32,
    pub speed_step_input_buffer: String,
    pub recipe_file_path: String,
    pub calibration_input_buffer: String,
    /// Number of outstanding serial commands awaiting a `Response` / `Error`.
    pub pending: usize,
    /// Human readable label for the most recent pending command.
    pub pending_label: String,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            selected_tab: 0,
            recipe: PumpRecipe::default(),
            connected: false,
            running: false,
            measuring: false,
            cycle_running: false,
            last_response: "-".to_string(),
            logs: VecDeque::new(),
            measurements: Vec::new(),
            speed_input_buffer: String::new(),
            speed_step_ml_min: 0.001,
            speed_step_input_buffer: "0.001".to_string(),
            recipe_file_path: "recipe.json".to_string(),
            calibration_input_buffer: "1.000".to_string(),
            pending: 0,
            pending_label: String::new(),
        }
    }
}

impl AppState {
    pub fn push_log(&mut self, msg: impl Into<String>) {
        self.logs.push_front(msg.into());
        while self.logs.len() > 120 {
            self.logs.pop_back();
        }
    }

    pub fn current_tab(&self) -> TabView {
        TabView::from_index(self.selected_tab)
    }

    /// Mark a new serial command as in-flight. The GUI calls this right before
    /// `PumpClient::send` so that the UI can reflect immediate feedback while
    /// waiting for a `Response`/`Error` event from the worker.
    pub fn mark_pending(&mut self, label: impl Into<String>) {
        self.pending = self.pending.saturating_add(1);
        self.pending_label = label.into();
    }

    fn clear_one_pending(&mut self) {
        self.pending = self.pending.saturating_sub(1);
        if self.pending == 0 {
            self.pending_label.clear();
        }
    }

    pub fn apply_event(&mut self, evt: PumpEvt) {
        match evt {
            PumpEvt::Connected(msg) => {
                self.connected = true;
                self.last_response = msg.clone();
                self.push_log(format!("Connected: {msg}"));
                self.clear_one_pending();
            }
            PumpEvt::Disconnected => {
                self.connected = false;
                self.running = false;
                self.measuring = false;
                self.cycle_running = false;
                self.pending = 0;
                self.pending_label.clear();
                self.push_log("Disconnected");
            }
            PumpEvt::Running(v) => {
                self.running = v;
            }
            PumpEvt::Response(text) => {
                self.last_response = text.clone();
                self.push_log(format!("Response: {text}"));
                self.clear_one_pending();
            }
            PumpEvt::MeasurementPoint(point) => {
                self.last_response = format!("Flow {:.3} ml/min", point.flow_ml_min);
                self.measurements.push(point);
                if self.measurements.len() > 10_000 {
                    self.measurements.remove(0);
                }
            }
            PumpEvt::MeasurementStarted => {
                self.measuring = true;
                self.push_log("Measurement started");
                self.clear_one_pending();
            }
            PumpEvt::MeasurementStopped => {
                self.measuring = false;
                self.push_log("Measurement stopped");
                self.clear_one_pending();
            }
            PumpEvt::CycleRunning(v) => {
                self.cycle_running = v;
            }
            PumpEvt::Error(err) => {
                self.last_response = err.clone();
                self.push_log(format!("Error: {err}"));
                self.clear_one_pending();
            }
        }
    }
}

/// Commands the GUI can issue to the worker.
///
/// All variants except [`PumpCmd::Quit`] represent *intent*. The worker is
/// responsible for translating them into serial transactions and emitting the
/// resulting [`PumpEvt`]s. Internal periodic work (measurement sampling,
/// cycle-program stepping) is driven by the worker itself.
#[derive(Debug, Clone)]
pub enum PumpCmd {
    Connect {
        port: String,
        addr: u8,
    },
    Disconnect,
    Start,
    Stop,
    SetSpeedMlMin(f32),
    SetBacksteps(u8),
    SetCycles(u16),
    #[allow(dead_code)] // Kept for protocol completeness; currently no GUI/CLI sender.
    SetPauseSeconds(f32),
    #[allow(dead_code)] // Kept for protocol completeness; currently no GUI/CLI sender.
    SetDispenseSeconds(f32),
    SetTubeDiameterMm(f32),
    SetCalibratedFlowMlMin(f32),
    #[allow(dead_code)] // Reserved calibration workflow command.
    CalibrateMaxFlow,
    SetMeasurementIntervalMs(u64),
    BeginMeasurement,
    StopMeasurement,
    StartCycleProgram {
        steps: Vec<CycleStep>,
        linear: bool,
        repeat_cycles: u16,
    },
    StopCycleProgram,
    ApplyRecipe(PumpRecipe),
    Quit,
}

#[derive(Debug, Clone)]
pub enum PumpEvt {
    Connected(String),
    Disconnected,
    Running(bool),
    Response(String),
    MeasurementPoint(MeasurementPoint),
    MeasurementStarted,
    MeasurementStopped,
    CycleRunning(bool),
    Error(String),
}

pub struct PumpClient {
    tx: Sender<PumpCmd>,
    rx: Receiver<PumpEvt>,
    worker: Option<JoinHandle<()>>,
}

impl PumpClient {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PumpCmd>();
        let (evt_tx, evt_rx) = mpsc::channel::<PumpEvt>();
        let worker = thread::spawn(move || run_worker(cmd_rx, evt_tx));
        Self {
            tx: cmd_tx,
            rx: evt_rx,
            worker: Some(worker),
        }
    }

    pub fn send(&self, cmd: PumpCmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn poll_events(&self) -> Vec<PumpEvt> {
        let mut out = Vec::new();
        while let Ok(evt) = self.rx.try_recv() {
            out.push(evt);
        }
        out
    }
}

impl Default for PumpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PumpClient {
    fn drop(&mut self) {
        let _ = self.tx.send(PumpCmd::Quit);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Serial layer
// ---------------------------------------------------------------------------

struct PumpSerial {
    port: Box<dyn SerialPort>,
    addr: u8,
    calibrated_flow_ml_min: f64,
}

impl PumpSerial {
    fn connect(port_name: &str, addr: u8) -> Result<Self> {
        let normalized_port = normalize_serial_port_for_platform(port_name);
        let port = serialport::new(&normalized_port, PUMP_BAUD)
            .timeout(SERIAL_READ_POLL)
            .open()
            .with_context(|| format!("Failed opening serial port {normalized_port}"))?;
        let mut serial = Self {
            port,
            addr,
            calibrated_flow_ml_min: 1.0,
        };
        if let Ok(flow) = serial.get_flow_rate() {
            serial.calibrated_flow_ml_min = flow.max(0.1);
        }
        Ok(serial)
    }

    /// Write one Ismatec-framed command (`<addr><cmd>\r`) and read a single
    /// response, tolerating partial reads and short timeouts.
    ///
    /// Terminates on `\r\n`, `*` (ACK) or `#` (NACK). Never blocks longer than
    /// [`CMD_TIMEOUT`]; on a dead line it returns `Err` instead of hanging the
    /// worker.
    fn write_and_read_line(&mut self, raw_cmd: &str) -> Result<String> {
        let frame = format!("{}{}\r", self.addr, raw_cmd);
        self.port
            .write_all(frame.as_bytes())
            .context("Serial write failed")?;
        self.port.flush().context("Serial flush failed")?;

        let mut buf = [0u8; 256];
        let mut out: Vec<u8> = Vec::with_capacity(64);
        let deadline = Instant::now() + CMD_TIMEOUT;
        loop {
            match self.port.read(&mut buf) {
                Ok(0) => {
                    // Nothing yet, keep polling until deadline.
                }
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if frame_complete(&out) {
                        break;
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::TimedOut => {
                    // Per-read timeout: fall through and check deadline. If a
                    // fully-framed response already arrived between poll-ups,
                    // return it.
                    if frame_complete(&out) {
                        break;
                    }
                }
                Err(e) => return Err(anyhow!("Serial read failed: {e}")),
            }
            if Instant::now() >= deadline {
                if out.is_empty() {
                    bail!("Pump response timed out after {:?}", CMD_TIMEOUT);
                }
                break;
            }
            // Small yield to avoid busy spinning when the port returns 0 bytes.
            thread::sleep(Duration::from_millis(2));
        }

        let text = String::from_utf8_lossy(&out).trim().to_string();
        if text.is_empty() {
            bail!("Empty pump response for '{raw_cmd}'");
        }
        Ok(text)
    }

    fn probe(&mut self) -> Result<String> {
        self.write_and_read_line("#")
    }
    fn start(&mut self) -> Result<String> {
        self.write_and_read_line("H")
    }
    fn stop(&mut self) -> Result<String> {
        self.write_and_read_line("I")
    }
    fn panel_lock(&mut self) -> Result<String> {
        self.write_and_read_line("B")
    }
    fn panel_manual(&mut self) -> Result<String> {
        self.write_and_read_line("A")
    }
    fn set_speed_ml_min(&mut self, speed_ml_min: f32) -> Result<String> {
        let target_ml_min = clamp_flow_ml_min(speed_ml_min, self.calibrated_flow_ml_min);
        let speed_percent = if self.calibrated_flow_ml_min <= 0.0 {
            0.0
        } else {
            (target_ml_min as f64 / self.calibrated_flow_ml_min * 100.0) as f32
        };
        let clamped_percent = clamp_speed(speed_percent);
        let encoded = (clamped_percent * 10.0).round() as u16;
        self.write_and_read_line(&format!("S{encoded:05}"))
    }
    fn set_backsteps(&mut self, backsteps: u8) -> Result<String> {
        let clamped = clamp_backsteps(backsteps);
        self.write_and_read_line(&format!("%{clamped:04}"))
    }
    fn set_cycles(&mut self, cycles: u16) -> Result<String> {
        self.write_and_read_line(&format!("\"{cycles:04}"))
    }
    fn set_pause_seconds(&mut self, pause_s: f32) -> Result<String> {
        let encoded = (clamp_seconds(pause_s) * 10.0).round() as u16;
        self.write_and_read_line(&format!("T{encoded:04}"))
    }
    fn set_dispense_seconds(&mut self, dispense_s: f32) -> Result<String> {
        let encoded = (clamp_seconds(dispense_s) * 10.0).round() as u16;
        self.write_and_read_line(&format!("V{encoded:04}"))
    }
    fn set_tube_diameter_mm(&mut self, id_mm: f32) -> Result<String> {
        let val = (id_mm.max(0.1) * 100.0).round() as u16;
        self.write_and_read_line(&format!("+{val:04}"))
    }
    fn set_calibrated_flow_ml_min(&mut self, ml_min: f32) -> Result<String> {
        let encoded = (ml_min.max(0.0) * 100.0).round() as u16;
        let reply = self.write_and_read_line(&format!("!{encoded:04}"))?;
        // Keep the cached calibration in sync for the mL/min → percent
        // conversion performed in `set_speed_ml_min`.
        self.calibrated_flow_ml_min = f64::from(ml_min.max(0.1));
        Ok(reply)
    }
    fn get_flow_rate(&mut self) -> Result<f64> {
        let raw = self.write_and_read_line("!")?;
        if raw.trim() == "#" {
            // In overload/invalid states the pump can reply with '#'. Per app
            // policy we treat that as "max flow" instead of failing sampling.
            return Ok(self.calibrated_flow_ml_min.max(0.1));
        }
        parse_flow_rate_ml_min(&raw)
    }
    fn get_speed_percent(&mut self) -> Result<f64> {
        let raw = self.write_and_read_line("S")?;
        parse_flow_rate_ml_min(&raw)
    }
    fn get_default_max_flow_ml_min(&mut self) -> Result<f64> {
        let raw = self.write_and_read_line("?")?;
        parse_flow_rate_ml_min(&raw)
    }
}

fn frame_complete(buf: &[u8]) -> bool {
    if buf.ends_with(b"\r\n") {
        return true;
    }
    matches!(buf.last().copied(), Some(ACK) | Some(NACK))
}

// ---------------------------------------------------------------------------
// Worker thread: the single source of truth for pump hardware state
// ---------------------------------------------------------------------------

struct CycleRuntime {
    steps: Vec<CycleStep>,
    linear: bool,
    repeat_cycles: u16, // 0 == infinite
    current_step: usize,
    step_started: Instant,
}

#[derive(Default)]
struct WorkerState {
    serial: Option<PumpSerial>,
    measuring: bool,
    measure_start: Option<Instant>,
    measure_last_sample: Option<Instant>,
    cycle_runtime: Option<CycleRuntime>,
    cycle_last_tick: Option<Instant>,
    speed_setpoint_ml_min: f64,
    recipe_context: PumpRecipe,
}

impl WorkerState {
    /// Handle one user command. Returns `false` if the worker should stop.
    fn handle_command(&mut self, cmd: PumpCmd, tx: &Sender<PumpEvt>) -> bool {
        match cmd {
            PumpCmd::Quit => return false,

            PumpCmd::Connect { port, addr } => {
                let evt = match PumpSerial::connect(&port, addr) {
                    Ok(mut s) => match s.probe() {
                        Ok(probe) => {
                            let lock_note = match s.panel_lock() {
                                Ok(resp) => format!(" | panel locked ({resp})"),
                                Err(e) => {
                                    let _ = emit(tx, PumpEvt::Error(format!(
                                        "Connected, but failed to lock panel: {e}"
                                    )));
                                    " | panel lock failed".to_string()
                                }
                            };
                            self.serial = Some(s);
                            PumpEvt::Connected(format!("{port} probe: {probe}{lock_note}"))
                        }
                        Err(e) => PumpEvt::Error(format!("Probe failed: {e}")),
                    },
                    Err(e) => PumpEvt::Error(e.to_string()),
                };
                return emit(tx, evt);
            }

            PumpCmd::Disconnect => {
                if let Some(s) = self.serial.as_mut()
                    && let Err(e) = s.panel_manual()
                {
                    let _ = emit(tx, PumpEvt::Error(format!(
                        "Failed to unlock panel during disconnect: {e}"
                    )));
                }
                self.serial = None;
                self.measuring = false;
                self.measure_last_sample = None;
                self.measure_start = None;
                self.cycle_runtime = None;
                self.cycle_last_tick = None;
                return emit(tx, PumpEvt::Disconnected);
            }

            PumpCmd::Start => {
                let ok = self.run_serial_cmd(tx, "Start", |s| s.start());
                if ok {
                    let _ = emit(tx, PumpEvt::Running(true));
                }
            }
            PumpCmd::Stop => {
                let _ = self.run_serial_cmd(tx, "Stop", |s| s.stop());
                // Report Running(false) regardless — the user wants the pump off.
                let _ = emit(tx, PumpEvt::Running(false));
                // Abort any in-flight cycle program.
                if self.cycle_runtime.take().is_some() {
                    let _ = emit(tx, PumpEvt::CycleRunning(false));
                }
            }

            PumpCmd::SetSpeedMlMin(v) => {
                self.speed_setpoint_ml_min = f64::from(v);
                self.run_serial_cmd(tx, "Set speed", |s| s.set_speed_ml_min(v));
            }
            PumpCmd::SetBacksteps(v) => {
                self.run_serial_cmd(tx, "Set backsteps", |s| s.set_backsteps(v));
            }
            PumpCmd::SetCycles(v) => {
                self.run_serial_cmd(tx, "Set cycles", |s| s.set_cycles(v));
            }
            PumpCmd::SetPauseSeconds(v) => {
                self.run_serial_cmd(tx, "Set pause", |s| s.set_pause_seconds(v));
            }
            PumpCmd::SetDispenseSeconds(v) => {
                self.run_serial_cmd(tx, "Set dispense", |s| s.set_dispense_seconds(v));
            }
            PumpCmd::SetTubeDiameterMm(v) => {
                self.recipe_context.tube_inner_diameter_mm = v;
                self.run_serial_cmd(tx, "Set tube ID", |s| s.set_tube_diameter_mm(v));
            }
            PumpCmd::SetCalibratedFlowMlMin(v) => {
                self.recipe_context.calibrated_max_flow_ml_min = v;
                self.run_serial_cmd(tx, "Set calibrated flow", |s| {
                    s.set_calibrated_flow_ml_min(v)
                });
            }
            PumpCmd::CalibrateMaxFlow => {
                let msg = match self.serial.as_mut() {
                    Some(s) => match s.get_default_max_flow_ml_min() {
                        Ok(default_max) => match s.set_calibrated_flow_ml_min(default_max as f32) {
                            Ok(_) => PumpEvt::Response(format!(
                                "Max calibration applied at {default_max:.3} ml/min"
                            )),
                            Err(e) => PumpEvt::Error(format!("Max cal write failed: {e}")),
                        },
                        Err(e) => PumpEvt::Error(format!("Max cal query failed: {e}")),
                    },
                    None => PumpEvt::Error("Not connected".to_string()),
                };
                return emit(tx, msg);
            }
            PumpCmd::SetMeasurementIntervalMs(v) => {
                let clamped = clamp_measurement_interval_ms(v);
                self.recipe_context.measurement_interval_ms = clamped;
                let _ = emit(
                    tx,
                    PumpEvt::Response(format!("Measurement interval set to {clamped} ms")),
                );
            }

            PumpCmd::BeginMeasurement => {
                self.measuring = true;
                let now = Instant::now();
                self.measure_start = Some(now);
                // Schedule the first sample immediately on the next tick.
                self.measure_last_sample =
                    Some(now - Duration::from_millis(self.recipe_context.measurement_interval_ms));
                return emit(tx, PumpEvt::MeasurementStarted);
            }
            PumpCmd::StopMeasurement => {
                self.measuring = false;
                self.measure_last_sample = None;
                return emit(tx, PumpEvt::MeasurementStopped);
            }

            PumpCmd::StartCycleProgram {
                steps,
                linear,
                repeat_cycles,
            } => {
                if steps.is_empty() {
                    return emit(tx, PumpEvt::Error("Cycle program is empty".to_string()));
                }
                let first_speed = steps[0].speed_ml_min;
                self.speed_setpoint_ml_min = f64::from(first_speed);
                if !self.run_serial_cmd(tx, "Cycle: set initial speed", |s| {
                    s.set_speed_ml_min(first_speed)
                }) {
                    return true;
                }
                if !self.run_serial_cmd(tx, "Cycle: start", |s| s.start()) {
                    return true;
                }
                self.cycle_runtime = Some(CycleRuntime {
                    steps,
                    linear,
                    repeat_cycles,
                    current_step: 0,
                    step_started: Instant::now(),
                });
                self.cycle_last_tick = Some(Instant::now());
                let _ = emit(tx, PumpEvt::CycleRunning(true));
            }
            PumpCmd::StopCycleProgram => {
                self.cycle_runtime = None;
                self.cycle_last_tick = None;
                let _ = self.run_serial_cmd(tx, "Cycle: stop", |s| s.stop());
                let _ = emit(tx, PumpEvt::CycleRunning(false));
            }

            PumpCmd::ApplyRecipe(recipe) => {
                self.recipe_context = recipe.clone();
                self.speed_setpoint_ml_min = f64::from(recipe.speed_ml_min);
                let result = self.apply_recipe_sequential(&recipe);
                let evt = match result {
                    Ok(()) => PumpEvt::Response("Recipe applied to pump".to_string()),
                    Err(e) => PumpEvt::Error(format!("Apply recipe failed: {e}")),
                };
                return emit(tx, evt);
            }
        }
        true
    }

    /// Run all periodic maintenance (measurement sampling, cycle stepping).
    fn run_periodic(&mut self, tx: &Sender<PumpEvt>) {
        if self.measuring && self.serial.is_some() {
            let now = Instant::now();
            let measure_interval =
                Duration::from_millis(self.recipe_context.measurement_interval_ms);
            let due = self
                .measure_last_sample
                .map(|t| now.duration_since(t) >= measure_interval)
                .unwrap_or(true);
            if due {
                self.measure_last_sample = Some(now);
                self.tick_measurement(tx);
            }
        }

        if self.cycle_runtime.is_some() {
            let now = Instant::now();
            let due = self
                .cycle_last_tick
                .map(|t| now.duration_since(t) >= CYCLE_TICK_INTERVAL)
                .unwrap_or(true);
            if due {
                self.cycle_last_tick = Some(now);
                self.tick_cycle(tx);
            }
        }
    }

    /// Execute a closure against the serial port and emit a `Response`/`Error`
    /// event for the GUI. Returns true on success.
    fn run_serial_cmd<F>(&mut self, tx: &Sender<PumpEvt>, label: &str, f: F) -> bool
    where
        F: FnOnce(&mut PumpSerial) -> Result<String>,
    {
        let evt = match self.serial.as_mut() {
            Some(s) => match f(s) {
                Ok(r) => PumpEvt::Response(r),
                Err(e) => PumpEvt::Error(format!("{label}: {e}")),
            },
            None => PumpEvt::Error(format!("{label}: not connected")),
        };
        let ok = matches!(evt, PumpEvt::Response(_));
        let _ = emit(tx, evt);
        ok
    }

    /// Apply a recipe as a single queued sequence. The worker holds the
    /// mutable serial handle, so the pump's receive buffer cannot be overrun:
    /// every sub-command is written, framed and acknowledged before the next
    /// one is queued. A single `Response` or `Error` event is emitted at the
    /// end via the caller.
    fn apply_recipe_sequential(&mut self, recipe: &PumpRecipe) -> Result<()> {
        let s = self
            .serial
            .as_mut()
            .ok_or_else(|| anyhow!("Not connected"))?;

        s.set_tube_diameter_mm(recipe.tube_inner_diameter_mm)
            .context("tube diameter")?;
        s.set_calibrated_flow_ml_min(recipe.calibrated_max_flow_ml_min)
            .context("calibrated flow")?;
        s.set_speed_ml_min(recipe.speed_ml_min).context("speed")?;
        s.set_dispense_seconds(recipe.dispense_duration_s)
            .context("dispense duration")?;
        s.set_pause_seconds(recipe.pause_duration_s)
            .context("pause duration")?;
        s.set_cycles(recipe.cycles).context("cycles")?;
        s.set_backsteps(recipe.backsteps).context("backsteps")?;
        Ok(())
    }

    fn tick_measurement(&mut self, tx: &Sender<PumpEvt>) {
        let Some(s) = self.serial.as_mut() else {
            return;
        };
        let measure_start = match self.measure_start {
            Some(t) => t,
            None => {
                let now = Instant::now();
                self.measure_start = Some(now);
                now
            }
        };
        let speed_percent = match s.get_speed_percent() {
            Ok(v) => v,
            Err(e) => {
                let _ = emit(tx, PumpEvt::Error(format!("Measurement read failed: {e}")));
                return;
            }
        };
        // Keep measurement estimates on the same calibrated basis used for
        // mL/min -> % speed commands so GUI and headless recipe runs align.
        let effective_max_flow = s.calibrated_flow_ml_min.max(0.1);

        let rpm = speed_percent / 100.0 * f64::from(self.recipe_context.rotor_rpm_max);
        let radius_mm = f64::from(self.recipe_context.tube_inner_diameter_mm) / 2.0;
        let area_mm2 = std::f64::consts::PI * radius_mm * radius_mm;
        let mm3_per_min = area_mm2 * f64::from(self.recipe_context.tube_advance_mm_per_rev) * rpm;
        let flow_theoretical_ml_min = mm3_per_min / 1000.0;
        let flow_estimated_ml_min = effective_max_flow * speed_percent / 100.0;

        let point = MeasurementPoint {
            timestamp_iso: DateTime::<Utc>::from(std::time::SystemTime::now()).to_rfc3339(),
            elapsed_s: measure_start.elapsed().as_secs_f64(),
            flow_ml_min: flow_estimated_ml_min,
            speed_setpoint_ml_min: self.speed_setpoint_ml_min,
            speed_percent,
            flow_theoretical_ml_min,
            flow_estimated_ml_min,
        };
        let _ = emit(tx, PumpEvt::MeasurementPoint(point));
    }

    fn tick_cycle(&mut self, tx: &Sender<PumpEvt>) {
        // Work on a snapshot so we can mutate `self.serial` inside the closure
        // without simultaneously holding a mutable borrow on `self.cycle_runtime`.
        let Some(rt) = self.cycle_runtime.as_mut() else {
            return;
        };
        if rt.steps.is_empty() {
            self.cycle_runtime = None;
            let _ = emit(tx, PumpEvt::CycleRunning(false));
            return;
        }

        let current = rt.steps[rt.current_step].clone();
        let next_idx = (rt.current_step + 1) % rt.steps.len();
        let next = rt.steps[next_idx].clone();
        let elapsed_s = rt.step_started.elapsed().as_secs_f32();

        // Linear interpolation between steps when enabled.
        if rt.linear && current.duration_s > 0.01 {
            let frac = (elapsed_s / current.duration_s).clamp(0.0, 1.0);
            let interp = current.speed_ml_min + (next.speed_ml_min - current.speed_ml_min) * frac;
            self.speed_setpoint_ml_min = f64::from(interp);
            if let Some(s) = self.serial.as_mut()
                && let Err(e) = s.set_speed_ml_min(interp)
            {
                let _ = emit(tx, PumpEvt::Error(format!("Cycle ramp failed: {e}")));
            }
        }

        // Step advance.
        if elapsed_s >= current.duration_s.max(0.01) {
            let rt = self.cycle_runtime.as_mut().expect("runtime present");
            rt.current_step += 1;
            if rt.current_step >= rt.steps.len() {
                rt.current_step = 0;
                if rt.repeat_cycles > 0 {
                    rt.repeat_cycles -= 1;
                    if rt.repeat_cycles == 0 {
                        self.cycle_runtime = None;
                        self.cycle_last_tick = None;
                        if let Some(s) = self.serial.as_mut()
                            && let Err(e) = s.stop()
                        {
                            let _ = emit(tx, PumpEvt::Error(format!("Cycle stop failed: {e}")));
                        }
                        let _ = emit(tx, PumpEvt::CycleRunning(false));
                        let _ = emit(tx, PumpEvt::Running(false));
                        return;
                    }
                }
            }
            let rt = self.cycle_runtime.as_mut().expect("runtime present");
            rt.step_started = Instant::now();
            let step_speed = rt.steps[rt.current_step].speed_ml_min;
            self.speed_setpoint_ml_min = f64::from(step_speed);
            if let Some(s) = self.serial.as_mut()
                && let Err(e) = s.set_speed_ml_min(step_speed)
            {
                let _ = emit(tx, PumpEvt::Error(format!("Cycle step apply failed: {e}")));
            }
        }
    }
}

fn run_worker(rx: Receiver<PumpCmd>, tx: Sender<PumpEvt>) {
    let mut state = WorkerState::default();
    loop {
        match rx.recv_timeout(WORKER_POLL) {
            Ok(cmd) => {
                if !state.handle_command(cmd, &tx) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        state.run_periodic(&tx);
    }
}

/// Send an event, returning `false` if the receiver is gone (which tells the
/// worker to stop).
fn emit(tx: &Sender<PumpEvt>, evt: PumpEvt) -> bool {
    tx.send(evt).is_ok()
}

// ---------------------------------------------------------------------------
// Platform / helpers / validation
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub fn default_serial_port() -> &'static str {
    "COM3"
}

#[cfg(not(windows))]
pub fn default_serial_port() -> &'static str {
    "/dev/ttyUSB0"
}

pub fn auto_detect_serial_port(preferred: &str) -> Option<String> {
    let preferred_trimmed = preferred.trim();
    let ports = serialport::available_ports().ok()?;
    if ports.is_empty() {
        return None;
    }

    if !preferred_trimmed.is_empty()
        && let Some(port) = ports
            .iter()
            .find(|p| p.port_name.trim() == preferred_trimmed)
            .map(|p| p.port_name.clone())
    {
        return Some(port);
    }

    let lower_name = |name: &str| name.to_ascii_lowercase();
    let rank = |name: &str| {
        let n = lower_name(name);
        if n.contains("usb") || n.contains("acm") {
            0
        } else if n.contains("com") {
            1
        } else if n.contains("tty") {
            2
        } else {
            3
        }
    };

    ports
        .iter()
        .min_by_key(|p| rank(&p.port_name))
        .map(|p| p.port_name.clone())
}

pub fn normalize_serial_port_for_platform(port_name: &str) -> String {
    #[cfg(windows)]
    {
        let trimmed = port_name.trim();
        if trimmed.is_empty() {
            return String::new();
        }

        // Users often type "COM4:" in terminal tools on Windows.
        let without_trailing_colon = trimmed.trim_end_matches(':');
        let upper = without_trailing_colon.to_ascii_uppercase();
        if upper.starts_with(r"\\.\COM") {
            return without_trailing_colon.to_string();
        }
        if let Some(num) = upper.strip_prefix("COM")
            && let Ok(parsed) = num.parse::<u16>()
        {
            if parsed >= 10 {
                return format!(r"\\.\COM{parsed}");
            }
            return format!("COM{parsed}");
        }
        without_trailing_colon.to_string()
    }
    #[cfg(not(windows))]
    {
        port_name.trim().to_string()
    }
}

pub fn parse_flow_rate_ml_min(raw: &str) -> Result<f64> {
    let mut digits = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            digits.push(ch);
        }
    }
    if digits.is_empty() {
        bail!("Unable to parse flow rate from: {raw}");
    }
    digits.parse::<f64>().context("Invalid flow rate float")
}

pub fn clamp_speed(v: f32) -> f32 {
    v.clamp(0.0, 100.0)
}
pub fn clamp_seconds(v: f32) -> f32 {
    v.clamp(0.0, 999.9)
}
pub fn clamp_backsteps(v: u8) -> u8 {
    v.min(100)
}
pub fn clamp_flow_ml_min(v: f32, calibrated_max: f64) -> f32 {
    let max_flow = calibrated_max.max(0.1) as f32;
    v.clamp(0.0, max_flow)
}
pub fn clamp_speed_step(v: f32) -> f32 {
    v.clamp(0.001, 100.0)
}

pub fn clamp_measurement_interval_ms(v: u64) -> u64 {
    v.clamp(50, 60_000)
}

fn default_measurement_interval_ms() -> u64 {
    1000
}

pub fn save_recipe(path: &str, recipe: &PumpRecipe) -> Result<()> {
    let file = File::create(path).with_context(|| format!("Create recipe file {path}"))?;
    serde_json::to_writer_pretty(file, recipe).context("Serialize recipe")
}

pub fn load_recipe(path: &str) -> Result<PumpRecipe> {
    let file = File::open(path).with_context(|| format!("Open recipe file {path}"))?;
    let recipe: PumpRecipe = serde_json::from_reader(file).context("Parse recipe json")?;
    validate_recipe(&recipe)?;
    Ok(recipe)
}

pub fn validate_recipe(recipe: &PumpRecipe) -> Result<()> {
    if recipe.schema_version != 1 {
        bail!("Unsupported schema_version: {}", recipe.schema_version);
    }
    if !(1..=8).contains(&recipe.pump_addr) {
        bail!("pump_addr must be 1..=8");
    }
    if recipe.csv_export_path.is_empty() {
        bail!("csv_export_path cannot be empty");
    }
    if recipe.measurement_interval_ms == 0 {
        bail!("measurement_interval_ms must be > 0");
    }
    if recipe.serial_port.is_empty() {
        bail!("serial_port cannot be empty");
    }
    if recipe.cycle_program.is_empty() {
        bail!("cycle_program cannot be empty");
    }
    if recipe.ui.recipe_folder.is_empty() {
        bail!("ui.recipe_folder cannot be empty");
    }
    if recipe.ui.data_folder.is_empty() {
        bail!("ui.data_folder cannot be empty");
    }
    Ok(())
}

pub fn export_measurements_csv(path: &str, points: &[MeasurementPoint]) -> Result<()> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        bail!("Parent directory does not exist for CSV path: {path}");
    }
    let mut wtr = Writer::from_path(path).with_context(|| format!("Open csv file {path}"))?;
    wtr.write_record([
        "timestamp_iso",
        "elapsed_s",
        "flow_ml_min",
        "speed_setpoint_ml_min",
        "speed_percent",
        "flow_theoretical_ml_min",
        "flow_estimated_ml_min",
    ])?;
    for point in points {
        wtr.serialize(point)?;
    }
    wtr.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clamp_ranges() {
        assert_eq!(clamp_speed(-1.0), 0.0);
        assert_eq!(clamp_speed(120.0), 100.0);
        assert_eq!(clamp_backsteps(250), 100);
        assert_eq!(clamp_seconds(-4.2), 0.0);
        assert_eq!(clamp_seconds(1300.0), 999.9);
    }

    #[test]
    fn test_recipe_json_roundtrip() {
        let recipe = PumpRecipe::default();
        let json = serde_json::to_string(&recipe).expect("serialize");
        let parsed: PumpRecipe = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed.schema_version, 1);
        assert!((parsed.speed_ml_min - recipe.speed_ml_min).abs() < f32::EPSILON);
    }

    #[test]
    fn test_parse_flow() {
        let v = parse_flow_rate_ml_min("25.30 ml/min").expect("flow parse");
        assert!((v - 25.3).abs() < 1e-6);
    }

    #[test]
    fn test_frame_complete() {
        assert!(frame_complete(b"OK\r\n"));
        assert!(frame_complete(b"*"));
        assert!(frame_complete(b"#"));
        assert!(!frame_complete(b"partial"));
        assert!(!frame_complete(b""));
    }

    #[test]
    fn test_tab_from_index() {
        assert_eq!(TabView::from_index(0), TabView::Connection);
        assert_eq!(TabView::from_index(5), TabView::Status);
        assert_eq!(TabView::from_index(99), TabView::Status);
    }

    #[test]
    fn test_pending_counter_flow() {
        let mut st = AppState::default();
        st.mark_pending("one");
        st.mark_pending("two");
        assert_eq!(st.pending, 2);
        st.apply_event(PumpEvt::Response("ok".into()));
        assert_eq!(st.pending, 1);
        st.apply_event(PumpEvt::Error("boom".into()));
        assert_eq!(st.pending, 0);
        assert!(st.pending_label.is_empty());
    }

    #[test]
    fn test_normalize_windows_com_port_prefix() {
        #[cfg(windows)]
        {
            assert_eq!(normalize_serial_port_for_platform("COM3"), "COM3");
            assert_eq!(normalize_serial_port_for_platform("COM10"), r"\\.\COM10");
            assert_eq!(normalize_serial_port_for_platform("com10"), r"\\.\COM10");
            assert_eq!(normalize_serial_port_for_platform("COM10:"), r"\\.\COM10");
            assert_eq!(normalize_serial_port_for_platform("com4:"), "COM4");
            assert_eq!(
                normalize_serial_port_for_platform(r"\\.\COM11"),
                r"\\.\COM11"
            );
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                normalize_serial_port_for_platform(" /dev/ttyUSB1 "),
                "/dev/ttyUSB1"
            );
        }
    }
}
