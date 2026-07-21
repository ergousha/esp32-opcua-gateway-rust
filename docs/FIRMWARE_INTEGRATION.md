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
    Establish the main telemetry session (`telemetry::run(&id)`) using the unique device certificate and client ID (matching the `ThingName`), and start transmitting sensor payload metrics.

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

### Full Device Erase
Alternatively, to clear all flash partitions including the application boot slots:
```sh
espflash erase-flash
```
