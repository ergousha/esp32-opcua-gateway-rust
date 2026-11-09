# Prompt: run the on-device OPC UA scenario on a Windows PC

Paste everything below the line into an AI coding agent (for example Claude
Code) running on a Windows PC that is on the same LAN as the gateway. Fill in
the two values marked `<…>` or let the agent ask for them.

---

You are running the hardware-in-the-loop (HIL) test of an ESP32-S3 OPC UA
gateway from this Windows PC. The goal is a complete, honest report of the
on-device scenario — not a green run at any cost.

## Context

- Repository: `https://github.com/ergousha/esp32-opcua-gateway-rust`, branch
  `feature/opcua-client` (PR #10). Read `README.md` (Architecture) and
  `docs/OPCUA_INTEGRATION_TEST.md` — §3, §6.2, §7.2, §9 and §10 — before
  running anything.
- The runner is the Rust crate `gateway-hil`. It starts an OPC UA test server
  inside its own process, configures the device through AWS IoT (shadow +
  retained tag bundle), and asserts on the device's shadow, on telemetry in
  CloudWatch, and on the device's serial log. Ten phases: `preflight`,
  `provision`, `telemetry`, `reconfig`, `ns_uri`, `reject_security`,
  `reject_digest`, `server_down`, `disable`, `reboot`.
- The device: thing `28848553144F`, on WiFi at `192.168.50.75`
  (subnet `192.168.50.0/24`). It already runs the current firmware, including
  the stack-overflow fix in `gateway_opcua::session::open`, so do **not**
  build or flash firmware unless told to.
- Why this PC: the device must open a TCP connection *to* this PC on port
  4855. The developer's Mac is MDM-managed and its firewall blocks that.
  The phases that need no such connection were already run there
  (`--offline`); your job is the full online scenario.

## Constraints

- Do not change assertions, thresholds or timeouts to make a phase pass. A
  failing check is a finding; report it with evidence.
- Do not commit or push unless the user asks. Do not print, log or commit AWS
  secrets.
- Only touch AWS through `gateway-hil` itself. It writes the `opcua` shadow of
  thing `28848553144F` and retained topics `cmd/28848553144F/opcua/tags/v*`;
  `--cleanup` removes the retained bundles it published. Nothing else.
- Natively on Windows is the simplest setup. WSL2 also works, but its NAT
  hides the listening port from the LAN (§3.5), so the port and the USB
  device have to be forwarded into WSL; see "Running from WSL2" below.

## Steps

1. **Tooling.** Install what is missing, and verify each one:
   - Git, and Rust through rustup with the stable MSVC toolchain, at least
     1.94.1: the AWS SDK crates refuse older compilers
     (`rustup update stable`).
   - Visual Studio 2022 Build Tools with the "Desktop development with C++"
     workload. The AWS SDK's TLS stack (`aws-lc-sys`, `ring`) compiles C
     code. If `aws-lc-sys` then asks for NASM or CMake, install them
     (`winget install NASM.NASM Kitware.CMake`) or set
     `AWS_LC_SYS_PREBUILT_NASM=1`.
   - `cargo install espflash --locked`.
2. **Clone** the repository and check out `feature/opcua-client`. In every
   shell you use, run `$env:RUSTUP_TOOLCHAIN = "stable"`. The repo's
   `rust-toolchain.toml` pins Espressif's `esp` toolchain, which only the
   firmware needs. Every host command needs `--target host-tuple`, because
   `.cargo/config.toml` defaults builds to xtensa.
3. **Sanity check on this machine, no device needed:**
   `cargo test -p gateway-opcua --target host-tuple`. Expect 19 passed in
   about 20 s. If this fails, stop and report: the OPC UA client itself is
   broken here, and the device run cannot be interpreted.
4. **AWS.** Set `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and
   `AWS_DEFAULT_REGION=eu-central-1` for the session (ask the user for
   them), or use a configured profile. The account is `637423178579`. Get the
   IoT data endpoint with
   `aws ssm get-parameter --name /esp32-ztp/poc/iot_endpoint --query Parameter.Value --output text`,
   or ask the user. It looks like `xxxx-ats.iot.eu-central-1.amazonaws.com`.
   Pass it as `--iot-endpoint`, since there is no `cfg.toml` on this machine.
5. **Network.**
   - Find this PC's IPv4 address on `192.168.50.0/24` (`ipconfig`). The
     connection profile must be *Private* (`Get-NetConnectionProfile`).
   - From an **elevated** PowerShell, allow the port:
     `New-NetFirewallRule -DisplayName "OPC UA test 4855" -Direction Inbound -Protocol TCP -LocalPort 4855 -Action Allow -Profile Private`.
     If policy forbids the rule, stop and report: the run cannot work.
   - Prove the path before involving the device. Start
     `cargo run -p opcua-test-server --target host-tuple -- --bind 0.0.0.0 --host <PC-IP>`,
     then ask the user to run `nc -z -w 3 <PC-IP> 4855 && echo reachable` from
     another machine on the LAN (the Mac). A check from this PC itself proves
     nothing: loopback bypasses the firewall. Type `quit` to stop the server.
6. **Serial.** Have the user move the device's USB cable to this PC. Find its
   COM port (`[System.IO.Ports.SerialPort]::GetPortNames()`, or Device
   Manager → Ports). Make sure no other program holds the port open.
7. **Run the full scenario. It takes about 5 minutes, a few more if the
   device's WiFi needs a restart at boot; do not interrupt it:**

   ```powershell
   cargo run -p gateway-hil --target host-tuple -- `
       --thing 28848553144F --server-host <PC-IP> --port <COMx> `
       --iot-endpoint <endpoint> --cleanup
   ```

   If Windows asks whether to allow `gateway-hil.exe` on the network, allow
   it on Private networks. Artifacts (serial log, `summary-*.json`) land in
   `gateway-hil\artifacts\`.
8. **Before calling anything a firmware bug**, apply §9.2:
   - a `reported: (stale …)` line means the device is not reporting at all;
   - identical snapshots with the same `uptime_s` across phases mean a
     crash loop — check `Select-String "memory allocation of" <serial log>`;
   - every phase also checks "device did not crash during the phase", which
     fails on `***ERROR***`, `Guru Meditation`, `abort() was called` or
     `Rebooting...` in that phase's serial log. A crash is a finding even when
     the phase's other checks pass: a reboot restores the NVS-cached config
     and can make a phase look green;
   - `WiFi start failed … restarting in 10 s` right after the monitor attach
     is the firmware recovering from a slow access point, not a failure, as
     long as a later boot logs `WiFi ready`.

   If `provision` never reaches `running` and the serial log shows connection
   timeouts to `<PC-IP>:4855`, the network path is the problem (step 5), not
   the firmware.

## What to expect

- The last hardware run (2026-09-27, from WSL2, current firmware) scored
  65/66, with no crashes. Every phase passed except the `provision` heap
  assertion. Say explicitly whether that still holds.
- Two earlier results were false passes and are now caught:
  - `reject_digest` matched the "Basic256Sha256" error left over from
    `reject_security`; it now requires `bundle sha256`;
  - `ns_uri` passed after a stack-overflow reboot.
- Still open, so report the number rather than judging it: the `provision`
  heap assertion `free_heap > 40 KB` (§9.4). The last run measured 39,832 B;
  earlier runs measured 40,192–40,564 B.
- The OPC UA thread's stack is 40 KiB with little margin. The firmware logs
  `OPC UA stack headroom: N of 40960 B never used` at each new low; the last
  run's lowest was 3,372 B. A 48 KiB stack crashed on every connect, for
  reasons not yet understood, so do not "fix" a low reading by raising it.
- Attaching the serial monitor reboots the device (§8.20). That is intended:
  every run starts from a clean boot. Do not "fix" it with `--no-reset`,
  which leaves the chip in its bootloader.
- The offline run on the Mac already verified, on this device and firmware:
  - the two-plane configuration and its NVS caching;
  - both refusal paths;
  - error reporting with bounded connect attempts and backoff;
  - disable while the server is unreachable;
  - the next config being taken up after a disable;
  - boot from the NVS cache.

## Report back

1. The summary table (passed/total per phase) and every failed check with its
   detail line.
2. For each failure: the relevant serial-log excerpt with timestamps, your
   diagnosis, and whether it is firmware, harness, network or environment.
3. The measured `free_heap`, the reconnect delays seen in `server_down`, the
   lowest `OPC UA stack headroom` value in the serial log, and the boot line
   `OPC UA stack: … at …` (§9.10 of `docs/OPCUA_INTEGRATION_TEST.md`).
4. Paths of the artifacts, and anything you had to install or change on this
   PC.

If a firmware change looks necessary, do not flash anything. Propose it, with
a loopback test that reproduces the problem (`gateway-opcua/tests/`) where the
problem is in the OPC UA client. Building and flashing firmware needs the ESP
toolchain; the Mac has it, and so does the WSL2 setup below.

A stack problem in the OPC UA client reproduces over loopback. Run
`OPCUA_TEST_STACK_BYTES=<bytes> cargo test --release -p gateway-opcua --target host-tuple --test scenario`
at decreasing sizes; a run that overflows aborts with `thread 'opcua' has
overflowed its stack`. The host needs more stack than the device, so compare
sizes against each other, not against the device's 40 KiB.

## Running from WSL2

The 2026-09-27 runs used this setup; it replaces steps 5–7 above.

- **Port.** From an elevated PowerShell, forward 4855 to WSL and allow it:
  `netsh interface portproxy add v4tov4 listenaddress=0.0.0.0 listenport=4855 connectaddress=<WSL-IP> connectport=4855`,
  plus the firewall rule from step 5. `<WSL-IP>` is the `eth0` address from
  `ip -4 addr show eth0` inside WSL. It can change when WSL restarts; update
  the proxy if it does.
- **Serial.** With [usbipd-win](https://github.com/dorssel/usbipd-win), run
  `usbipd list` to find the ESP32-S3 (`303a:1001`). Run
  `usbipd bind --busid <id>` once, elevated. Then keep
  `usbipd attach --wsl --busid <id> --auto-attach` running in a Windows
  terminal, so the device re-attaches after each reset. It shows up as
  `/dev/ttyACM0`, and your user must be in `plugdev` or `dialout`.
- **Run.** Pass `--server-host` as the Windows PC's LAN address, not the WSL
  address. The device connects to Windows, and the proxy forwards to WSL:

  ```sh
  RUSTUP_TOOLCHAIN=stable cargo run -p gateway-hil --target host-tuple -- \
      --thing 28848553144F --server-host <PC-IP> --port /dev/ttyACM0 \
      --iot-endpoint <endpoint> --cleanup
  ```

- **Firmware**, only when told to. Put `~/.cargo/bin` on `PATH` (for
  `ldproxy`), create `cfg.toml` from `cfg.toml.example` with the WiFi
  credentials, and run `cargo +esp build --release`. The first build
  downloads ESP-IDF into `.embuild/`. Flash with
  `espflash flash --port /dev/ttyACM0 --partition-table partitions.csv --target-app-partition ota_0 target/xtensa-esp32s3-espidf/release/esp32-opcua-gateway`.
  To decode backtraces, pass that ELF to `gateway-hil` as `--elf`.
