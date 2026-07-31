# OPC UA Gateway — Integration Test Architecture & Scenario

Status: **harness complete and self-verified; on-device run blocked on a host
network policy** (§9). Three firmware/infrastructure defects were found and
fixed while bringing the harness up (§8).

This document is the full specification of how the OPC UA gateway is tested
end to end: what the test doubles are, what contract each side is held to, what
the scenario asserts phase by phase, and how to reproduce the whole thing on a
different machine.

The companion quick-start is [`test-harness/README.md`](../test-harness/README.md).
This file is the reference; that one is the cheat sheet.

---

## 1. What is under test, and what is not

The unit under test is the **whole gateway path**, from the cloud handing down a
configuration to a batch of PLC values landing in AWS:

```
cloud config  ──▶  device applies it  ──▶  OPC UA subscription  ──▶  telemetry
```

Deliberately **not** under test here:

| Not tested | Why, and where it is covered |
| --- | --- |
| Pure logic (parsing, batching, diffing, digests, encoding) | 92 unit tests in `gateway-core`, run on the host: `cargo test -p gateway-core --target <host-triple>`. Duplicating them against hardware would be slower and prove less. |
| Fleet provisioning by claim | Already exercised; the device under test is provisioned and reuses its NVS identity. See [`PROVISIONING.md`](PROVISIONING.md). |
| OTA / Jobs | Separate concern. The scenario only asserts that OPC UA faults never take the OTA path down (§6, `server_down`). |
| `Sign` / `SignAndEncrypt`, non-anonymous identity | Out of scope by decision D2/D3 in [`OPCUA_CLIENT_REQUIREMENTS.md`](OPCUA_CLIENT_REQUIREMENTS.md). The scenario asserts they are **refused**, not that they work. |

The guiding rule: **everything is observed from outside the firmware.** No test
hook, no debug build, no instrumentation. Nothing in production will be able to
reach inside the device either, so a test that does would be proving something
the operator can never rely on.

That leaves exactly three observation channels, and the scenario uses all three:

1. **`reported`** on the `opcua` named shadow — driver state, applied/failed
   counts, last error, free heap.
2. **Telemetry in CloudWatch Logs** — the actual batch payloads, routed by the
   `dt/+/opcua` IoT rule.
3. **The device serial log** — for the things that never reach the cloud:
   backoff timing, the unencrypted-link warning, boot-from-NVS.

---

## 2. Architecture

```
                          ┌─────────────────────────────┐
                          │  test-harness/tags.py       │
                          │  THE TAG CATALOGUE          │
                          │  (single source of truth)   │
                          └───────┬─────────────┬───────┘
                    builds nodes  │             │  builds the bundle
                                  ▼             ▼
  ┌───────────────────────────────────┐   ┌──────────────────────────────┐
  │ opcua_test_server.py (asyncua)    │   │ cloud.py                     │
  │  opc.tcp://<host>:4855/…          │   │  bundle + sha256             │
  │  SecurityPolicy None, anonymous   │   │  retained publish            │
  │  ns: urn:ergousha:opcua-test      │   │  shadow desired/reported     │
  │  14 nodes + 1 absent address      │   │  CloudWatch telemetry reads  │
  └──────────────┬────────────────────┘   └───────────┬──────────────────┘
                 │ opc.tcp (plain TCP, port 4855)     │ HTTPS (IAM)
                 │                                    │
                 ▼                                    ▼
        ┌──────────────────┐            ┌──────────────────────────────────┐
        │   ESP32-S3       │  mTLS MQTT │          AWS IoT Core            │
        │   gateway        │───────────▶│  shadow name/opcua   (control)   │
        │                  │            │  cmd/…/tags/v<N>     (data, ret) │
        │  USB serial ─────┼──┐         │  dt/<thing>/opcua    (telemetry) │
        └──────────────────┘  │         └───────────────┬──────────────────┘
                              │                         │ IoT rule dt/+/opcua
                              ▼                         ▼
                     ┌────────────────┐     ┌────────────────────────────┐
                     │ device-serial  │     │ CloudWatch Logs            │
                     │ .log           │     │ /esp32-ztp/opcua-telemetry │
                     └────────┬───────┘     └───────────┬────────────────┘
                              │                         │
                              └────────┬────────────────┘
                                       ▼
                            ┌──────────────────────┐
                            │  run_scenario.py     │
                            │  phases + assertions │
                            │  artifacts/*.json    │
                            └──────────────────────┘
```

### 2.1 Why a single tag catalogue

`tags.py` is read by the server (to create nodes) **and** by the publisher (to
build the bundle). If the two were maintained separately, a one-character typo
in an address would produce `BadNodeIdUnknown` on the device and look exactly
like a gateway bug. Sharing the catalogue makes that class of false positive
impossible.

### 2.2 Why a self-check client

`selfcheck_client.py` performs the *same* call sequence as
[`src/opcua/driver.rs`](../src/opcua/driver.rs): connect anonymously, read
`Server_NamespaceArray` (`i=2255`), one subscription per distinct scan rate,
chunked `CreateMonitoredItems` at 50 per request, tolerate per-item failures.

It exists purely to answer one question when something breaks: *is the harness
wrong, or is the device wrong?* If the self-check passes and the device fails,
the fault is in the firmware or the network path. That question came up for real
during bring-up (§8, §9) and the self-check settled it in seconds each time.

### 2.3 Why the cloud side needs no device certificate

Everything the harness does with AWS goes through IAM-authorised HTTP APIs —
`UpdateThingShadow`, `GetThingShadow`, `Publish` (with `retain`), and CloudWatch
`FilterLogEvents`. There is no MQTT client and therefore no X.509 identity to
provision for the test itself. Telemetry is observed through the IoT rule rather
than by subscribing, which is what keeps this true.

---

## 3. Prerequisites

### 3.1 Hardware

- Waveshare ESP32-S3-ETH, connected over USB (native USB-Serial-JTAG, no driver
  needed). It appears as `/dev/cu.usbmodem*` on macOS.
- The device must already be provisioned (it reuses its NVS identity). This one
  is thing `28848553144F`.

### 3.2 Toolchain

```sh
. ~/export-esp.sh                 # espup toolchain + espflash on PATH
cargo install espflash --locked   # one-time
```

### 3.3 Python harness

```sh
cd test-harness
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
```

### 3.4 AWS

```sh
source aws-env.sh                 # credentials for the account holding the thing
```

The device policy **must** grant the named-shadow topics. See §8.1 — this was
missing and is the single most likely thing to be missing again in a fresh
account.

### 3.5 Network — the part that actually bites

The OPC UA server runs on the workstation and the **device connects inbound to
it**. Three things must hold:

1. The server binds `0.0.0.0`, not `127.0.0.1`.
2. `--server-host` is a LAN address the device can route to (same subnet as the
   device's DHCP lease is simplest).
3. **The workstation's firewall allows inbound TCP 4855 to the Python process.**

Point 3 is not hypothetical: it is what blocked the on-device run on the machine
this was developed on (§9). Verify it from a *different* host before blaming the
firmware:

```sh
nc -z -w 3 <workstation-lan-ip> 4855 && echo reachable
```

Testing from the workstation itself proves nothing — loopback bypasses the
macOS Application Firewall entirely.

---

## 4. The tag catalogue

15 addresses: 14 nodes the server creates, and 1 it deliberately does not.
Every entry earns its place by proving a distinct behaviour.

| Address | OPC UA type | Scan | Behaviour | What it proves |
| --- | --- | --- | --- | --- |
| `Line1.Temp` | Double | 1 s | sine 15–25 | Double → bare JSON number |
| `Line1.Pressure` | Float | 1 s | ramp 1.0–1.9 | `f32` widened by **shortest decimal**, not `as f64`. A naive cast surfaces `1.100000023841858`; the encoder must emit `1.1`. |
| `Line1.Running` | Boolean | 1 s | toggles / 4 ticks | Boolean → JSON `true`/`false` |
| `Line1.State` | String | 1 s | RUN/IDLE/FAULT | String → JSON string |
| `Line1.Counter` | UInt32 | 1 s | increments | integer below 2⁵³ stays a bare number |
| `Line1.BigCounter` | Int64 | 1 s | starts at 2⁵³+1 | **`{"$t":"i64","v":"…"}`** — lossless past double precision |
| `Line1.Serial` | UInt64 | 1 s | starts at 2⁵³+8 | **`{"$t":"u64","v":"…"}`** |
| `Line1.Blob` | ByteString | 1 s | 5 bytes, varies | **`{"$t":"b64","v":"…"}`** |
| `Line1.BatchId` | Guid | 1 s | constant | **`{"$t":"guid","v":"…"}`** |
| `Line1.Profile` | Double[5] | 1 s | varies | array Variant → JSON array |
| `Line1.Static` | Double | 1 s | **never changes** | report-by-exception: must be reported **once**, not every second. This is the canary that distinguishes a real subscription from a polling loop. |
| `Line1.Faulty` | Double | 1 s | `BadDeviceFailure` | a Bad StatusCode travels as the optional **4th** row element |
| `Line2.Level` | Double | 5 s | slow ramp | a second scan rate ⇒ a second subscription |
| `Line2.Mode` | Int16 | 5 s | cycles 1–3 | Int16 → bare JSON number |
| `Line1.DoesNotExist` | — **absent** | 1 s | — | one unknown NodeId is reported as failed **without** taking the other 14 down (requirements finding A4) |

`Line1.Faulty` is written with `Server.write_attribute_value()` rather than the
public `write_value()`, because the public path validates the StatusCode and
refuses to store a Bad one — which is exactly the case the gateway must handle.

---

## 5. Wire contracts

All four payloads below are from the real bring-up run against thing
`28848553144F`.

### 5.1 Tag bundle — retained on `cmd/<thing>/opcua/tags/v1`

280 bytes for 15 tags, grouped by scan rate. SHA-256
`d037a0b510d550e108342d39fa080ff7b1588509169d19ced87d911a8ad50754`.

```json
{"v":1,"g":[{"r":1000,"a":["Line1.Temp","Line1.Pressure","Line1.Running","Line1.State","Line1.Counter","Line1.BigCounter","Line1.Serial","Line1.Blob","Line1.BatchId","Line1.Profile","Line1.Static","Line1.Faulty","Line1.DoesNotExist"]},{"r":5000,"a":["Line2.Level","Line2.Mode"]}]}
```

The digest is computed over **exactly these bytes**, so the publisher serialises
with `separators=(",", ":")` and no key sorting. That is a contract, not a
formatting preference — reformat the JSON and the device correctly rejects it.

### 5.2 Shadow `state.desired` — `$aws/things/<thing>/shadow/name/opcua`

507 bytes, comfortably inside the 8 KB shadow limit.

```json
{"enabled":true,
 "instance":{"endpoint":"opc.tcp://192.168.50.28:4855/ergousha/test",
             "ns":2,"ns_uri":"urn:ergousha:opcua-test","id_type":"s",
             "sec_mode":"None","sec_policy":"None",
             "session_timeout_ms":60000,"keepalive_ms":10000,"publish_ms":1000},
 "telemetry":{"topic":"dt/28848553144F/opcua","qos":1,
              "batch_max_items":100,"batch_max_bytes":16384,"batch_max_age_ms":2000},
 "cfg":{"v":1,"n":15,"sha256":"d037a0b5…","topic":"cmd/28848553144F/opcua/tags/v1"}}
```

The device applies a bundle only when `bundle.v == cfg.v` **and**
`sha256(bundle) == cfg.sha256`. The two planes therefore cannot desynchronise
silently, and re-delivering the same `cfg.v` is a no-op.

### 5.3 Telemetry batch — `dt/<thing>/opcua`

```json
{"t":1753660800000,"v":1,
 "d":[["Line1.Temp",1753660799871,23.5],
      ["Line1.Running",1753660799902,true],
      ["Line1.State",1753660799902,"RUN"],
      ["Line1.BigCounter",1753660799902,{"$t":"i64","v":"9007199254740993"}],
      ["Line1.Blob",1753660799902,{"$t":"b64","v":"AAECvv8="}],
      ["Line1.Faulty",1753660799910,null,2156396544]]}
```

Row shape is `[address, source_ts_ms, value]` with an optional 4th element
carrying the StatusCode **only when it is not Good** — the overwhelmingly common
case stays three elements. `2156396544` is `0x808B0000`, `BadDeviceFailure`.

### 5.4 Shadow `state.reported`

```json
{"fw":"0.0.1","cfg_v":1,"state":"running","applied":14,"failed":1,
 "failed_sample":["Line1.DoesNotExist"],"srv_publish_ms":1000,
 "last_error":null,"uptime_s":312,"free_heap":118432}
```

---

## 6. The scenario

Ten phases, run in order because they share state — `reconfig` is only
meaningful once `provision` has applied something. Individually selectable with
`--phases` so a single failure can be re-run without repeating the whole thing.

### Phase 1 — `preflight` (no device involved)

| | |
| --- | --- |
| **Action** | Start the server; run `selfcheck_client.py` against it; size-check the bundle and the shadow document. |
| **Asserts** | Server answers the firmware's exact call sequence; 14 items applied and exactly `Line1.DoesNotExist` failed; the static tag reported once; the faulty tag carries a Bad status; bundle ≤ 10 KiB (`MAX_BUNDLE_BYTES`); desired document < 8 KB. |
| **Why first** | If this fails, nothing downstream is interpretable. |

### Phase 2 — `provision`

| | |
| --- | --- |
| **Action** | Publish bundle v1 retained, then `state.desired` pointing at it. |
| **Asserts** | `state=running`, `cfg_v=1`; `applied=14`; `failed=1`; `failed_sample=["Line1.DoesNotExist"]`; **the other 14 tags still run** (finding A4); `free_heap > 40 KB`; serial log contains the `UNENCRYPTED and UNAUTHENTICATED` warning (NFR §7) and `OPC UA synced: 2 subscriptions`. |

### Phase 3 — `telemetry`

| | |
| --- | --- |
| **Action** | Wait for ≥3 batches to reach CloudWatch. |
| **Asserts** | Every batch stamped with the applied `cfg.v`; the absent tag never produces a sample; **per-type encoding** for all 9 encodable types, driven off the catalogue's `expects` field; Good status omitted (3-element row); Bad status present as element 4 with `0x808B0000`; `Line1.Static` reported ≤2 times while `Line1.Temp` reported many (report-by-exception); the 5 s group reports less often than the 1 s group; no batch exceeds `batch_max_bytes`. |

### Phase 4 — `reconfig`

| | |
| --- | --- |
| **Action** | Publish v2: drop the `Line2.*` group and the absent tag, move `Line1.Temp` from the 1 s to the 5 s group. |
| **Asserts** | `cfg_v=2` with `state=running` and **no reboot or reflash**; all v2 tags applied, `failed=0`; serial log shows `config v1 -> v2`; telemetry re-stamped `v:2` and the removed addresses stop appearing. |

### Phase 5 — `ns_uri`

| | |
| --- | --- |
| **Action** | Publish v3 with a deliberately **wrong** `ns: 99` but a correct `ns_uri`. |
| **Asserts** | Device resolves the URI against the server's NamespaceArray, renumbers every NodeId, and applies all items. Without URI resolution every item would be rejected and the driver would bail with "all items rejected". Serial log must **not** contain a namespace-fallback warning. |

### Phase 6 — `reject_security`

| | |
| --- | --- |
| **Action** | Publish v4 with `sec_policy: "Basic256Sha256"`, `sec_mode: "SignAndEncrypt"`. |
| **Asserts** | `last_error` names the offending value; `cfg_v` **stays 3**. The point is the negative: a gateway that silently downgrades to an unsecured channel is worse than one that refuses. |

### Phase 7 — `reject_digest`

| | |
| --- | --- |
| **Action** | Publish v5 whose shadow `cfg.sha256` is 64 zeros while the bundle is real. |
| **Asserts** | Bundle refused with a digest error; `cfg_v` stays 3. Proves the two planes cannot be desynchronised by a torn or stale publish. |

### Phase 8 — `server_down`

| | |
| --- | --- |
| **Action** | Re-establish a good config (v6), then `SIGKILL` the OPC UA server mid-session. Restart it afterwards. |
| **Asserts** | Device reaches `state=error`; retry delays in the serial log show real backoff, not a tight loop; **the MQTT/OTA path stays alive** while OPC UA is down; the device reconnects **by itself** once the server returns and re-creates every item. |

### Phase 9 — `disable`

| | |
| --- | --- |
| **Action** | `enabled: false`, wait, then re-enable under a fresh version. |
| **Asserts** | Driver goes `idle`; **zero** telemetry batches in a 45 s window (measured after a 30 s drain, because the IoT-rule→CloudWatch hop lags); re-enabling returns it to `running`. |

### Phase 10 — `reboot`

| | |
| --- | --- |
| **Action** | Detach the monitor, `espflash reset`, re-attach with `--no-reset`. |
| **Asserts** | Serial log contains `booting with cached OPC UA config v<N> (<n> tags)` — i.e. the NVS-cached bundle is applied **before** the cloud answers — and the device returns to `running` on the same `cfg_v`. |

---

## 7. Running it

```sh
source aws-env.sh
. ~/export-esp.sh
cd test-harness

.venv/bin/python run_scenario.py \
    --thing 28848553144F \
    --server-host 192.168.50.28 \      # LAN IP the DEVICE can reach
    --port /dev/cu.usbmodem21401 \
    --flash \
    --cleanup
```

| Flag | Effect |
| --- | --- |
| `--flash` | Build must already exist; flashes it with `partitions.csv` into `ota_0` first. |
| `--phases a,b` | Run a subset. |
| `--cleanup` | Clear the retained tag bundles afterwards, so a later boot cannot pick up a stale config from a forgotten run. |
| `--artifacts DIR` | Where the serial log, server log and JSON summary land (default `test-harness/artifacts/`). |

Exit code is 0 only if every check in every phase passed.

Before the scenario, the host-side unit tests should be green:

```sh
cargo test -p gateway-core --target aarch64-apple-darwin   # 92 tests
```

---

## 8. Defects found during bring-up

All three were found by the harness before a single scenario phase ran, and all
three are fixed. None of them were harness bugs.

### 8.1 The device policy had no shadow permissions — the config plane could never work

`esp32-ztp-device-policy` granted `dt/…`, `cmd/…` and Jobs topics, but nothing
under `$aws/things/<thing>/shadow/`. The firmware reads its entire OPC UA
configuration from the **named** shadow `opcua`, so its first `SUBSCRIBE` was
unauthorised — and AWS IoT does not merely refuse the subscription, it **drops
the whole MQTT connection**, which would have taken Jobs and OTA down with it.

Fixed in `iot-platform-infra/iot.tf` by adding Publish/Subscribe/Receive on
`$aws/things/${iot:Connection.Thing.ThingName}/shadow/*`.

The first attempt to apply it failed:

```
InvalidRequestException: Policy cannot be created - size exceeds hard limit (2048)
```

An AWS IoT policy document has a **hard 2048-byte limit**, and the policy was
written one statement per topic group, repeating the 38-character ARN prefix 13
times. It was restructured to one statement **per action** with a resource list
— same per-thing scoping, same Publish/Subscribe/Receive separation, 1280 bytes.

> Anyone extending that policy should check the size, not just the syntax.

### 8.2 `telemetry::spawn_driver` never registered the eventfd VFS

```
E (17071) esp32_opcua_gateway::telemetry: could not start the OPC UA runtime: Permission denied (os error 13)
```

Tokio wakes its reactor through an `eventfd`. On ESP-IDF that syscall is
unavailable until `esp_vfs_eventfd_register()` has been called —
`components/vfs/vfs_eventfd.c` returns `EACCES` while its VFS id is still `-1`.
So `Runtime::build()` failed and the OPC UA thread died at startup, **silently**:
MQTT, Jobs and OTA all kept running, and the only symptom was that no OPC UA
data ever appeared.

Fixed with `register_eventfd()` in [`src/telemetry/mod.rs`](../src/telemetry/mod.rs),
called before the runtime is built.

### 8.3 Tokio's signal driver panicked the OPC UA thread on every boot

```
thread 'opcua' (2) panicked at crates/tokio/src/signal/unix.rs:84:53
```

With 8.2 fixed, the runtime got one step further and aborted. Tokio's signal
driver is built around a self-pipe made with `UnixStream::pair()`, i.e.
`socketpair(AF_UNIX)` — which lwIP does not implement. The panic aborts the
process, so the device **reboot-looped**.

This is not avoidable from the firmware's own `Cargo.toml`: `async-opcua-client`
enables tokio's `full` feature, and Cargo's feature unification means
`default-features = false` on our side cannot switch `signal` back off. The
driver is constructed whenever the I/O driver is enabled, which async-opcua
requires.

Fixed by patching the already-vendored `crates/tokio` so the signal driver
constructs **inert** on ESP-IDF (`receiver: Option<UnixStream>`, `None` there):
it parks and shuts down normally and simply never dispatches a signal. Signals
are not deliverable on this platform anyway — the repo already ships a
`signal-hook-registry` stub saying exactly that, so this follows an established
pattern rather than inventing one.

> `crates/` is a `[patch.crates-io]` vendor directory. Re-vendoring tokio will
> drop this patch; the `LOCAL PATCH (ESP-IDF)` comments mark what to reapply.

---

## 9. Current status

### Verified working on hardware

Captured from the device after the §8 fixes, with the server reachable only from
the workstation:

```
shadow v21: config v1 with 15 tags from cmd/28848553144F/opcua/tags/v1
waiting for the retained tag bundle on cmd/28848553144F/opcua/tags/v1
cached OPC UA config v1 (280 B bundle) to NVS
OPC UA driver: idle -> connecting
OPC UA link to opc.tcp://192.168.50.28:4855/… is UNENCRYPTED and UNAUTHENTICATED …
telemetry -> dt/28848553144F/opcua (qos 1, <=100 items, <=16384 B, <=2000 ms)
```

That covers: MQTT with the corrected policy, named-shadow subscribe and `get`,
desired-document parse and validation, retained-bundle delivery, SHA-256 and
version verification, NVS caching, driver state machine entry, the security
warning, and telemetry-policy application. Reconnect attempts were spaced
1 s → 2 s → 4 s → 8 s → 16 s, confirming the backoff schedule.

Harness-side, `preflight` passes in full against the server.

### Blocked

The remaining phases need the device to complete a TCP connection to the
workstation. On the development machine the macOS Application Firewall has the
Python binary set to **"Block incoming connections"**, so the device's
connections are reset during the OPC UA handshake:

```
Unexpected error while waiting for server ACK. Expected ACK, got ConnectionReset
```

Loopback bypasses the firewall, which is why `selfcheck_client.py` passes from
the same machine while the device cannot get through. That machine is centrally
managed and the setting cannot be changed, so the run must be completed on a
host where inbound TCP 4855 is permitted. On such a host:

```sh
sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp \
  "$(cd test-harness && .venv/bin/python -c 'import sys; print(sys.executable)')"
```

or simply run the server on a machine without an inbound-blocking firewall.
Nothing in the harness or the firmware needs to change.

---

## 10. Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| `ConnectionReset` waiting for server ACK; server logs no connection | Host firewall blocking inbound to the Python process | §3.5. Verify with `nc` from another host — never from the workstation. |
| MQTT connects then drops repeatedly, Jobs also broken | Device policy missing the named-shadow topics | §8.1 |
| `could not start the OPC UA runtime: Permission denied (os error 13)` | eventfd VFS not registered | §8.2 |
| `panicked at .../signal/unix.rs`, device reboot-loops | Vendored tokio patch lost | §8.3 |
| `shadow document unusable: shadow has no desired state` | No config published yet | Normal before `provision`. |
| `tag bundle rejected: bundle sha256 …` | Bundle bytes differ from what the digest was computed over | Republish; do not reformat the JSON (§5.1). |
| All items fail, driver reports "all N monitored items were rejected" | Wrong namespace or `id_type` | Check `ns_uri` is published by the server; phase `ns_uri` covers this deliberately. |
| `reported` never updates | Reports are throttled to state-change or every 30 s | Wait, or trigger a state change. |
| Telemetry absent from CloudWatch but the device says it published | `dt/+/opcua` rule or its log group missing | §8.1's terraform also creates them. |
| Server exits at start with `OSError: [Errno 22]` on stdin | kqueue refuses `/dev/null` as a read pipe | Harmless; the server logs a warning and runs without the fault-injection channel. |

---

## 11. Extending the scenario

- **A new value type**: add one `Tag` to `tags.py` with an `expects` key present
  in `ENCODING_PREDICATES`. The server creates the node and the telemetry phase
  picks up the assertion automatically — no edit to `run_scenario.py`.
- **A new phase**: decorate a function with `@phase("name")`, take `Ctx`, return
  a `PhaseResult`. It is appended to the default run order and becomes selectable
  via `--phases`.
- **Scale testing toward the 250-tag cap** (follow-up F3 in the requirements):
  generate addresses programmatically in `tags.py`, keep the number of distinct
  scan rates small, and watch `reported.free_heap` — the cap is supposed to be a
  measured number, and this harness is where that measurement should happen.
