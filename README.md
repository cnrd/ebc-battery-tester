# EBC Battery Tester

Open-source control software for the ZKETECH EBC-A20 battery tester. It can run
as a persistent server with a browser UI, as a native desktop application, or
directly in a WebUSB-capable browser.

![Application](images/app.png)

The project is written in Rust with [egui](https://github.com/emilk/egui) and
[eframe](https://github.com/emilk/egui/tree/master/crates/eframe). Native builds
are available on the [releases page](https://github.com/Kazhuu/ebc-battery-tester/releases).

## Modes

### Remote server (recommended)

The server owns the serial connection, persists measurements, serves the WASM
UI, and exposes its API on the same origin. This is the best mode for an
unattended test: closing or reloading a browser does not stop the test. Reopen
the page to reconnect to the running server.

Because the browser does not access USB in this mode, the UI works in current
Safari on iPhone, iPad, and desktop, Firefox, Chrome, Edge, and other modern
browsers. The Docker build compiles this mode as its default. HTTP, WebSocket,
and static files normally use the same origin; reverse-proxy deployments may
set `EBC_ALLOWED_ORIGIN` to one exact external origin.

### Native desktop

The default Cargo feature builds the desktop GUI. It talks to the operating
system serial port directly and is suitable when the computer remains attached.
Closing the application stops and disconnects the test during orderly shutdown.

### Direct WebUSB

Standalone Trunk builds, GitHub Pages, CI artifacts, and release archives
default to WebUSB. Add `?transport=remote` to use a same-origin server instead.
The Docker image defaults to remote mode. `?transport=webusb` and
`?transport=remote` always override the build default explicitly; a hash
override is also accepted, which is useful for an installed PWA launch.

Direct mode requires a secure context and WebUSB, currently provided by
Chromium-based desktop browsers such as Chrome and Edge. Firefox and Safari,
including iOS/iPadOS Safari, do not support direct WebUSB. The browser owns the
device in this mode, so do not close the page during a test.

## Architecture

```mermaid
flowchart LR
    Browser[Browser / installed PWA] -->|same-origin HTTP + WebSocket| Server[ebc-server]
    Server -->|serial, 9600 baud| CH340[CH340 USB cable]
    CH340 --> EBC[EBC-A20]
    Server --> Data[/data/session.json + samples.csv + runs/]
    Desktop[Native desktop GUI] -->|serial directly| CH340
    WebUSB[Chromium direct WebUSB mode] -->|USB directly| CH340
```

Only one process or browser may own the device at a time. The supplied cable
contains a CH340 adapter; a plain mini-USB cable does not provide the serial
interface.

HTTP handlers and serial polling do not share an async-runtime worker: all
device lifecycle, protocol I/O, timing, and persistence are serialized by one
dedicated `ebc-device-actor` operating-system thread. HTTP requests communicate
with that single owner through messages.

## Docker

Build and run the production image:

```bash
docker build -t ebc-battery-tester .
docker run -d --name ebc-battery-tester \
  --restart unless-stopped \
  --device /dev/ttyUSB0:/dev/ttyUSB0 \
  --group-add "$(stat -c '%g' /dev/ttyUSB0)" \
  -p 8080:8080 \
  -v /srv/ebc-battery-tester:/data \
  ebc-battery-tester
```

Open `http://SERVER:8080/`. The image compiles the server without GUI/X11
dependencies and includes the Trunk-built WASM assets, CA certificates, and the
runtime `libudev` library. It does not contain a compiler, browser, VNC, or
desktop stack. Its health check calls the existing `/api/status` endpoint using
the server binary itself.

### Docker Compose and Unraid

`docker-compose.yml` defaults to Unraid's conventional UID/GID and appdata path:

```bash
SERIAL_GID="$(stat -c '%g' /dev/ttyUSB0)" docker compose up -d --build
```

The defaults are `PUID=99`, `PGID=100`, port `8080`, device `/dev/ttyUSB0`, and
host data path `/mnt/cache/appdata/ebc-battery-tester`. Override them as needed:

```bash
PUID=1000 PGID=1000 SERIAL_GID=20 EBC_DEVICE=/dev/ttyUSB1 \
EBC_PORT=8081 EBC_DATA_PATH=/mnt/user/appdata/ebc-battery-tester \
docker compose up -d --build
```

`SERIAL_GID` must be the numeric group owner of the host serial device. Obtain
it with `stat -c '%g' /dev/ttyUSB0` (or `ls -ln /dev/ttyUSB0`). Compose runs with
that supplementary group and never uses `privileged: true`.

In server/native serial mode, leave the Linux `ch341` driver attached. Do not
install an unbind udev rule: the server needs the resulting `/dev/ttyUSB*`
device. On Windows, use the normal CH340 serial driver for the native app.

## Configuration

The server accepts these environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `EBC_HTTP_ADDR` | `0.0.0.0:8080` | HTTP listen address |
| `EBC_SERIAL_PORT` | `/dev/ttyUSB0` | Serial device path |
| `EBC_DATA_DIR` | `/data` | Persistent state directory |
| `EBC_STATIC_DIR` | `dist` | Trunk static asset directory |
| `EBC_MOCK` | `false` | Use the simulated device |
| `EBC_ALLOWED_ORIGIN` | unset | Exact browser `Origin` accepted behind a reverse proxy |
| `RUST_LOG` | `info` in Docker | Rust log filter |

Persist `/data`. `session.json` stores the current device/test metadata and
`samples.csv` stores current-run measurements. Before a fresh test replaces a
meaningful current run, the server archives its metadata and CSV under
`/data/runs/<run-id>.json` and `/data/runs/<run-id>.csv`. Run IDs are sanitized
UTC start timestamps with collision suffixes. `GET /api/runs` lists archived
runs and `GET /api/runs/<run-id>.csv` downloads one archive. Current history
remains available from `GET /api/history.csv`.

Cycle executions also retain an independent continuous telemetry stream under
`/data/cycles/<execution-id>.csv`. It includes normal mode reports from device
steps, settling, rests, and repeat boundaries without changing the per-run
sample files or metrics. `GET /api/cycle/history.csv` exports the latest cycle,
and `GET /api/cycles/<execution-id>/history.csv` exports a specific execution.

The durable CSV retains the complete current run. Initial browser snapshots
are limited to 5000 presentation samples, and the browser remains bounded to
5000 points for the lifetime of the page. Incremental deterministic compaction
keeps the first and latest samples plus voltage/current extrema from time
buckets. This affects only browser memory and plotting; raw current and archived
CSV downloads remain complete. Export requests flush and sync the current CSV,
capture its durable byte length, and then stream only that prefix from an
independent file handle. A concurrent sample append therefore cannot extend or
corrupt an in-progress download, and a slow download does not block device
telemetry or commands.

Samples and test status include cumulative `energy_wh`. The server integrates
measured voltage and current with the trapezoidal rule over backend elapsed
time, but never integrates across disconnect, restart, or uncertain-ownership
gaps. The device's two-byte base-240 capacity counter is a raw `u16` value with
a 57,600 mAh modulus. During a confirmed backend-owned run, the server
normalizes plausible high-to-low wraps into a cumulative `u64 capacity_mah` and
ignores stale regressions; it does not infer a wrap across a recovery gap.
`DeviceState.capacity_mah` remains the latest raw hardware counter for
diagnostics, while samples, test status, run summaries, browser live capacity,
and CSV use the normalized cumulative value (divide by 1000 for Ah). Existing
CSV and JSON data without energy fields loads with zero energy.

After a server restart or any serial observation gap, saved history and
configuration are retained but ownership of a pending or running test is
invalidated. The test is marked `recovered_uncertain`, device activity is
unknown, and its clock is stopped. The server never automatically starts or
blindly resumes it. An active hardware report keeps the run uncertain and does
not advance backend elapsed time or energy because ownership of that interval
cannot be proven. An inactive report resolves the run to `stopped`; the user can
then start a fresh run or explicitly resume a stopped run. While recovery
remains uncertain, Start, Resume, Adjust, and Calibration are rejected; explicit
Stop and Disconnect remain available. Start and Stop are exposed as `starting`
and `stopping` until hardware reports confirm their result. Because the protocol
has no command acknowledgement, inactive reports received while Starting are
treated as potentially buffered pre-command telemetry; only an active report
confirms Start or Resume. The state remains pending until that confirmation, an
explicit Stop, or a connection gap. Normal mode reports preserve the device's
Idle and Finished states: Idle resolves an owned run to `stopped`, while Finished
resolves it to `completed`. Firmware-inactive reports do not contain that
distinction, so the server waits for the interleaved normal mode report when an
owned run ends. Disconnects,
serial errors, and uncertain recovery break the trapezoidal energy accumulator,
so neither elapsed time nor energy is invented across an observation gap. A
confirmed backend-owned start/resume starts a fresh clock at zero or resumes
from the preserved elapsed value, respectively.

Start and Calibration require telemetry from the current serial connection;
persisted or pre-disconnect voltage/activity values are never accepted as proof
that hardware is ready. All four calibration references must be staged on the
same uninterrupted connection before Confirm is accepted.

Metadata replacement and run archival use write, sync, rename, and directory
sync. Samples are append-only and flushed with `sync_data` at least once per
second while reports arrive, and are flushed on orderly stop/shutdown. On load,
one torn final CSV row is truncated; corruption in a complete row is rejected.
SIGTERM/SIGINT performs graceful actor shutdown and flushing. SIGKILL or power
loss cannot run cleanup and may lose the not-yet-synced tail (normally up to the
one-second sync interval), but previously synced rows and atomically replaced
metadata remain recoverable.

## Install on iPhone or iPad

Use the remote server URL in Safari, tap **Share**, then **Add to Home Screen**.
The installed PWA reconnects to the same server. iOS does not support direct
WebUSB, so the server and remote-default Docker UI are required. On narrow
screens the toolbar and controls reflow, Start or the active primary Stop action
appears before the graph, test settings follow it, and direct-USB selection and
connection actions use separate touch-friendly rows.

## Direct WebUSB setup

These driver changes apply only to direct WebUSB mode. Do not apply them to the
Docker server or native desktop mode.

### Windows

Use [Zadig](https://zadig.akeo.ie/) to replace the `USB Serial` device driver
with WinUSB: enable **Options > List All Devices**, select `USB Serial`, choose
WinUSB, and click **Replace Driver**. This removes the COM port until the CH340
driver is restored.

![Zadig Windows](images/zadig-windows.png)

### Linux

The kernel `ch341` driver normally claims the interface. For a one-time direct
WebUSB session, identify the USB interface and unbind it, for example:

```bash
sudo sh -c 'echo "1-2.3:1.0" > /sys/bus/usb/drivers/ch341/unbind'
```

Grant the logged-in user access to USB vendor `1a86`, product `7523`, with an
appropriate udev rule. Avoid a permanent automatic unbind rule unless this host
is dedicated to WebUSB, because unbinding removes `/dev/ttyUSB0` from native and
server use.

## Mock mode

Run the complete container stack without hardware:

```bash
EBC_MOCK=true docker compose up --build
```

Or run it from source after building the web UI:

```bash
EBC_WASM_DEFAULT_TRANSPORT=remote trunk build
EBC_MOCK=true EBC_DATA_DIR=./data EBC_STATIC_DIR=./dist \
  cargo run --no-default-features --features server --bin ebc-server
```

Connect the simulated device in the browser, configure a test, and start it to
produce one sample per second.

## Build from source

Rust 1.92 or newer is required. Linux builds also need the development package
for `libudev`; desktop builds need the normal eframe/X11 or Wayland development
libraries.

```bash
# Native desktop GUI
cargo run

# WASM UI (install wasm32-unknown-unknown and Trunk first)
rustup target add wasm32-unknown-unknown
cargo install --locked trunk --version 0.21.14
trunk serve

# Production headless server binary, with no GUI feature
cargo build --release --no-default-features --features server --bin ebc-server

# All formatting, native/server/WASM checks, tests, Clippy, and Trunk build
./check.sh
```

Use `http://127.0.0.1:8080/#dev` during Trunk development. Development mode
unregisters only this app's service worker and clears only caches prefixed
`ebc-battery-tester-`. The production service worker is network-first, never
caches `/api`, derives a unique cache generation from each Trunk-generated page,
atomically populates a fresh generation, and removes only this app's older
caches after successful installation. A failed installation leaves the active
old cache in place, and no generated JS/WASM hash is hardcoded.

The project contains unit tests for validation, recovery and command races,
run/sequence client deduplication and bounds, transport selection, mobile layout
ordering, exports, and server persistence. Docker CI also runs the final image
as a non-root UID/GID with mock hardware and a persistent volume, checks health
and static hashed JS/WASM assets, exercises status/start/stop/CSV, restarts the
container, and verifies persisted API and static content. Automated tests do
not replace a real-device check.

## Real hardware checklist

1. Verify the CH340 cable appears as `/dev/ttyUSB0` and note its numeric GID.
2. Confirm no other native app, container, or WebUSB tab owns the interface.
3. Start with the tester idle and no battery/load at unsafe limits.
4. Connect and verify model, firmware, voltage, and current readings.
5. Run a short low-current discharge and confirm live samples and stop behavior.
6. Download `/api/history.csv` and verify `/data/samples.csv` after restart.
7. Start another short test, close the remote browser, reopen it, and verify the test continued.
8. Restart the server during a controlled test and verify it reports recovery as uncertain without auto-starting.
9. Interrupt and restore serial communication during Starting, Running, and Stopping; verify an active report remains uncertain and an inactive report resolves to stopped.
10. During a controlled multi-minute run, compare the tester's display and cutoff behavior with and without `0x0A` timer sync before relying on it operationally.
11. Exercise charge, constant-power, adjustment, resume, and calibration only with appropriate instrumentation and safe limits.

The available protocol documentation is ambiguous about serial parity. This
project preserves the known-working odd-parity implementation; deployment work
does not change protocol behavior.

## API outline

All endpoints are under `/api`:

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/status` | Current authoritative snapshot |
| `GET` | `/api/history` | Measurement history as JSON |
| `GET` | `/api/history.csv` | Measurement history as CSV |
| `GET` | `/api/runs` | Archived run summaries |
| `GET` | `/api/runs/{id}.csv` | Archived run CSV download |
| `GET` | `/api/cycle/history.csv` | Current/latest full-resolution cycle telemetry |
| `GET` | `/api/cycles/{id}/history.csv` | Cycle telemetry by execution ID |
| `GET` | `/api/ws` | Snapshot/sample/cycle-sample WebSocket stream |
| `POST` | `/api/connect`, `/api/disconnect` | Serial connection control |
| `POST` | `/api/test/start`, `/api/test/adjust` | JSON test configuration |
| `POST` | `/api/test/stop`, `/api/test/resume` | Test lifecycle |
| `POST` | `/api/cycle/start`, `/api/cycle/stop` | Cycle lifecycle |
| `POST` | `/api/calibration` | JSON calibration command |

Every mutating `POST` requires `X-EBC-Command: 1`; the remote UI sends it and API
errors are displayed in the connection panel. Browser WebSockets require an
`Origin`. By default an HTTP or HTTPS origin must match `Host`; when TLS terminates or
the public host differs at a reverse proxy, set `EBC_ALLOWED_ORIGIN` to the exact
external value such as `https://battery.example.com`. Forward the original
`Origin`, support WebSocket upgrades, and do not strip `X-EBC-Command`.

The custom header and origin checks reduce accidental cross-site commands but
are not authentication or authorization. Treat the server as a hardware control
endpoint: expose it only on a trusted LAN, or place it behind an HTTPS reverse
proxy with authentication and WebSocket support. Do not publish port 8080
directly to the internet.

## Current limitations

- One device per process.
- No software capacity/percentage completion targets, nested cycle repeats,
  internal-resistance test, plot image export, imported CSV/`.dat` replay,
  firmware update, or support guarantee for models other than EBC-A20.
- Server recovery is conservative and does not automatically restart a test.

## Protocol and firmware research

- [Frame reference](FRAMES.md)
- [Reverse-engineering notes](REVERSE_ENGINEERING.md)
- `extract_firmware_from_exe.py` extracts firmware images from the original
  Windows executable.
- `extract_firmware.py firmware-update.pcap firmware_extracted.bin` reconstructs
  an image from a USB capture.

The firmware files are not distributed by this project. The protocol work began
from the [ZKETECH EBC-A20 reverse-engineering article](https://pop.fsck.pl/hardware/zketech-ebc-a20.html)
and the [WebUsbSerialTerminal CH340 implementation](https://github.com/selevo/WebUsbSerialTerminal/blob/main/serial.js).

This project was featured by
[Hackaday](https://hackaday.com/2026/06/18/battery-tester-gets-an-app-upgrade/).
