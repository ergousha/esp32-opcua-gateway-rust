//! The gateway's OPC UA client against a real OPC UA server, over loopback.
//!
//! Client and server are the two halves of the same library (`async-opcua`),
//! in one process on `127.0.0.1`: no device, no LAN, and no host firewall in
//! the way. Every test observes the client the way the cloud does — through
//! the health report and the telemetry it would publish — and never reaches
//! into the driver.
//!
//! These cover the OPC UA half of every phase in
//! `docs/OPCUA_INTEGRATION_TEST.md`, plus failures the on-device run cannot
//! stage (a silent link, a hung server during a disable, an endpoint change).
//!
//! ```sh
//! cargo test -p gateway-opcua --target host-tuple
//! RUST_LOG=info cargo test -p gateway-opcua --target host-tuple -- --nocapture server_loss
//! ```

mod common;

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use common::*;
use gateway_core::codec::base64_encode;
use gateway_core::settings::DesiredSettings;
use gateway_core::MAX_BUNDLE_BYTES;
use gateway_opcua::ConfigError;
use opcua_test_server::catalogue::{
    self, Encoding, ABOVE_2_53, BATCH_ID, FAST_MS, FAULT_CLEARED_VALUE, FAULT_STATUS,
    NAMESPACE_URI, SAFE_INT, SLOW_MS,
};
use opcua_test_server::documents::{self, Desired};
use opcua_test_server::{Blackhole, TestServer};
use serde_json::Value;

/// `BadNodeIdUnknown`, which the absent tag must be reported with.
const BAD_NODE_ID_UNKNOWN: u32 = 0x8034_0000;

// ---------------------------------------------------------------------------
// configuration: the gate every document passes (no server involved)
// ---------------------------------------------------------------------------

#[test]
fn catalogue_documents_pass_the_gateways_own_parser() {
    // The fixture writes the documents with plain serde_json; the gateway
    // parses them with gateway-core. Agreement here is a real cross-check.
    let config = config_for(
        "opc.tcp://127.0.0.1:4855/x",
        2,
        1,
        &catalogue::tags(),
        |_| {},
    );
    assert_eq!(config.tags.len(), catalogue::tags().len());
    assert_eq!(config.tags[0].node_id, "ns=2;s=Line1.Temp");
    assert_eq!(config.tags[0].scan_rate_ms, FAST_MS);
}

#[test]
fn documents_fit_their_transport_budgets() {
    let bundle = documents::bundle(THING, 1, &catalogue::tags());
    assert!(
        bundle.payload.len() <= MAX_BUNDLE_BYTES,
        "{} B",
        bundle.payload.len()
    );

    let desired = Desired::new("opc.tcp://192.168.100.100:4855/ergousha/test", 2);
    let shadow = serde_json::to_vec(&serde_json::json!({
        "state": { "desired": desired.to_json(THING, &bundle) }
    }))
    .unwrap();
    assert!(
        shadow.len() < 8 * 1024,
        "AWS caps a shadow at 8 KB: {} B",
        shadow.len()
    );
}

#[test]
fn a_secured_configuration_is_refused_never_downgraded() {
    let err = try_config_for(
        "opc.tcp://127.0.0.1:4855/x",
        2,
        4,
        &catalogue::present(),
        |d| {
            d.sec_policy = "Basic256Sha256".into();
            d.sec_mode = "SignAndEncrypt".into();
        },
    )
    .unwrap_err();
    assert!(matches!(err, ConfigError::Settings(_)), "{err:?}");
    assert!(err.to_string().contains("Basic256Sha256"), "{err}");
}

#[test]
fn a_bundle_whose_digest_does_not_match_is_refused() {
    let err = try_config_for(
        "opc.tcp://127.0.0.1:4855/x",
        2,
        5,
        &catalogue::present(),
        |d| {
            d.sha256_override = Some("0".repeat(64));
        },
    )
    .unwrap_err();
    assert!(matches!(err, ConfigError::Bundle(_)), "{err:?}");
    assert!(err.to_string().contains("sha256"), "{err}");
}

#[test]
fn a_bundle_for_another_version_is_refused() {
    let bundle = documents::bundle(THING, 7, &catalogue::present());
    let mut desired = Desired::new("opc.tcp://127.0.0.1:4855/x", 2).to_json(THING, &bundle);
    desired["cfg"]["v"] = 8.into();
    let settings: DesiredSettings = serde_json::from_value(desired).unwrap();
    let err = gateway_opcua::AppliedConfig::new(settings, &bundle.payload).unwrap_err();
    assert!(err.to_string().contains("version"), "{err}");
}

// ---------------------------------------------------------------------------
// provision
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn provision_applies_every_present_tag_and_names_the_absent_one() {
    let rig = Rig::start().await;
    let reported = rig
        .apply_and_wait(rig.config(1, &catalogue::tags(), |_| {}))
        .await;

    assert_eq!(reported.applied, catalogue::present().len());
    assert_eq!(reported.failed, 1);
    let failed: Vec<_> = reported
        .failed_sample
        .iter()
        .map(|f| f.a.as_str())
        .collect();
    assert_eq!(failed, catalogue::missing_addresses());
    assert_eq!(reported.failed_sample[0].s, BAD_NODE_ID_UNKNOWN);
    assert_eq!(reported.last_error, None);
    assert_eq!(rig.server.sessions_activated(), 1);

    // Re-delivering the configuration that is already running is a no-op:
    // no reconnect, no resubscribe.
    rig.client
        .apply(rig.config(1, &catalogue::tags(), |_| {}))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let reported = rig.client.reported();
    assert!(reported.state_is("running"), "{reported:?}");
    assert_eq!(rig.server.sessions_activated(), 1);
}

// ---------------------------------------------------------------------------
// telemetry: the §4.3 wire format, per type
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn telemetry_matches_the_wire_contract_for_every_type() {
    let rig = Rig::start().await;
    rig.apply_and_wait(rig.config(1, &catalogue::tags(), |_| {}))
        .await;

    // Long enough for the 5 s group to report more than its initial value.
    let batches = encode(collect(&rig.client, Duration::from_millis(6_500)).await);
    assert!(!batches.is_empty(), "no telemetry at all");
    assert!(
        batches.iter().all(|b| b["v"] == 1),
        "every batch carries cfg v1"
    );

    let rows = rows_by_address(&batches);
    let last = |address: &str| -> Vec<Value> {
        rows.get(address)
            .and_then(|r| r.last().cloned())
            .unwrap_or_else(|| panic!("{address} never reported; saw {:?}", rows.keys()))
    };

    for address in catalogue::missing_addresses() {
        assert!(
            !rows.contains_key(address),
            "{address} does not exist but reported"
        );
    }

    // Every tag declares its encoding in the catalogue; each is checked here.
    for tag in catalogue::present() {
        let encoding = tag.encoding.expect("present tags declare an encoding");
        let row = last(tag.address);
        assert!(
            encoding.matches(&row[2]),
            "{}: {} — got {}",
            tag.address,
            tag.proves,
            row[2]
        );
    }

    // f32 widened through its shortest decimal: 1.1, never 1.100000023841858.
    for row in &rows["Line1.Pressure"] {
        let v = row[2].as_f64().unwrap();
        assert_eq!(
            (v * 10.0).round() / 10.0,
            v,
            "Line1.Pressure widened naively: {v}"
        );
    }

    // 64-bit integers past 2^53 arrive exactly, as tagged strings.
    for (address, offset) in [("Line1.BigCounter", 0i128), ("Line1.Serial", 7)] {
        for row in &rows[address] {
            let v: i128 = row[2]["v"].as_str().unwrap().parse().unwrap();
            let ticks = v - ABOVE_2_53 as i128 - offset;
            assert!(
                v > SAFE_INT as i128 && (0..10_000).contains(&ticks),
                "{address}: {v}"
            );
        }
    }

    // ByteString: base64 of [tick, 1, 2, 0xfe, 0xff].
    let blob = last("Line1.Blob")[2]["v"].as_str().unwrap().to_string();
    assert!(
        (0..=255u8).any(|t| base64_encode(&[t, 1, 2, 0xfe, 0xff]) == blob),
        "Line1.Blob = {blob}"
    );
    assert_eq!(last("Line1.BatchId")[2]["v"], BATCH_ID);
    assert_eq!(last("Line1.Profile")[2].as_array().unwrap().len(), 5);

    // Status: omitted when Good, the 4th element when not.
    assert_eq!(last("Line1.Temp").len(), 3, "a Good status is omitted");
    let faulty = last("Line1.Faulty");
    assert_eq!(
        faulty.len(),
        4,
        "a Bad status travels as the 4th element: {faulty:?}"
    );
    assert_eq!(faulty[3], FAULT_STATUS.bits());

    // Report-by-exception: a value that never changes is reported once.
    let n = |address: &str| rows.get(address).map_or(0, Vec::len);
    assert_eq!(
        n("Line1.Static"),
        1,
        "Line1.Static repeated: the subscription is polling"
    );
    assert!(n("Line1.Temp") >= 4, "Line1.Temp n={}", n("Line1.Temp"));

    // A second scan rate is a second, slower subscription.
    assert!(
        n("Line2.Level") < n("Line1.Temp"),
        "Line2.Level n={} vs Line1.Temp n={}",
        n("Line2.Level"),
        n("Line1.Temp")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn status_transitions_are_reported_as_they_happen() {
    let rig = Rig::start().await;
    rig.apply_and_wait(rig.config(1, &catalogue::present(), |_| {}))
        .await;
    collect(&rig.client, Duration::from_millis(1_500)).await;

    rig.server.set_fault(false);
    let rows = rows_by_address(&encode(collect(&rig.client, secs(2)).await));
    let cleared = rows["Line1.Faulty"].last().unwrap().clone();
    assert_eq!(cleared.len(), 3, "Good again: {cleared:?}");
    assert_eq!(cleared[2], FAULT_CLEARED_VALUE);

    rig.server.set_fault(true);
    let rows = rows_by_address(&encode(collect(&rig.client, secs(2)).await));
    let faulted = rows["Line1.Faulty"].last().unwrap().clone();
    assert_eq!(
        faulted.get(3),
        Some(&Value::from(FAULT_STATUS.bits())),
        "{faulted:?}"
    );
}

// ---------------------------------------------------------------------------
// reconfiguration
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn reconfiguration_is_applied_live_on_the_same_session() {
    let rig = Rig::start().await;
    rig.apply_and_wait(rig.config(1, &catalogue::tags(), |_| {}))
        .await;

    // Drop the slow group and the absent tag; move Line1.Temp to the slow rate.
    let mut v2: Vec<_> = catalogue::present()
        .into_iter()
        .filter(|t| !t.address.starts_with("Line2."))
        .collect();
    v2.iter_mut()
        .filter(|t| t.address == "Line1.Temp")
        .for_each(|t| t.scan_rate_ms = SLOW_MS);

    let reported = rig.apply_and_wait(rig.config(2, &v2, |_| {})).await;
    assert_eq!(reported.applied, v2.len());
    assert_eq!(reported.failed, 0);
    assert_eq!(
        rig.server.sessions_activated(),
        1,
        "reconfiguring must not reconnect"
    );

    // Anything queued up to now may legitimately still be v1.
    rig.client.drain(usize::MAX);
    let samples = collect(&rig.client, secs(3)).await;
    assert!(!samples.is_empty());
    assert!(
        samples.iter().all(|s| s.cfg_v == 2),
        "samples after the switch are stamped v2"
    );

    // The old subscriptions are gone: removed tags stop, and nothing is
    // delivered twice (once by the old subscription and once by the new).
    let removed: Vec<_> = samples
        .iter()
        .filter(|s| s.address.starts_with("Line2."))
        .collect();
    assert!(
        removed.is_empty(),
        "removed tags still reporting: {removed:?}"
    );
    let mut seen = HashSet::new();
    for s in &samples {
        assert!(
            seen.insert((s.address.clone(), s.ts_ms, format!("{:?}", s.value))),
            "delivered twice: {s:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_endpoint_is_a_new_session() {
    let rig = Rig::start().await;
    let other = TestServer::start(opcua_test_server::Options::loopback())
        .await
        .unwrap();
    rig.apply_and_wait(rig.config(1, &catalogue::present(), |_| {}))
        .await;

    let moved = config_for(
        &other.endpoint(),
        other.namespace_index(),
        2,
        &catalogue::present(),
        |_| {},
    );
    rig.apply_and_wait(moved).await;
    assert_eq!(
        other.sessions_activated(),
        1,
        "connected to the new endpoint"
    );

    // Taking the old server away must not matter any more.
    rig.server.stop().await;
    rig.client.drain(usize::MAX);
    let samples = collect(&rig.client, secs(2)).await;
    assert!(
        samples.iter().any(|s| s.address == "Line1.Temp"),
        "data keeps flowing from the new server"
    );
    assert!(rig.client.reported().state_is("running"));
}

#[tokio::test(flavor = "multi_thread")]
async fn namespace_uri_rescues_a_wrong_index() {
    let rig = Rig::start().await;
    // ns=99 is deliberately wrong; only URI resolution can save it.
    let config = rig.config(3, &catalogue::present(), |d| {
        d.ns = 99;
        d.ns_uri = Some(NAMESPACE_URI.into());
    });
    let reported = rig.apply_and_wait(config).await;
    assert_eq!(reported.applied, catalogue::present().len());
    assert_eq!(reported.failed, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_namespace_without_a_uri_fails_loudly() {
    let rig = Rig::start().await;
    let config = rig.config(3, &catalogue::present(), |d| {
        d.ns = 99;
        d.ns_uri = None;
    });
    rig.client.apply(config).unwrap();
    let reported = wait_for(&rig.client, "an error", secs(10), |r| r.state_is("error")).await;
    let error = reported.last_error.unwrap_or_default();
    assert!(error.contains("monitored items were rejected"), "{error}");
}

// ---------------------------------------------------------------------------
// resilience
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn server_loss_is_noticed_backed_off_and_recovered_from() {
    let rig = Rig::start().await;
    rig.apply_and_wait(rig.config(1, &catalogue::present(), |_| {}))
        .await;

    // Kill the PLC mid-session.
    let options = rig.server.stop().await;
    let started = Instant::now();
    let reported = wait_for(&rig.client, "the loss to be noticed", secs(5), |r| {
        r.state_is("error")
    })
    .await;
    assert!(
        reported
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("session lost"),
        "{reported:?}"
    );
    log::info!("server loss noticed after {:?}", started.elapsed());

    // With the host up and the server down, every attempt is refused at once;
    // a tight loop would make hundreds of them.
    let listener = CountingListener::bind(options.port).await;
    tokio::time::sleep(secs(6)).await;
    let attempts = listener.attempts();
    listener.close().await;
    assert!(
        (1..=7).contains(&attempts),
        "{attempts} reconnect attempts in 6 s"
    );

    // Bring the same endpoint back: the gateway must return on its own.
    let server = TestServer::start(options).await.unwrap();
    let reported = wait_for(&rig.client, "recovery", secs(45), |r| r.state_is("running")).await;
    assert_eq!(reported.applied, catalogue::present().len());
    assert_eq!(server.sessions_activated(), 1);
    assert!(
        !collect(&rig.client, secs(2)).await.is_empty(),
        "data flows again"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_link_is_noticed_by_the_keepalive() {
    let server = TestServer::start(opcua_test_server::Options::loopback())
        .await
        .unwrap();
    let relay = Blackhole::start(SocketAddr::from((Ipv4Addr::LOCALHOST, server.port())))
        .await
        .unwrap();
    let endpoint = format!(
        "opc.tcp://127.0.0.1:{}{}",
        relay.port(),
        opcua_test_server::DEFAULT_PATH
    );
    let client = client();
    let config = config_for(
        &endpoint,
        server.namespace_index(),
        1,
        &catalogue::present(),
        |d| {
            d.keepalive_ms = 1_000;
        },
    );
    client.apply(config).unwrap();
    wait_for(&client, "running", secs(20), |r| r.state_is("running")).await;

    // No RST, no FIN: the cable has been pulled.
    relay.set_silent(true);
    let started = Instant::now();
    let reported = wait_for(&client, "the silent link to be noticed", secs(40), |r| {
        r.state_is("error")
    })
    .await;
    log::info!("silent link noticed after {:?}", started.elapsed());
    assert!(
        reported
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("session lost"),
        "{reported:?}"
    );

    relay.set_silent(false);
    wait_for(&client, "recovery", secs(45), |r| r.state_is("running")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_endpoint_is_retried_with_backoff() {
    let listener = CountingListener::bind(0).await;
    let client = client();
    let endpoint = format!("opc.tcp://127.0.0.1:{}/nothing-here", listener.port);
    client
        .apply(config_for(&endpoint, 2, 1, &catalogue::present(), |_| {}))
        .unwrap();

    let reported = wait_for(&client, "an error", secs(10), |r| r.state_is("error")).await;
    assert!(
        reported
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains(&endpoint),
        "the error names the endpoint: {reported:?}"
    );
    tokio::time::sleep(secs(6)).await;
    let attempts = listener.attempts();
    assert!((2..=8).contains(&attempts), "{attempts} attempts in ~6 s");
}

// ---------------------------------------------------------------------------
// disable (docs/OPCUA_INTEGRATION_TEST.md §8.11)
// ---------------------------------------------------------------------------

/// Upper bound on how long a disable may take: the shutdown timeout plus
/// polling slack.
const DISABLE_WITHIN: Duration = Duration::from_secs(6);

#[tokio::test(flavor = "multi_thread")]
async fn disable_idles_the_driver_and_stops_telemetry_then_resumes() {
    let rig = Rig::start().await;
    rig.apply_and_wait(rig.config(1, &catalogue::present(), |_| {}))
        .await;

    rig.client.disable().unwrap();
    let reported = wait_for(&rig.client, "idle", DISABLE_WITHIN, |r| r.state_is("idle")).await;
    assert_eq!(reported.cfg_v, 1, "disabling keeps the configuration");

    rig.client.drain(usize::MAX);
    let quiet = collect(&rig.client, secs(2)).await;
    assert!(quiet.is_empty(), "{} samples while disabled", quiet.len());

    rig.apply_and_wait(rig.config(2, &catalogue::present(), |_| {}))
        .await;
    assert!(
        !collect(&rig.client, secs(2)).await.is_empty(),
        "data flows again"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disable_completes_while_the_server_is_down() {
    let rig = Rig::start().await;
    rig.apply_and_wait(rig.config(1, &catalogue::present(), |_| {}))
        .await;

    let _options = rig.server.stop().await;
    wait_for(&rig.client, "error", secs(5), |r| r.state_is("error")).await;

    rig.client.disable().unwrap();
    wait_for(&rig.client, "idle", DISABLE_WITHIN, |r| r.state_is("idle")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn disable_completes_while_the_server_is_hung() {
    let server = TestServer::start(opcua_test_server::Options::loopback())
        .await
        .unwrap();
    let relay = Blackhole::start(SocketAddr::from((Ipv4Addr::LOCALHOST, server.port())))
        .await
        .unwrap();
    let endpoint = format!(
        "opc.tcp://127.0.0.1:{}{}",
        relay.port(),
        opcua_test_server::DEFAULT_PATH
    );
    let client = client();
    client
        .apply(config_for(
            &endpoint,
            server.namespace_index(),
            1,
            &catalogue::present(),
            |_| {},
        ))
        .unwrap();
    wait_for(&client, "running", secs(20), |r| r.state_is("running")).await;

    // The session is still up as far as the client knows, but CloseSession
    // will never be answered.
    relay.set_silent(true);
    let started = Instant::now();
    client.disable().unwrap();
    wait_for(&client, "idle", DISABLE_WITHIN, |r| r.state_is("idle")).await;
    log::info!("disable against a hung server took {:?}", started.elapsed());

    // And the driver is not wedged: the next configuration is taken up.
    relay.set_silent(false);
    client
        .apply(config_for(
            &endpoint,
            server.namespace_index(),
            2,
            &catalogue::present(),
            |_| {},
        ))
        .unwrap();
    wait_for(&client, "running on v2", secs(20), |r| {
        r.state_is("running") && r.cfg_v == 2
    })
    .await;
}

// ---------------------------------------------------------------------------
// a sanity check on the fixture itself
// ---------------------------------------------------------------------------

#[test]
fn every_catalogue_encoding_is_exercised() {
    let used: HashSet<_> = catalogue::present()
        .iter()
        .filter_map(|t| t.encoding)
        .map(|e| format!("{e:?}"))
        .collect();
    for e in [
        Encoding::Number,
        Encoding::Bool,
        Encoding::String,
        Encoding::Array,
        Encoding::I64,
        Encoding::U64,
        Encoding::B64,
        Encoding::Guid,
    ] {
        assert!(used.contains(&format!("{e:?}")), "no tag exercises {e:?}");
    }
}
