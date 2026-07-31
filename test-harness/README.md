# OPC UA integration test harness

Quick reference. The full specification — architecture rationale, wire
contracts, per-phase assertions, the defects this harness uncovered, and
troubleshooting — is in
[`docs/OPCUA_INTEGRATION_TEST.md`](../docs/OPCUA_INTEGRATION_TEST.md).

An end-to-end scenario for the OPC UA gateway: a real ESP32-S3 talking to a real
OPC UA server over the LAN, configured from a real AWS IoT account.

Everything the firmware does is observed from *outside* it — the `opcua` named
shadow's `reported` block, the telemetry that lands in CloudWatch via the
`dt/+/opcua` IoT rule, and the device's serial log. Nothing is asserted by
reaching into the firmware, because nothing in production will be able to
either.

```
   ┌──────────────┐  opc.tcp (None/anonymous)  ┌───────────────────────┐
   │  ESP32-S3    │◀───────────────────────────│ opcua_test_server.py  │
   │  gateway     │                            │  (asyncua, this repo) │
   └──────┬───────┘                            └───────────────────────┘
          │ mTLS MQTT
          ▼
   ┌──────────────────────── AWS IoT Core ────────────────────────┐
   │  shadow  $aws/things/<thing>/shadow/name/opcua   control     │
   │  retained cmd/<thing>/opcua/tags/v<N>            tag bundle  │
   │  telemetry dt/<thing>/opcua ──rule──▶ CloudWatch Logs        │
   └──────────────────────────────────────────────────────────────┘
```

## Files

| File | Role |
| --- | --- |
| `tags.py` | The tag catalogue. **Single source of truth** — the server creates nodes from it and the cloud publisher builds the bundle from it, so an address typo cannot masquerade as a gateway bug. |
| `opcua_test_server.py` | The OPC UA server. SecurityPolicy `None`, anonymous, one namespace, string NodeIds. Takes `fault on/off`, `freeze/thaw`, `quit` on stdin so faults can be injected without a restart. |
| `selfcheck_client.py` | A client that performs the same sequence as `src/opcua/driver.rs`. If this passes and the device fails, the fault is in the firmware or the network — not the harness. |
| `cloud.py` | Bundle construction + SHA-256, retained publish, shadow read/write, CloudWatch telemetry reads. |
| `run_scenario.py` | The scenario: phases, assertions, artifacts. |

## Setup

```sh
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
source ../aws-env.sh          # AWS credentials
source ~/export-esp.sh        # espflash on PATH
```

## Running

```sh
.venv/bin/python run_scenario.py \
    --thing 28848553144F \
    --server-host 192.168.50.28 \      # a LAN IP the DEVICE can reach, not 127.0.0.1
    --port /dev/cu.usbmodem21401 \
    --flash
```

`--phases provision,telemetry` re-runs a subset. Artifacts (serial log, server
log, JSON summary) land in `artifacts/`.

The server must bind an address the device can route to. macOS will prompt to
allow incoming connections the first time; if the device logs connection
timeouts, that prompt was probably declined.

## What each phase proves

| Phase | Proves |
| --- | --- |
| `preflight` | The server answers the firmware's exact call sequence, and the bundle/shadow fit their size budgets — before hardware is involved. |
| `provision` | Two-plane config (shadow pointer + retained bundle) is applied; 14 items created, the one absent NodeId is reported as failed **without** taking the other 14 down (requirements finding A4); the unencrypted-link warning is logged. |
| `telemetry` | The wire format of §4.3, per type: bare numbers, bools, strings, `{"$t":"i64"/"u64"/"b64"/"guid"}` tagging past 2⁵³, arrays, Good status omitted, Bad status as the 4th element, `f32` widened by shortest-decimal rather than `as f64`. Also that an unchanging tag is *not* re-reported — i.e. this is a subscription, not a polling loop. |
| `reconfig` | A new tag set is applied live, without a reboot or reflash, and telemetry is re-stamped with the new `cfg.v`. |
| `ns_uri` | A deliberately wrong namespace index (`ns: 99`) is rescued by `ns_uri` resolution against the server's NamespaceArray. Without it every item would be rejected. |
| `reject_security` | A `Basic256Sha256`/`SignAndEncrypt` request is **refused** with an explicit error and the previous config keeps running — never a silent downgrade to an unsecured channel. |
| `reject_digest` | A bundle whose SHA-256 does not match the shadow pointer is refused; the two planes cannot desynchronise. |
| `server_down` | The PLC is killed mid-session: the driver reports `error`, retries with real backoff (not a tight loop), leaves the MQTT/OTA path alive, and reconnects by itself when the server returns. |
| `disable` | `enabled: false` idles the driver and stops telemetry without deleting the config; re-enabling resumes it. |
| `reboot` | After a USB reset the NVS-cached bundle is applied at boot, before the cloud replies. |

## Prerequisite: IoT policy

The device policy must allow the **named shadow** topics
(`$aws/things/<thing>/shadow/*` for Publish/Subscribe/Receive). Without them the
device's SUBSCRIBE is refused and AWS IoT drops the whole MQTT connection,
taking Jobs/OTA with it. This is implemented in `iot-platform-infra/iot.tf`;
note that an IoT policy document has a hard 2048-byte limit, which is why that
policy is written one-statement-per-action rather than one-per-topic.
