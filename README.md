# ESP32-S3-ETH OPC UA Gateway (Rust)

`no_std`-free (std, ESP-IDF-based) Rust firmware targeting the [Waveshare ESP32-S3-ETH](hardware/README.md)
board — an ESP32-S3 (Xtensa LX7) with an on-board W5500 wired Ethernet chip,
PoE header, TF card slot, and camera interface. See [`hardware/README.md`](hardware/README.md)
for the full component list, pinout, and board dimensions.

The firmware now performs **AWS IoT zero-touch provisioning**: on first boot it
obtains a unique X.509 identity via AWS IoT **Fleet Provisioning by Claim**,
stores it in NVS, and then connects to IoT Core with its own certificate to
publish telemetry. Networking tries **wired Ethernet (W5500 + DHCP)** first and
falls back to **WiFi** (`cfg.toml`) if there's no link/lease. See
[`docs/PROVISIONING.md`](docs/PROVISIONING.md) for the full flow.

> **Status: verified end-to-end on hardware** (ESP32-S3-ETH, 2026-07-16). Over
> WiFi fallback the device provisioned itself (thing `28848553144F` created,
> unique cert attached, DynamoDB `provisioned=true`), streamed telemetry to
> CloudWatch, and on reboot reused its NVS identity (skipped re-provisioning).

## Monorepo layout

```
.
├── src/                     # Rust firmware (this crate)
│   ├── main.rs              # boot: eth up -> provision-or-load -> telemetry
│   ├── eth.rs               # W5500 via ESP-IDF esp_eth (lwIP netif + DHCP)
│   ├── wifi.rs              # WiFi fallback (cfg.toml, toml-cfg)
│   ├── device_id.rs         # embedded claim certs + NVS device identity + MAC
│   ├── provisioning.rs      # Fleet Provisioning by Claim client
│   ├── telemetry.rs         # normal-operation MQTT publish loop
│   ├── mqtt_util.rs         # mutual-TLS MQTT client wrapper
│   └── config.rs            # PoC config (endpoint, template, secret)
├── cfg.toml.example         # WiFi creds template -> copy to cfg.toml (gitignored)
├── certs/                   # claim cert + root CA (build-embedded; see certs/README.md)
├── terraform/               # ALL AWS infra (serverless, on-demand)
│   ├── iot.tf               # IoT Core: template, policies, claim cert, rule
│   ├── dynamodb.tf          # device registry (MAC + secret)
│   ├── lambda.tf + lambda/  # pre-provisioning hook (validates MAC+secret)
│   └── ...
├── scripts/seed_device.py   # register a device (MAC+secret) in DynamoDB
└── docs/                    # AWS access + provisioning guides
```

## AWS zero-touch provisioning — quick start

1. **Deploy AWS infra** — see [`docs/AWS_ACCESS_SETUP.md`](docs/AWS_ACCESS_SETUP.md):
   ```sh
   cd terraform && terraform init && terraform apply
   ```
2. **Embed the claim identity** into the firmware:
   ```sh
   terraform output -raw claim_certificate_pem > ../certs/claim.crt.pem
   terraform output -raw claim_private_key     > ../certs/claim.private.key
   ```
3. **Set** `src/config.rs` → `MQTT_ENDPOINT` (from `terraform output iot_endpoint`),
   `PROVISIONING_TEMPLATE`, and `DEVICE_SECRET`. For WiFi fallback, copy
   `cfg.toml.example` → `cfg.toml` and fill in your SSID/password.
4. **Register the device** in DynamoDB (MAC is logged on first boot):
   ```sh
   python3 scripts/seed_device.py --mac AA:BB:CC:DD:EE:FF --secret change-me-shared-secret
   ```
5. **Build & flash** (below). On first boot the device provisions itself; on
   later boots it reuses the NVS-stored identity.

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

With an Ethernet cable connected, expect the serial log to show the W5500
coming up and a DHCP lease (`Ethernet ready. IP: ...`), then — on first boot —
the provisioning flow (`CreateKeysAndCertificate` → `RegisterThing` →
`Provisioning APPROVED. thingName=...`), and finally a message indicating
it connected and keeps the connection open. On subsequent boots it logs
`Registered device identity found; skipping provisioning.` and goes straight to
the listening loop. See [`docs/PROVISIONING.md`](docs/PROVISIONING.md) for troubleshooting.
