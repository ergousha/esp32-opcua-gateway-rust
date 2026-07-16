# Zero-Touch Provisioning — Architecture and Flow

Approach: **AWS IoT Fleet Provisioning by Claim**. The device leaves the factory with a **common** "claim" (bootstrap) certificate for the entire fleet. Upon first connection, it generates its own **unique** certificate; the MAC + secret are validated against DynamoDB by a Lambda hook. If approved, IoT Core creates the `thing` + `certificate` + `policy` for the device.

## Components

| Layer | Component | Role |
| --- | --- | --- |
| Device | ESP32-S3-ETH (Rust) | Connects with claim, gets its own identity, writes to NVS |
| AWS | IoT Core Fleet Provisioning template | Defines the thing/cert/policy |
| AWS | Lambda pre-provisioning hook | Validates MAC+secret in DynamoDB (allow/deny) |
| AWS | DynamoDB `*-device-registry` | Allowed device records (MAC, secret, allowed) |
| AWS | IoT Rule -> CloudWatch Logs | For monitoring telemetry |

## Flow Diagram

```
  ESP32-S3                         AWS IoT Core                 Lambda        DynamoDB
     |                                  |                          |             |
     |-- TLS connect (CLAIM cert) ----->|                          |             |
     |-- pub $aws/certificates/create ->|                          |             |
     |<- accepted {certPem,key,token} --|  (generates unique cert) |             |
     |                                  |                          |             |
     |-- pub .../provision {token,      |                          |             |
     |         SerialNumber,MacAddress, |-- pre-provisioning hook ->|             |
     |         Secret} ---------------->|                          |-- get MAC ->|
     |                                  |                          |<- secret ---|
     |                                  |<- allowProvisioning:true-|             |
     |                                  | (thing+cert+policy created)            |
     |<- accepted {thingName} ----------|                          |             |
     |  (cert+key+thing written to NVS) |                          |             |
     |                                  |                          |             |
     |== reconnect (DEVICE cert) ======>|                          |             |
     |                                  |                          |             |
```

## Device Boot Logic (`src/main.rs`)

1. **Network**: First `eth::start(...)` — W5500 brings up via esp_eth, wait 10s for link+DHCP. If no link/lease, fallback to WiFi (`cfg.toml`) via `wifi::start(...)`. Note: Ethernet handle is kept ALIVE even in no-link condition (if dropped, SpiDriver::drop panics; see pitfalls below).
2. Is there an identity in NVS? (`DeviceStore::exists`)
   - **Yes** → `load()` → directly proceed to connection.
   - **No** → `provisioning::run()` → `save()` → proceed to connection.
3. `telemetry::run(&id)` — connect with device identity and keep the connection open.

## Hardware Pitfalls Encountered (Resolved)

This PoC was verified on actual ESP32-S3-ETH hardware; issues resolved along the way:

| Symptom | Root Cause | Solution |
| --- | --- | --- |
| `spi_master: txdata transfer > host maximum` | SPI bus DMA off (~64B limit), W5500 sends ~1.5KB frame | `SpiDriverConfig::new().dma(Dma::Auto(4096))` (`eth.rs`) |
| Panic when switching to WiFi: `spi_bus_free().unwrap()` INVALID_STATE | Dropping eth handle on no-link, but esp_eth SPI device is still on bus | Do not drop the handle; keep it alive inside `Net::Wifi { eth, .. }` (`main.rs`) |
| `memory allocation of ~1GB failed` (on connect) | 5 `&str` arguments exceeded the register limit for fat-pointer arguments on xtensa, leading to incorrect reads | Pass certs within a single `Creds` struct reference (`mqtt_util.rs`) |
| General instability | TLS+MQTT+serde in main task, 8K stack was too small | `CONFIG_ESP_MAIN_TASK_STACK_SIZE=16384` |

MAC note: `ESP_MAC_ETH` is used as the device identity; this is derived from the eFuse base MAC (base `..:4C` → ETH `..:4F`). When seeding, use the MAC address logged by the device on its first boot.

## MQTT Topics

| Purpose | Topic |
| --- | --- |
| Create Cert (request) | `$aws/certificates/create/json` |
| Create Cert (response) | `$aws/certificates/create/json/{accepted,rejected}` |
| RegisterThing (request) | `$aws/provisioning-templates/<template>/provision/json` |
| RegisterThing (response) | `$aws/provisioning-templates/<template>/provision/json/{accepted,rejected}` |
| Commands (allowed in device policy) | `cmd/<thingName>/*` |

## Security Model

- **Claim policy** (Terraform `iot.tf`): only `iot:Connect` + provisioning topics. Telemetry **cannot** be sent with the claim cert.
- **Device policy**: using policy variables, each device can only access its own `dt/<thingName>/*` and `cmd/<thingName>/*` topics; client_id = thingName is mandatory.
- **Lambda hook**: provisioning is rejected if there is no MAC record, if `allowed=false`, or if the secret mismatches. Uses constant-time secret comparison.

## ESP32-S3 Security Features (PoC vs Production)

At the PoC level (chosen): certificates are in unencrypted NVS, eFuses are **not** burned.
Going to production:

- **Flash Encryption** + **Secure Boot v2**: protects cert+key in NVS/flash and the firmware (burning eFuses — irreversible).
- **DS (Digital Signature) peripheral**: device private key is kept encrypted in eFuse, TLS signing is performed without leaving RAM. Using `esp-idf` `esp_ds` API; the provisioning transitions to the `CreateCertificateFromCsr` flow (sign CSR with DS and send to AWS), so the private key never leaves the device.
- **Per-device secret**: common in PoC; unique per device in production and protected by DS/eFuse.

## Troubleshooting

| Symptom | Possible Cause |
| --- | --- |
| `netif did not come up` | No Ethernet cable connected / no DHCP server |
| `connection timeout` (claim) | `MQTT_ENDPOINT` incorrect; claim cert/policy missing; time/TLS issues |
| Provisioning `REJECTED` | MAC is not in DynamoDB / `allowed=false` / secret mismatch |
| `certificatePem missing` | buffer_size is small (4096 in code) or JSON topic is not used (CBOR instead) |
| Connection works, but no telemetry | Telemetry rule prefix and `config::TELEMETRY_TOPIC_PREFIX` differ (Note: telemetry loop has been removed, so this is no longer applicable) |

Logs: CloudWatch `/{project}/telemetry` and `/aws/lambda/{project}-pre-provisioning-hook` on the AWS side. Serial monitor on the device side.

## Re-provisioning (Test)

To provision the device from scratch, erase NVS:

```sh
espflash erase-region 0x9000 0x6000   # NVS partition (default table)
# or erase the entire flash:
espflash erase-flash
```
