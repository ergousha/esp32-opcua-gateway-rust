# ESP32 Firmware Integration & Hardware Reference

This reference guide details the ESP32-S3-ETH firmware architecture, boot sequence, hardware pitfalls resolved, and local testing/provisioning commands.

---

## 1. Device Boot & Connection Logic

The firmware boot process in `src/main.rs` follows this sequence. Everything about identity, the AWS IoT connection and OTA is the platform SDK's (`device-sdk` in [`ergousha/iot-device-sdk`](https://github.com/ergousha/iot-device-sdk)):

1.  **Rollback guard**: `device_sdk::start_rollback_guard()`. On a new OTA image that is not yet marked valid, it restarts the device if the image has not reached AWS IoT within 10 minutes, so the bootloader rolls it back.

2.  **Platform init** (`device_sdk::Platform::init`), before the network:
    *   Counts the boot (NVS `platform`).
    *   On a unit that is not enrolled, generates a P-256 key in NVS `device_id/key` while the radio is still off, so the RNG draws on its radio-off entropy source.
    *   Starts the serial console that `ergo commission` talks to (`PLATFORM INFO`, `SET`, `ENROL`, `ERASE`, `FORGET`).
    *   *Transitional:* `Platform::seed_wifi` copies the fallback Wi-Fi compiled in from `cfg.toml` (`src/config.rs`) into NVS `platform`, if that has none yet. The next build drops this.

3.  **Network Initialization**:
    The device starts by attempting to bring up the physical Ethernet interface (`eth::start(...)`). The W5500 transceiver brings up the link via `esp_eth` and waits up to 10 seconds for a link-up and DHCP lease.
    *   **Fallback**: If no Ethernet link or lease is acquired within 10 seconds, the device falls back to Wi-Fi (`wifi::start(...)`) with the credentials in NVS `platform`, written at the bench by `ergo commission --wifi-ssid … --wifi-psk-env …`. Without them, or if Wi-Fi fails, it restarts after 10 s.
    *   **Resource Management**: The Ethernet driver handle is kept alive even during a Wi-Fi fallback. Dropping it would trigger a spi-driver crash (see *Hardware Pitfalls* below).

4.  **Identity** (`platform.identity()`):
    *   **Identity stored**: the certificate, key and thing name in NVS `device_id`. Units enrolled before the SDK keep the RSA identity they got from `CreateKeysAndCertificate` there, and keep using it.
    *   **Not enrolled**: the device waits until a claim is in NVS `platform` (written by `ergo commission`), then enrols: it connects with the claim, sends a CSR signed with its soft key (`CreateCertificateFromCsr`), and registers the thing (`RegisterThing`); the platform's pre-provisioning hook approves only the registered MAC and public key, once, inside its enrolment window. The identity is saved and the claim erased. A refusal is retried with a backoff from 60 s to 6 h.

5.  **Connection and gateway loop**:
    `platform.connect()` connects as the thing (MQTT input buffer 12 KB, enough for a whole retained tag bundle) and returns the session; `telemetry::run(session)` runs the gateway on it.
    *   **Image validity**: on the first CONNACK the running image marks itself valid. A fresh OTA image that fails before that point restarts instead of stopping, so the bootloader restores the previous image.
    *   **Pending OTA job**: If the previous boot installed an update, its job is reported now, by the image that actually runs (see *OTA Updates and Job Status* below).
    *   **Session**: Jobs and the `platform` shadow are handled inside the session; `Session::poll` hands the gateway only its own messages (the `opcua` shadow and the tag bundle). OPC UA telemetry is published only while the `platform` shadow has `desired.telemetry.enabled = true`.

---

## 2. Hardware Pitfalls & Solutions

During the PoC validation on physical ESP32-S3-ETH hardware, several hardware and runtime issues were encountered and resolved:

| Pitfall / Error | Root Cause | Solution |
| :--- | :--- | :--- |
| **`spi_master: txdata transfer > host maximum`** | The SPI bus DMA was disabled, limiting transfer size to ~64 Bytes. The W5500 chip regularly transmits MTU-sized Ethernet frames (~1.5 KB). | Enabled SPI DMA auto-allocation: `SpiDriverConfig::new().dma(Dma::Auto(4096))` in `eth.rs`. |
| **`spi_bus_free().unwrap() INVALID_STATE`** (Panic when switching from Eth to Wi-Fi) | The firmware was dropping the Ethernet driver handle when no cable was detected. However, the underlying SPI device was still attached to the bus, causing a resource panic. | Do not drop the Ethernet driver handle. Retain it inside the state struct `Net::Wifi { eth, .. }` in `main.rs` to keep the bus registration active. |
| **`memory allocation of ~1GB failed`** (Crash on TLS connection) | Passing 5 distinct `&str` references (certificates/keys) exceeded the Xtensa register-window argument limit, forcing fat-pointer argument corruption on stack boundaries. | Consolidated certificates and credentials into a single `Creds` struct reference (formerly in `mqtt_util.rs`) to fit within parameter register limits. The SDK's MQTT client keeps the pattern: `device_sdk::mqtt::Mqtt::connect` takes one `&Settings`. |
| **General Instability / Stack Overflow** | Processing TLS handshake, MQTT packets, and serde JSON parsing inside the main thread exceeded the default task stack size (8 KB). | Increased the main task stack allocation to 16 KB by setting `CONFIG_ESP_MAIN_TASK_STACK_SIZE=16384` in the SDK config. |

> [!NOTE]
> **MAC Address Extraction**: The unique identifier for each device is its Ethernet MAC address (`ESP_MAC_ETH`; `mac_type = "eth"` in `platform.toml`, frozen). This is derived from the hardware eFuse base MAC (typically base MAC `..:4C` maps to Ethernet MAC `..:4F`), so it differs from the MAC printed on the module. `ergo commission` reads it off the device's console (`PLATFORM REGISTER mac=…`); nothing has to be copied by hand.

---

## 3. Local Testing & Re-provisioning

To enrol a previously enrolled device from scratch, re-commission it from the SDK repository with `--reprovision`: the device forgets its identity, restarts, and enrols again with a new registry record.

```sh
cargo run -p ergo -- commission --product gateway --port <tty> \
    --wifi-ssid <ssid> --wifi-psk-env WIFI_PSK --reprovision
```

### Erase NVS Partition Only
Run the following `espflash` command to wipe the default NVS partition boundaries (`0x9000` to `0xf000` under the standard partition table):
```sh
espflash erase-region 0x9000 0x6000
```
This erases the identity and key, the Wi-Fi, and the record of a pending OTA job (§4). The device then generates a new key and waits for commissioning; its registry record still exists, so commission it with `--reprovision`. An execution left IN_PROGRESS in `phase: rebooting` is still settled from the status details the Jobs service holds for it (§4.2), as long as the device comes back as the same thing.

### Full Device Erase
Alternatively, to clear all flash partitions including the application boot slots:
```sh
espflash erase-flash
```

---

## 4. OTA Updates and Job Status

Firmware updates arrive as AWS IoT Jobs, handled entirely by the platform SDK ([`ergousha/iot-device-sdk`](https://github.com/ergousha/iot-device-sdk)) inside the session; the gateway only restarts when the session asks it to (`Event::Restart` → `device_sdk::restart_into_update()`). The decisions (how a job is read, whether it is admitted, what its outcome is, what is reported) are pure functions in the SDK's `iot-jobs` crate, and the state machine that drives them is `iot-jobs/src/client.rs`, written against narrow traits for MQTT, NVS and the OTA partitions and unit-tested on the host; `device-sdk` plugs in the device (OTA partitions, the job record in NVS). The same code serves the platform's other products.

A job is admitted in this order:

1. A job whose document names another `product` (this one is `gateway`, from `platform.toml`) is `REJECTED` (`reason: product`) without a download; so is any operation other than `firmware_update` (`reason: operation`).
2. A signed manifest, when the job carries one, must verify against the keyring compiled in from `platform.toml` `[signing]` and name this product and the job's version, else `FAILED`, `reason: sig`. (The gateway's keyring is empty for now, so a signed job cannot verify yet.)
3. A job for the running version is `SUCCEEDED` without a download. Otherwise only a **greater** version is installed: a job for a lower one is `SUCCEEDED`, `reason: already-newer`, with nothing installed, unless its document says `downgrade: true` (the owner's rollback).

The image is hashed while it streams into the inactive slot and checked against the manifest, if any (`reason: verify` on a mismatch).

With `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`, a new image that resets before it marks itself valid is rolled back by the bootloader; one that has not reached AWS IoT within 10 minutes is restarted by the rollback guard, with the same result. The image that downloads an update therefore cannot know whether it will work, and does not report SUCCEEDED. It records the job in NVS (namespace `ota_job` in the `nvs` partition), reports IN_PROGRESS, and restarts. Whichever image boots next reports the outcome. The last settled job is also reported in the `platform` shadow (`last_job`).

Failure reasons are the platform contract's: `verify`, `sig`, `version`, `rollback`, `boot`, `download`.

### 4.1 What the execution goes through

| When | Reported by | Status | `statusDetails` |
| :--- | :--- | :--- | :--- |
| Download starts | the running image | `IN_PROGRESS` | `phase: downloading`, `from`, `target` |
| Image written, verified, recorded in NVS, made the boot slot | the running image | `IN_PROGRESS`, `stepTimeoutInMinutes: 30` | `phase: rebooting`, `from`, `target`, `target_slot` |
| After the reboot: the new image runs from `target_slot`, is `target`, and marked itself valid | the **new** image | `SUCCEEDED` | `from`, `running`, `running_slot` |
| After the reboot: the previous image runs | the **previous** image | `FAILED` | `reason: rollback`, `detail`, `running`, `running_slot`, `target`, `target_slot` |
| After the reboot: the new image runs from `target_slot` but is another version | the new image | `FAILED` | `reason: version`, … |
| After the reboot: the new image could not mark itself valid | the new image | `FAILED` | `reason: boot`, `detail` |
| Download, write or activation failed | the running image | `FAILED` | `reason: download`, `detail`, `target` |
| The image does not match its manifest | the running image | `FAILED` | `reason: verify`, `detail`, `target` |
| The signed manifest does not verify or does not bind this product and version | the running image | `FAILED` | `reason: sig`, `detail`, `target` |
| Three downloads of the job started and none finished | the running image | `FAILED` | `reason: download`, `detail`, `target` |
| The job names the version already running | the running image | `SUCCEEDED` | `detail: already running this version; nothing installed`, `running`, `running_slot` |
| The job names a lower version and not `downgrade` | the running image | `SUCCEEDED` | `reason: already-newer`, `target`, `running`, `running_slot` |
| Another product, or an operation other than `firmware_update` | the running image | `REJECTED` | `reason: product` or `operation`, `detail` |
| The firmware will not run the job (no `download_url`, or not `https://`) | the running image | `REJECTED` | `reason: job`, `detail` |

`target` is the job document's `firmware_version` normalised: `firmware_v0.1.0`, `esp32-opcua-gateway-v0.1.0` and `0.1.0` all compare as `0.1.0`, the form the shadow reports as `reported.fw`. A pre-release suffix is kept, so `0.2.0-rc.1` is not `0.2.0`.

The running slot is the primary evidence and the version the secondary: a rebuild shipped under an unchanged version cannot run from a slot it was not written to. `detail` of a rollback says whether the bootloader marked the new slot invalid (`esp_ota_get_last_invalid_partition()`) or the device simply booted the other slot.

### 4.2 Edge cases

* **The same execution is offered again at boot.** The Jobs service offers an execution for as long as it is IN_PROGRESS, and it still is when the device comes back. While a recorded job is being reported, the device takes no other job and asks for none; `notify-next` and `$next/get` answers are ignored until the service has answered the report. Should the record be missing, an execution offered IN_PROGRESS in `phase: rebooting` is reported from its own status details, never downloaded again.
* **Power loss.** The record is written after the image is verified and before its slot becomes the boot slot. A power cut earlier leaves the old image active, and the job is offered again IN_PROGRESS in `phase: downloading`, which is downloaded again. A cut after activation boots the new image with its record. A cut between the two leaves a record for an image that never booted: the device comes back on the image that took the job, from its own slot, with nothing marked invalid. In a real rollback the bootloader always marks the target slot invalid, so this is told apart (`iot_jobs::was_activated` in the SDK): the device drops the record, logs `… was recorded but never activated`, and takes the job up again, still `phase: downloading`. A target slot that an *earlier* failed update left marked invalid makes that case look like a rollback, and it is reported `FAILED`, `rollback`, as before.
* **A download that keeps dying.** A download that crashes the device, or that power keeps being cut during, reports nothing, and the job is offered again at every boot. The device counts the downloads of a job it has started in NVS (key `attempts` next to the record), and at the fourth offer reports `FAILED`, `reason: download` (`3 downloads started and none finished; …`) instead of downloading again. A download that fails without crashing is reported `FAILED`, `reason: download` at once and is not retried.
* **Cancelled during the download.** The download blocks every other message, so a cancellation sent meanwhile cannot be heard while it runs. Once the image is written and verified, and before it is activated, the device asks the Jobs service about the execution (`$aws/things/{thing}/jobs/{jobId}/get`) and waits up to 10 s for the answer. If the execution has ended (the service refuses to describe it with `TerminalStateReached`, whether it was cancelled, timed out or removed) or the job is gone (`ResourceNotFound`), the image is not activated, nothing is reported (there is nothing left to report on), and the device logs `not installing it, because …` and asks for the next job. No answer, or an unclear one, installs it: the check spares a needless update, and must not become a way for one to fail. Messages that arrive while it waits are handled afterwards, not lost.
* **A job the firmware refuses** (no `download_url`, or one that is not `https://`) is reported `REJECTED`, `reason: job`, with the detail. Left alone it would stay `QUEUED` and be offered again at every boot. Like any outcome, it is settled before the next job is taken.
* **The execution already ended.** A job cancelled with `--force`, timed out, or deleted while the device was rebooting makes the report fail with `InvalidStateTransition`, `TerminalStateReached` or `ResourceNotFound`. The device logs it, clears the record and goes back to taking jobs. `RequestThrottled` and `InternalError` are retried. Any other refusal is final too, and the job is ignored until the next boot: an execution the service still holds open is offered again on the next `$next/get`, and reporting on it again would be refused again, in a loop.
* **No answer.** A report the service has not answered is sent again every 30 s, and on every reconnect. The subscriptions to `…/update/accepted` and `…/update/rejected` are renewed with each send.
* **An image that cannot reach AWS IoT.** Returning from `main` would end the main task and park the device until someone reset it: with the image unverified, it was never rolled back (on Ethernet, an image with an empty endpoint used to hang this way); with it valid, the device sat with no MQTT, so no Jobs and no way to deliver a fix. A failure in `main` restarts the device instead: after 2 s while the image is unverified, so the bootloader restores the previous one, and after 30 s once it is valid, to try again without turning a lasting outage into a tight boot loop. The SDK keeps retrying the connection itself, with its backoff, so one TLS or DNS failure on a new image's first boot does not roll back a good update; the rollback guard restarts an unverified image that has not reached AWS IoT within 10 minutes (`device_sdk::VERIFY_DEADLINE`).
* **Neither image comes back.** `stepTimeoutInMinutes: 30` on the `rebooting` report, and the job-wide in-progress timeout the OTA pipeline sets ([iot-platform#3](https://github.com/ergousha/iot-platform/issues/3)), end the execution `TIMED_OUT`. A device that reports after that is told the execution already ended.
* **The version already running, or an older one.** A job for the running version is reported `SUCCEEDED` without a download, which is what a unit flashed over USB with the current release, or one that joins the thing group late, is offered. A job for a lower version is `SUCCEEDED`, `reason: already-newer`, also without a download, unless the job says `downgrade: true`. A rebuild under an unchanged version is therefore not installed; give it a greater version.
* **The download URL** is taken only from the `$next/get` answer, where AWS IoT fills in the presigned URL, and must be `https://`.

### 4.3 Hardware check: a good and a bad update

A manual procedure; it touches real AWS resources and one real device. It needs the AWS CLI with rights to write to the firmware bucket, presign, create and describe jobs, and read the shadow; `jq`; and a serial monitor on the device.

The device must already run a build that contains this logic, flashed over USB (`espflash flash --bootloader target/bootloader.bin --partition-table partitions.csv --target-app-partition ota_0 target/xtensa-esp32s3-espidf/release/esp32-opcua-gateway`): the image it rolls back to is the one that reports the rollback. Each image below gets its own version, or it would be reported as already running. The slot names below assume the USB-flashed image runs from `ota_0`; swap them if the serial log says otherwise.

**The bootloader must be the project's.** `espflash flash` writes espflash's own bundled bootloader unless it is given `--bootloader`, and that one is built without `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`: it ignores the OTA image states, never rolls back, and boots the bad image below again and again ([#22](https://github.com/ergousha/esp32-opcua-gateway-rust/issues/22)). The build copies the bootloader it produced to `target/bootloader.bin`, and every flashing path in the repository passes it: the `cargo run` runner, `gateway-hil --flash`, and [`espflash.toml`](../espflash.toml) for a hand-typed `espflash flash` in the repository root. The boot log cannot confirm it: the USB-Serial-JTAG re-enumerates on every reset, and the monitor misses the bootloader's lines. The HIL `bootloader` phase reads it back from flash instead (`cargo run -p gateway-hil --target host-tuple -- --thing <thing> --server-host <lan-ip> --port <port> --phases bootloader`); it names espflash's as `v5.5.1-838-gd66ebb86d2e`. A unit flashed without it gets the project's with `espflash write-bin --chip esp32s3 0x0 target/bootloader.bin`.

**Setup.**

```sh
THING=<thing name>        # serial log: "starting gateway. thing=..."
BUCKET=<firmware bucket>  # repository variable AWS_FIRMWARE_BUCKET
THING_ARN=$(aws iot describe-thing --thing-name "$THING" --query thingArn --output text)

# Uploads an image and creates a job for this one thing. The key does not end
# in .bin: the bucket notification that starts the fleet-wide OTA Lambda
# filters on that suffix.
make_job() {  # make_job <job id> <image> <firmware_version>
  aws s3 cp "$2" "s3://$BUCKET/manual/$1.img"
  url=$(aws s3 presign "s3://$BUCKET/manual/$1.img" --expires-in 3600)
  aws iot create-job --job-id "$1" --targets "$THING_ARN" --target-selection SNAPSHOT \
    --timeout-config inProgressTimeoutInMinutes=30 \
    --document "$(jq -cn --arg v "$3" --arg u "$url" \
      '{operation: "firmware_update", firmware_version: $v, download_url: $u}')"
}

show_job() {  # show_job <job id>
  aws iot describe-job-execution --job-id "$1" --thing-name "$THING" \
    --query 'execution.{status: status, details: statusDetails.detailsMap}'
}

DATA=https://$(aws iot describe-endpoint --endpoint-type iot:Data-ATS --query endpointAddress --output text)
show_fw() {
  aws iot-data get-thing-shadow --endpoint-url "$DATA" --thing-name "$THING" \
    --shadow-name opcua /dev/stdout | jq -r .state.reported.fw
}

save_image() {  # save_image <file>
  cargo espflash save-image --release --chip esp32s3 --flash-size 16mb \
    --partition-table partitions.csv --target-app-partition ota_0 "$1"
}
```

**Images.** `FW_VERSION` sets the version an image reports; it must be a bare `X.Y.Z`, and since only a greater version is installed, both must be greater than the running one and `BAD` greater than `GOOD` (for a device on `0.5.0`, say `GOOD=0.5.1 BAD=0.5.2`). Build both, then undo the edit (`git checkout -- src/main.rs`).

* `good.bin`: `FW_VERSION=$GOOD save_image good.bin`.
* `bad.bin`: add `anyhow::bail!("deliberately bad image");` as the first line of `run()` in `src/main.rs`, then `FW_VERSION=$BAD save_image bad.bin`.

**Good update.**

```sh
JOB=ota-manual-good-$(date +%s)
make_job "$JOB" good.bin "firmware_v$GOOD"
show_job "$JOB"   # repeat while it runs
```

Expected:

1. `IN_PROGRESS`, `phase: downloading`; then `IN_PROGRESS`, `phase: rebooting`, `target: $GOOD`, `target_slot: ota_1`. The serial log shows `recorded OTA job … (<running> -> $GOOD in ota_1)` and `OTA image in ota_1; restarting into it`.
2. The device reboots. **Only after the new image's boot banner and `firmware marked valid (no rollback)`**, the log shows `OTA job …: $GOOD is running from ota_1; reporting SUCCEEDED` and `OTA job …: outcome recorded`.
3. `show_job` returns `SUCCEEDED` with `running: $GOOD`, `running_slot: ota_1`. `show_fw` prints `$GOOD`.

**Bad update**, from the image the good update left running:

```sh
JOB=ota-manual-bad-$(date +%s)
make_job "$JOB" bad.bin "firmware_v$BAD"
show_job "$JOB"
```

Expected:

1. `IN_PROGRESS`, `phase: rebooting`, `target: $BAD`, `target_slot: ota_0`.
2. The bad image boots with the rollback guard armed (`running a new OTA image that is not verified yet; …`), fails in `run()` and logs `deliberately bad image; restarting so the bootloader restores the previous image`, with the image still unverified. (An image that fails later, for instance by never reaching AWS IoT, is restarted by the rollback guard after 10 minutes instead: `the new image never reached AWS IoT; restarting so the bootloader rolls it back`.)
3. The bootloader boots `ota_1` again. That image logs `OTA job …: rollback (the bootloader marked ota_0 invalid and booted ota_1); reporting FAILED` and `outcome recorded`.
4. `show_job` returns `FAILED` with `reason: rollback`, `running: $GOOD`, `running_slot: ota_1`, `target: $BAD`, `target_slot: ota_0`. `show_fw` still prints `$GOOD`.

**Optional checks.**

* *Already running:* `make_job ota-manual-same-$(date +%s) good.bin "firmware_v$GOOD"` ends `SUCCEEDED` with `detail: already running this version; nothing installed`, and the serial log shows no download.
* *Older version:* a job for a version lower than `$GOOD` ends `SUCCEEDED`, `reason: already-newer`, with no download; with `downgrade: true` in its document it is installed.
* *Cancelled during the download:* create a job for a greater version and, while the serial log still shows `OTA: … bytes`, run `aws iot cancel-job-execution --job-id "$JOB" --thing-name "$THING" --force`. The download runs to its end, then the device logs `not installing it, because its execution has ended (TerminalStateReached: …)` and does not restart; `show_fw` is unchanged and the execution stays `CANCELED`.
* *Refused job:* `aws iot create-job --job-id "ota-manual-http-$(date +%s)" --targets "$THING_ARN" --document '{"operation":"firmware_update","firmware_version":"firmware_v9.9.9","download_url":"http://example.com/x.bin"}'` ends `REJECTED`, `reason: job`, `detail: download_url is not https`.
* *Interrupted download:* cut power during the download. After the boot the same job is offered IN_PROGRESS in `phase: downloading` and is downloaded again. After three such cuts it ends `FAILED`, `reason: download`.

**Cleanup.** `aws s3 rm "s3://$BUCKET/manual/" --recursive`. To go back to a release build, flash it over USB.
