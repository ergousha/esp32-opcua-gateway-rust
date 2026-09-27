//! Standalone OPC UA test server, for a real device to dial.
//!
//! ```sh
//! cargo run -p opcua-test-server --target host-tuple -- --bind 0.0.0.0 --host 192.168.1.20
//! ```
//!
//! For host-only work you do not need this at all: `gateway-opcua`'s tests and
//! its `local_gateway` example start the same server in-process.
//!
//! Fault injection is a line-oriented command channel on stdin, so a harness
//! (or a person) can drive it without a restart:
//!
//! ```text
//! fault on | fault off   set/clear the Bad StatusCode on Line1.Faulty
//! freeze   | thaw        stop/resume value updates
//! quit                   shut down
//! ```

use std::net::IpAddr;
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use opcua_test_server::{catalogue, Options, TestServer, DEFAULT_PATH, DEFAULT_PORT};
use tokio::io::{AsyncBufReadExt, BufReader};

const USAGE: &str = "\
usage: opcua-test-server [--bind ADDR] [--port PORT] [--path PATH] [--host NAME]

  --bind ADDR   address to listen on        (default 0.0.0.0)
  --port PORT   TCP port                     (default 4855)
  --path PATH   endpoint path                (default /ergousha/test)
  --host NAME   host name in the endpoint    (default: the bind address)";

fn parse_args() -> Result<Options> {
    let mut options = Options {
        bind: "0.0.0.0".parse().expect("valid address"),
        port: DEFAULT_PORT,
        path: DEFAULT_PATH.to_string(),
        ..Options::default()
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--bind" => options.bind = value()?.parse::<IpAddr>().context("--bind")?,
            "--port" => options.port = value()?.parse().context("--port")?,
            "--path" => options.path = value()?,
            "--host" => options.advertised_host = Some(value()?),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    Ok(options)
}

async fn run() -> Result<()> {
    let server = TestServer::start(parse_args()?).await?;

    println!("serving {}", server.endpoint());
    println!(
        "namespace {:?} = ns {}",
        catalogue::NAMESPACE_URI,
        server.namespace_index()
    );
    for tag in catalogue::tags() {
        let marker = if tag.present { "" } else { " (not created)" };
        println!(
            "  ns={};s={:<20} {:>5} ms  {}{marker}",
            server.namespace_index(),
            tag.address,
            tag.scan_rate_ms,
            tag.proves
        );
    }
    // A harness waits for this line rather than sleeping a guessed interval.
    println!("READY");

    // No `tokio::signal::ctrl_c()`: the workspace patches
    // `signal-hook-registry` with a stub (signals do not exist on ESP-IDF), so
    // it would fail at once. SIGINT's default action ends the process anyway.
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match line.trim().to_ascii_lowercase().as_str() {
                "fault on" => server.set_fault(true),
                "fault off" => server.set_fault(false),
                "freeze" => server.set_frozen(true),
                "thaw" => server.set_frozen(false),
                "quit" => break,
                "" => {}
                other => eprintln!("unknown command {other:?}"),
            },
            // stdin closed (e.g. `< /dev/null`): keep serving without the
            // command channel rather than dying with it.
            Ok(None) | Err(_) => std::future::pending::<()>().await,
        }
    }

    server.stop().await;
    Ok(())
}

fn main() -> ExitCode {
    // `opcua_crypto` logs an ERROR on every start because there is no
    // certificate — correct and irrelevant at security `None`, and alarming
    // to anyone reading the output for the first time.
    env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("info,opcua_server=warn,opcua_crypto=off,tracing::span=warn"),
    )
    .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
