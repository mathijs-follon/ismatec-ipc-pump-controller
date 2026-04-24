# ISMATEC IPC Pump Controller

## First Stable Release

- Version: `v1.0.0`
- Url: [first release page](https://github.com/mathijs-follon/ismatec-ipc-pump-controller/releases/tag/v1.0.0)

Rust-based controller for an Ismatec pump with:
- a desktop GUI (`eframe/egui`) for interactive operation,
- a headless CLI mode for running executable recipes,
- recipe-based configuration, measurement capture, and measurment CSV + flow graph SVG export.

## Features

- Serial communication with Ismatec protocol framing
- Pump connect/disconnect, start/stop, speed/backsteps/cycles control
- Cycled program execution with optional linear speed transitions
- Measurement sampling with live plotting in the GUI
- Export of measured samples to CSV
- Export of plots to SVG (GUI and headless)
- JSON recipe load/save/apply workflow
- Auto-connect and auto-measure startup behavior (recipe-controlled)

## Project layout

- `src/main.rs`: entry point, GUI vs headless mode selection
- `src/gui.rs`: desktop user interface and workflow actions
- `src/pump_api.rs`: serial protocol worker, recipe model, validation, exports

## Requirements

- Rust toolchain (recommended stable)
- Access to pump serial port (Linux examples: `/dev/ttyUSB0`, `/dev/ttyACM0`, Windows examples: `COM3`, `COM6`) normally autoconnect works this out for you.
- Permissions to open serial devices (e.g. `dialout` group or equivalent)

## Build and run

From `/ismatec-ipc-pump-controller`:

```bash
cargo build --release
```

### Run GUI mode

When started without arguments, the GUI is launched:

```bash
cargo run
```

### Run headless mode

When a recipe file path is provided as the first argument, the app runs headless:

```bash
cargo run -- recipe.json
```


### Run from release

Linux:

```bash
./ipc_pump recipe.json
```

Windows:

```bash
ipc_pump.exe recipe.json
```


Notes:
- Headless execution only runs recipes where `recipe_kind` is `"executable"`.
- `cycles` must be greater than `0` in headless mode.
- If measurement is enabled, flow profile samples CSV and graph plot SVG are exported when execution ends.

## GUI manual

The GUI has six main views: `Connection`, `Manual`, `Cycled`, `Measurement`, `Recipe`, `Status`.

### 1) Connection

Use this page to establish communication and set calibration-sensitive parameters.

- Set `Port` and pump `Address` (`1..=8`)
- `Connect` / `Disconnect`
- Set `Tube ID (mm)`
- Set `Calibrated max flow (ml/min)`

Recommended first-time sequence:
1. Enter serial port and address.
2. Connect.
3. Confirm no errors in status.
4. Set tube diameter and calibrated max flow.

### 2) Manual

Direct pump actuation controls:

- `Start` / `Stop`
- Increment/decrement speed by configurable step
- Set exact speed in ml/min
- Adjust backsteps

Safety note: `Stop` remains available even when other commands are pending.

### 3) Cycled

Program repeated speed steps:

- Set cycle count (`0` means infinite in GUI cycle mode)
- Toggle linear transition between adjacent steps
- Edit cycle steps as `(duration_s, speed_ml_min)`
- Start or stop cycle program

### 4) Measurement

Acquire and export runtime data:

- Start/stop measurement
- Set sample interval (ms)
- View live plot
- Configure plot options (series, grid, legend, axis behavior)
- Export CSV
- Export SVG plot

CSV columns:
- `timestamp_iso`
- `elapsed_s`
- `flow_ml_min`
- `speed_setpoint_ml_min`
- `speed_percent`
- `flow_theoretical_ml_min`
- `flow_estimated_ml_min`

### 5) Recipe

Manage recipe files and app behavior:

- Set recipe folder and data folder
- Choose recipe kind: `config` or `executable`
- Toggle:
  - `auto_connect_on_start`
  - `auto_measure_on_start`
- Scan/select/load/save/delete recipe JSON files
- Apply current recipe directly to connected pump

### 6) Status

- Displays recent command and event log
- Useful for troubleshooting protocol and connection issues

## Recipe workflow (recommended operation recipe)

Use this as the standard workflow for repeatable runs.

1. **Prepare hardware**
   - Connect the pump over serial.
   - Confirm device path (`/dev/ttyUSB*` or `/dev/ttyACM*` on Linux).

2. **Open GUI and connect**
   - Start app: `cargo run`
   - Go to `Connection`, set port/address, click `Connect`.

3. **Calibrate and base settings**
   - Set tube diameter.
   - Set calibrated max flow.
   - Verify no error in status bar/log.

4. **Define run profile**
   - In `Cycled`, set cycle steps and cycle count.
   - Enable linear transition if smooth ramps are required.

5. **Configure measurement**
   - In `Measurement`, set interval and export filenames.
   - Enable desired plot series.

6. **Save recipe JSON**
   - In `Recipe`, choose filename and `Save recipe`.
   - Keep recipe files in your selected recipe folder.

7. **Execute**
   - GUI execution: Start cycle program from `Cycled`.
   - Automated/headless execution: set `recipe_kind: "executable"` and run:
     `cargo run -- your_recipe.json`

8. **Collect outputs**
   - Export CSV and/or SVG from GUI, or let headless mode export at completion.
   - Store outputs in your configured `data_folder`.

9. **Review and iterate**
   - Check flow behavior in plot and CSV.
   - Adjust calibration, step timing, and speeds as needed.

## Headless execution details

Headless mode performs this sequence:
1. Load and validate recipe JSON
2. Auto-detect serial port (or fallback to recipe port)
3. Connect to pump
4. Apply recipe settings
5. Optionally start measurement (based on recipe settings)
6. Run cycle program for configured number of cycles
7. Stop measurement and export CSV + SVG (if samples exist)

## Recipe format

The recipe is JSON and validated by the application.

Minimum practical fields include:
- `schema_version` (currently `1`)
- `recipe_kind` (`"config"` or `"executable"`)
- `serial_port`
- `pump_addr`
- cycle and control values
- `cycle_program` (must not be empty)
- `ui.recipe_folder` and `ui.data_folder`

Example:

```json
{
  "schema_version": 1,
  "recipe_kind": "executable",
  "ui": {
    "theme": "dark",
    "density": 1.0,
    "font_scale": 1.0,
    "show_sidebar_on_wide": true,
    "auto_connect_on_start": false,
    "auto_measure_on_start": true,
    "recipe_folder": ".",
    "data_folder": ".",
    "plot": {
      "title": "Measurement Plot",
      "show_legend": true,
      "show_flow": true,
      "show_setpoint": true,
      "show_theoretical": false,
      "show_estimated": false,
      "show_grid": true,
      "show_points": false,
      "auto_scale_y": true,
      "y_min": 0.0,
      "y_max": 1.5,
      "line_width": 2.0,
      "max_points": 600,
      "svg_export_path": "measurement_plot.svg"
    }
  },
  "serial_port": "/dev/ttyUSB0",
  "pump_addr": 1,
  "speed_ml_min": 0.1,
  "dispense_duration_s": 4.5,
  "pause_duration_s": 2.0,
  "cycles": 5,
  "backsteps": 0,
  "measurement_enabled": true,
  "measurement_interval_ms": 1000,
  "csv_export_path": "flowrate_export.csv",
  "tube_inner_diameter_mm": 0.6,
  "rotor_rpm_max": 45.0,
  "tube_advance_mm_per_rev": 50.0,
  "linear_transition_between_steps": false,
  "cycle_program": [
    { "duration_s": 2.0, "speed_ml_min": 0.01 },
    { "duration_s": 4.0, "speed_ml_min": 0.1 }
  ],
  "calibrated_max_flow_ml_min": 0.15
}
```

## Troubleshooting

- **Cannot connect to serial port**
  - Verify port path and cable
  - Check serial permissions on OS
  - Ensure no other process is holding the same port

- **Recipe fails to load**
  - Validate JSON syntax
  - Ensure required fields are present and non-empty
  - Ensure `schema_version` is `1`

- **Headless run exits early**
  - Set `recipe_kind` to `"executable"`
  - Ensure `cycles > 0`
  - Ensure `cycle_program` has at least one step

- **No CSV/SVG output in headless mode**
  - Enable measurement (`measurement_enabled` or `ui.auto_measure_on_start`)
  - Ensure output paths and parent directories exist
