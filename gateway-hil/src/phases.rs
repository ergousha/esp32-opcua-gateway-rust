//! The scenario, phase by phase.
//!
//! Phases run in order and share state, because that is how the device works:
//! the reconfiguration phase only means something once the happy path has been
//! applied. Each is individually selectable with `--phases`.
//!
//! Everything is asserted from OUTSIDE the firmware — the shadow's `reported`
//! block, telemetry in CloudWatch, the serial log — because nothing in
//! production can reach inside the device either.

use std::time::Duration;

use anyhow::{anyhow, Result};
use gateway_core::health::DriverState;
use gateway_core::MAX_BUNDLE_BYTES;
use opcua_test_server::catalogue::{self, Tag, FAULT_STATUS, NAMESPACE_URI, SLOW_MS};
use opcua_test_server::documents::{self, Bundle, Desired};
use opcua_test_server::{Options, TestServer};
use regex::Regex;
use serde_json::Value;

use crate::cloud::{now_ms, rows_by_address, Cloud};
use crate::device::SerialMonitor;
use crate::report::{info, PhaseResult};

/// Default run order. For a host the device cannot reach, see
/// [`crate::offline::PHASES`].
pub const PHASES: &[&str] = &[
    "preflight",
    "provision",
    "telemetry",
    "reconfig",
    "ns_uri",
    "reject_security",
    "reject_digest",
    "server_down",
    "disable",
    "reboot",
];

const RUNNING: &str = DriverState::Running.as_str();
const IDLE: &str = DriverState::Idle.as_str();
const ERROR: &str = DriverState::Error.as_str();

/// Telemetry batch budget the scenario configures (`Desired::new`'s default).
const BATCH_MAX_BYTES: usize = 16_384;

/// The OPC UA test server, owned by the runner so the resilience phase can
/// take it away mid-session and bring it back on the same endpoint.
pub struct Server {
    options: Options,
    running: Option<TestServer>,
    ns: u16,
}

impl Server {
    pub fn new(options: Options) -> Self {
        Self {
            options,
            running: None,
            ns: 2,
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        if self.running.is_none() {
            let server = TestServer::start(self.options.clone()).await?;
            self.options.port = server.port();
            self.ns = server.namespace_index();
            info(format!("OPC UA server up on {}", self.endpoint()));
            self.running = Some(server);
        }
        Ok(())
    }

    /// Abrupt on purpose: simulates a PLC dropping off the network.
    pub async fn stop(&mut self) {
        if let Some(server) = self.running.take() {
            server.stop().await;
            info("OPC UA server stopped");
        }
    }

    /// The endpoint the DEVICE dials: the advertised LAN address.
    pub fn endpoint(&self) -> String {
        let host = self
            .options
            .advertised_host
            .clone()
            .unwrap_or_else(|| self.options.bind.to_string());
        format!(
            "opc.tcp://{host}:{}{}",
            self.options.port, self.options.path
        )
    }

    /// The same server over loopback, for the host-side preflight.
    fn loopback_endpoint(&self) -> String {
        format!(
            "opc.tcp://127.0.0.1:{}{}",
            self.options.port, self.options.path
        )
    }

    pub fn namespace_index(&self) -> u16 {
        self.ns
    }
}

/// State shared by the phases.
pub struct Ctx {
    pub cloud: Cloud,
    pub server: Server,
    pub monitor: Option<SerialMonitor>,
    pub thing: String,
    /// Config version currently expected to be applied on the device.
    pub applied_version: u32,
    /// Unix ms at the start of the run; nothing older counts.
    pub t0_ms: i64,
    /// Bundle versions published by this run, for `--cleanup`.
    pub published: Vec<u32>,
    /// Highest config version the shadow had asked for, or this run has
    /// published; [`Ctx::next_version`] continues from it.
    pub last_version: u32,
}

impl Ctx {
    /// A config version the device cannot have applied already.
    ///
    /// A device ignores a version it is already running (requirements §7,
    /// idempotency), so reusing one — a cached v1 from an earlier run, say —
    /// would make a new configuration look like a re-delivery (§9.9).
    pub(crate) fn next_version(&mut self) -> u32 {
        self.last_version += 1;
        self.last_version
    }

    pub(crate) async fn publish_config(
        &mut self,
        version: u32,
        tags: &[Tag],
        tweak: impl FnOnce(&mut Desired),
    ) -> Result<Bundle> {
        let bundle = documents::bundle(&self.thing, version, tags);
        let mut desired = Desired::new(self.server.endpoint(), self.server.namespace_index());
        tweak(&mut desired);
        self.cloud.publish_bundle(&bundle).await?;
        self.cloud
            .update_desired(&desired.to_json(&self.thing, &bundle))
            .await?;
        self.published.push(version);
        self.last_version = self.last_version.max(version);
        info(format!(
            "published config v{version}: {} tags, {} B bundle, sha256={}…",
            bundle.count,
            bundle.payload.len(),
            &bundle.sha256[..12]
        ));
        Ok(bundle)
    }

    pub(crate) async fn expect_state(
        &mut self,
        phase: &mut PhaseResult,
        label: &str,
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        let (ok, reported) = self
            .cloud
            .wait_for_reported(predicate, timeout, show(summary))
            .await;
        let mut detail = truncate(&reported.to_string(), 300);
        if !ok {
            // A stale document is a different failure from a wrong one, and
            // saying so is what stops later phases being read as findings
            // about the firmware.
            if let Some(stale) = self.cloud.stale_note() {
                detail = format!("{stale} (last reported: {detail})");
            }
        }
        phase.check(label, ok, detail);
        reported
    }

    pub(crate) fn mark(&self) -> u64 {
        self.monitor.as_ref().map_or(0, SerialMonitor::mark)
    }

    pub(crate) fn log_since(&self, mark: u64) -> Option<String> {
        self.monitor.as_ref().map(|m| m.read_since(mark))
    }

    /// Waits for `pattern` in the serial log after `mark`.
    pub(crate) async fn wait_log(
        &self,
        pattern: &str,
        mark: u64,
        timeout: Duration,
    ) -> Option<String> {
        let monitor = self.monitor.as_ref()?;
        let pattern = Regex::new(pattern).expect("valid pattern");
        monitor.wait_for(&pattern, mark, timeout).await
    }

    /// Reboots the chip over USB, capturing the new boot from its first line.
    ///
    /// Returns when the monitor is attached; use the returned instant to tell
    /// a report from the new boot from one written before it.
    pub(crate) async fn reset_device(&mut self) -> Result<std::time::Instant> {
        info("rebooting the device over USB…");
        let monitor = self
            .monitor
            .as_mut()
            .ok_or_else(|| anyhow!("rebooting the device needs the serial monitor (--port)"))?;
        let at = std::time::Instant::now();
        monitor.restart().await?;
        Ok(at)
    }
}

/// Runs one phase by name.
pub async fn run(name: &str, ctx: &mut Ctx) -> Result<PhaseResult> {
    match name {
        "preflight" => preflight(ctx).await,
        "provision" => provision(ctx).await,
        "telemetry" => telemetry(ctx).await,
        "reconfig" => reconfig(ctx).await,
        "ns_uri" => ns_uri(ctx).await,
        "reject_security" => reject_security(ctx).await,
        "reject_digest" => reject_digest(ctx).await,
        "server_down" => server_down(ctx).await,
        "disable" => disable(ctx).await,
        "reboot" => reboot(ctx).await,
        "offline_config" => crate::offline::config(ctx).await,
        "offline_disable" => crate::offline::disable(ctx).await,
        "offline_reject_security" => crate::offline::reject_security(ctx).await,
        "offline_reject_digest" => crate::offline::reject_digest(ctx).await,
        "offline_reenable" => crate::offline::reenable(ctx).await,
        "offline_reboot" => crate::offline::reboot(ctx).await,
        other => Err(anyhow!("unknown phase {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// True when the report's `uptime_s` places its boot after `since`.
pub(crate) fn booted_since(r: &Value, since: std::time::Instant) -> bool {
    r["uptime_s"]
        .as_u64()
        .is_some_and(|uptime| uptime <= since.elapsed().as_secs() + 5)
}

pub(crate) fn state(r: &Value) -> &str {
    r["state"].as_str().unwrap_or("")
}

fn cfg_v(r: &Value) -> u64 {
    r["cfg_v"].as_u64().unwrap_or(0)
}

fn summary(r: &Value) -> String {
    format!(
        "state={} cfg_v={} applied={} failed={} heap={} err={}",
        r["state"], r["cfg_v"], r["applied"], r["failed"], r["free_heap"], r["last_error"]
    )
}

/// An `on_poll` that never prints a stale document as if it were live:
/// printing an old boot's fields is what makes a dead device look like a slow
/// one.
pub(crate) fn show(fmt: impl Fn(&Value) -> String) -> impl Fn(&Value, bool) {
    move |r, fresh| {
        if fresh {
            info(format!("reported: {}", fmt(r)));
        } else {
            info("reported: (stale — nothing written since the run started)");
        }
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn first_match(log: &str, pattern: &str) -> Option<String> {
    Regex::new(pattern)
        .expect("valid pattern")
        .find(log)
        .map(|m| m.as_str().to_string())
}

fn compact_len(v: &Value) -> usize {
    serde_json::to_vec(v).map_or(0, |b| b.len())
}

// ---------------------------------------------------------------------------
// phases
// ---------------------------------------------------------------------------

/// The server answers the firmware's own client, and the documents fit their
/// budgets — before any hardware is involved.
///
/// Where the Python harness replayed the firmware's call sequence in another
/// client library, this runs the firmware's actual OPC UA client
/// (`gateway-opcua`) against the server over loopback. Note what that does
/// NOT prove: that the device can reach this host. Loopback bypasses the host
/// firewall; check with `nc -z <lan-ip> <port>` from another machine.
async fn preflight(ctx: &mut Ctx) -> Result<PhaseResult> {
    let mut r = PhaseResult::new("preflight");
    ctx.server.start().await?;

    let bundle = documents::bundle(&ctx.thing, 1, &catalogue::tags());
    let desired = Desired::new(ctx.server.loopback_endpoint(), ctx.server.namespace_index());
    let settings = serde_json::from_value(desired.to_json(&ctx.thing, &bundle))?;
    let config = gateway_opcua::AppliedConfig::new(settings, &bundle.payload)?;

    let (client, driver) = gateway_opcua::new(gateway_opcua::Options::new("preflight"));
    gateway_opcua::spawn_thread(driver, "opcua-preflight", 4 * 1024 * 1024)?;
    client.apply(config)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut reported = client.reported();
    while std::time::Instant::now() < deadline && reported.state != DriverState::Running {
        tokio::time::sleep(Duration::from_millis(100)).await;
        reported = client.reported();
    }
    let expected = catalogue::present().len();
    r.check(
        "the gateway's own OPC UA client syncs against the server",
        reported.state == DriverState::Running
            && reported.applied == expected
            && reported.failed == catalogue::missing_addresses().len(),
        format!(
            "state={} applied={} (expected {expected}) failed={} error={:?}",
            reported.state.as_str(),
            reported.applied,
            reported.failed,
            reported.last_error
        ),
    );
    drop(client);

    r.check(
        "tag bundle fits the firmware's NVS budget",
        bundle.payload.len() <= MAX_BUNDLE_BYTES,
        format!("{} B <= {MAX_BUNDLE_BYTES} B", bundle.payload.len()),
    );
    let desired = Desired::new(ctx.server.endpoint(), ctx.server.namespace_index());
    let shadow_bytes = compact_len(&desired.to_json(&ctx.thing, &bundle));
    r.check(
        "shadow desired document fits the 8 KB AWS IoT limit",
        shadow_bytes < 8192,
        format!("{shadow_bytes} B"),
    );
    Ok(r)
}

/// Config v1 on both planes, and the device applies it.
async fn provision(ctx: &mut Ctx) -> Result<PhaseResult> {
    let mut r = PhaseResult::new("provision");
    ctx.server.start().await?;

    let mark = ctx.mark();
    ctx.publish_config(1, &catalogue::tags(), |_| {}).await?;
    ctx.applied_version = 1;

    let reported = ctx
        .expect_state(
            &mut r,
            "driver reaches state=running with cfg_v=1",
            Duration::from_secs(240),
            |x| state(x) == RUNNING && cfg_v(x) == 1,
        )
        .await;

    let expected = catalogue::present().len() as u64;
    let missing = catalogue::missing_addresses();
    r.check(
        format!("{expected} monitored items applied"),
        reported["applied"].as_u64() == Some(expected),
        format!("applied={} expected={expected}", reported["applied"]),
    );
    r.check(
        "exactly one tag failed (the deliberately absent one)",
        reported["failed"].as_u64() == Some(missing.len() as u64),
        format!("failed={}", reported["failed"]),
    );
    let named: Vec<&str> = reported["failed_sample"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["a"].as_str().or_else(|| e.as_str()))
        .collect();
    r.check(
        "the failed tag is named in failed_sample",
        named == missing,
        format!("failed_sample={}", reported["failed_sample"]),
    );
    r.check(
        "one bad NodeId did not take the other tags down (finding A4)",
        reported["applied"].as_u64().unwrap_or(0) > 0 && state(&reported) == RUNNING,
        "",
    );
    r.check(
        "free heap reported and above 40 KB",
        reported["free_heap"].as_u64().unwrap_or(0) > 40_000,
        format!("free_heap={}", reported["free_heap"]),
    );

    if let Some(log) = ctx.log_since(mark) {
        r.check(
            "device logged the unencrypted-link warning on connect (NFR §7)",
            log.contains("UNENCRYPTED and UNAUTHENTICATED"),
            "",
        );
        let synced = first_match(&log, r"OPC UA synced:[^\n]*");
        r.check(
            "device logged two subscriptions (one per scan rate)",
            synced
                .as_deref()
                .is_some_and(|l| l.contains("synced: 2 subscriptions")),
            synced.unwrap_or_else(|| "not found".into()),
        );
    }
    Ok(r)
}

/// The actual bytes that reached AWS, per value type.
async fn telemetry(ctx: &mut Ctx) -> Result<PhaseResult> {
    let mut r = PhaseResult::new("telemetry");
    // Never older than the run: batches from an earlier run on the same log
    // group would otherwise be judged against this run's configuration.
    let start = (now_ms() - 120_000).max(ctx.t0_ms);
    info("waiting for batches to land in CloudWatch (the IoT rule hop lags)…");
    let batches = ctx
        .cloud
        .wait_for_telemetry(start, 3, Duration::from_secs(180))
        .await;
    r.check(
        "telemetry batches arrived",
        batches.len() >= 3,
        format!("{} batches", batches.len()),
    );
    if batches.is_empty() {
        return Ok(r);
    }

    let versions: std::collections::BTreeSet<u64> =
        batches.iter().filter_map(|b| b["v"].as_u64()).collect();
    r.check(
        "batches are stamped with the applied config version",
        versions.iter().all(|v| *v == ctx.applied_version as u64),
        format!("versions={versions:?}"),
    );

    let rows = rows_by_address(&batches);
    info(format!(
        "addresses seen: {} — {:?}",
        rows.len(),
        rows.keys()
    ));
    r.check(
        "the absent tag never produced a sample",
        catalogue::missing_addresses()
            .iter()
            .all(|a| !rows.contains_key(*a)),
        "",
    );
    let last = |address: &str| rows.get(address).and_then(|v| v.last()).cloned();

    // Driven by the catalogue's `encoding`, so a new tag brings its own
    // assertion instead of needing an edit here.
    for tag in catalogue::present() {
        let Some(encoding) = tag.encoding else {
            continue;
        };
        let row = last(tag.address);
        let value = row
            .as_ref()
            .map(|row| row[2].clone())
            .unwrap_or(Value::Null);
        if tag.bad_status.is_some() && value.is_null() {
            continue; // a Bad-status sample may legitimately carry no value
        }
        r.check(
            format!("{}: {}", tag.address, tag.proves),
            row.is_some() && encoding.matches(&value),
            format!("= {}", truncate(&value.to_string(), 80)),
        );
    }

    if let Some(row) = last("Line1.Pressure") {
        let v = row[2].as_f64().unwrap_or(f64::NAN);
        r.check(
            "Float is widened via its shortest decimal form, not `as f64`",
            (v * 10.0).round() / 10.0 == v,
            format!("Line1.Pressure = {v}"),
        );
    }

    let good = last("Line1.Temp");
    r.check(
        "a Good status is omitted from the row (3 elements)",
        good.as_ref().is_some_and(|row| row.len() == 3),
        format!("len={}", good.map_or(0, |row| row.len())),
    );
    let faulty = last("Line1.Faulty");
    r.check(
        format!(
            "a Bad status travels as the 4th element ({FAULT_STATUS}={:#010x})",
            FAULT_STATUS.bits()
        ),
        faulty.as_ref().is_some_and(|row| {
            row.len() == 4 && row[3].as_u64() == Some(FAULT_STATUS.bits() as u64)
        }),
        format!("Line1.Faulty row = {faulty:?}"),
    );

    let n = |address: &str| rows.get(address).map_or(0, Vec::len);
    r.check(
        "an unchanging tag is not re-reported every scan (subscription, not polling)",
        n("Line1.Static") <= 2 && n("Line1.Temp") > n("Line1.Static"),
        format!(
            "Line1.Static n={} vs Line1.Temp n={}",
            n("Line1.Static"),
            n("Line1.Temp")
        ),
    );
    r.check(
        "the 5 s group reports less often than the 1 s group",
        n("Line1.Temp") > 0 && n("Line2.Level") < n("Line1.Temp"),
        format!(
            "Line2.Level n={} vs Line1.Temp n={}",
            n("Line2.Level"),
            n("Line1.Temp")
        ),
    );
    let largest = batches.iter().map(compact_len).max().unwrap_or(0);
    r.check(
        "no batch exceeded the configured byte budget",
        largest <= BATCH_MAX_BYTES,
        format!("max={largest} B"),
    );
    Ok(r)
}

/// A new tag set is applied live, without a reboot or a reflash.
async fn reconfig(ctx: &mut Ctx) -> Result<PhaseResult> {
    let mut r = PhaseResult::new("reconfig");
    ctx.server.start().await?;

    // Drop the slow group and the absent tag; move Line1.Temp to the slow rate.
    let mut subset: Vec<Tag> = catalogue::present()
        .into_iter()
        .filter(|t| !t.address.starts_with("Line2."))
        .collect();
    subset
        .iter_mut()
        .filter(|t| t.address == "Line1.Temp")
        .for_each(|t| t.scan_rate_ms = SLOW_MS);

    let mark = ctx.mark();
    ctx.publish_config(2, &subset, |_| {}).await?;
    ctx.applied_version = 2;

    let reported = ctx
        .expect_state(
            &mut r,
            "device applied config v2 live",
            Duration::from_secs(240),
            |x| cfg_v(x) == 2 && state(x) == RUNNING,
        )
        .await;
    r.check(
        "every tag in v2 applied and none failed",
        reported["applied"].as_u64() == Some(subset.len() as u64)
            && reported["failed"].as_u64() == Some(0),
        format!(
            "applied={} failed={} expected={}",
            reported["applied"],
            reported["failed"],
            subset.len()
        ),
    );

    if let Some(log) = ctx.log_since(mark) {
        let transition = first_match(&log, r"config v1 -> v2[^\n]*");
        r.check(
            "device logged the config transition without reconnecting",
            transition.is_some(),
            transition.unwrap_or_else(|| "not found".into()),
        );
    }

    let start = now_ms();
    let batches = ctx
        .cloud
        .wait_for_telemetry(start, 2, Duration::from_secs(180))
        .await;
    let rows = rows_by_address(&batches);
    let versions: std::collections::BTreeSet<u64> =
        batches.iter().filter_map(|b| b["v"].as_u64()).collect();
    r.check(
        "telemetry is re-stamped with v2 and drops the removed tags",
        !batches.is_empty()
            && versions.iter().all(|v| *v == 2)
            && !rows.keys().any(|a| a.starts_with("Line2.")),
        format!(
            "{} batches, versions={versions:?}, addresses={:?}",
            batches.len(),
            rows.keys()
        ),
    );
    Ok(r)
}

/// A wrong namespace index is rescued by the namespace URI.
async fn ns_uri(ctx: &mut Ctx) -> Result<PhaseResult> {
    let mut r = PhaseResult::new("ns_uri");
    ctx.server.start().await?;
    let present = catalogue::present();

    let mark = ctx.mark();
    // ns=99 is deliberately wrong. Without URI resolution every item would be
    // rejected and the driver would fail with "all items rejected".
    ctx.publish_config(3, &present, |d| {
        d.ns = 99;
        d.ns_uri = Some(NAMESPACE_URI.into());
    })
    .await?;
    ctx.applied_version = 3;

    let reported = ctx
        .expect_state(
            &mut r,
            "device recovered the right namespace from ns_uri despite ns=99",
            Duration::from_secs(240),
            |x| cfg_v(x) == 3 && state(x) == RUNNING,
        )
        .await;
    r.check(
        "all items applied under the resolved namespace",
        reported["applied"].as_u64() == Some(present.len() as u64),
        format!("applied={} expected={}", reported["applied"], present.len()),
    );
    if let Some(log) = ctx.log_since(mark) {
        r.check(
            "device did NOT log a namespace fallback",
            !log.contains("not published by the server"),
            "",
        );
    }
    Ok(r)
}

/// A non-`None` security policy is refused, never downgraded.
async fn reject_security(ctx: &mut Ctx) -> Result<PhaseResult> {
    let mut r = PhaseResult::new("reject_security");
    ctx.publish_config(4, &catalogue::present(), |d| {
        d.sec_policy = "Basic256Sha256".into();
        d.sec_mode = "SignAndEncrypt".into();
    })
    .await?;

    let (ok, reported) = ctx
        .cloud
        .wait_for_reported(
            |x| {
                x["last_error"]
                    .as_str()
                    .is_some_and(|e| e.contains("Basic256Sha256"))
            },
            Duration::from_secs(120),
            show(|x| format!("last_error={} cfg_v={}", x["last_error"], x["cfg_v"])),
        )
        .await;
    r.check(
        "device refused the secured config with an explicit error",
        ok,
        format!("last_error={}", reported["last_error"]),
    );
    r.check(
        "device did NOT silently downgrade — it kept running the previous config",
        cfg_v(&reported) == 3,
        format!("cfg_v={} (expected 3)", reported["cfg_v"]),
    );
    ctx.cloud
        .clear_retained(&documents::bundle_topic(&ctx.thing, 4))
        .await?;
    Ok(r)
}

/// A bundle whose SHA-256 does not match the shadow is refused.
async fn reject_digest(ctx: &mut Ctx) -> Result<PhaseResult> {
    let mut r = PhaseResult::new("reject_digest");
    ctx.publish_config(5, &catalogue::present(), |d| {
        d.sha256_override = Some("0".repeat(64));
    })
    .await?;

    let (ok, reported) = ctx
        .cloud
        .wait_for_reported(
            // Not just "sha256": the previous phase's error names Basic256Sha256.
            |x| {
                x["last_error"]
                    .as_str()
                    .is_some_and(|e| e.contains("bundle sha256"))
            },
            Duration::from_secs(120),
            show(|x| format!("last_error={} cfg_v={}", x["last_error"], x["cfg_v"])),
        )
        .await;
    r.check(
        "device refused the bundle on digest mismatch",
        ok,
        format!("last_error={}", reported["last_error"]),
    );
    r.check(
        "device kept running the last known-good config",
        cfg_v(&reported) == 3,
        format!("cfg_v={}", reported["cfg_v"]),
    );
    ctx.cloud
        .clear_retained(&documents::bundle_topic(&ctx.thing, 5))
        .await?;
    Ok(r)
}

/// The PLC disappears mid-session; the gateway backs off and recovers by
/// itself.
async fn server_down(ctx: &mut Ctx) -> Result<PhaseResult> {
    let mut r = PhaseResult::new("server_down");
    let present = catalogue::present();

    // Back to a known-good config first: the negative phases left v4/v5
    // rejected, so the device is still on v3.
    ctx.publish_config(6, &present, |_| {}).await?;
    ctx.applied_version = 6;
    ctx.expect_state(
        &mut r,
        "baseline config v6 running before the fault",
        Duration::from_secs(240),
        |x| cfg_v(x) == 6 && state(x) == RUNNING,
    )
    .await;

    let mark = ctx.mark();
    ctx.server.stop().await;

    let (ok, reported) = ctx
        .cloud
        .wait_for_reported(
            |x| state(x) == ERROR,
            Duration::from_secs(180),
            show(|x| format!("state={} err={}", x["state"], x["last_error"])),
        )
        .await;
    r.check(
        "device noticed the server was gone",
        ok,
        format!("state={}", reported["state"]),
    );

    if let Some(log) = ctx.log_since(mark) {
        let delays: Vec<u64> = Regex::new(r"retrying in (\d+) ms")
            .expect("valid pattern")
            .captures_iter(&log)
            .filter_map(|c| c[1].parse().ok())
            .collect();
        r.check(
            "reconnect uses backoff, not a tight loop",
            delays.iter().max().is_some_and(|max| *max >= 1000),
            format!(
                "retry delays observed: {:?}",
                &delays[..delays.len().min(8)]
            ),
        );
        r.check(
            "the MQTT/OTA path stayed alive while OPC UA was down",
            !log.contains("MQTT event channel closed") && !log.contains("panicked"),
            "",
        );
    }

    info("restarting the OPC UA server…");
    ctx.server.start().await?;
    let (ok, reported) = ctx
        .cloud
        .wait_for_reported(
            |x| state(x) == RUNNING,
            Duration::from_secs(240),
            show(|x| format!("state={} applied={}", x["state"], x["applied"])),
        )
        .await;
    r.check(
        "device reconnected on its own once the server came back",
        ok,
        format!(
            "state={} applied={}",
            reported["state"], reported["applied"]
        ),
    );
    r.check(
        "all items re-created after the reconnect",
        reported["applied"].as_u64() == Some(present.len() as u64),
        format!("applied={} expected={}", reported["applied"], present.len()),
    );
    Ok(r)
}

/// `enabled: false` stops the driver without deleting the config.
async fn disable(ctx: &mut Ctx) -> Result<PhaseResult> {
    let mut r = PhaseResult::new("disable");
    let present = catalogue::present();

    // A fresh version, not 6: re-sending the applied version is by definition
    // a no-op, and the phase would read as a firmware failure.
    ctx.publish_config(7, &present, |d| d.enabled = false)
        .await?;
    let (ok, reported) = ctx
        .cloud
        .wait_for_reported(
            |x| state(x) == IDLE,
            Duration::from_secs(180),
            show(|x| format!("state={}", x["state"])),
        )
        .await;
    r.check(
        "driver went idle on enabled=false",
        ok,
        format!("state={}", reported["state"]),
    );

    // Batches published just BEFORE the disable land in CloudWatch a few
    // seconds late; let the pipeline drain before measuring silence.
    info("letting the CloudWatch pipeline drain before measuring silence…");
    tokio::time::sleep(Duration::from_secs(30)).await;
    let start = now_ms();
    tokio::time::sleep(Duration::from_secs(45)).await;
    let quiet = ctx.cloud.telemetry_since(start).await;
    r.check(
        "telemetry stopped while disabled",
        quiet.is_empty(),
        format!("{} batches in 45 s", quiet.len()),
    );

    ctx.publish_config(8, &present, |_| {}).await?;
    ctx.applied_version = 8;
    ctx.expect_state(
        &mut r,
        "driver resumed on enabled=true",
        Duration::from_secs(240),
        |x| state(x) == RUNNING && cfg_v(x) == 8,
    )
    .await;
    Ok(r)
}

/// After a reset, the NVS-cached bundle is applied before the cloud answers.
async fn reboot(ctx: &mut Ctx) -> Result<PhaseResult> {
    let mut r = PhaseResult::new("reboot");
    if ctx.monitor.is_none() {
        r.skipped = true;
        r.note = "needs the serial monitor (--port)".into();
        return Ok(r);
    }

    ctx.server.start().await?;
    let mark = ctx.mark();

    let rebooted_at = ctx.reset_device().await?;

    let hit = ctx
        .wait_log(
            r"booting with cached OPC UA config v\d+ \(\d+ tags\)",
            mark,
            Duration::from_secs(120),
        )
        .await;
    r.check(
        "device applied the NVS-cached config at boot, before the shadow replied",
        hit.is_some(),
        hit.unwrap_or_else(|| "not logged".into()),
    );

    // Only a report from the new boot counts: `uptime_s` must fit inside the
    // time since the reboot, or it was written before it.
    let applied = ctx.applied_version as u64;
    ctx.expect_state(
        &mut r,
        &format!("device is running again on cfg_v={applied} after the reboot"),
        Duration::from_secs(300),
        move |x| state(x) == RUNNING && cfg_v(x) == applied && booted_since(x, rebooted_at),
    )
    .await;
    Ok(r)
}
