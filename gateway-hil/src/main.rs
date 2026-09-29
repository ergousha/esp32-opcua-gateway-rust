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
//!
//! On the way out the run puts the device's configuration back the way it
//! found it — after a failure, a panic in a phase, or Ctrl-C too; see
//! [`restore`]. A run that dies before it can leaves that to `--restore`.

mod cloud;
mod device;
mod interrupt;
mod offline;
mod phases;
mod report;
mod restore;

use std::future::Future;
use std::net::IpAddr;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::task::{Context as TaskContext, Poll};

use anyhow::{anyhow, bail, Context, Result};
use opcua_test_server::documents;

use cloud::{now_ms, Cloud};
use device::SerialMonitor;
use phases::{Ctx, Server, PHASES};
use report::{banner, info, PhaseResult};
use restore::Snapshot;

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
  --cleanup             clear the retained tag bundles this run published, except
                        the ones the restored configuration points at
  --restore             put back the configuration a run that did not finish
                        saved in --artifacts, then exit; needs only --thing

Every run puts the device's configuration back the way it found it on exit,
Ctrl-C included.";

const DEFAULT_REGION: &str = "eu-central-1";

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
    restore: bool,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("gateway-hil lives in the workspace")
        .to_path_buf()
}

fn default_artifacts() -> PathBuf {
    repo_root().join("gateway-hil/artifacts")
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
        region: DEFAULT_REGION.into(),
        iot_endpoint: None,
        artifacts: default_artifacts(),
        cleanup: false,
        restore: false,
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
            "--restore" => args.restore = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    if args.thing.is_empty() || (args.server_host.is_empty() && !args.restore) {
        bail!("--thing and --server-host are required\n\n{USAGE}");
    }
    if args.restore {
        return Ok(args);
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

fn when(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || format!("{ms} ms"),
        |t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

fn iot_endpoint(args: &Args, repo: &Path) -> Result<String> {
    match args.iot_endpoint.clone() {
        Some(e) => Ok(e),
        None => iot_endpoint_from_cfg(repo),
    }
}

/// The command that finishes a restore, with the flags it needs from this
/// one.
fn restore_command(args: &Args) -> String {
    let mut command = format!(
        "cargo run -p gateway-hil --target host-tuple -- --thing {} --restore",
        args.thing
    );
    if args.region != DEFAULT_REGION {
        command += &format!(" --region {}", args.region);
    }
    if let Some(endpoint) = &args.iot_endpoint {
        command += &format!(" --iot-endpoint {endpoint}");
    }
    if args.artifacts != default_artifacts() {
        command += &format!(" --artifacts {}", args.artifacts.display());
    }
    command
}

/// Said last and loudly: the device is still pointed at a server that has
/// stopped, and nothing else will tell anyone.
fn print_unrestored(args: &Args, pending: &Path) {
    banner("THE DEVICE WAS NOT RESTORED");
    println!("  It is still configured for a gateway-hil test server that is no longer");
    println!("  running. What it had before the run is saved in");
    println!("  {}. Put it back with:\n", pending.display());
    println!("      {}\n", restore_command(args));
}

fn catch_interrupts(what: &str) {
    match interrupt::catch() {
        Ok(()) => info(format!("Ctrl-C {what}; a second Ctrl-C quits at once")),
        Err(e) => log::warn!(
            "cannot catch Ctrl-C ({e}): an interrupted run leaves the device to --restore"
        ),
    }
}

async fn run() -> Result<bool> {
    let args = parse_args()?;
    let repo = repo_root();
    std::fs::create_dir_all(&args.artifacts)?;
    let pending = restore::path(&args.artifacts, &args.thing);
    if args.restore {
        return restore_pending(&args, &repo, &pending).await;
    }
    // Starting over an unfinished restore would save the leftover as the
    // pre-run configuration, and put the device back on that.
    if let Some(snapshot) = Snapshot::load(&pending)? {
        bail!(
            "{} holds the configuration a run found on {} at {} and never put back.\n\
             Restore it first:\n\n    {}\n\nor delete the file to give it up.",
            pending.display(),
            snapshot.thing,
            when(snapshot.taken_ms),
            restore_command(&args)
        );
    }
    let stamp = stamp();
    let serial_log = args.artifacts.join(format!("device-serial-{stamp}.log"));

    if args.flash {
        banner("FLASHING FIRMWARE");
        let port = args.port.as_deref().expect("checked in parse_args");
        device::flash(port, &args.elf, &repo).await?;
    }

    let mut cloud = Cloud::new(&args.thing, &args.region, &iot_endpoint(&args, &repo)?).await?;
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

    let mut ctx = Ctx {
        cloud,
        server,
        monitor,
        thing: args.thing.clone(),
        applied_version: 0,
        t0_ms,
        published: Vec::new(),
        last_version: 0,
    };

    if let Some(monitor) = ctx.monitor.as_mut() {
        // Attaching reboots the device (see `SerialMonitor`), so every run
        // starts from a clean boot whose log is captured from the first line.
        monitor.start().await?;
    }

    // As late as possible, and on disk before anything changes: from here on
    // nothing may return early, because the device has to be put back.
    banner("PRE-RUN CONFIGURATION");
    let snapshot = restore::take(&ctx.cloud, &args.thing, &ctx.server.endpoint())
        .await
        .context("saving the configuration the run is about to change")?;
    snapshot.save(&pending)?;
    for line in snapshot.describe() {
        info(line);
    }
    info(format!(
        "kept in {} until it is restored",
        pending.display()
    ));
    ctx.last_version = snapshot.last_version;
    catch_interrupts("stops the run and restores the device");

    let mut results = Vec::new();
    let interrupted = tokio::select! {
        () = run_phases(&mut ctx, &args.phases, &mut results) => false,
        () = interrupt::wait() => true,
    };
    if interrupted {
        // The phase in flight is the first one without a result.
        let name = args
            .phases
            .get(results.len())
            .map_or("interrupted", String::as_str);
        banner(&format!("INTERRUPTED DURING {name}"));
        results.push(failed(name, "phase ran to the end", "interrupted".into()));
    }

    ctx.server.stop().await;
    let mut published: Vec<String> = ctx
        .published
        .iter()
        .map(|v| documents::bundle_topic(&args.thing, *v))
        .collect();
    let changed = !published.is_empty();
    let restored = restore::finish(
        &mut ctx.cloud,
        &snapshot,
        ctx.last_version,
        &mut published,
        changed,
    )
    .await;
    if restored.settled {
        if let Err(e) = std::fs::remove_file(&pending) {
            log::warn!("could not remove {}: {e}", pending.display());
        }
    }
    if let Some(monitor) = ctx.monitor.as_mut() {
        monitor.stop();
    }
    if args.cleanup {
        // Retained messages outlive the test. Left behind, a later boot would
        // pull a tag bundle from a run nobody remembers.
        match restore::cleanup(&ctx.cloud, &snapshot, &published).await {
            Ok((cleared, kept)) => info(format!(
                "cleared {} retained tag bundles this run published; kept {kept:?}",
                cleared.len()
            )),
            Err(e) => log::warn!("cleared no retained tag bundle: {e:#}"),
        }
    }
    results.push(restored.result);

    let (passed, total) = report::print_summary(&results);
    println!("  restore    : {}", restored.line);
    if ctx.monitor.is_some() {
        println!("  serial log : {}", serial_log.display());
    }
    let mut summary = report::summary_json(&args.thing, &ctx.server.endpoint(), &results);
    summary["restore"] = restored.json;
    let summary_path = args.artifacts.join(format!("summary-{stamp}.json"));
    std::fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)?;
    println!("  summary    : {}", summary_path.display());
    if !restored.settled {
        print_unrestored(&args, &pending);
    }
    Ok(passed == total)
}

/// `--restore`: puts back what a run that did not finish saved.
async fn restore_pending(args: &Args, repo: &Path, pending: &Path) -> Result<bool> {
    let snapshot = Snapshot::load(pending)?.ok_or_else(|| {
        anyhow!(
            "nothing to restore: there is no {} (a run removes it once it has put the \
             device back)",
            pending.display()
        )
    })?;
    banner("PRE-RUN CONFIGURATION");
    info(format!("saved by a run at {}", when(snapshot.taken_ms)));
    for line in snapshot.describe() {
        info(line);
    }
    if args.cleanup {
        info("--cleanup does nothing here: the list of bundles the run published died with it");
    }
    let mut cloud = Cloud::new(&args.thing, &args.region, &iot_endpoint(args, repo)?).await?;
    catch_interrupts("stops waiting for the device");

    let restored = restore::finish(
        &mut cloud,
        &snapshot,
        snapshot.last_version,
        &mut Vec::new(),
        true,
    )
    .await;
    report::print_summary(std::slice::from_ref(&restored.result));
    println!("  restore    : {}", restored.line);
    if restored.settled {
        std::fs::remove_file(pending).with_context(|| format!("removing {}", pending.display()))?;
    } else {
        print_unrestored(args, pending);
    }
    Ok(restored.result.ok())
}

/// Runs the phases in order. A phase's failure is its own — an error and a
/// panic alike — so the run always gets as far as the restore.
async fn run_phases(ctx: &mut Ctx, names: &[String], results: &mut Vec<PhaseResult>) {
    // Printed by the ESP-IDF panic handler, never by a USB reset.
    let crash = regex::Regex::new(
        r"Guru Meditation|\*\*\*ERROR\*\*\*.*|abort\(\) was called.*|Rebooting\.\.\.",
    )
    .expect("valid pattern");
    for name in names {
        banner(&format!("PHASE: {name}"));
        let mark = ctx.mark();
        let mut result = match Unwound(Box::pin(phases::run(name, ctx))).await {
            Ok(Ok(r)) => r,
            // A phase blowing up is a failure, not a crash of the run.
            Ok(Err(e)) => failed(name, "phase completed without an error", format!("{e:#}")),
            // Nor is a bug in one: the device still has to be put back.
            Err(panic) => failed(
                name,
                "phase completed without panicking",
                panic_message(&*panic),
            ),
        };
        if let Some(log) = ctx.log_since(mark) {
            // A reboot can restore the expected state from NVS and hide itself.
            let found = crash.find(&log).map(|m| m.as_str().trim().to_string());
            result.check(
                "device did not crash during the phase",
                found.is_none(),
                found.unwrap_or_default(),
            );
        }
        results.push(result);
    }
}

fn failed(name: &str, check: &str, detail: String) -> PhaseResult {
    let mut r = PhaseResult::new(name);
    r.check(check, false, detail);
    r
}

/// A future whose panics come out as an `Err`, as `catch_unwind` does for a
/// closure.
struct Unwound<F>(Pin<Box<F>>);

impl<F: Future> Future for Unwound<F> {
    type Output = std::thread::Result<F::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let inner = self.0.as_mut();
        // Safe enough to assert: a future that panicked is never polled
        // again, and nothing in `Ctx` depends on a phase finishing.
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Err(panic) => Poll::Ready(Err(panic)),
        }
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".into())
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn phase(fail: bool) -> u32 {
        if fail {
            panic!("boom");
        }
        7
    }

    #[tokio::test]
    async fn a_panicking_phase_comes_out_as_an_error() {
        assert_eq!(Unwound(Box::pin(phase(false))).await.unwrap(), 7);
        let panic = Unwound(Box::pin(phase(true))).await.unwrap_err();
        assert_eq!(panic_message(&*panic), "boom");
    }
}
