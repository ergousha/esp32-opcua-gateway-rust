# ESP32 Firmware Integration & Hardware Reference

This reference guide details the ESP32-S3-ETH firmware architecture, boot sequence, hardware pitfalls resolved, and local testing/provisioning commands.

---

## 1. Device Boot & Connection Logic

The firmware boot process in `src/main.rs` follows this sequence:

1.  **Network Initialization**: 
    The device starts by attempting to bring up the physical Ethernet interface (`eth::start(...)`). The W5500 transceiver brings up the link via `esp_eth` and waits up to 10 seconds for a link-up and DHCP lease. 
    *   **Fallback**: If no Ethernet link or lease is acquired within 10 seconds, the device falls back to Wi-Fi (`wifi::start(...)`) using the configuration parameters specified in `cfg.toml`.
    *   **Resource Management**: The Ethernet driver handle is kept alive even during a Wi-Fi fallback. Dropping it would trigger a spi-driver crash (see *Hardware Pitfalls* below).

2.  **Identity Verification (NVS)**:
    The device checks the Non-Volatile Storage (NVS) (`DeviceStore::exists`) for a provisioned identity.
    *   **Identity Exists**: Loads the stored unique certificate and private key, and immediately connects to AWS IoT Core using mutual TLS.
    *   **Identity Absent**: Triggers the Zero-Touch Provisioning (ZTP) flow (`provisioning::run()`), obtains unique credentials, saves them permanently to NVS, and restarts the connection.

3.  **Telemetry Loop**:
    Establish the main telemetry session (`telemetry::run(&id, job_store)`) using the unique device certificate and client ID (matching the `ThingName`), and start transmitting sensor payload metrics.
    *   **Image validity**: Once the MQTT session is up, the running image marks itself valid (`ota::mark_valid()`). A fresh OTA image that fails before that point restarts instead of stopping, so the bootloader restores the previous image.
    *   **Pending OTA job**: If the previous boot installed an update, its job is reported now, by the image that actually runs (see *OTA Updates and Job Status* below).

---

## 2. Hardware Pitfalls & Solutions

During the PoC validation on physical ESP32-S3-ETH hardware, several hardware and runtime issues were encountered and resolved:

| Pitfall / Error | Root Cause | Solution |
| :--- | :--- | :--- |
| **`spi_master: txdata transfer > host maximum`** | The SPI bus DMA was disabled, limiting transfer size to ~64 Bytes. The W5500 chip regularly transmits MTU-sized Ethernet frames (~1.5 KB). | Enabled SPI DMA auto-allocation: `SpiDriverConfig::new().dma(Dma::Auto(4096))` in `eth.rs`. |
| **`spi_bus_free().unwrap() INVALID_STATE`** (Panic when switching from Eth to Wi-Fi) | The firmware was dropping the Ethernet driver handle when no cable was detected. However, the underlying SPI device was still attached to the bus, causing a resource panic. | Do not drop the Ethernet driver handle. Retain it inside the state struct `Net::Wifi { eth, .. }` in `main.rs` to keep the bus registration active. |
| **`memory allocation of ~1GB failed`** (Crash on TLS connection) | Passing 5 distinct `&str` references (certificates/keys) exceeded the Xtensa register-window argument limit, forcing fat-pointer argument corruption on stack boundaries. | Consolidated certificates and credentials into a single `Creds` struct reference (`mqtt_util.rs`) to fit within parameter register limits. |
| **General Instability / Stack Overflow** | Processing TLS handshake, MQTT packets, and serde JSON parsing inside the main thread exceeded the default task stack size (8 KB). | Increased the main task stack allocation to 16 KB by setting `CONFIG_ESP_MAIN_TASK_STACK_SIZE=16384` in the SDK config. |

> [!NOTE]
> **MAC Address Extraction**: The unique identifier for each device is its Ethernet MAC address (`ESP_MAC_ETH`). This is derived from the hardware eFuse base MAC (typically base MAC `..:4C` maps to Ethernet MAC `..:4F`). Always use the exact MAC logged by the device serial monitor on its first boot when seeding DynamoDB registry entries.

---

## 3. Local Testing & Re-provisioning

To test the Zero-Touch Provisioning flow from scratch on a previously provisioned device, you must erase the device's credentials from NVS.

### Erase NVS Partition Only
Run the following `espflash` command to wipe the default NVS partition boundaries (`0x9000` to `0xf000` under the standard partition table):
```sh
espflash erase-region 0x9000 0x6000
```
This also erases the record of a pending OTA job (§4). An execution left IN_PROGRESS in `phase: rebooting` is still settled from the status details the Jobs service holds for it (§4.2), as long as the device comes back as the same thing.

### Full Device Erase
Alternatively, to clear all flash partitions including the application boot slots:
```sh
espflash erase-flash
```

---

## 4. OTA Updates and Job Status

Firmware updates arrive as AWS IoT Jobs (`src/jobs.rs`); the image is written by `src/ota.rs`. The decisions (how a job is read, what its outcome is, what is reported) are pure functions in `gateway-core/src/jobs.rs`, unit-tested on the host.

With `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`, a new image that resets before it marks itself valid is rolled back by the bootloader. The image that downloads an update therefore cannot know whether it will work, and does not report SUCCEEDED. It records the job in NVS (`src/job_store.rs`, namespace `ota_job` in the `nvs` partition), reports IN_PROGRESS, and restarts. Whichever image boots next reports the outcome.

### 4.1 What the execution goes through

| When | Reported by | Status | `statusDetails` |
| :--- | :--- | :--- | :--- |
| Download starts | the running image | `IN_PROGRESS` | `phase: downloading`, `from`, `target` |
| Image written, verified, recorded in NVS, made the boot slot | the running image | `IN_PROGRESS`, `stepTimeoutInMinutes: 30` | `phase: rebooting`, `from`, `target`, `target_slot` |
| After the reboot: the new image runs from `target_slot`, is `target`, and marked itself valid | the **new** image | `SUCCEEDED` | `from`, `running`, `running_slot` |
| After the reboot: the previous image runs | the **previous** image | `FAILED` | `reason: rolled back`, `detail`, `running`, `running_slot`, `target`, `target_slot` |
| After the reboot: the new image runs from `target_slot` but is another version | the new image | `FAILED` | `reason: version mismatch`, … |
| After the reboot: the new image could not mark itself valid | the new image | `FAILED` | `reason: not marked valid`, `detail` |
| Download, write or activation failed | the running image | `FAILED` | `reason: install failed`, `detail`, `target` |
| The job names the version already running | the running image | `SUCCEEDED` | `detail: already running this version; nothing installed`, `running`, `running_slot` |

`target` is the job document's `firmware_version` normalised: `firmware_v0.1.0`, `esp32-opcua-gateway-v0.1.0` and `0.1.0` all compare as `0.1.0`, the form the shadow reports as `reported.fw`. A pre-release suffix is kept, so `0.2.0-rc.1` is not `0.2.0`.

The running slot is the primary evidence and the version the secondary: a rebuild shipped under an unchanged version cannot run from a slot it was not written to. `detail` of a rollback says whether the bootloader marked the new slot invalid (`esp_ota_get_last_invalid_partition()`) or the device simply booted the other slot.

### 4.2 Edge cases

* **The same execution is offered again at boot.** The Jobs service offers an execution for as long as it is IN_PROGRESS, and it still is when the device comes back. While a recorded job is being reported, the device takes no other job and asks for none; `notify-next` and `$next/get` answers are ignored until the service has answered the report. Should the record be missing, an execution offered IN_PROGRESS in `phase: rebooting` is reported from its own status details, never downloaded again.
* **Power loss.** The record is written after the image is verified and before its slot becomes the boot slot. A power cut earlier leaves the old image active, and the job is offered again IN_PROGRESS in `phase: downloading`, which is downloaded again. A cut after activation boots the new image with its record. The window between the two writes reports `FAILED`, `rolled back` (`booted ota_0, not ota_1`): the device is on its previous image either way.
* **The execution already ended.** A job cancelled with `--force`, timed out, or deleted while the device was rebooting makes the report fail with `InvalidStateTransition`, `TerminalStateReached` or `ResourceNotFound`. The device logs it, clears the record and goes back to taking jobs. `RequestThrottled` and `InternalError` are retried.
* **No answer.** A report the service has not answered is sent again every 30 s, and on every reconnect. The subscriptions to `…/update/accepted` and `…/update/rejected` are renewed with each send.
* **An image that cannot reach AWS IoT.** Returning from `main` would end the main task with the image unverified, never rolled back: on Ethernet, an image with an empty endpoint used to hang this way. While the running image is unverified, a failure in `main` restarts the device instead.
* **Neither image comes back.** `stepTimeoutInMinutes: 30` on the `rebooting` report, and the job-wide in-progress timeout the OTA pipeline sets (iot-platform-infra#3), end the execution `TIMED_OUT`. A device that reports after that is told the execution already ended.
* **The version already running.** A job for it is reported `SUCCEEDED` without a download, which is what a unit flashed over USB with the current release, or one that joins the thing group late, is offered. A rebuild under an unchanged version is therefore not installed; give it a new version.
* **The download URL** is taken only from the `$next/get` answer, where AWS IoT fills in the presigned URL, and must be `https://`.

### 4.3 Hardware check: a good and a bad update

A manual procedure; it touches real AWS resources and one real device. It needs the AWS CLI with rights to write to the firmware bucket, presign, create and describe jobs, and read the shadow; `jq`; and a serial monitor on the device.

The device must already run a build that contains this logic, flashed over USB (`espflash flash --partition-table partitions.csv --target-app-partition ota_0 target/xtensa-esp32s3-espidf/release/esp32-opcua-gateway`): the image it rolls back to is the one that reports the rollback. Each image below gets its own version, or it would be reported as already running. The slot names below assume the USB-flashed image runs from `ota_0`; swap them if the serial log says otherwise.

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

**Images.** Build both, then undo the edits (`git checkout -- Cargo.toml Cargo.lock src/config.rs`).

* `good.bin`: set `version = "0.1.1-ota.1"` in `Cargo.toml`, then `cargo build --release && save_image good.bin`.
* `bad.bin`: set `version = "0.1.1-ota.bad"`, and make `config::mqtt_url()` return `"mqtts://:8883".to_string()`, which is exactly what an empty `iot_endpoint` produces. Setting `iot_endpoint = ""` in `cfg.toml` is not enough: `build.rs` writes the endpoint from SSM back into it whenever it can read SSM. Then `cargo build --release && save_image bad.bin`.

**Good update.**

```sh
JOB=ota-manual-good-$(date +%s)
make_job "$JOB" good.bin firmware_v0.1.1-ota.1
show_job "$JOB"   # repeat while it runs
```

Expected:

1. `IN_PROGRESS`, `phase: downloading`; then `IN_PROGRESS`, `phase: rebooting`, `target: 0.1.1-ota.1`, `target_slot: ota_1`. The serial log shows `recorded OTA job … (0.1.0 -> 0.1.1-ota.1 in ota_1)` and `OTA image in ota_1; restarting into it`.
2. The device reboots. **Only after the new image's boot banner and `Firmware marked as valid`**, the log shows `OTA job …: 0.1.1-ota.1 is running from ota_1; reporting SUCCEEDED` and `OTA job …: outcome recorded`.
3. `show_job` returns `SUCCEEDED` with `running: 0.1.1-ota.1`, `running_slot: ota_1`. `show_fw` prints `0.1.1-ota.1`.

**Bad update**, from the image the good update left running:

```sh
JOB=ota-manual-bad-$(date +%s)
make_job "$JOB" bad.bin firmware_v0.1.1-ota.bad
show_job "$JOB"
```

Expected:

1. `IN_PROGRESS`, `phase: rebooting`, `target: 0.1.1-ota.bad`, `target_slot: ota_0`.
2. The bad image cannot connect, and logs `<error>; restarting so the bootloader restores the previous image`, the error being `Failed to set up MQTT client`, `connection lost before it was established` or `timed out connecting to AWS IoT Core`. On WiFi, a failed association restarts it just the same.
3. The bootloader boots `ota_1` again. That image logs `OTA job …: rolled back (the bootloader marked ota_0 invalid and booted ota_1); reporting FAILED` and `outcome recorded`.
4. `show_job` returns `FAILED` with `reason: rolled back`, `running: 0.1.1-ota.1`, `running_slot: ota_1`, `target: 0.1.1-ota.bad`, `target_slot: ota_0`. `show_fw` still prints `0.1.1-ota.1`.

**Optional checks.**

* *Already running:* `make_job ota-manual-same-$(date +%s) good.bin firmware_v0.1.1-ota.1` ends `SUCCEEDED` with `detail: already running this version; nothing installed`, and the serial log shows no download.
* *Execution ended before the report:* create a job for another version and, while the serial log still shows `Downloaded … bytes`, run `aws iot cancel-job-execution --job-id "$JOB" --thing-name "$THING" --force`. The download is not interrupted (the firmware does not watch for cancellation mid-download), and the device restarts into the new image. That image's report is refused: it logs `outcome rejected (…); dropping it` and goes back to taking jobs, and the execution stays `CANCELED`.
* *Interrupted download:* cut power during the download. After the boot the same job is offered IN_PROGRESS in `phase: downloading` and is downloaded again.

**Cleanup.** `aws s3 rm "s3://$BUCKET/manual/" --recursive`. To go back to a release build, flash it over USB.
