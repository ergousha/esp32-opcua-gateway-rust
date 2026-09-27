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
  (subnet `192.168.50.0/24`). It already runs the current firmware, flashed
  from a Mac, so do **not** build or flash firmware unless told to.
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
- Run natively on Windows, not in WSL: WSL2's NAT hides the listening port
  from the LAN (§3.5).

## Steps

1. **Tooling.** Install what is missing, and verify each one:
   - Git, and Rust through rustup with the stable MSVC toolchain.
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
7. **Run the full scenario. It takes 20–30 minutes; do not interrupt it:**

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
     crash loop — check `Select-String "memory allocation of" <serial log>`.

   If `provision` never reaches `running` and the serial log shows connection
   timeouts to `<PC-IP>:4855`, the network path is the problem (step 5), not
   the firmware.

## What to expect

- The last hardware run, with a retired Python harness, scored 48/56. Since
  then the driver was fixed for §9.3, §9.6 and §9.7, and the harness for
  §9.8. `disable`, `reboot` and `server_down` are therefore expected to pass
  now, as is the telemetry version check. Say explicitly whether they did.
- Still open, so report the number rather than judging it: the `provision`
  heap assertion `free_heap > 40 KB` (§9.4).
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
3. The measured `free_heap`, and the reconnect delays seen in `server_down`.
4. Paths of the artifacts, and anything you had to install or change on this
   PC.

If a firmware change looks necessary, do not flash anything. Propose it, with
a loopback test that reproduces the problem (`gateway-opcua/tests/`) where the
problem is in the OPC UA client. Building and flashing firmware needs the ESP
toolchain, and the Mac has it.
