//! Hardware-in-the-loop scenario for the ESP32-S3 OPC UA gateway.
//!
//! Drives a real device against the Rust OPC UA test server (in this process)
//! and a real AWS IoT account, and asserts on the three things observable from
//! outside the firmware: the `opcua` shadow's `reported` block, the telemetry
//! that reaches CloudWatch via the `dt/+/opcua` rule, and the serial log.
//!
//! Most of what this checks is also covered without hardware by
//! `cargo test -p gateway-opcua`, which runs the same client against the same
//! server over loopback. This run is for what only the device can show: heap,
//! the ESP-IDF runtime, NVS, and the MQTT/AWS planes.
//!
//! ```sh
//! source aws-env.sh
//! cargo run -p gateway-hil --target host-tuple -- \
//!     --thing 28848553144F --server-host 192.168.50.28 \
//!     --port /dev/cu.usbmodem21401 --flash
//! ```
//!
//! The device dials this host, so `--server-host` must be a LAN address it can
//! route to, and the host firewall must admit inbound TCP on `--server-port`
//! (see `docs/OPCUA_INTEGRATION_TEST.md` §3.5).

mod cloud;
mod device;
mod offline;
mod phases;
mod report;

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};

use cloud::{now_ms, Cloud};
use device::SerialMonitor;
use phases::{Ctx, Server, PHASES};
use report::{banner, info, PhaseResult};

const USAGE: &str = "\
usage: gateway-hil --thing NAME --server-host LAN-IP [options]

  --thing NAME          AWS IoT thing name of the device under test
  --server-host IP      address the DEVICE uses to reach this host (not 127.0.0.1)
  --server-port PORT    OPC UA server port                        (default 4855)
  --server-path PATH    OPC UA endpoint path                      (default /ergousha/test)
  --port DEVICE         serial port of the device; enables log assertions and `reboot`
  --flash               flash target/xtensa-esp32s3-espidf/release/esp32-opcua-gateway first
  --elf PATH            firmware ELF to flash and to decode the log against
  --offline             for a host the device cannot reach (e.g. a firewalled
                        workstation): run the phases that need no route from the
                        device to this host; requires --port
  --phases A,B,...      subset of phases to run
  --region REGION       AWS region                                (default eu-central-1)
  --iot-endpoint HOST   IoT data endpoint                         (default: cfg.toml's iot_endpoint)
  --artifacts DIR       where logs and the summary go             (default gateway-hil/artifacts)
  --cleanup             clear the retained tag bundles this run published";

struct Args {
    thing: String,
    server_host: String,
    server_port: u16,
    server_path: String,
    port: Option<String>,
    flash: bool,
    elf: PathBuf,
    phases: Vec<String>,
    offline: bool,
    region: String,
    iot_endpoint: Option<String>,
    artifacts: PathBuf,
    cleanup: bool,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("gateway-hil lives in the workspace")
        .to_path_buf()
}

fn parse_args() -> Result<Args> {
    let repo = repo_root();
    let mut args = Args {
        thing: String::new(),
        server_host: String::new(),
        server_port: opcua_test_server::DEFAULT_PORT,
        server_path: opcua_test_server::DEFAULT_PATH.to_string(),
        port: None,
        flash: false,
        elf: repo.join("target/xtensa-esp32s3-espidf/release/esp32-opcua-gateway"),
        phases: Vec::new(),
        offline: false,
        region: "eu-central-1".into(),
        iot_endpoint: None,
        artifacts: repo.join("gateway-hil/artifacts"),
        cleanup: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--thing" => args.thing = value()?,
            "--server-host" => args.server_host = value()?,
            "--server-port" => args.server_port = value()?.parse().context("--server-port")?,
            "--server-path" => args.server_path = value()?,
            "--port" => args.port = Some(value()?),
            "--flash" => args.flash = true,
            "--elf" => args.elf = value()?.into(),
            "--phases" => args.phases = value()?.split(',').map(str::to_string).collect(),
            "--offline" => args.offline = true,
            "--region" => args.region = value()?,
            "--iot-endpoint" => args.iot_endpoint = Some(value()?),
            "--artifacts" => args.artifacts = value()?.into(),
            "--cleanup" => args.cleanup = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    if args.thing.is_empty() || args.server_host.is_empty() {
        bail!("--thing and --server-host are required\n\n{USAGE}");
    }
    if args
        .server_host
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
    {
        bail!("--server-host must be an address the DEVICE can reach, not loopback");
    }
    if args.flash && args.port.is_none() {
        bail!("--flash needs --port");
    }
    if args.offline && args.port.is_none() {
        bail!("--offline asserts on the serial log and needs --port");
    }
    let default = if args.offline {
        offline::PHASES
    } else {
        PHASES
    };
    if args.phases.is_empty() {
        args.phases = default.iter().map(|p| p.to_string()).collect();
    }
    let known = |p: &str| PHASES.contains(&p) || offline::PHASES.contains(&p);
    let unknown: Vec<_> = args.phases.iter().filter(|p| !known(p)).collect();
    if !unknown.is_empty() {
        bail!(
            "unknown phases {unknown:?}; known: {PHASES:?}, offline: {:?}",
            offline::PHASES
        );
    }
    Ok(args)
}

/// The IoT endpoint the firmware itself is built with.
fn iot_endpoint_from_cfg(repo: &Path) -> Result<String> {
    let path = repo.join("cfg.toml");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {} (or pass --iot-endpoint)", path.display()))?;
    let table: toml::Table = text.parse().context("cfg.toml is not valid TOML")?;
    table
        .get("esp32-opcua-gateway")
        .and_then(|t| t.get("iot_endpoint"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("cfg.toml has no [esp32-opcua-gateway] iot_endpoint"))
}

fn stamp() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

async fn run() -> Result<bool> {
    let args = parse_args()?;
    let repo = repo_root();
    std::fs::create_dir_all(&args.artifacts)?;
    let stamp = stamp();
    let serial_log = args.artifacts.join(format!("device-serial-{stamp}.log"));

    if args.flash {
        banner("FLASHING FIRMWARE");
        let port = args.port.as_deref().expect("checked in parse_args");
        device::flash(port, &args.elf, &repo).await?;
    }

    let iot_endpoint = match args.iot_endpoint.clone() {
        Some(e) => e,
        None => iot_endpoint_from_cfg(&repo)?,
    };
    let mut cloud = Cloud::new(&args.thing, &args.region, &iot_endpoint).await?;
    let t0_ms = now_ms();
    // Anchor shadow freshness to the start of the run, so a `reported` block
    // left over from an earlier boot can never satisfy an assertion.
    cloud.require_fresh_since(t0_ms);

    // Offline, nothing may listen on the LAN: the device is meant to find
    // its endpoint silent. The server still serves `preflight` over loopback.
    let bind = if args.offline { "127.0.0.1" } else { "0.0.0.0" };
    let server = Server::new(opcua_test_server::Options {
        bind: bind.parse().expect("valid address"),
        port: args.server_port,
        path: args.server_path.clone(),
        advertised_host: Some(args.server_host.clone()),
        ..opcua_test_server::Options::default()
    });
    let monitor = args
        .port
        .as_deref()
        .map(|p| SerialMonitor::new(p, serial_log.clone(), args.elf.clone(), repo.clone()));

    let last_version = cloud
        .last_cfg_version()
        .await
        .context("reading the opcua shadow")?;
    let mut ctx = Ctx {
        cloud,
        server,
        monitor,
        thing: args.thing.clone(),
        applied_version: 0,
        t0_ms,
        published: Vec::new(),
        last_version,
    };

    if let Some(monitor) = ctx.monitor.as_mut() {
        // Attaching reboots the device (see `SerialMonitor`), so every run
        // starts from a clean boot whose log is captured from the first line.
        monitor.start().await?;
    }

    let mut results = Vec::new();
    for name in &args.phases {
        banner(&format!("PHASE: {name}"));
        let result = match phases::run(name, &mut ctx).await {
            Ok(r) => r,
            // A phase blowing up is a failure, not a crash of the run.
            Err(e) => {
                let mut r = PhaseResult::new(name);
                r.check("phase completed without an error", false, format!("{e:#}"));
                r
            }
        };
        results.push(result);
    }

    ctx.server.stop().await;
    if let Some(monitor) = ctx.monitor.as_mut() {
        monitor.stop();
    }
    if args.cleanup {
        // Retained messages outlive the test. Left behind, a later boot would
        // pull a tag bundle from a run nobody remembers.
        for version in ctx.published.iter().copied() {
            let topic = opcua_test_server::documents::bundle_topic(&args.thing, version);
            if let Err(e) = ctx.cloud.clear_retained(&topic).await {
                log::warn!("could not clear {topic}: {e:#}");
            }
        }
        info("cleared the retained tag bundles this run published");
    }

    let (passed, total) = report::print_summary(&results);
    if ctx.monitor.is_some() {
        println!("  serial log : {}", serial_log.display());
    }
    let summary = report::summary_json(&args.thing, &ctx.server.endpoint(), &results);
    let summary_path = args.artifacts.join(format!("summary-{stamp}.json"));
    std::fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)?;
    println!("  summary    : {}", summary_path.display());
    Ok(passed == total)
}

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(
        "info,opcua_server=warn,opcua_client=warn,opcua_crypto=off,tracing::span=warn,aws=warn",
    ))
    .init();
    match run().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}
