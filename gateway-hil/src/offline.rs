//! Phases for a host the device cannot reach (`--offline`).
//!
//! On a workstation whose firewall refuses the device's inbound connection
//! (`docs/OPCUA_INTEGRATION_TEST.md` §3.5) the OPC UA data path cannot be
//! exercised — but everything around it can: the firmware booting on ESP-IDF,
//! both configuration planes through AWS, NVS caching, the refusal paths, and
//! how the driver behaves when its server never answers. Three of the driver
//! fixes live exactly there (§8.10–8.12), so this is worth running on hardware
//! even without a reachable server.
//!
//! The device is pointed at this host's LAN address, where nothing listens:
//! in offline mode the test server is bound to loopback only (for
//! `preflight`). Assertions come from the serial log and the shadow, so
//! `--port` is required.
//!
//! Config versions continue from whatever the shadow last asked for
//! ([`Ctx::next_version`]), so a device holding a cached configuration never
//! mistakes a new one for a re-delivery.

use std::time::{Duration, Instant};

use anyhow::Result;
use opcua_test_server::catalogue;
use opcua_test_server::documents::{self, Desired};
use regex::Regex;
use serde_json::Value;

use crate::phases::{booted_since, show, state, truncate, Ctx};
use crate::report::{info, PhaseResult};

/// Default run order with `--offline`.
pub const PHASES: &[&str] = &[
    "preflight",
    "offline_config",
    "offline_disable",
    "offline_reject_security",
    "offline_reject_digest",
    "offline_reenable",
    "offline_reboot",
];

/// A 1 s keep-alive makes the request timeout 5 s and the connect deadline
/// 10 s, so each attempt against the silent endpoint ends quickly instead of
/// after 40 s.
fn fast_attempts(d: &mut Desired) {
    d.keepalive_ms = 1_000;
}

fn last_error(r: &Value) -> &str {
    r["last_error"].as_str().unwrap_or("")
}

fn trying(r: &Value) -> bool {
    matches!(state(r), "connecting" | "error")
}

fn summary(r: &Value) -> String {
    format!(
        "state={} free_heap={} err={}",
        r["state"],
        r["free_heap"],
        truncate(last_error(r), 90)
    )
}

/// Retry delays the driver logged after `mark`, in order.
fn retry_delays(log: &str) -> Vec<u64> {
    Regex::new(r"retrying in (\d+) ms")
        .expect("valid pattern")
        .captures_iter(log)
        .filter_map(|c| c[1].parse().ok())
        .collect()
}

fn found(hit: &Option<String>) -> String {
    hit.clone().unwrap_or_else(|| "not logged".into())
}

/// Offline mode reads everything important from the serial log.
fn needs_monitor(ctx: &Ctx, name: &str) -> Option<PhaseResult> {
    if ctx.monitor.is_some() {
        return None;
    }
    let mut r = PhaseResult::new(name);
    r.skipped = true;
    r.note = "offline phases need the serial monitor (--port)".into();
    Some(r)
}

/// A valid configuration reaches the device on both planes, and the driver
/// fails against an unreachable server the way it should: visibly, bounded,
/// with backoff.
pub async fn config(ctx: &mut Ctx) -> Result<PhaseResult> {
    if let Some(skipped) = needs_monitor(ctx, "offline_config") {
        return Ok(skipped);
    }
    let mut r = PhaseResult::new("offline_config");
    let mark = ctx.mark();
    let v = ctx.next_version();
    let endpoint = ctx.server.endpoint();
    ctx.publish_config(v, &catalogue::tags(), fast_attempts)
        .await?;
    ctx.applied_version = v;

    let received = ctx
        .wait_log(
            &format!(r"config v{v} with \d+ tags from \S+"),
            mark,
            Duration::from_secs(120),
        )
        .await;
    r.check(
        format!("device read config v{v} from the opcua shadow (control plane)"),
        received.is_some(),
        found(&received),
    );
    let cached = ctx
        .wait_log(
            &format!(r"cached OPC UA config v{v} \(\d+ B bundle\) to NVS"),
            mark,
            Duration::from_secs(60),
        )
        .await;
    r.check(
        "the retained bundle arrived, matched version and SHA-256, and was cached to NVS",
        cached.is_some(),
        found(&cached),
    );

    let log = ctx.log_since(mark).unwrap_or_default();
    r.check(
        "the device runs the new firmware (gateway_opcua in the log)",
        log.contains("gateway_opcua::"),
        "",
    );
    r.check(
        "device logged the unencrypted-link warning on connect (NFR §7)",
        log.contains("UNENCRYPTED and UNAUTHENTICATED"),
        "",
    );

    let target = endpoint.clone();
    let (ok, reported) = ctx
        .cloud
        .wait_for_reported(
            move |x| trying(x) && last_error(x).contains(&target),
            Duration::from_secs(180),
            show(summary),
        )
        .await;
    r.check(
        "the failure is reported in the shadow, naming the endpoint, instead of an \
         endless `connecting` (§8.12)",
        ok,
        format!(
            "state={} last_error={}",
            reported["state"],
            last_error(&reported)
        ),
    );
    r.check(
        "free heap reported and above 40 KB",
        reported["free_heap"].as_u64().unwrap_or(0) > 40_000,
        format!("free_heap={}", reported["free_heap"]),
    );

    // Three failures take about 10 s each plus 1-4 s of backoff.
    let deadline = Instant::now() + Duration::from_secs(150);
    let mut delays = Vec::new();
    while Instant::now() < deadline {
        delays = retry_delays(&ctx.log_since(mark).unwrap_or_default());
        if delays.len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    r.check(
        "reconnects back off (every delay at least 1 s, no tight loop)",
        delays.len() >= 3 && delays.iter().all(|d| *d >= 1_000),
        format!("retry delays: {:?}", &delays[..delays.len().min(8)]),
    );

    let log = ctx.log_since(mark).unwrap_or_default();
    let bounded = Regex::new(r"(timed out after \d+ s connecting to|could not connect to) \S+")?
        .find(&log)
        .map(|m| m.as_str().to_string());
    r.check(
        "every connect attempt ends by itself: refused, or at the connect deadline (§8.12)",
        bounded.is_some(),
        found(&bounded),
    );
    r.check(
        "no panic and no failed allocation; the MQTT loop stayed up",
        !log.contains("panicked")
            && !log.contains("memory allocation of")
            && !log.contains("MQTT event channel closed"),
        "",
    );
    Ok(r)
}

/// `enabled: false` idles the driver while it is still trying to reach a
/// server that never answers — the situation that wedged it on hardware.
pub async fn disable(ctx: &mut Ctx) -> Result<PhaseResult> {
    if let Some(skipped) = needs_monitor(ctx, "offline_disable") {
        return Ok(skipped);
    }
    let mut r = PhaseResult::new("offline_disable");
    let mark = ctx.mark();
    let v = ctx.next_version();
    ctx.publish_config(v, &catalogue::tags(), |d| {
        fast_attempts(d);
        d.enabled = false;
    })
    .await?;

    let started = Instant::now();
    let (ok, reported) = ctx
        .cloud
        .wait_for_reported(
            |x| state(x) == "idle",
            Duration::from_secs(90),
            show(summary),
        )
        .await;
    r.check(
        "driver went idle while its server was unreachable (§8.11)",
        ok,
        format!(
            "state={} after {:.0} s",
            reported["state"],
            started.elapsed().as_secs_f64()
        ),
    );
    let disabled = ctx
        .wait_log(
            "OPC UA disabled by configuration",
            mark,
            Duration::from_secs(10),
        )
        .await;
    r.check(
        "device logged the disable",
        disabled.is_some(),
        found(&disabled),
    );

    // Idle means idle: no connect attempts once disabled.
    let quiet_from = ctx.mark();
    tokio::time::sleep(Duration::from_secs(20)).await;
    let attempts = retry_delays(&ctx.log_since(quiet_from).unwrap_or_default()).len();
    r.check(
        "no reconnect attempts while disabled",
        attempts == 0,
        format!("{attempts} in 20 s"),
    );
    Ok(r)
}

/// A non-`None` security policy is refused, never downgraded.
pub async fn reject_security(ctx: &mut Ctx) -> Result<PhaseResult> {
    if let Some(skipped) = needs_monitor(ctx, "offline_reject_security") {
        return Ok(skipped);
    }
    let mut r = PhaseResult::new("offline_reject_security");
    let mark = ctx.mark();
    let v = ctx.next_version();
    ctx.publish_config(v, &catalogue::present(), |d| {
        fast_attempts(d);
        d.sec_policy = "Basic256Sha256".into();
        d.sec_mode = "SignAndEncrypt".into();
    })
    .await?;

    let refused = ctx
        .wait_log(
            r#"rejected: sec_policy="Basic256Sha256" is not supported"#,
            mark,
            Duration::from_secs(90),
        )
        .await;
    r.check(
        "device refused the secured config explicitly",
        refused.is_some(),
        found(&refused),
    );
    // The driver is idle, so nothing overwrites the error before the next
    // periodic report (at most 30 s).
    let (ok, reported) = ctx
        .cloud
        .wait_for_reported(
            |x| last_error(x).contains("Basic256Sha256"),
            Duration::from_secs(75),
            show(summary),
        )
        .await;
    r.check(
        "the refusal is visible in the shadow's last_error",
        ok,
        format!("last_error={}", last_error(&reported)),
    );
    let log = ctx.log_since(mark).unwrap_or_default();
    r.check(
        "nothing was cached or applied: no silent downgrade",
        !log.contains(&format!("cached OPC UA config v{v} ")) && state(&reported) == "idle",
        format!("state={}", reported["state"]),
    );
    ctx.cloud
        .clear_retained(&documents::bundle_topic(&ctx.thing, v))
        .await?;
    Ok(r)
}

/// A bundle whose SHA-256 does not match the shadow is refused.
pub async fn reject_digest(ctx: &mut Ctx) -> Result<PhaseResult> {
    if let Some(skipped) = needs_monitor(ctx, "offline_reject_digest") {
        return Ok(skipped);
    }
    let mut r = PhaseResult::new("offline_reject_digest");
    let mark = ctx.mark();
    let v = ctx.next_version();
    ctx.publish_config(v, &catalogue::present(), |d| {
        fast_attempts(d);
        d.sha256_override = Some("0".repeat(64));
    })
    .await?;

    let refused = ctx
        .wait_log(
            r"tag bundle rejected: bundle sha256 [0-9a-f]{64}",
            mark,
            Duration::from_secs(90),
        )
        .await;
    r.check(
        "device refused the bundle on a digest mismatch",
        refused.is_some(),
        found(&refused).chars().take(120).collect::<String>(),
    );
    let (ok, reported) = ctx
        .cloud
        .wait_for_reported(
            |x| last_error(x).contains("sha256"),
            Duration::from_secs(75),
            show(summary),
        )
        .await;
    r.check(
        "the refusal is visible in the shadow's last_error",
        ok,
        format!("last_error={}", truncate(last_error(&reported), 100)),
    );
    let log = ctx.log_since(mark).unwrap_or_default();
    r.check(
        "the mismatched bundle was neither cached nor applied",
        !log.contains(&format!("cached OPC UA config v{v} ")) && state(&reported) == "idle",
        format!("state={}", reported["state"]),
    );
    ctx.cloud
        .clear_retained(&documents::bundle_topic(&ctx.thing, v))
        .await?;
    Ok(r)
}

/// After a disable, the next valid configuration is taken up — on hardware the
/// wedged driver ignored every later one.
pub async fn reenable(ctx: &mut Ctx) -> Result<PhaseResult> {
    if let Some(skipped) = needs_monitor(ctx, "offline_reenable") {
        return Ok(skipped);
    }
    let mut r = PhaseResult::new("offline_reenable");
    let mark = ctx.mark();
    let v = ctx.next_version();
    ctx.publish_config(v, &catalogue::tags(), fast_attempts)
        .await?;
    ctx.applied_version = v;

    let cached = ctx
        .wait_log(
            &format!(r"cached OPC UA config v{v} \(\d+ B bundle\) to NVS"),
            mark,
            Duration::from_secs(120),
        )
        .await;
    r.check(
        format!("config v{v} was accepted and cached"),
        cached.is_some(),
        found(&cached),
    );
    let resumed = ctx
        .wait_log(
            "OPC UA driver: idle -> connecting",
            mark,
            Duration::from_secs(30),
        )
        .await;
    r.check(
        "the driver left idle for the new config: not wedged by the disable (§8.11)",
        resumed.is_some(),
        found(&resumed),
    );
    let (ok, reported) = ctx
        .cloud
        .wait_for_reported(trying, Duration::from_secs(60), show(summary))
        .await;
    r.check(
        "the shadow shows the driver trying again",
        ok,
        format!("state={}", reported["state"]),
    );
    Ok(r)
}

/// After a reset, the NVS-cached configuration drives the driver before the
/// cloud has said anything.
pub async fn reboot(ctx: &mut Ctx) -> Result<PhaseResult> {
    if let Some(skipped) = needs_monitor(ctx, "offline_reboot") {
        return Ok(skipped);
    }
    let mark = ctx.mark();
    let mut r = PhaseResult::new("offline_reboot");
    // Run on its own, the phase accepts whichever version the device cached.
    let (v, which) = match ctx.applied_version {
        0 => (r"\d+".to_string(), "its".to_string()),
        v => (v.to_string(), format!("v{v}")),
    };

    let rebooted_at = ctx.reset_device().await?;
    let booted = ctx
        .wait_log(
            &format!(r"booting with cached OPC UA config v{v} \(\d+ tags\)"),
            mark,
            Duration::from_secs(120),
        )
        .await;
    r.check(
        format!("device applied {which} NVS-cached config at boot, before the cloud answered"),
        booted.is_some(),
        found(&booted),
    );
    let connecting = ctx
        .wait_log(
            "OPC UA driver: idle -> connecting",
            mark,
            Duration::from_secs(30),
        )
        .await;
    r.check(
        "the driver started on the cached config",
        connecting.is_some(),
        found(&connecting),
    );

    let (ok, reported) = ctx
        .cloud
        .wait_for_reported(
            move |x| trying(x) && booted_since(x, rebooted_at),
            Duration::from_secs(120),
            show(summary),
        )
        .await;
    r.check(
        "the rebooted device reports in the shadow (from the new boot)",
        ok,
        format!(
            "state={} uptime_s={}",
            reported["state"], reported["uptime_s"]
        ),
    );
    info(format!(
        "the device is left on {which} config, cached in NVS"
    ));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delays_are_read_from_both_driver_generations() {
        let log = "E OPC UA failure (attempt 1): x; retrying in 1000 ms\n\
                   E OPC UA sync failed (attempt 2): y; retrying in 1873 ms\n";
        assert_eq!(retry_delays(log), vec![1000, 1873]);
    }
}
