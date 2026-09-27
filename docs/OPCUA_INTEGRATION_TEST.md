# OPC UA Gateway — Test Architecture & Scenario

Status: **the OPC UA client is now tested end to end without hardware.** The
loopback suite runs the gateway's real client against a real OPC UA server —
the server half of the same library — in one process on `127.0.0.1`, and passes
19/19 in about 20 seconds (§9.1). Against the code as it stood after the last
hardware run, 9 of those 19 fail, reproducing the three open on-device bugs and
four defects nobody had seen (§8.10–8.16). The on-device scenario has been
ported from Python to Rust (`gateway-hil`). Its offline mode (§7.3), which needs
no route from the device back to the host, passes **27/27 on hardware**. The
full online scenario has not been re-run yet, because the development
workstation's firewall blocks the device's inbound connection;
[`HIL_ON_WINDOWS.md`](HIL_ON_WINDOWS.md) runs it from another machine.

This document is the full specification of how the gateway is tested: the
layers, the test doubles, the contract each side is held to, what every
scenario asserts, and how to reproduce all of it on another machine.

---

## 1. What is under test, and where

The unit under test is the **whole gateway path**, from the cloud handing down a
configuration to a batch of PLC values landing in AWS:

```
cloud config  ──▶  device applies it  ──▶  OPC UA subscription  ──▶  telemetry
```

It is covered in three layers, from cheapest to most faithful:

| Layer | What runs | Needs | Command |
| --- | --- | --- | --- |
| **Unit** | `gateway-core`: parsing, validation, digests, encoding, batching, diffing, backoff | nothing | `cargo test -p gateway-core --target host-tuple` |
| **Loopback integration** | `gateway-opcua` — the firmware's actual OPC UA client — against `opcua-test-server`, both in one process on `127.0.0.1` | nothing | `cargo test -p gateway-opcua --target host-tuple` |
| **Hardware-in-the-loop** | the firmware on an ESP32-S3 against the same server, configured through a real AWS IoT account | device, AWS credentials, a LAN path from device to host | `cargo run -p gateway-hil --target host-tuple -- …` |

### 1.1 Why the loopback layer exists

The firmware's OPC UA client has no device dependency — it is plain Rust on
`async-opcua` and tokio — so it runs on a laptop exactly as it runs on the
ESP32. Once it lives in its own crate, the question "does the client behave?"
no longer needs a device at all:

- **The same library on both ends.** The server is `async-opcua-server`, the
  server half of the `async-opcua` stack the client is built on, at the same
  version. There is no second protocol implementation in the loop whose quirks
  have to be told apart from the gateway's.
- **No network, so no firewall.** The previous harness ran its server on the
  workstation and had the device connect *inbound*. On a managed macOS machine
  the application firewall refuses that, and there is no user-level way around
  it (§3.5). Loopback traffic is not filtered: client and server in one process
  on `127.0.0.1` run on any machine, including CI.
- **Failures a device run cannot stage.** A server that goes silent without
  closing the socket, a disable while the server is hung, an endpoint change —
  each is a few lines against an in-process server and a relay
  ([`Blackhole`](../opcua-test-server/src/blackhole.rs)).
- **Seconds, not twenty minutes.** The suite runs in ~20 s and every test owns
  its own server on an ephemeral port, so they run in parallel.

### 1.2 What only the device can show

| Not covered below the HIL layer | Why | Where |
| --- | --- | --- |
| Heap and allocation behaviour | a laptop has gigabytes; the device has ~300 kB and no PSRAM | §8.5–8.8, §9.4 |
| The ESP-IDF runtime under tokio | eventfd VFS, the inert signal driver | §8.2, §8.3 |
| NVS caching and boot from cache | the `opcua` partition | `reboot` phase |
| The MQTT/AWS planes | device policy, shadow, retained bundle, IoT rule | §8.1, all phases |

Deliberately **not** under test anywhere:

| Not tested | Why, and where it is covered |
| --- | --- |
| Fleet provisioning by claim | Already exercised; the device under test reuses its NVS identity. See [`PROVISIONING.md`](PROVISIONING.md). |
| OTA / Jobs | Separate concern. The HIL run only asserts that OPC UA faults never take the OTA path down (`server_down`). |
| `Sign` / `SignAndEncrypt`, non-anonymous identity | Out of scope by decision D2/D3 in [`OPCUA_CLIENT_REQUIREMENTS.md`](OPCUA_CLIENT_REQUIREMENTS.md). Both layers assert they are **refused**, not that they work. |

The guiding rule, in every layer: **observe the gateway from outside.** The
loopback tests read the health report and the telemetry the client would
publish — never driver internals. The HIL run reads the shadow, CloudWatch and
the serial log, because nothing in production can reach inside the device
either.

---

## 2. Architecture

```
                 ┌──────────────────────────────────────────────────┐
                 │ opcua-test-server   (host only; a test fixture)  │
                 │                                                  │
                 │  catalogue.rs   THE TAG CATALOGUE — single source │
                 │  documents.rs   bundle + shadow desired (cloud)  │
                 │  server.rs      TestServer on async-opcua-server │
                 │  blackhole.rs   TCP relay that can go silent     │
                 │  main.rs        standalone binary, stdin faults  │
                 └───────┬──────────────────┬───────────────┬───────┘
                         │                  │               │
          in-process, 127.0.0.1    in-process, 127.0.0.1    │ in-process, bound to the LAN
                         ▼                  ▼               ▼
  ┌──────────────────────────┐ ┌───────────────────────┐ ┌───────────────────────────────┐
  │ gateway-opcua/tests      │ │ gateway-opcua/examples│ │ gateway-hil                   │
  │ the real client, 19      │ │ local_gateway: the    │ │ phases + assertions; drives   │
  │ scenarios, cargo test    │ │ client on a laptop    │ │ espflash, AWS and the server  │
  └──────────────────────────┘ └───────────────────────┘ └──────┬──────────────┬─────────┘
                                                                │ HTTPS (IAM)  │ espflash monitor
                                                                ▼              ▼
                                               ┌───────────────────┐   ┌─────────────────┐
                                               │ AWS IoT Core      │   │ ESP32-S3        │
                                               │ shadow, retained  │◀──│ firmware, which │
                                               │ bundle, IoT rule  │   │ embeds the same │
                                               │ → CloudWatch      │   │ gateway-opcua   │
                                               └───────────────────┘   └─────────────────┘
```

The firmware links `gateway-opcua` exactly as the tests do: through
`gateway_opcua::new(options) -> (Client, Driver)` and `spawn_thread`. The tests
even run the driver the same way — on its own thread, on a current-thread
runtime.

### 2.1 Why a single tag catalogue

[`catalogue.rs`](../opcua-test-server/src/catalogue.rs) is read by the server
(to create nodes) **and** by the document builders (to build the bundle). If the
two were maintained separately, a one-character typo in an address would produce
`BadNodeIdUnknown` and look exactly like a gateway bug. Each tag also declares
the JSON encoding the gateway must produce for it, so a new value type is one new
entry: both the loopback suite and the HIL `telemetry` phase pick up its
assertion without further edits.

### 2.2 Why no replica client

The Python harness shipped `selfcheck_client.py`, a second client that replayed
the driver's call sequence in another library, so that "is the harness wrong or
is the device wrong?" could be answered in seconds. That question now has a
direct answer: the HIL `preflight` phase runs **the firmware's own client**
(`gateway-opcua`) against the server over loopback. If that passes and the
device fails, the fault is in the device, the network path or the cloud — not
in the server and not in the client logic.

### 2.3 Why the documents are not built with `gateway-core`

[`documents.rs`](../opcua-test-server/src/documents.rs) writes the tag bundle and
the shadow's `state.desired` with plain `serde_json`. They stand in for a
producer the firmware does not control (the cloud side), so they are written
independently of the parser they are fed to. Built with `gateway-core`'s own
types, a schema bug would agree with itself and pass.

### 2.4 Why the cloud side needs no device certificate

Everything `gateway-hil` does with AWS goes through IAM-authorised HTTPS —
`UpdateThingShadow`, `GetThingShadow`, `Publish` (with `retain`), and CloudWatch
`FilterLogEvents`. There is no MQTT client and therefore no X.509 identity to
provision for the test itself. Telemetry is observed through the IoT rule rather
than by subscribing, which is what keeps this true.

### 2.5 Servers differ; the gateway must not

`async-opcua-server` accepts a monitored item for a NodeId that does not exist
and reports `BadNodeIdUnknown` in the first notification instead; most PLCs, and
the Python `asyncua` server, reject it in `CreateMonitoredItems`. The
specification allows both. The first loopback run showed that the gateway's idea
of a *failed* tag depended on which one it was talking to (§8.16), and the fix
went into the gateway, not the fixture — the test server stays lenient on
purpose, because real servers are.

---

## 3. Prerequisites

### 3.1 For the host layers

Only a Rust toolchain. The espup `esp` toolchain pinned in `rust-toolchain.toml`
works (it ships host `std`), and so does upstream stable:

```sh
cargo test -p gateway-core -p gateway-opcua -p opcua-test-server --target host-tuple
RUSTUP_TOOLCHAIN=stable cargo test -p gateway-opcua --target host-tuple   # same, on stable
```

`--target host-tuple` is required: `.cargo/config.toml` defaults every build to
`xtensa-esp32s3-espidf`.

### 3.2 Hardware (HIL only)

- Waveshare ESP32-S3-ETH, connected over USB (native USB-Serial-JTAG, no driver
  needed). It appears as `/dev/cu.usbmodem*` on macOS.
- The device must already be provisioned (it reuses its NVS identity). This one
  is thing `28848553144F`.

### 3.3 Toolchain (HIL only)

```sh
. ~/export-esp.sh                     # espup toolchain
cargo install espflash --locked       # one-time; lands in ~/.cargo/bin
cargo build --release                 # the firmware image the run flashes
```

`export-esp.sh` does not put `~/.cargo/bin` on `PATH`; the runner finds
`espflash` there regardless.

### 3.4 AWS (HIL only)

```sh
source aws-env.sh                     # credentials for the account holding the thing
```

The IoT data endpoint is read from `cfg.toml` (`iot_endpoint`, the same value the
firmware is built with); pass `--iot-endpoint` to override. The device policy
**must** grant the named-shadow topics — see §8.1; this was missing and is the
single most likely thing to be missing again in a fresh account.

### 3.5 Network — the part that actually bites (HIL only)

The OPC UA server runs inside `gateway-hil` on the workstation and the **device
connects inbound to it**. Three things must hold:

1. The server binds `0.0.0.0` (the runner does this), not `127.0.0.1`.
2. `--server-host` is a LAN address the device can route to (same subnet as the
   device's DHCP lease is simplest). The runner refuses a loopback address.
3. **The workstation's firewall admits inbound TCP on `--server-port`.**

Point 3 is not hypothetical: the macOS Application Firewall on a managed machine
blocked every on-device run from the workstation this was developed on, and a
managed profile cannot be overridden locally. Verify from a *different* host
before blaming the firmware:

```sh
nc -z -w 3 <workstation-lan-ip> 4855 && echo reachable
```

Testing from the workstation itself proves nothing — loopback bypasses the host
firewall entirely, which is exactly why the loopback layer works there.

If the workstation cannot admit the connection, run `gateway-hil` from a machine
that can (any Linux box on the device's LAN — it is plain Rust with no
workstation-specific dependency), or run the standalone server there and keep
only the AWS side local:

```sh
cargo run -p opcua-test-server --target host-tuple -- --bind 0.0.0.0 --host <that-host-ip>
```

**On WSL2 there is a fourth requirement.** WSL2's default networking is NAT, so
the server binds an address (e.g. `172.20.x.x`) that exists only inside the VM
and no device on the LAN can route to. `--server-host` must be the **Windows
host's** LAN IP, with a port proxy forwarding to WSL, created from an
Administrator PowerShell:

```powershell
netsh interface portproxy add v4tov4 listenaddress=0.0.0.0 listenport=4855 `
    connectaddress=<wsl-ip> connectport=4855
netsh advfirewall firewall add rule name="OPCUA test 4855" dir=in `
    action=allow protocol=TCP localport=4855
```

The WSL IP changes across reboots, so re-check it (`hostname -I`) and update the
proxy.

The advertised endpoint URL does not matter: the firmware connects straight to
its configured endpoint and skips discovery precisely because "servers behind
NAT or Docker routinely advertise unreachable hostnames"
(`gateway-opcua/src/session.rs`).

---

## 4. The tag catalogue

15 addresses: 14 nodes the server creates, and 1 it deliberately does not.
Every entry earns its place by proving a distinct behaviour. Defined in
[`opcua-test-server/src/catalogue.rs`](../opcua-test-server/src/catalogue.rs).

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
| `Line1.Faulty` | Double | 1 s | `BadDeviceFailure` while the fault is injected | a Bad StatusCode travels as the optional **4th** row element |
| `Line2.Level` | Double | 5 s | slow ramp | a second scan rate ⇒ a second subscription |
| `Line2.Mode` | Int16 | 5 s | cycles 1–3 | Int16 → bare JSON number |
| `Line1.DoesNotExist` | — **absent** | 1 s | — | one unknown NodeId is reported as failed **without** taking the other 14 down (requirements finding A4) |

The server steps values every 500 ms and writes each as a full `DataValue`
(value, status, source and server timestamps), which is how `Line1.Faulty` gets
its Bad status. The fault starts injected and is written only on a transition,
never re-written every tick — that would hide whether the gateway reports by
exception. `set_fault(false)` turns it into a Good `123.45`.

---

## 5. Wire contracts

All four payloads below are from the real bring-up run against thing
`28848553144F`. `opcua-test-server` produces byte-identical bundles for the same
tag list.

### 5.1 Tag bundle — retained on `cmd/<thing>/opcua/tags/v1`

280 bytes for 15 tags, grouped by scan rate. SHA-256
`d037a0b510d550e108342d39fa080ff7b1588509169d19ced87d911a8ad50754`.

```json
{"v":1,"g":[{"r":1000,"a":["Line1.Temp","Line1.Pressure","Line1.Running","Line1.State","Line1.Counter","Line1.BigCounter","Line1.Serial","Line1.Blob","Line1.BatchId","Line1.Profile","Line1.Static","Line1.Faulty","Line1.DoesNotExist"]},{"r":5000,"a":["Line2.Level","Line2.Mode"]}]}
```

The digest is computed over **exactly these bytes**, so the builder serialises
without whitespace and in this field order. That is a contract, not a formatting
preference — reformat the JSON and the device correctly rejects it.

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
`sha256(bundle) == cfg.sha256` — in one place, `gateway_opcua::AppliedConfig::new`,
whether the bundle came from MQTT, from NVS or from a file on a laptop. The two
planes therefore cannot desynchronise silently, and re-delivering the same
`cfg.v` is a no-op.

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

`v` is the configuration version the rows were **collected** under. Every sample
carries it from the subscription that produced it, and a batch never mixes two
(§8.17).

### 5.4 Shadow `state.reported`

```json
{"fw":"0.0.1","cfg_v":1,"state":"running","applied":14,"failed":1,
 "failed_sample":[{"a":"Line1.DoesNotExist","s":2150891520}],"srv_publish_ms":1000,
 "last_error":null,"uptime_s":312,"free_heap":118432}
```

`failed_sample` entries are `{a: address, s: StatusCode}`; §4.1 of the
requirements still shows bare strings (§9.5).

---

## 6. The scenarios

### 6.1 Loopback suite — `gateway-opcua/tests/scenario.rs`

Each test starts its own server on an ephemeral loopback port and its own
driver. They observe the client through `Client::reported()` (the future shadow
`reported`) and `Client::drain()` run through the real batcher (the future
telemetry payload).

| Test | Asserts | Mirrors |
| --- | --- | --- |
| `catalogue_documents_pass_the_gateways_own_parser` | the independently-built documents parse with `gateway-core` and expand to the expected NodeIds | `preflight` |
| `documents_fit_their_transport_budgets` | bundle ≤ `MAX_BUNDLE_BYTES`, shadow < 8 KB | `preflight` |
| `a_secured_configuration_is_refused_never_downgraded` | `Basic256Sha256` / `SignAndEncrypt` refused, naming the value | `reject_security` |
| `a_bundle_whose_digest_does_not_match_is_refused` | digest mismatch refused, error names `sha256` | `reject_digest` |
| `a_bundle_for_another_version_is_refused` | `bundle.v != cfg.v` refused | — |
| `provision_applies_every_present_tag_and_names_the_absent_one` | `running`, 14 applied, 1 failed with `BadNodeIdUnknown`, one session; re-applying the running config is a no-op | `provision` |
| `telemetry_matches_the_wire_contract_for_every_type` | every catalogue encoding; `f32` shortest decimal; `i64`/`u64` exact past 2⁵³; base64, GUID, arrays; Good omitted, Bad as element 4; `Line1.Static` exactly once; 5 s group slower than 1 s | `telemetry` |
| `status_transitions_are_reported_as_they_happen` | Bad → Good → Bad on `Line1.Faulty` reaches telemetry each time | — |
| `reconfiguration_is_applied_live_on_the_same_session` | v2 applied without reconnecting; every later sample stamped v2; removed tags stop; nothing delivered twice | `reconfig` |
| `a_new_endpoint_is_a_new_session` | a config naming another server connects to it, and survives the old one disappearing | — |
| `namespace_uri_rescues_a_wrong_index` | `ns: 99` + `ns_uri` → all applied | `ns_uri` |
| `a_wrong_namespace_without_a_uri_fails_loudly` | `ns: 99` alone → `error`, "monitored items were rejected" | — |
| `server_loss_is_noticed_backed_off_and_recovered_from` | killed server → `error` within 5 s, "session lost"; ≤ 7 reconnect attempts in 6 s; same endpoint back → `running`, all items re-created | `server_down` |
| `a_silent_link_is_noticed_by_the_keepalive` | a link that swallows bytes without closing → `error` via keep-alive; link back → `running` | — |
| `an_unreachable_endpoint_is_retried_with_backoff` | a port that refuses at boot → `error` naming the endpoint, 2–8 attempts in 6 s | — |
| `disable_idles_the_driver_and_stops_telemetry_then_resumes` | `idle` within 6 s, config kept, no samples while disabled, re-enable resumes | `disable` |
| `disable_completes_while_the_server_is_down` | disable after a server loss → `idle` | `disable` (§8.11) |
| `disable_completes_while_the_server_is_hung` | disable while `CloseSession` can never be answered → `idle` within the 3 s bound; the next config is taken up | `disable` (§8.11) |
| `every_catalogue_encoding_is_exercised` | the catalogue covers every encoding | — |

### 6.2 On-device scenario — `gateway-hil`

Ten phases, run in order because they share state — `reconfig` is only
meaningful once `provision` has applied something. Individually selectable with
`--phases` so a single failure can be re-run without repeating the whole thing.

#### Phase 1 — `preflight` (no device involved)

| | |
| --- | --- |
| **Action** | Start the server; run the firmware's own OPC UA client (`gateway-opcua`) against it over loopback; size-check the bundle and the shadow document. |
| **Asserts** | The client reaches `running` with 14 items applied and exactly the absent tag failed; bundle ≤ 10 KiB (`MAX_BUNDLE_BYTES`); desired document < 8 KB. |
| **Why first** | If this fails, nothing downstream is interpretable. It does **not** prove the device can reach the host (§3.5). |

#### Phase 2 — `provision`

| | |
| --- | --- |
| **Action** | Publish bundle v1 retained, then `state.desired` pointing at it. |
| **Asserts** | `state=running`, `cfg_v=1`; `applied=14`; `failed=1`; `failed_sample` names `Line1.DoesNotExist`; **the other 14 tags still run** (finding A4); `free_heap > 40 KB`; serial log contains the `UNENCRYPTED and UNAUTHENTICATED` warning (NFR §7) and `OPC UA synced: 2 subscriptions`. |

#### Phase 3 — `telemetry`

| | |
| --- | --- |
| **Action** | Wait for ≥3 batches to reach CloudWatch, counting only batches written since the run started. |
| **Asserts** | Every batch stamped with the applied `cfg.v`; the absent tag never produces a sample; **per-type encoding** for every catalogue tag; Good status omitted (3-element row); Bad status present as element 4 with `0x808B0000`; `Line1.Static` reported ≤2 times while `Line1.Temp` reported many (report-by-exception); the 5 s group reports less often than the 1 s group; no batch exceeds `batch_max_bytes`. |

#### Phase 4 — `reconfig`

| | |
| --- | --- |
| **Action** | Publish v2: drop the `Line2.*` group and the absent tag, move `Line1.Temp` from the 1 s to the 5 s group. |
| **Asserts** | `cfg_v=2` with `state=running` and **no reboot or reflash**; all v2 tags applied, `failed=0`; serial log shows `config v1 -> v2`; telemetry re-stamped `v:2` and the removed addresses stop appearing. |

#### Phase 5 — `ns_uri`

| | |
| --- | --- |
| **Action** | Publish v3 with a deliberately **wrong** `ns: 99` but a correct `ns_uri`. |
| **Asserts** | Device resolves the URI against the server's NamespaceArray, renumbers every NodeId, and applies all items. Serial log must **not** contain a namespace-fallback warning. |

#### Phase 6 — `reject_security`

| | |
| --- | --- |
| **Action** | Publish v4 with `sec_policy: "Basic256Sha256"`, `sec_mode: "SignAndEncrypt"`. |
| **Asserts** | `last_error` names the offending value; `cfg_v` **stays 3**. A gateway that silently downgrades to an unsecured channel is worse than one that refuses. |

#### Phase 7 — `reject_digest`

| | |
| --- | --- |
| **Action** | Publish v5 whose shadow `cfg.sha256` is 64 zeros while the bundle is real. |
| **Asserts** | Bundle refused with a digest error; `cfg_v` stays 3. |

#### Phase 8 — `server_down`

| | |
| --- | --- |
| **Action** | Re-establish a good config (v6), then stop the OPC UA server mid-session. Restart it on the same endpoint afterwards. |
| **Asserts** | Device reaches `state=error`; retry delays in the serial log show real backoff, not a tight loop; **the MQTT/OTA path stays alive** while OPC UA is down; the device reconnects **by itself** once the server returns and re-creates every item. |

#### Phase 9 — `disable`

| | |
| --- | --- |
| **Action** | `enabled: false` under a fresh version, wait, then re-enable under another. |
| **Asserts** | Driver goes `idle`; **zero** telemetry batches in a 45 s window (measured after a 30 s drain, because the IoT-rule→CloudWatch hop lags); re-enabling returns it to `running`. |

#### Phase 10 — `reboot`

| | |
| --- | --- |
| **Action** | Re-attach the serial monitor, which reboots the chip and captures the new boot from its first line (§8.20). |
| **Asserts** | Serial log contains `booting with cached OPC UA config v<N> (<n> tags)` — i.e. the NVS-cached bundle is applied **before** the cloud answers — and the device returns to `running` on the same `cfg_v`. |

---

## 7. Running it

### 7.1 Host

```sh
cargo test -p gateway-core -p gateway-opcua -p opcua-test-server --target host-tuple

# one test, with the driver's own log lines (the device's serial-console view)
RUST_LOG=info cargo test -p gateway-opcua --target host-tuple -- --nocapture server_loss
```

The client on a laptop — against the in-process test server by default, or any
server with `--endpoint`. Telemetry goes to stdout exactly as it would be
published; health changes to stderr:

```sh
cargo run -p gateway-opcua --example local_gateway --target host-tuple
cargo run -p gateway-opcua --example local_gateway --target host-tuple -- \
    --endpoint opc.tcp://192.168.1.50:4840 --ns-uri urn:plc:ns --bundle tags.json
```

The standalone server, for a device to dial, with fault injection on stdin
(`fault on`/`fault off`, `freeze`/`thaw`, `quit`):

```sh
cargo run -p opcua-test-server --target host-tuple -- --bind 0.0.0.0 --host <lan-ip>
```

### 7.2 Hardware-in-the-loop

```sh
source aws-env.sh
. ~/export-esp.sh
cargo build --release                     # the image --flash writes

cargo run -p gateway-hil --target host-tuple -- \
    --thing 28848553144F \
    --server-host 192.168.50.28 \         # LAN IP the DEVICE can reach
    --port /dev/cu.usbmodem21401 \
    --flash \
    --cleanup
```

| Flag | Effect |
| --- | --- |
| `--flash` | Flashes the existing release build with `partitions.csv` into `ota_0` first. |
| `--phases a,b` | Run a subset. |
| `--cleanup` | Clear the retained tag bundles the run published, so a later boot cannot pick up a stale config from a forgotten run. |
| `--artifacts DIR` | Where the serial log and JSON summary land (default `gateway-hil/artifacts/`). |
| `--iot-endpoint HOST` | IoT data endpoint, if not the one in `cfg.toml`. |
| `--offline` | Run the phases that need no route from the device to this host (§7.3). Requires `--port`. |

Exit code is 0 only if every check in every phase passed, 1 on a failed check,
2 if the run itself could not proceed.

### 7.3 Offline mode — when the device cannot reach this host

On a workstation whose firewall refuses the device's inbound connection
(§3.5), the OPC UA data path cannot be exercised, but everything around it can:

```sh
cargo run -p gateway-hil --target host-tuple -- \
    --thing 28848553144F --server-host <this-host-lan-ip> \
    --port /dev/cu.usbmodem21401 --flash --offline --cleanup
```

The device is pointed at this host's LAN address, where nothing listens: the
test server is bound to loopback only, for `preflight`. Every assertion comes
from the serial log and the shadow. Config versions continue from whatever the
shadow last asked for, so a cached configuration can never be mistaken for a
re-delivery (§9.9).

| Phase | Asserts |
| --- | --- |
| `preflight` | As in §6.2. |
| `offline_config` | The config arrives on both planes and is cached to NVS. The device runs the new firmware and logs the unencrypted-link warning. The failure is reported in the shadow, naming the endpoint, rather than an endless `connecting` (§8.12). Every connect attempt ends by itself. Reconnects back off at ≥ 1 s. No panic and no failed allocation. |
| `offline_disable` | `enabled: false` idles the driver while it is still trying to connect (§8.11), and no attempts follow. |
| `offline_reject_security` | A secured config is refused explicitly, shown in `last_error`, and neither cached nor applied. |
| `offline_reject_digest` | A bundle with a mismatched SHA-256 is refused, shown in `last_error`, and neither cached nor applied. |
| `offline_reenable` | The next valid config is cached and takes the driver out of `idle` (the wedged driver of §9.3 ignored it). |
| `offline_reboot` | After a USB reset the NVS-cached config drives the driver before the cloud answers. |

What it cannot show — the session, telemetry and its encoding, live
reconfiguration, namespace resolution, and recovery once a server returns —
needs a host the device can reach. [`HIL_ON_WINDOWS.md`](HIL_ON_WINDOWS.md) is
a ready-made prompt for running the full scenario from a Windows PC.

---

## 8. Defects found

§8.1–8.3 were found before a single scenario phase ran. §8.4–8.8 were found by
the first run that reached hardware, each one uncovered only after the previous
was fixed. §8.9 collects the old harness's own bugs. §8.10–8.16 were found by the
loopback suite on its first run — each is a failing test against the previous
code, listed in §9.1 — and §8.17–8.19 while moving the client into its own crate.
§8.20 was found by the first on-device run of the Rust runner.

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
called before the runtime is built. Since the move to `gateway-opcua`, a runtime
that cannot be built is also no longer silent: `spawn_thread` returns the error,
and the firmware puts it in `reported.last_error` with `state: error` while
keeping MQTT and OTA up.

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

### 8.4 The endpoint carried no Anonymous token policy — every session was rejected

```
Sending CreateSession request; …                       <- succeeds
Sending ActivateSession request; … session_id=1
session:1 Cannot find user token type Anonymous for this endpoint, cannot connect
BadSecurityPolicyRejected
```

`src/opcua/session.rs` (now `gateway-opcua/src/session.rs`) built its endpoint with `EndpointDescription::from(&str)`,
and that conversion leaves `user_identity_tokens` **empty**
(`crates/async-opcua-types/src/impls.rs`). Because the firmware deliberately
skips discovery (§2 of that file's header — servers behind NAT advertise
unreachable hostnames), nothing else ever populated it. `ActivateSession` then
looked for the policy matching the `IdentityToken::Anonymous` being presented,
found an empty list, and refused.

The test server was innocent: querying its endpoints directly returns
`('anonymous', UserTokenType.Anonymous)` with `SecurityPolicy None`.

Fixed by constructing the endpoint with `UserTokenPolicy::anonymous()`, whose
`policy_id` is `"anonymous"` — exactly what the server advertises.

> This is the nastiest of the set in the field: `CreateSession` **succeeds**, so
> the server looks reachable and the symptom is an endless reconnect loop
> against a server that is answering perfectly.

### 8.5 `NodeId::from_str` compiled a regex — a 200 kB allocation on a 320 kB device

```
OPC UA driver: connecting -> syncing
memory allocation of 200000 bytes failed
```

Decoding the backtrace against the ELF:

```
driver::sync → NodeId::from_str → LazyLock<Regex>
             → regex_automata::nfa::thompson::compiler → handle_alloc_error
```

`crates/async-opcua-types/src/node_id/mod.rs` parsed `ns=2;s=Tag` with
`^(ns=(?P<ns>[0-9]+);)?(?P<t>[isgb]=.+)$`. Building that pattern's NFA wants
~200 kB in one block; the whole free heap is smaller. The first tag address
parsed therefore aborted the process.

Replaced with a hand-rolled parser. To keep the patch to *allocation* behaviour
only, it was differential-tested against the original regex over 2444 inputs
(hand-picked edge cases plus brute-forced 4-char strings over `n s = ; 2 i a`).
The first attempt disagreed on three newline cases — regex `.` excludes `\n` —
so that quirk was preserved deliberately; the second pass agreed on all 2444.

> `regex` is still used by `expanded_node_id.rs`, `qualified_name.rs`,
> `relative_path.rs` and `numeric_range.rs` in the same crate. This firmware
> does not reach those paths today. Each is the same landmine.

### 8.6 The generated type table cost 9 kB, claimed after TLS had taken the heap

```
OPC UA synced: 2 subscriptions, 14 items applied, 1 failed
memory allocation of 9220 bytes failed
```

`opcua_types::generated::types::TYPES` is a `LazyLock` holding a decoder per
generated OPC UA type. Filling its hash maps costs ~9 kB contiguous, and it is
built on the **first `ExtensionObject` decode** — which lands after mbedTLS and
the OPC UA session have taken their share. Measured: `free_heap` was 232 kB at
MQTT connect and under 9 kB two seconds later.

Fixed with `preload_types()` in the vendored crate, called from `main()` before
`Peripherals::take()`. The table is then paid for out of ~292 kB.

Two size budgets were cut alongside it, in `gateway-opcua/src/session.rs`:
`MAX_MESSAGE_SIZE` 64 KiB → 16 KiB, `MAX_CHUNK_SIZE` 16 KiB → 8 KiB,
`MAX_CHUNK_COUNT` 8 → 4.

> The 16 KiB message budget clears the ~15–25 KiB that
> `OPCUA_CLIENT_REQUIREMENTS.md` §D5 estimates for a 250-tag publish response
> only because a message is bounded by `MAX_CHUNK_COUNT × MAX_CHUNK_SIZE`
> chunks, not one contiguous buffer. **This has not been verified at 250 tags**
> — only at the 15 the harness uses. Revisit against a real payload before
> approaching the cap.

### 8.7 `SettingsStore::try_load` always allocated the 10 kB maximum

```
OPC UA driver: syncing -> running
memory allocation of 10240 bytes failed   <- from ConfigPlane::handle
```

`try_load` read the cached bundle into `vec![0u8; MAX_BUNDLE_BYTES]` regardless
of what was stored — 10 KiB for a bundle that is 280 bytes in this scenario. It
survives at boot when the heap is free, but `ConfigPlane::handle` calls `load`
again on **every shadow delta**, by which point it does not fit.

Fixed by sizing both buffers from `nvs.blob_len()`. The length is bounded
against the existing ceilings before it is used: it comes from flash, and a
corrupt entry would otherwise drive the allocation.

### 8.8 mbedTLS pinned its record buffers for the life of the connection

With §8.4–8.7 fixed the device reached `running`, then aborted on allocations of
**1008, 1172 and 3620 bytes** during reconfiguration — the heap was simply gone,
not fragmented by one bad request.

`CONFIG_MBEDTLS_SSL_IN_CONTENT_LEN=16384` was held for the whole MQTT session
and `CONFIG_MBEDTLS_DYNAMIC_BUFFER` was unset. Setting the latter frees the
record buffers between records.

`IN_CONTENT_LEN` was **left at 16384 deliberately**. Shrinking it buys ~8 kB but
risks choking on a large TLS record from AWS IoT, which carries Jobs and OTA as
well as telemetry — a worse failure than the one being fixed.

This helped (43 → 45 of 56, device stable at the end of the run rather than
frozen) but did **not** eliminate the aborts. See §9.4.


### 8.9 Harness defects (Python harness, retired)

Bugs in the test harness itself, found by the first hardware runs. Each made a
healthy device look broken, which is the expensive kind of test bug. The Rust
runner keeps every fix.

| # | Defect | Effect, and what the harness does now |
| --- | --- | --- |
| a | Docs claimed `export-esp.sh` puts `espflash` on `PATH`; it only adds the xtensa toolchain | The run died after the build. `device::espflash()` resolves `~/.cargo/bin` explicitly. |
| b | Monitor attached after flashing with `--no-reset` | An ESP32-S3 re-enumerates its USB-JTAG on every reset, so the monitor held a node that never delivered a byte — every log assertion failed. `wait_for_port()` waits for a stable inode before attaching. (The deeper problem with `--no-reset` is §8.20.) |
| c | `reported` was read without checking its age | A shadow written **4.6 hours earlier** satisfied the predicates, and eight failures described firmware that was not running. `Cloud::require_fresh_since(t0_ms)` rejects any document older than the run. |
| d | `failed_sample` assumed bare strings | The firmware reports `{"a", "s"}` objects; the harness accepts either shape. The spec mismatch itself is still open (§9.5). |
| e | `disable` republished an already-applied version | A no-op by definition, so the phase reported a failure that never happened. Versions are 1–8 with no collisions. |
| f | `telemetry` counted batches from up to 120 s before the run | A previous run's batches could fail the version check (a possible reading of §9.6). The window now starts no earlier than the run. |

### 8.10 The library's own reconnect loop hid a dead server (was §9.7)

```
✗ device noticed the server was gone — state=running
```

`async-opcua`'s session event loop reconnects on its own when the transport
drops — up to 10 times, backing off from 1 s to 30 s — and the driver's
liveness check was `event_loop.is_finished()`. While the library retried, the
handle was not finished, so a dead server was reported as `running` for minutes
and the driver's own backoff never engaged.

This also recasts an earlier observation: the "1 s → 2 s → 4 s → 8 s → 16 s"
retry spacing verified on hardware was the **library's** schedule, not the
driver's (whose delays are jittered).

**Fixed** by making the driver's backoff the only retry mechanism:
`session_retry_limit(0)` for the initial connect and
`Session::disable_reconnects()` once the session is up, so a lost transport ends
the event loop and the driver sees it at once. Steady state is now event-driven —
the driver awaits the session's end rather than polling every 500 ms.
Loopback: `server_loss_is_noticed_backed_off_and_recovered_from` — `error`
within 5 s, not "never".

### 8.11 `Session::disconnect` could wait forever (was §9.3)

```
OPC UA disabled by configuration
W session:1 Failed to close session, channel will be closed anyway: BadConnectionClosed
E Failed to send disconnect message, queue full: BadConnectionClosed
```

`disconnect()` sends `CloseSession`, closes the channel, and then waits for the
event loop to publish `SessionState::Disconnected`. The event loop only
publishes that from a **connected** transport closing. Mid-reconnect (§8.10) it
never did, so `connection.shutdown().await` never returned, `Idle` was never
reported, and the wedged driver ignored every later configuration — which is
why `reboot` also failed.

**Fixed** twice over: after §8.10 a lost session has already ended, so shutdown
skips `disconnect` entirely; and a live session gets 3 s to acknowledge
`CloseSession` before it is abandoned regardless. Loopback:
`disable_completes_while_the_server_is_down` and
`disable_completes_while_the_server_is_hung` — the latter stages the harder case
the device run could not, a session that still looks healthy while the server
never answers.

### 8.12 A server down at boot left the driver in `connecting` forever

`Connection::connect` awaited `Session::wait_for_connection()`, which "never
returns" (its own documentation) if the event loop ends. The event loop ends when
the library's retries run out, so against a server that was down at boot the
driver sat in `connecting` through the whole internal retry schedule and then
**for good** — no `error`, no `last_error`, no backoff. **Fixed** by racing
`wait_for_connection` against the event loop and a deadline. Loopback:
`an_unreachable_endpoint_is_retried_with_backoff`.

### 8.13 A silent link was never noticed

A peer that stops answering without closing the socket — a pulled cable, a hung
PLC — sends no RST, so nothing closes the transport until TCP gives up. The
client does send keep-alive reads, but `max_failed_keep_alive_count` defaults to
**0, "never close"**. **Fixed** by closing the session after 2 failed keep-alives,
with the request timeout tied to the configured keep-alive (2×, clamped to
5–20 s): roughly 70 s to notice at the default 10 s keep-alive, about 16 s at
1 s. Loopback: `a_silent_link_is_noticed_by_the_keepalive`, using
`opcua_test_server::Blackhole`.

### 8.14 A live reconfiguration never deleted the old subscriptions

`resync` created new subscriptions for the new configuration and left the old
ones in place. They kept delivering — removed tags reappeared in telemetry,
tags present in both versions arrived twice, and every reconfiguration leaked a
set of subscriptions on a device that is short of heap already. **Fixed** by
deleting the previous subscriptions first, and by a generation flag in their
sink so a notification already in flight is dropped rather than misfiled.
Loopback: `reconfiguration_is_applied_live_on_the_same_session`.

### 8.15 A new endpoint was resynchronised on the old session

`resync` diffed only the tag lists. A configuration that moved the gateway to a
different PLC (or changed its session parameters) was applied by rebuilding the
subscriptions **on the old session** — the gateway kept reading the old PLC
under the new configuration version. **Fixed**: any change to `instance` is a new
session. Loopback: `a_new_endpoint_is_a_new_session`.

### 8.16 Unknown NodeIds counted as applied on a lenient server

On its first run the loopback suite reported 15 applied, 0 failed. As §2.5
explains, `async-opcua-server` accepts a monitored item for a node that does not
exist and reports the problem only in the data, which is legal. On such a server
a typo'd address counts as *applied*, never reaches `failed_sample`, and finding
A4 silently stops holding. **Fixed** in the gateway: before subscribing, each
chunk's `NodeClass` is read (one integer per tag, one request per 50) and
`BadNodeIdUnknown`/`BadNodeIdInvalid` tags are failed without being subscribed.
A failure of that read is not fatal; the server then judges as before. Loopback:
`provision_applies_every_present_tag_and_names_the_absent_one`.

### 8.17 Telemetry could be stamped with the wrong config version (was §9.6)

Samples carried no version; the publisher stamped each batch with the version it
was built for. The firmware rebuilds the publisher the moment a configuration is
*dispatched*, while the driver is still delivering samples from the old
subscription — so those went out labelled with the new version. **Fixed** at the
source: each sample carries the version of the subscription that produced it,
and the batcher closes a batch when the version changes
(`FlushReason::Version`). A batch's `v` is true by construction.
`gateway-core` unit tests: `a_new_config_version_closes_the_batch`,
`versions_never_mix_within_a_batch`.

### 8.18 A failed resync leaked its session

When a resynchronisation failed, the `Connection` was dropped without
`shutdown()`. Dropping a tokio `JoinHandle` detaches the task rather than
cancelling it, so the old event loop kept running — holding the session and its
subscriptions — behind a driver that believed it had none. **Fixed**: every error
path shuts the connection down, and `Connection` aborts its event loop on drop as
a safety net.

### 8.19 The partition-table glob nested itself until the firmware build failed

```
Using esp-idf v5.5.3 at '…/.embuild/espressif/esp-idf/v5.5.3'
Error: File name too long (os error 63)
```

`ESP_IDF_GLOB_PARTITIONS_0 = "partitions.csv"` in `.cargo/config.toml` was
unanchored. `embuild` walks the glob base (the repo root) following links, and an
unanchored pattern matches at **any depth** — so each build copied every
`partitions.csv` it found into its OUT_DIR under its relative path, including the
ESP-IDF examples under `.embuild/` and the copies left in *earlier* esp-idf-sys
OUT_DIRs under `target/`. One OUT_DIR already held 572 nested copies, the longest
path 951 bytes; the next rebuild passed macOS's 1024 and failed. This would have
struck CI as well, whose `target/` is cached. **Fixed** by anchoring the pattern:
`"/partitions.csv"`.

### 8.20 `espflash monitor --no-reset` left the chip in its bootloader (harness)

```
[… INFO ] Serial port: '/dev/cu.usbmodem212101'
[… INFO ] Connecting...
[… INFO ] Using flash stub
                                   <- and then nothing, for minutes
```

`espflash monitor` (4.x) cannot attach to running firmware: it first connects
to the chip's ROM loader, which means resetting it into download mode. By
default it then hard-resets the chip so the firmware boots. `--no-reset` is
`--after no-reset`: it skips only that second reset, **leaving the chip in the
bootloader**. The firmware stops, and the monitor shows exactly what a silent
device would.

Both harnesses attached with `--no-reset` whenever they had not just flashed.
That covers the `reboot` phase, whose "booting with cached OPC UA config" line
therefore could never appear (the open §9.8), and every run started without
`--flash`, which froze the device before the first phase. `--before
no-reset-no-sync` does not help either: espflash then fails to connect to
running firmware.

**Fixed** by always attaching with espflash's default reset. An attach is
therefore a reboot, which the runner now uses deliberately: every run starts
from a clean boot captured from its first line, and `reboot` is a re-attach.
The reboot checks also no longer accept a shadow report written before the
reboot: its `uptime_s` must fit inside the time since.

---

## 9. Current status

### 9.1 Scorecard

**Host (2026-09-27).**

| Suite | Result |
| --- | --- |
| `gateway-core` unit | **94/94** |
| `opcua-test-server` unit | **9/9** (incl. the §5.1 bundle, byte for byte) |
| `gateway-opcua` loopback | **19/19**, ~20 s |
| `gateway-hil` unit, and its `preflight` phase run locally | **2/2**, **3/3** |
| Firmware `cargo build --release` | clean; image 5.32 MB of the 7.34 MB `ota_0` slot |
| `clippy -D warnings`, `rustdoc -D warnings`, `rustfmt --check` | clean — firmware (xtensa) and host crates (esp toolchain and stable 1.96) |
| `cargo audit` | clean, with RUSTSEC-2023-0071 ignored for the reason recorded in `.cargo/audit.toml` |

The loopback suite was run four times in a row without a failure. Measured
there: a killed server is `error` before `stop()` returns, a silent link is
noticed after 16.0 s at a 1 s keep-alive, and a disable against a hung server
completes in 3.05 s.

The same loopback suite against the driver and session as they stood after the
last hardware run fails **9 of 19**:

| Failing test | Defect |
| --- | --- |
| `server_loss_is_noticed_backed_off_and_recovered_from` | §8.10 |
| `disable_completes_while_the_server_is_down` | §8.10 |
| `disable_completes_while_the_server_is_hung` | §8.11 |
| `an_unreachable_endpoint_is_retried_with_backoff` | §8.12 |
| `a_silent_link_is_noticed_by_the_keepalive` | §8.13 |
| `reconfiguration_is_applied_live_on_the_same_session` | §8.14 |
| `a_new_endpoint_is_a_new_session` | §8.15 |
| `provision_applies_every_present_tag_and_names_the_absent_one` | §8.16 |
| `telemetry_matches_the_wire_contract_for_every_type` | §8.16 |

**Hardware, offline mode (2026-09-27).** Firmware at this branch's head,
flashed to thing `28848553144F` (WiFi). The device dialled the workstation,
whose firewall drops the connection silently (§3.5).

| Phase | Result | Note |
| --- | --- | --- |
| `preflight` | **3/3** | |
| `offline_config` | **9/9** | both planes and the NVS cache verified; `free_heap` 74 KB while retrying |
| `offline_disable` | **3/3** | `idle` 3 s after the disable, with the server unreachable |
| `offline_reject_security` | **3/3** | |
| `offline_reject_digest` | **3/3** | |
| `offline_reenable` | **3/3** | |
| `offline_reboot` | **3/3** | the `booting with cached OPC UA config` line of §9.8, now observed |

27/27, with no panic, failed allocation or watchdog reset in either serial
log. The first attempt scored 26/28: the monitor re-attach froze the chip in
its bootloader, which is §8.20, fixed before the rerun.

Seen on hardware along the way, as §8.12 intended: the device booted on a
cached config pointing at a host that no longer answers. The attempt ended at
its 40 s deadline and was reported. The new configuration, already waiting,
then cut the backoff short, and the driver moved to the new endpoint. The
previous firmware stayed in `connecting` in exactly this situation.

**Hardware, online.** The last full run was 2026-08-01 with the retired Python
harness: 48/56. Its eight failures traced to §9.3, §9.4, §9.6, §9.7 and §9.8;
all but §9.4 are now fixed. Expected on the next online run: `disable`,
`reboot` and `server_down` pass; the heap assertion in `provision` stays open
(§9.4).

### 9.2 How to read a failing run

Two failure modes are *not* firmware defects and have both been mistaken for
them:

1. **A stale `reported` block.** Guarded since §8.9c — a rejected document prints
   `reported: (stale — …)` and the failure says so explicitly.
2. **A device that is crash-looping.** Symptom: every phase from some point on
   reports an *identical* snapshot, including the same `uptime_s`. That is one
   fault reported ten times, not ten faults. Check
   `grep -c "memory allocation of" <serial log>` before reading further.

And one new rule: **if the loopback suite fails, fix that first.** A behaviour
it covers cannot be diagnosed faster on the device.

### 9.3 FIXED — `Command::Disable` never completed

Root cause and fix in §8.11 (and §8.10, which caused it). Four of the eight
hardware failures traced here.

### 9.4 OPEN — heap headroom is still marginal

`provision` asserts `free_heap > 40_000`; the last run reported **32428**. This
moved from 23828 after §8.8, so the fixes are working, but the assertion should
be read as the harness correctly reporting reality rather than a threshold to
relax.

**Six aborts occurred, all at device uptime 20–36 s** — the initial-sync peak —
at sizes 2048–5120 bytes. The device recovered and the final boot ran stably for
677 s, but early phases were run against a device that may reboot underneath
them.

§8.14 may account for part of this: every reconfiguration in that run leaked the
previous subscriptions. The next hardware run will tell. The remaining lever not
taken is trimming the OPC UA buffers further (`MAX_MESSAGE_SIZE` 16 K → 8 K,
`MAX_CHUNK_SIZE` 8 K → 4 K), which pushes further below the §D5 250-tag cap.
Shrinking `MBEDTLS_SSL_IN_CONTENT_LEN` was considered and rejected — see §8.8.

> There is no PSRAM on this board. `espflash board-info` reports only
> "Embedded Flash", and `OPCUA_CLIENT_REQUIREMENTS.md` §D5 states the cap is for
> "the current no-PSRAM build". Buying headroom in hardware is not an option.

### 9.5 OPEN — `failed_sample` shape contradicts the spec

`gateway_core::health::FailedTag` serialises `{"a": address, "s": statuscode}`;
§4.1 of `OPCUA_CLIENT_REQUIREMENTS.md` shows `["Chan1.Dev1.Bad", …]`. The
firmware's shape is richer and looks deliberate — it carries the StatusCode the
operator needs — but the two disagree. **Decide which is normative and correct the
other**; this is a wire contract, and the shadow has other consumers. Both test
layers accept either shape until then.

### 9.6 FIXED — telemetry batches carried two config versions

A real race, fixed in §8.17. The observation itself (`versions=[1, 7]`) may also
have been the harness reading an earlier run's batches (§8.9f); both causes are
gone.

### 9.7 FIXED — the driver did not notice a dead server

Root cause and fix in §8.10; the silent-link variant in §8.13.

### 9.8 FIXED — the boot-time cached-config line was not observed

A harness defect, not a firmware one. Re-attaching the monitor with
`--no-reset` left the chip in its bootloader, so the rebooted firmware never
ran (§8.20).

### 9.9 Environment prerequisites discovered the hard way

- **A managed macOS firewall** blocks inbound TCP to the server and cannot be
  overridden locally (§3.5). This is what the loopback layer exists to route
  around; for the HIL run, use a host that can admit the connection.
- **WSL2 NAT.** Requires a `netsh interface portproxy` on the Windows host plus
  an inbound rule, and `--server-host` set to the *host's* LAN IP (§3.5).
- **A stale `opcua` NVS partition wins silently.** `provision` always publishes
  **v1**; if the device holds a cached v1 pointing at a different server it keeps
  dialling the old address and ignores the shadow (idempotency, requirements §7).
  Clear it with `espflash erase-region --port <port> 0x13000 0xd000` — the `opcua`
  partition only; identity lives in `nvs` at `0x9000` and survives.
- **WiFi association is ~50/50 on a 40 MHz 2.4 GHz channel**, and a single
  association timeout at boot is **terminal** — the firmware logs
  `WiFi start failed` and stops, with no retry, until someone resets the board.
  Worth addressing independently of this harness.

---

## 10. Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| `cargo test` builds for xtensa and fails | `.cargo/config.toml` defaults to `xtensa-esp32s3-espidf` | Add `--target host-tuple` for every host crate. |
| Firmware build: `File name too long (os error 63)` | Unanchored partition-table glob | §8.19; `cargo clean -p esp-idf-sys --release` removes the nested copies. |
| Device: `ConnectionReset` waiting for the server's ACK; server sees nothing | Host firewall blocking inbound TCP | §3.5. Verify with `nc` from another host — never from the workstation. |
| MQTT connects then drops repeatedly, Jobs also broken | Device policy missing the named-shadow topics | §8.1 |
| `state: error`, `last_error: "OPC UA did not start: …"` | The OPC UA runtime could not be built | §8.2; the eventfd VFS registration. MQTT/OTA stay up by design. |
| `panicked at .../signal/unix.rs`, device reboot-loops | Vendored tokio patch lost | §8.3 |
| `shadow document unusable: shadow has no desired state` | No config published yet | Normal before `provision`. |
| `tag bundle rejected: bundle sha256 …` | Bundle bytes differ from what the digest was computed over | Republish; do not reformat the JSON (§5.1). |
| All items fail: "all N monitored items were rejected" | Wrong namespace or `id_type` | Check `ns_uri` is published by the server; `ns_uri` phase and `a_wrong_namespace_without_a_uri_fails_loudly` cover this. |
| `last_error: "session lost: …"`, then `running` again | The server went away and came back | Expected; backoff is 1 s doubling to 60 s with jitter. |
| `reported` never updates | Reports are throttled to state-change or every 30 s | Wait, or trigger a state change. |
| Telemetry absent from CloudWatch but the device says it published | `dt/+/opcua` rule or its log group missing | §8.1's terraform also creates them. |
| `espflash not found` | `export-esp.sh` does not add `~/.cargo/bin` | `cargo install espflash`; the runner looks in `~/.cargo/bin` itself. |
| Serial log is a few hundred bytes; every log assertion fails | Monitor attached while the USB-JTAG was re-enumerating | §8.9b |
| Monitor shows `Using flash stub`, then nothing; the device goes silent | Attached with `espflash monitor --no-reset`, which leaves the chip in its bootloader | §8.20. Attach without it; `espflash reset` recovers the device. |
| Every phase reports the same `reported` snapshot, same `uptime_s` | Device is crash-looping | §9.2. `grep -c "memory allocation of" <serial log>`. |
| `reported: (stale — nothing written since the run started)` | Device never booted, never joined the network, or is dead | Read the serial log; the shadow is from an earlier boot (§8.9c). |
| `BadSecurityPolicyRejected: Cannot find user token type Anonymous` | Endpoint built without a token policy | §8.4 |
| `memory allocation of N bytes failed`, device reboots | Heap exhaustion. Decode with `xtensa-esp32s3-elf-addr2line -e <elf> -f -C` | §8.5–8.8, §9.4 |
| Device dials a server address nobody configured | Stale cached config; same `cfg.v` is a no-op | §9.9 — erase `0x13000`. |
| `WiFi start failed … ESP_ERR_TIMEOUT`, device then silent | Association timeout is terminal, no retry | §9.9. Reset and retry. |
| Host run logs `Failed to read own certificate … Check paths, crypto won't work` | `async-opcua` looking for a certificate it does not need at security `None` | Harmless. |

---

## 11. Extending the scenario

- **A new value type**: add one `Tag` to
  [`catalogue.rs`](../opcua-test-server/src/catalogue.rs) with an `encoding`. The
  server creates the node, and both `telemetry_matches_the_wire_contract_for_every_type`
  and the HIL `telemetry` phase check its encoding — no other edit. Add a new
  `Encoding` variant if the JSON shape is new.
- **A new failure mode**: most belong in the loopback suite. `TestServer` can be
  stopped and restarted on the same port (`stop()` returns its `Options`),
  faulted (`set_fault`), frozen (`set_frozen`) and written to (`write`);
  `Blackhole` makes a link go silent; `sessions_activated()` tells a resync from
  a reconnect.
- **A new on-device phase**: an `async fn(&mut Ctx) -> Result<PhaseResult>` in
  [`gateway-hil/src/phases.rs`](../gateway-hil/src/phases.rs), a match arm in
  `run`, and its name in `PHASES`.
- **Scale toward the 250-tag cap** (follow-up F3 in the requirements): generate
  addresses in the catalogue, keep the number of distinct scan rates small, and
  watch `reported.free_heap` on hardware — the cap is supposed to be a measured
  number, and the HIL run is where that measurement has to happen.
