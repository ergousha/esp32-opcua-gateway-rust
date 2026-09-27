//! The gateway's OPC UA client, running on a laptop.
//!
//! The same `gateway-opcua` code the ESP32 runs — same driver, same session
//! limits, same wire format — with stdout standing in for MQTT: telemetry
//! batches are printed exactly as they would be published to
//! `dt/<thing>/opcua`, and health as it would be reported to the shadow.
//!
//! With no arguments it starts the OPC UA test server in-process on loopback
//! and applies the test catalogue, so client and server run together with
//! nothing else involved:
//!
//! ```sh
//! cargo run -p gateway-opcua --example local_gateway --target host-tuple
//! ```
//!
//! Pointed at any other server — a real PLC on the LAN, say — it becomes a
//! field tool. The laptop dials out, so no inbound firewall rule is needed:
//!
//! ```sh
//! cargo run -p gateway-opcua --example local_gateway --target host-tuple -- \
//!     --endpoint opc.tcp://192.168.1.50:4840 --ns 3 \
//!     --tag Channel1.Device1.Tag1 --tag Channel1.Device1.Tag2 --rate 1000
//!
//! # or with a bundle file in the cloud's wire form, {"v":1,"g":[{"r":1000,"a":[…]}]}
//! cargo run -p gateway-opcua --example local_gateway --target host-tuple -- \
//!     --endpoint opc.tcp://192.168.1.50:4840 --ns-uri urn:plc:ns --bundle tags.json
//! ```

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use gateway_core::batcher::Batcher;
use gateway_core::bundle::TagBundle;
use gateway_core::codec::sha256_hex;
use gateway_core::health::Reported;
use gateway_core::settings::DesiredSettings;
use gateway_opcua::{AppliedConfig, Options};
use opcua_test_server::catalogue::{self, FAST_MS};
use opcua_test_server::documents::{self, Desired};
use opcua_test_server::TestServer;
use serde_json::json;

const THING: &str = "laptop";

const USAGE: &str = "\
usage: local_gateway [--endpoint URL] [--ns N] [--ns-uri URI] [--id-type s|i|g|b]
                     [--bundle FILE | --tag ADDRESS... [--rate MS]]
                     [--keepalive MS] [--seconds N]

Without --endpoint, an OPC UA test server is started in-process on loopback
and the test catalogue is applied. With one, --ns defaults to 2 and --ns-uri,
when given, wins over it (resolved against the server's NamespaceArray).

Telemetry batches go to stdout, one JSON document per line, exactly as they
would be published; health changes go to stderr.";

#[derive(Default)]
struct Args {
    endpoint: Option<String>,
    ns: Option<u16>,
    ns_uri: Option<String>,
    id_type: Option<String>,
    bundle: Option<String>,
    tags: Vec<String>,
    rate: Option<u32>,
    keepalive_ms: Option<u32>,
    seconds: Option<u64>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--endpoint" => args.endpoint = Some(value()?),
            "--ns" => args.ns = Some(value()?.parse().context("--ns")?),
            "--ns-uri" => args.ns_uri = Some(value()?),
            "--id-type" => args.id_type = Some(value()?),
            "--bundle" => args.bundle = Some(value()?),
            "--tag" => args.tags.push(value()?),
            "--rate" => args.rate = Some(value()?.parse().context("--rate")?),
            "--keepalive" => args.keepalive_ms = Some(value()?.parse().context("--keepalive")?),
            "--seconds" => args.seconds = Some(value()?.parse().context("--seconds")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    if args.bundle.is_some() && !args.tags.is_empty() {
        bail!("--bundle and --tag are mutually exclusive");
    }
    Ok(args)
}

/// The bundle bytes to apply: from a file, from `--tag`s, or the catalogue.
fn bundle_bytes(args: &Args) -> Result<Vec<u8>> {
    if let Some(path) = &args.bundle {
        return std::fs::read(path).with_context(|| format!("reading {path}"));
    }
    if !args.tags.is_empty() {
        let doc = json!({ "v": 1, "g": [{ "r": args.rate.unwrap_or(FAST_MS), "a": args.tags }] });
        return Ok(serde_json::to_vec(&doc)?);
    }
    Ok(documents::bundle(THING, 1, &catalogue::tags()).payload)
}

/// Builds `state.desired` pointing at `bundle`, as the cloud would.
fn settings(args: &Args, endpoint: &str, default_ns: u16, bytes: &[u8]) -> Result<DesiredSettings> {
    let parsed: TagBundle = serde_json::from_slice(bytes).context("bundle is not valid JSON")?;
    let bundle = documents::Bundle {
        version: parsed.v,
        payload: bytes.to_vec(),
        sha256: sha256_hex(bytes),
        count: parsed.g.iter().map(|g| g.a.len()).sum(),
        topic: documents::bundle_topic(THING, parsed.v),
    };

    let mut desired = Desired::new(endpoint, args.ns.unwrap_or(default_ns));
    // The catalogue's namespace only means something on the test server.
    desired.ns_uri = match (&args.ns_uri, &args.endpoint) {
        (Some(uri), _) => Some(uri.clone()),
        (None, None) => Some(catalogue::NAMESPACE_URI.into()),
        (None, Some(_)) => None,
    };
    if let Some(ms) = args.keepalive_ms {
        desired.keepalive_ms = ms;
    }
    let mut doc = desired.to_json(THING, &bundle);
    if let Some(id_type) = &args.id_type {
        doc["instance"]["id_type"] = json!(id_type);
    }
    Ok(serde_json::from_value(doc)?)
}

fn print_health(r: &Reported) {
    let failed: Vec<String> = r
        .failed_sample
        .iter()
        .map(|f| format!("{} {:#010x}", f.a, f.s))
        .collect();
    eprintln!(
        "[health] state={} cfg_v={} applied={} failed={}{}{}",
        r.state.as_str(),
        r.cfg_v,
        r.applied,
        r.failed,
        if failed.is_empty() {
            String::new()
        } else {
            format!(" ({})", failed.join(", "))
        },
        r.last_error
            .as_deref()
            .map(|e| format!(" error={e:?}"))
            .unwrap_or_default()
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(
        "info,opcua_client=warn,opcua_server=warn,opcua_crypto=off,tracing::span=warn",
    ))
    .init();
    let args = parse_args()?;

    // Kept alive for the whole run when we started it ourselves.
    let mut local_server = None;
    let (endpoint, default_ns) = match &args.endpoint {
        Some(endpoint) => (endpoint.clone(), 2),
        None => {
            let server = TestServer::start(opcua_test_server::Options::loopback()).await?;
            let endpoint = server.endpoint();
            let ns = server.namespace_index();
            eprintln!("[server] in-process OPC UA test server on {endpoint}");
            local_server = Some(server);
            (endpoint, ns)
        }
    };

    let bundle = bundle_bytes(&args)?;
    let settings = settings(&args, &endpoint, default_ns, &bundle)?;
    let telemetry = settings.telemetry.clone();
    let config = AppliedConfig::new(settings, &bundle).map_err(|e| anyhow!("{e}"))?;
    eprintln!(
        "[config] v{} with {} tags against {endpoint}",
        config.settings.cfg.v,
        config.tags.len()
    );

    let (client, driver) = gateway_opcua::new(Options::new("local"));
    let driver_thread = gateway_opcua::spawn_thread(driver, "opcua", 4 * 1024 * 1024)?;
    client.apply(config)?;

    let mut batcher = Batcher::new(&telemetry);
    let deadline = args
        .seconds
        .map(|s| Instant::now() + Duration::from_secs(s));
    let mut last_health = None;
    loop {
        let now = gateway_opcua::system_clock_ms();
        for sample in client.drain(usize::MAX) {
            if let Some((batch, _)) = batcher.push(sample, now) {
                println!("{}", serde_json::to_string(&batch)?);
            }
        }
        if let Some((batch, _)) = batcher.poll(now) {
            println!("{}", serde_json::to_string(&batch)?);
        }

        let health = client.reported();
        let summary = (
            health.state,
            health.cfg_v,
            health.applied,
            health.last_error.clone(),
        );
        if last_health.as_ref() != Some(&summary) {
            print_health(&health);
            last_health = Some(summary);
        }

        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if let Some((batch, _)) = batcher.flush(gateway_opcua::system_clock_ms()) {
        println!("{}", serde_json::to_string(&batch)?);
    }

    // Client first: dropping the last handle ends the driver, which closes its
    // session while the server is still there to acknowledge it.
    drop(client);
    tokio::task::spawn_blocking(move || driver_thread.join())
        .await?
        .map_err(|_| anyhow!("the OPC UA driver thread panicked"))?;
    if let Some(server) = local_server {
        server.stop().await;
    }
    Ok(())
}
