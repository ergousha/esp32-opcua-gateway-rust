# ESP32-S3-ETH OPC UA Gateway (Rust)

`no_std`-free (std, ESP-IDF-based) Rust firmware targeting the [Waveshare ESP32-S3-ETH](hardware/README.md)
board — an ESP32-S3 (Xtensa LX7) with an on-board W5500 wired Ethernet chip,
PoE header, TF card slot, and camera interface. See [`hardware/README.md`](hardware/README.md)
for the full component list, pinout, and board dimensions.

This bring-up brings the W5500 up over SPI, confirms communication via its
VERSIONR register, and reports Ethernet PHY link status — the foundation for
the OPC UA gateway to come.

## Hardware

Only the Ethernet chip is wired up so far. Pin assignments (from
[`hardware/pins.png`](hardware/pins.png)):

| Function | GPIO |
| -------- | ---- |
| W5500 MOSI | 11 |
| W5500 MISO | 12 |
| W5500 SCLK | 13 |
| W5500 CS   | 14 |
| W5500 RST  | 9  |
| W5500 INT  | 10 (reserved, not yet used) |

Not yet used by this firmware, but present on the board: TF card (SPI:
MOSI=6, MISO=5, CLK=7, CS=4), PoE module header, and the OV2640/OV5640
camera interface.

## Toolchain setup

This targets the ESP32-S3 (Xtensa LX7), which needs Espressif's Rust fork
instead of upstream `rustc`, plus the native ESP-IDF build tools (`std`
firmware links against ESP-IDF, unlike a bare-metal `no_std` build):

```sh
cargo install espup --locked
espup install --targets esp32s3
. ~/export-esp.sh   # run in every new shell before building

cargo install ldproxy --locked   # linker shim required by .cargo/config.toml
```

The first build downloads and builds the ESP-IDF SDK version pinned in
[`.cargo/config.toml`](.cargo/config.toml) (`ESP_IDF_VERSION`) via `embuild`
— this needs `python3`, `git`, `cmake`, and `ninja` on `PATH`, and takes a
while the first time; it's cached under `.embuild/` afterward.

## Build & flash

```sh
cargo build --release             # compile only, no device needed
cargo install espflash --locked   # one-time
cargo run --release               # builds, flashes, and opens the serial monitor
```

[`.cargo/config.toml`](.cargo/config.toml) sets `espflash flash --monitor` as the
runner, so `cargo run` builds, flashes, and attaches the monitor in one step. If
there's more than one serial device attached, `espflash` will prompt you to pick
a port; to target one directly instead:

```sh
espflash flash --port /dev/cu.usbmodem<n> target/xtensa-esp32s3-none-elf/release/esp32-opcua-gateway
espflash monitor --port /dev/cu.usbmodem<n>   # optional: view logs over serial
```

To find the port yourself:

- **macOS**: the ESP32-S3's native USB shows up as `/dev/cu.usbmodem*` (no
  CH340/CP210x driver needed — this board uses the chip's built-in USB-Serial-JTAG,
  not an external USB-UART bridge). `ls /dev/cu.*` before and after plugging in;
  the new entry is the port.
- **Linux**: typically `/dev/ttyACM0` for the same reason; `dmesg | tail` after
  plugging in confirms it.

`espflash monitor` opens an interactive session (`Ctrl+R` reset, `Ctrl+C` exit)
and needs a real terminal with a TTY attached — it fails with "Failed to
initialize input reader" if run from a script or a non-interactive shell.

Expect `W5500 VERSIONR = 0x04` followed by periodic `Ethernet link: up/down`
lines once an Ethernet cable is connected.
