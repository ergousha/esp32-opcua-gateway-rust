# OPC UA Client — Requirements & Settings Specification

Status: **Decisions agreed (§9). Flash layout frozen (§5.3). Ready to implement §6.**

Scope: how the ESP32-S3 gateway obtains its OPC UA configuration, what that configuration
contains, which parts are actually implementable with `async-opcua 0.19` on ESP-IDF, and how
large tag sets are transported and executed.

### Agreed constraints (see §9 for the full record)

| Decision | Consequence |
|---|---|
| `uuid` is **not** sent to the device; the backend maps `address → uuid` | ~40 % smaller bundle |
| **Anonymous identity only** in phase 1 | No username/password, no user X.509 |
| **No mounted filesystem** | ⇒ no OPC UA certificates at all, ⇒ persistence is NVS-only (§5.2) |
| **No `data_type` field** — no casting; the server's own type is authoritative | Field removed from the schema |
| Tag cap **~250**, 1000 is a far-future ceiling | No PSRAM work in phase 1 |
| **One OPC UA server per gateway** | `instance` stays singular |

Together these mean phase 1 is `SecurityPolicy::None` + `MessageSecurityMode::None` +
`IdentityToken::Anonymous`, which conveniently removes every filesystem and RSA dependency
from the device (§2.2).

---

## 1. Review of the current implementation (`src/opcua_client.rs`)

The feature branch code is a working spike, but it is not shippable. Findings, ordered by
severity.

### 1.1 Correctness / will-not-work-on-device

| # | Finding | Impact |
|---|---|---|
| C1 | `ClientBuilder::create_sample_keypair(true)` generates an RSA-2048 keypair **on every session attempt**, in pure Rust (`rsa` crate), and writes it to `pki_dir`. | Multi-second-to-minute CPU burn per reconnect, large transient heap use, and it needs a writable filesystem. |
| C2 | `pki_dir` defaults to a relative path (`./pki`). ESP-IDF has no CWD and no mounted VFS in this project (the `storage` partition is reserved but never mounted). | `CertificateStore::ensure_pki_path()` fails → client logs "missing application instance certificate" and **any secured endpoint silently cannot work**. |
| C3 | `connect_to_endpoint_id(config.endpoint_url)` is the wrong API. `endpoint_id` is a *key into `ClientConfig::endpoints`*, not a URL. | Connection can never be established as written. Correct APIs: `connect_to_matching_endpoint(EndpointDescription, IdentityToken)` or `connect_to_endpoint_directly(...)`. |
| C4 | Worker thread stack is `32 * 1024`. It hosts a Tokio runtime + the full OPC UA stack (+ `rsa` if security is on). | Stack overflow. Realistic floor is 32 KB for `SecurityPolicy::None`, 48–64 KB with crypto. Must be measured, not guessed. |
| C5 | `tokio::spawn(session_loop.run())` — the `JoinHandle` is dropped and never awaited or supervised. | Session loop failures are invisible; the polling loop keeps issuing reads against a dead session. |
| C6 | `node_ids` built with `filter_map(... .ok())`, then `node_ids.iter().zip(results)`. | Any unparsable NodeId is silently dropped. If the server returns a differently-sized result vector the zip **misaligns values to the wrong tags** — silent data corruption. |
| C7 | `value: format!("{:?}", v)` — `Variant`'s `Debug` output is the wire format. | Not a stable contract; unparseable downstream; loses type information. |

### 1.2 Architecture / scalability

| # | Finding | Impact |
|---|---|---|
| A1 | Single global `poll_interval` with a `read()` of all nodes. | Per-tag `scan_rate` is unrepresentable. Polling also cannot do report-by-exception. Should be **Subscription + MonitoredItems**. |
| A2 | Unbounded `std::sync::mpsc::channel`. | If MQTT is down while OPC UA keeps producing, the queue grows until OOM. Needs a bounded queue with an explicit overflow policy. |
| A3 | One MQTT publish **per tag per sample**. | 1000 tags @ 1 s = 1000 publishes/s. AWS IoT caps a connection at **100 publishes/s**. Hard failure. Must batch. |
| A4 | Any read error returns `Err` and tears down the whole session. | A single `BadNodeIdUnknown` on one tag drops all 999 others. |
| A5 | OPC UA drain (`rx.try_recv()`) is interleaved with the AWS IoT Jobs/OTA handler in one 1 s `sleep` loop in `telemetry::run`. | Two unrelated concerns coupled; OTA blocks telemetry and vice versa; 1 s granularity floor. |
| A6 | Configuration is `toml-cfg` (compile-time constants). | No remote configuration at all — the reason this document exists. |

### 1.3 Testability

`opcua_client.rs` is one file with `use` statements inside functions, no trait boundaries, no
tests, and every layer (config parsing, session management, value mapping, transport) fused
together. Nothing can be unit-tested on the host because everything transitively needs a real
`Session`. This is the main thing the refactor must fix.

---

## 2. Feasibility of the proposed settings, field by field

Verified against `async-opcua-client 0.19.0` / `async-opcua-crypto 0.19.0` sources.

### 2.1 `instance`

| Field | Verdict | Notes |
|---|---|---|
| `endpoint` | ✅ Supported | `opc.tcp://host:port/path`. Note the device resolves DNS via lwIP; prefer IP or ensure DNS is configured. |
| `namespace` | ✅ Supported, but insufficient alone | `u16`. See §2.4 — namespace **index** is not stable across server restarts; namespace **URI** is. |
| `username` / `password` | ❌ **Out of scope (decided)** | `IdentityToken::UserName` exists, but under `SecurityPolicy::None` the password travels in cleartext and most servers reject `UserName` tokens on unsecured channels. Securing it requires an application instance certificate, which requires a filesystem — explicitly excluded. Phase 1 is `IdentityToken::Anonymous`. |
| `security_mode` | ⚠️ Field kept, value pinned | `None` \| `Sign` \| `SignAndEncrypt` (`MessageSecurityMode`). Only `None` is accepted in phase 1; anything else is a **validation error**, never a silent downgrade. |
| `security_policy` | ⚠️ Field kept, value pinned | Only `None` accepted in phase 1. When re-enabled: `Basic256Sha256`, `Aes128_Sha256_RsaOaep`, `Aes256_Sha256_RsaPss`. `Basic128Rsa15`/`Basic256` are deprecated — never expose them. |

The two security fields stay in the schema so the backend contract does not have to change later,
but the device rejects any value other than `"None"` with an explicit `reported.last_error`.

### 2.2 `opcua.client_pem` / `opcua.private_key` — **omitted (decided)**

Dropped from the schema entirely. The reasoning, recorded so it is not relitigated:

The original proposal conflates **two different certificates**:

**(a) Application instance certificate** — identifies the *client application*, required for
`Sign`/`SignAndEncrypt`. In `async-opcua 0.19` it is loaded **only** by `CertificateStore` from
the filesystem (`ClientBuilder::pki_dir` / `certificate_path` / `private_key_path`); there is no
public in-memory constructor. Supporting it needs a mounted SPIFFS/LittleFS partition — excluded
by decision D3.

**(b) X.509 user identity token** — authenticates the *user*. This one *would* work fully in
memory (`IdentityToken::X509(X509::from_pem(..), PrivateKey::from_pem(..))`), but with anonymous
identity agreed for phase 1 it is not needed.

Consequences of omitting both — all of them favourable for phase 1:
- `create_sample_keypair(false)` always. No on-device RSA-2048 key generation (which with the
  pure-Rust `rsa` crate on Xtensa is a multi-second-to-minute operation).
- No `pki_dir`, no mounted filesystem, no VFS dependency.
- The `rsa` crate's RUSTSEC-2023-0071 (Marvin timing attack) exposure is not on any hot path.
- Significant flash and heap saved.

When certificates return (phase 3), this section is the starting point: mount the already
reserved `storage` partition (§5.3) + write PEMs at boot + `certificate_path`/`private_key_path`.
No partition-table change — and therefore no fleet re-flash — will be needed.

### 2.3 `tags[]`

| Field | Verdict | Notes |
|---|---|---|
| `uuid` | ❌ **Removed (decided)** | 22-char short-UUID ≈ 24 B/tag ≈ 24 KB per 1000 tags, for no on-device purpose. `address` is already the tag key; the backend re-attaches the uuid on ingest. |
| `address` | ✅ | Becomes the NodeId identifier and the telemetry key. Needs an identifier-type qualifier (§2.4). |
| `scan_rate` | ✅ | Maps to `MonitoredItemCreateRequest.requested_parameters.sampling_interval` (f64 ms). Server may negotiate a different value — the *revised* value must be reported back. |
| `data_type` | ❌ **Removed (decided)** | No casting is required, so the field has no job. The server reports the true type in every `DataValue`; the device serialises the `Variant` natively (§4.3) and the type travels with the value. This also removes the need to mirror any external enum on the device. A mismatch check can be re-added later as a pure function in `value.rs` without a schema change. |

### 2.4 Fields that are **missing** and are needed

| Field | Why |
|---|---|
| `id_type` (global default, per-tag override) | `address: "slow"` implies `ns=2;s=slow`. Numeric, GUID and opaque identifiers need `i=`, `g=`, `b=`. |
| `namespace_uri` | Namespace *indexes* are assigned per server session and can change after a server reconfiguration. The URI is stable; the device should resolve URI→index from the server's namespace array at connect time, falling back to the literal `namespace` index. |
| `publishing_interval` | Distinct from `scan_rate`. Sampling is how often the server *reads*; publishing is how often it *sends*. Without it we cannot control MQTT egress rate. |
| `deadband` (abs or percent) | The single most effective lever for making 1000 tags viable — report-by-exception instead of every sample. |
| `queue_size` / `discard_oldest` | MonitoredItem parameters; needed to bound server-side buffering when the link drops. |
| `enabled` | Ability to remotely stop the OPC UA task without deleting the config. |
| `session_timeout`, `keep_alive_interval`, `connect_retry` backoff | Currently hardcoded 5 s retry. Must be tunable, with exponential backoff + jitter. |
| Telemetry policy: `batch_max_items`, `batch_max_bytes`, `batch_max_age_ms`, `topic`, `qos` | §5. |
| `config_version` + `sha256` | Idempotency + safe rollback; the device must be able to say "I applied v7". |

---

## 3. Transport of the settings — why Device Shadow alone does not work

### 3.1 Hard AWS IoT limits (verified)

| Limit | Value |
|---|---|
| Max size of one shadow JSON state document | **8 KB** (adjustable, but not to 100 KB) |
| Max shadow JSON depth | 8 |
| Shadow requests/second per shadow | 20 |
| Max MQTT publish payload | **128 KB** |
| Subscriptions per connection | 50 |
| Publishes/second per client connection | **100** |
| Client connection throughput | 512 KB/s |

### 3.2 Size of the payload

Assuming an average 24-character `address` (e.g. `Channel1.Device1.Tag0001`):

| Encoding | Bytes/tag | 250 tags | 1000 tags | Fits 8 KB shadow? | Fits 128 KB MQTT? |
|---|---|---|---|---|---|
| Originally proposed verbose objects | ~103 | ~26 KB | ~100 KB | ✗ (12× over) | borderline |
| Positional arrays, uuid kept | ~61 | ~15 KB | ~61 KB | ✗ | ✓ |
| Positional arrays, uuid dropped | ~36 | ~9 KB | ~36 KB | ✗ | ✓ |
| **Grouped by `scan_rate`, uuid + data_type dropped** ← chosen | **~27** | **~6.8 KB** | ~27 KB | ✗ | ✓ |
| Grouped + gzip + base64 | ~4–6 | ~1.5 KB | ~5 KB | ✓ (fragile) | ✓ |

Dropping `uuid` and `data_type` (decisions D1/D4) takes the bundle from ~103 to ~27 bytes per
tag — a **74 % reduction**, and the single largest win available.

Conclusion: **the tag list must not live in the shadow.** Even at the 250-tag cap the grouped
bundle (~6.8 KB) sits right at the 8 KB ceiling with no headroom for the rest of the document.
The gzip+base64 variant would fit, but it is brittle (a slightly larger tag set silently blows
the limit), it makes shadow deltas all-or-nothing, and it forces a DEFLATE decompressor
(`miniz_oxide`, ~32 KB window) onto a device we are trying to keep lean. Not the primary path.

### 3.3 Recommended two-plane design

```
┌─────────────────────────── AWS IoT Core ───────────────────────────┐
│                                                                    │
│  Named shadow  "opcua"           ≤ 8 KB   ── control plane         │
│    desired:  { enabled, instance{...}, telemetry{...},             │
│                cfg: { v: 7, sha256: "...", n: 1000 } }             │
│    reported: { cfg_v: 7, state, applied, failed, last_error, ... } │
│                                                                    │
│  Retained topic  cmd/<thing>/opcua/tags/v7   ≤ 128 KB ── data plane│
│    compact grouped tag bundle (§4)                                 │
│                                                                    │
└────────────────────────────────────────────────────────────────────┘
```

- The **shadow** carries everything small, benefits from delta semantics, and gives us a clean
  reported-state channel for observability. This is exactly what shadows are good at.
- The **retained topic** carries the tag bundle. Retained means the device gets it immediately on
  subscribe after any reboot — no request/response dance, no S3, no extra IAM. 128 KB is
  comfortably above the ~36 KB needed for 1000 tags.
- The device applies a bundle only when `sha256(bundle) == shadow.desired.cfg.sha256` **and**
  `bundle.v == shadow.desired.cfg.v`. This makes the two planes self-consistent and prevents
  applying a torn/stale config.
- **Escape hatch for very large tag sets:** if `cfg` additionally contains `url` (a presigned S3
  GET), the device downloads over HTTPS instead. The OTA path already does HTTPS downloads, so
  the code exists. Optional, later addition — not needed at the 250-tag cap.

**Rejected alternatives** (recorded so we do not revisit them):
- *Raise the 8 KB shadow quota* — adjustable, but not by an order of magnitude, and the 128 KB
  MQTT cap still applies. Does not solve it.
- *Shard across N named shadows* (~200 compact tags each ⇒ 5 shadows) — works, but 5× the
  reconciliation logic, 5× the delta handling, and no atomicity across shards.
- *MQTT-based file delivery (streams)* — designed for this, but heavier to operate (stream
  resources, job-like flow) than a retained topic, for no benefit at this size.

---

## 4. Proposed settings schema

Two representations. The **authoring form** is what humans and the backend work with; the
**wire form** is what the device receives. The backend transforms one into the other.

### 4.1 Shadow document — `$aws/things/<thing>/shadow/name/opcua`

```jsonc
{
  "state": {
    "desired": {
      "enabled": true,
      "instance": {
        "endpoint": "opc.tcp://172.17.0.1:4855",
        "ns": 2,                       // namespace index (fallback)
        "ns_uri": "urn:example:server", // preferred, resolved at connect
        "id_type": "s",                // default identifier type: s|i|g|b
        "sec_mode": "None",            // phase 1: must be "None"
        "sec_policy": "None",          // phase 1: must be "None"
        "session_timeout_ms": 60000,
        "keepalive_ms": 10000,
        "publish_ms": 1000             // subscription publishing interval
      },
      "telemetry": {
        "topic": "dt/<thing>/opcua",
        "qos": 1,
        "batch_max_items": 200,
        "batch_max_bytes": 65536,
        "batch_max_age_ms": 1000
      },
      "cfg": {
        "v": 7,
        "n": 250,
        "sha256": "9f2c...e1",
        "topic": "cmd/<thing>/opcua/tags/v7"
      }
    },
    "reported": {
      "fw": "0.0.1",
      "cfg_v": 7,
      "state": "running",              // idle|connecting|syncing|running|error
      "applied": 248,
      "failed": 2,
      "failed_sample": ["Chan1.Dev1.Bad", "Chan1.Dev1.Missing"],
      "srv_publish_ms": 1000,          // revised value from the server
      "last_error": null,
      "uptime_s": 3612,
      "free_heap": 118432
    }
  }
}
```

The `reported` block is deliberately bounded (`failed_sample` truncated to N entries, no full
tag echo) so it can never approach 8 KB.

### 4.2 Tag bundle — retained on `cmd/<thing>/opcua/tags/v<N>`

Grouped by `(scan_rate, deadband, id_type)`; the address is the only per-tag string.

```jsonc
{
  "v": 7,
  "g": [
    { "r": 1000, "d": 0.0, "a": ["slow", "Chan1.Dev1.Tag0001", "Chan1.Dev1.Tag0002"] },
    { "r": 250,            "a": ["fast", "Chan1.Dev1.Tag0500"] }
  ]
}
```

- `r` = `scan_rate` ms, `d` = optional deadband, `a` = addresses.
  Optional per-group `"i"` overrides the instance-level `id_type`.
- **No `uuid`, no `data_type`** (decisions D1/D4). `address` is the tag key end to end; the
  backend keeps the `address ↔ uuid` mapping and re-attaches the uuid on ingest, and the value's
  type travels with the value itself (§4.3).
- Each distinct `r` becomes one OPC UA subscription, so a small number of distinct scan rates
  keeps the subscription count — and therefore memory — low.

### 4.3 Telemetry payload (device → cloud), batched

Because `data_type` no longer travels in the config, the value carries its own type. JSON's
native types cover the common cases; anything else is tagged explicitly.

```jsonc
{
  "t": 1753660800000,          // batch timestamp, ms
  "v": 7,                      // config version the batch was produced under
  "d": [
    ["slow", 1753660799871, 23.5],        // [address, source_ts_ms, value]
    ["fast", 1753660799902, true],
    ["txt",  1753660799902, "RUN"],
    ["bad",  1753660799910, null, 2153775104]  // 4th element = StatusCode, present only when not Good
  ]
}
```

- `status` is omitted entirely when Good — the overwhelmingly common case — keeping the row to
  three elements.
- Types that JSON cannot represent losslessly (`Int64`/`UInt64` beyond 2^53, `ByteString`,
  `Guid`, arrays) are encoded as a tagged object `{"$t":"i64","v":"..."}` by `value.rs`. The exact
  table is a pure function with exhaustive unit tests, so it can evolve safely.

---

## 5. Runtime feasibility on this hardware

This is a separate constraint from *transporting* the config, and it is the tighter one.

Current build: ESP32-S3, **PSRAM is not enabled** in [sdkconfig.defaults](../sdkconfig.defaults).
That leaves ~320 KB internal DRAM, minus lwIP + mbedTLS + esp-mqtt + TLS session buffers.
Realistic free heap after the MQTT/TLS session is up: **~100–150 KB**.

| Cost centre | 250 tags | 1000 tags |
|---|---|---|
| Client-side MonitoredItem state (NodeId string + params + cache), ~150–300 B each | ~40–75 KB — tight but viable | ~150–300 KB ✗ exceeds free DRAM |
| One publish response carrying all `DataValue`s | ~15–25 KB | ~60–100 KB ✗ exceeds default `max_message_size` |
| MQTT egress at 1 sample/tag/s | ~20 KB/s, needs batching | ~80 KB/s, 1000 msg/s unbatched ✗ (100 publish/s cap) |

**Decision D5: phase 1 caps at ~250 tags** on the current no-PSRAM build. The cap is a validated
constant — a bundle exceeding it is rejected wholesale with an explicit `reported.last_error`,
never silently truncated.

Even at 250 tags the following are still required:
1. **Batched MQTT publishing** with `batch_max_bytes ≤ 64 KB` and a publish-rate limiter.
2. **Chunked `CreateMonitoredItems`** — items sent in pages (e.g. 100/request), never all in one
   service call. `ClientBuilder::recreate_monitored_items_chunk` already covers the reconnect
   path; the initial creation path needs the same treatment.
3. Tuned `max_message_size` / `max_chunk_size` / `max_chunk_count` / `max_array_length`.
4. Heap headroom monitoring surfaced via `reported.free_heap`, so the real ceiling is measured
   rather than assumed.

Raising the cap toward 1000 additionally requires enabling PSRAM (`CONFIG_SPIRAM=y` plus the
octal/quad settings for the specific module) and deadband/report-by-exception. Phase 2.

### 5.2 Config persistence — dedicated `opcua` NVS partition (decision D3)

With no filesystem, the persisted bundle lives in NVS as a **blob** (`EspNvs::set_blob`; the
~4 KB limit applies to `set_str`, blobs may span pages).

It does **not** share the default `nvs` partition. [partitions.csv](../partitions.csv) reserves
a second NVS partition named `opcua`, so that a large or corrupt tag bundle can never evict the
AWS IoT device identity:

```rust
let part = EspNvsPartition::<NvsCustom>::take("opcua")?;
let mut nvs = EspNvs::new(part, "cfg", true)?;
```

| Item | Size |
|---|---|
| `opcua` partition ([partitions.csv](../partitions.csv)) | `0xd000` = 52 KB |
| Usable after NVS page headers + entry tables + one reserved page | ~40 KB |
| Grouped bundle @ 250 tags (§3.2) | ~6.8 KB |
| Instance + telemetry settings | <1 KB |
| **Headroom** | **~5×** |

The default `nvs` partition stays at 24 KB and holds only the device identity (~5 KB) and WiFi
credentials, unchanged from the currently deployed layout.

Two rules still apply:
- `settings.rs` must enforce a **serialised-size** limit, not just a tag count, and the limit
  belongs in one named constant derived from the partition size.
- NVS wear: the device must **skip the write when the incoming `sha256` matches what is stored**.

### 5.3 Flash layout is frozen (resolved)

The partition table lives at flash offset `0x8000` and is **not** rewritten by an OTA, so it was
finalised before implementation. [partitions.csv](../partitions.csv) is now the single source of
truth for both the ESP-IDF build and `espflash` — verified byte-identical.

| | Before | After |
|---|---|---|
| Effective table | `partitions_two_ota_large.csv` from sdkconfig, diverging from the CSV espflash used | `partitions.csv` for both |
| App slot | 4 MiB (CSV) / 1700 KB (sdkconfig) | **7 MiB** |
| App image | 5.02 MiB — **did not fit either slot** | 5.02 MiB, 71.6 % of slot |
| Offsets | auto-assigned (blank column) | explicit |
| Rollback | `mark_valid()` was a no-op | `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y` |
| Reserved for later | none | `opcua` NVS, `nvs_key`, `storage`, `coredump` |

`nvs`, `phy_init`, `otadata` and the `ota_0` **offset** were deliberately kept at their
previously flashed values, so already-provisioned units keep their NVS device identity. Only
`ota_1` moves, and every unit must be re-flashed over USB once to pick up the new table.

---

## 6. Target architecture (modular, host-testable) — **implemented**

The guiding rule: **everything that can be a pure function of data must be, and must compile and
test on the host target** (no `esp-idf-svc` in those modules). Device-specific pieces sit behind
narrow traits.

The realised layout splits everything that is not device I/O into workspace members of its own,
so none of it can accidentally grow a device dependency — the host test run never even builds
`esp-idf-sys`:

```
gateway-core/            // pure crate: serde + serde_json + sha2, nothing else.
  src/
    lib.rs               // MAX_TAGS = 250, MAX_BUNDLE_BYTES = 10 KiB
    codec.rs             // base64, hex, sha256 helpers
    node.rs              // (ns, id_type, address) -> NodeId string; namespace URI resolution
    value.rs             // TagValue + self-describing JSON encoding (§4.3)
    settings.rs          // DesiredSettings/Instance/Telemetry/CfgRef + validation + defaults
    bundle.rs            // grouped wire form -> Vec<TagSpec>, sha256/version/count verification
    batcher.rs           // Sample/Batch, size/count/age-triggered batching
    queue.rs             // bounded SampleQueue, coalesce-per-address overflow policy
    diff.rs              // current vs desired tag set -> {add, remove, modify}
    plan.rs              // scan-rate grouping + chunking (MAX_ITEMS_PER_REQUEST = 50)
    health.rs            // DriverState + the `reported` document
    shadow.rs            // shadow topic names, desired/delta/rejected parsing, reported encoding
    backoff.rs           // full-jitter exponential backoff (1 s -> 60 s)

gateway-opcua/           // the OPC UA client: platform-neutral, runs on the device AND the host
  src/
    lib.rs               // public API: new() -> (Client, Driver), spawn_thread, AppliedConfig
    variant.rs           // opcua_types::Variant -> gateway_core TagValue (all 27 arms)
    session.rs           // Connection: connect / namespace_array / missing_nodes /
                         // create_subscription / create_items / delete_subscriptions /
                         // closed / shutdown, over async_opcua_client::Session
    driver.rs            // state machine: Idle -> Connecting -> Syncing -> Running -> Error
  tests/scenario.rs      // the real client against a real server over loopback
  examples/local_gateway.rs // the client on a laptop, against the test server or a PLC

opcua-test-server/       // async-opcua-server + the tag catalogue (test fixture, host only)

src/                     // firmware: the effectful shell
  settings_store.rs      // NVS blob cache in the dedicated `opcua` partition
  shadow.rs              // ConfigPlane: shadow conversation + retained bundle -> Client::apply
  jobs.rs                // AWS IoT Jobs / OTA, flattened out of the telemetry loop
  mqtt_util.rs           // MqttTransport trait + EspMqttClient impl
  telemetry/
    mod.rs               // orchestrator: one MQTT loop over config/control/data planes
    publisher.rs         // batch -> publish, rate limit, backlog cap, drop counters
```

Key boundaries that make it testable:

- **`gateway-core` has no device dependency at all.** Every decision with a rule behind it —
  validation, digest verification, batching, chunking, diffing, backoff, encoding — is a pure
  function tested on the host. The firmware crate is left with I/O and glue.
- **`gateway-opcua` has none either.** The firmware injects what only it can provide — a clock, a
  per-device backoff seed, the thread (after registering the eventfd VFS) — and talks to the
  client only through its `Client` handle: `apply`, `disable`, `drain`, `reported`.
- **`MqttTransport` trait** (`src/mqtt_util.rs`) is what `ConfigPlane`, `JobsClient` and
  `Publisher` are written against, not `EspMqttClient`.
- **Explicit `now_ms` parameters** rather than an ambient clock: `batcher.rs` flush-on-age is
  deterministic in tests without any injection machinery.
- **Bounded queue with an explicit overflow policy** between driver and publisher: at capacity a
  new sample replaces the pending sample *for the same address* and only otherwise drops the
  oldest, so no tag can starve. `dropped`/`coalesced` are surfaced in `reported`.

Test matrix (all implemented in `gateway-core`, run with
`cargo test -p gateway-core --target <host triple>`):
- `settings.rs`: golden JSON round-trips, rejection of out-of-range `scan_rate`, non-`None`
  `sec_policy`/`sec_mode`, tag count over cap, serialised size over the NVS budget, duplicate
  addresses.
- `bundle.rs`: grouped→flat expansion, sha256 mismatch rejection, version mismatch rejection,
  malformed/truncated bundle.
- `node.rs`: all four identifier types, namespace URI resolution and fallback.
- `value.rs`: every `Variant` arm → JSON, lossless `i64`/`u64`/`ByteString`/`Guid` tagging,
  Bad status handling, status omitted when Good.
- `batcher.rs`: flush by count / by bytes / by age; empty-flush suppression; golden wire format.
- `queue.rs`: coalescing under sustained overflow, no starvation.
- `diff.rs`: add-only, remove-only, scan-rate change, no-op.
- `plan.rs`: 250 tags at chunk 50 → 5 requests; handle allocation is unique and stable.
- `health.rs`, `shadow.rs`, `backoff.rs`, `codec.rs`: size budgets, topic conventions, delta
  relevance, jitter saturation, RFC 4648 / SHA-256 vectors.

The driver and session sit directly on `async-opcua`, whose `Session` is a concrete struct rather
than a trait, so they are not unit-tested against a mock. They do not need to be: they are tested
against the real thing. `gateway-opcua/tests/scenario.rs` runs the client against
`opcua-test-server` — built on `async-opcua-server`, the server half of the same library — in one
process over loopback: provisioning with a missing tag, every value encoding, live
reconfiguration, namespace-URI rescue, server loss and recovery, a silent link, an endpoint change,
and disable under each failure. The suite found seven defects the unit tests could not see
(`OPCUA_INTEGRATION_TEST.md` §8.10–8.16).


---

## 7. Non-functional requirements

- **Security:** default deny — a `sec_policy`/`sec_mode` other than `None` is a hard validation
  failure in phase 1, never a silent downgrade to an unsecured channel. Phase 1 runs unsecured
  by design, so the OPC UA link must be on a trusted OT network; this is an explicit, recorded
  risk acceptance, and it must be logged at every connect.
- **Secrets:** none in phase 1 — anonymous identity means no credentials in the shadow. When
  auth returns (phase 3), note that the shadow document is plaintext and readable by anyone with
  `iot:GetThingShadow` on the thing, so credentials must not simply be added to `desired`.
- **Resilience:** exponential backoff with jitter (1 s → 60 s), no tight reconnect loop; the OPC
  UA task must never be able to panic the process or block the MQTT/OTA task.
- **Observability:** `reported` state as in §4.1, updated on transition and at most every N
  seconds (shadow allows 20 req/s per shadow; we should use ≪1/s).
- **Boot behaviour:** apply the persisted bundle immediately, then reconcile with the shadow when
  connectivity is available.
- **Idempotency:** re-delivery of the same `cfg.v` is a no-op.

---

## 8. Phasing

**Phase 1 — foundation (no crypto, no filesystem, no auth)** — **implemented**
- Shadow control plane + retained tag bundle, `sha256`/version verified.
- `SecurityPolicy::None` + `MessageSecurityMode::None` + `IdentityToken::Anonymous`; any other
  value rejected.
- Subscriptions + MonitoredItems with per-tag `scan_rate`, chunked creation (50 per request).
- Batched telemetry with self-describing values, bounded queue, reported state.
- NVS-blob config cache with a serialised-size budget (§5.2), written only on change.
- Full module split of §6 with host unit tests.
- Hard cap at the tested tag count (~250).

**Phase 2 — scale**
- Enable PSRAM; re-measure; raise the cap toward 1000.
- Deadband / report-by-exception.
- Config diffing without session teardown.

**Phase 3 — security & auth** (all gated on accepting a data partition)
- Data partition (SPIFFS/LittleFS) + mount at boot.
- Application instance certificate written from cloud-provisioned PEMs → `Sign` /
  `SignAndEncrypt`.
- Username/password and X.509 user identity tokens, with a non-plaintext delivery mechanism.
- Presigned-S3 bundle escape hatch for very large tag sets.

---

## 9. Decisions (agreed)

| # | Question | Decision |
|---|---|---|
| **D1** | Should `uuid` reach the device? | **No.** Dropped from the bundle; the backend maps `address → uuid` on ingest. `address` is the tag key end to end. |
| **D2** | How are OPC UA server credentials delivered? | **Anonymous identity only in phase 1.** Username/password and X.509 user tokens deferred to phase 3. No secrets in the shadow. |
| **D3** | Add a SPIFFS/LittleFS data partition? | **Not mounted.** ⇒ OPC UA certificates are out of scope entirely (§2.2), and persisted config lives in NVS (§5.2). Flash space for a future filesystem *is* reserved as the `storage` partition, because the table cannot change after deployment (§5.3). |
| **D4** | What is the `data_type` enum mapping? | **Field removed.** No casting is required, so no external enum needs mirroring. The server's own type travels with each value in the telemetry payload (§4.3). |
| **D5** | Is 1000 tags near-term? PSRAM available? | **No — 1000 is a far-future ceiling.** Phase 1 ships a hard ~250-tag cap on the current no-PSRAM build. |
| **D6** | More than one OPC UA server per gateway? | **No.** `instance` stays singular; the driver is a single state machine. |

### Remaining follow-ups (non-blocking)

| # | Item | Needed by |
|---|---|---|
| ~~F1~~ | ~~Confirm whether [partitions.csv](../partitions.csv) is actually in use~~ — **resolved**: it was not. The layout is now finalised, the two tables are unified, and a dedicated 52 KB `opcua` NVS partition holds the tag bundle (§5.2, §5.3). | — |
| F2 | How heterogeneous is `scan_rate` in practice? Few distinct values keeps subscription count and memory low (§4.2). | Sizing the subscription strategy. |
| F3 | Measure real free heap and per-MonitoredItem cost on device to validate the ~250 cap. | Before publishing the cap as a supported number. |
| F4 | Measure the OPC UA thread's actual stack high-water mark. `src/telemetry/mod.rs` reserves 40 KiB, which is a guess sized against the spike's (too small) 32 KiB. | Before publishing the tag cap. |

### Deployment note

The partition table changed (§5.3). Every device already in the field needs **one USB re-flash**
to pick it up — OTA writes only the app slot and cannot deliver a new table.
