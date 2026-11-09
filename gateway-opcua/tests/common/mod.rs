//! Shared rig for the loopback integration tests.
//!
//! Each test gets its own [`TestServer`] on an ephemeral loopback port and its
//! own driver, run exactly the way the firmware runs it: on a dedicated thread
//! with a current-thread runtime ([`gateway_opcua::spawn_thread`]). Tests are
//! therefore independent and run in parallel.

#![allow(dead_code)] // each test binary uses a different subset

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gateway_core::batcher::{Batcher, Sample};
use gateway_core::health::Reported;
use gateway_core::settings::{DesiredSettings, TelemetrySettings};
use gateway_opcua::{AppliedConfig, Client, ConfigError, Options};
use opcua_test_server::catalogue::Tag;
use opcua_test_server::documents::{self, Desired};
use opcua_test_server::TestServer;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Thing name used in topics; nothing here talks to AWS.
pub const THING: &str = "local-test";

/// Driver thread stack. Generous: this is the host, not the device.
const STACK_BYTES: usize = 4 * 1024 * 1024;

/// Overrides [`STACK_BYTES`], so the stack the client needs can be bisected
/// over loopback: a run that overflows aborts the whole test binary. The OS
/// rounds the size up to whole pages, so the resolution is 4 KiB on x86_64
/// Linux but 16 KiB on Apple silicon.
///
/// ```sh
/// OPCUA_TEST_STACK_BYTES=65536 cargo test --release -p gateway-opcua --target host-tuple --test scenario
/// ```
const STACK_BYTES_ENV: &str = "OPCUA_TEST_STACK_BYTES";

/// [`STACK_BYTES`], or the override from [`STACK_BYTES_ENV`].
fn stack_bytes() -> usize {
    match std::env::var(STACK_BYTES_ENV) {
        Ok(bytes) => bytes
            .parse()
            .unwrap_or_else(|_| panic!("{STACK_BYTES_ENV}={bytes:?} is not a byte count")),
        Err(_) => STACK_BYTES,
    }
}

/// A server plus a gateway client pointed at it.
pub struct Rig {
    pub server: TestServer,
    pub client: Client,
}

impl Rig {
    /// A fresh server on loopback and a fresh, idle driver.
    pub async fn start() -> Self {
        init_logging();
        let server = TestServer::start(opcua_test_server::Options::loopback())
            .await
            .expect("test server starts");
        Self {
            server,
            client: client(),
        }
    }

    /// A configuration for this rig's server.
    pub fn config(
        &self,
        version: u32,
        tags: &[Tag],
        tweak: impl FnOnce(&mut Desired),
    ) -> AppliedConfig {
        config_for(
            &self.server.endpoint(),
            self.server.namespace_index(),
            version,
            tags,
            tweak,
        )
    }

    /// Applies `config` and waits until it is running.
    pub async fn apply_and_wait(&self, config: AppliedConfig) -> Reported {
        let v = config.settings.cfg.v;
        self.client.apply(config).expect("driver running");
        wait_for(
            &self.client,
            &format!("running on cfg v{v}"),
            secs(20),
            |r| r.state_is("running") && r.cfg_v == v,
        )
        .await
    }
}

/// A driver on its own thread, as on the device.
pub fn client() -> Client {
    init_logging();
    let (client, driver) = gateway_opcua::new(Options::new("test"));
    gateway_opcua::spawn_thread(driver, "opcua", stack_bytes()).expect("driver thread starts");
    client
}

/// Builds the documents the cloud would send and runs them through the same
/// gate the firmware uses.
pub fn config_for(
    endpoint: &str,
    ns: u16,
    version: u32,
    tags: &[Tag],
    tweak: impl FnOnce(&mut Desired),
) -> AppliedConfig {
    try_config_for(endpoint, ns, version, tags, tweak).expect("configuration is valid")
}

/// As [`config_for`], returning the refusal instead of panicking.
pub fn try_config_for(
    endpoint: &str,
    ns: u16,
    version: u32,
    tags: &[Tag],
    tweak: impl FnOnce(&mut Desired),
) -> Result<AppliedConfig, ConfigError> {
    let bundle = documents::bundle(THING, version, tags);
    let mut desired = Desired::new(endpoint, ns);
    tweak(&mut desired);
    let settings: DesiredSettings = serde_json::from_value(desired.to_json(THING, &bundle))
        .expect("desired document matches the schema");
    AppliedConfig::new(settings, &bundle.payload)
}

pub fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

/// Polls the health report until `predicate` holds, like the cloud polls the
/// shadow. Panics with the last report on timeout.
pub async fn wait_for(
    client: &Client,
    what: &str,
    timeout: Duration,
    predicate: impl Fn(&Reported) -> bool,
) -> Reported {
    let deadline = Instant::now() + timeout;
    loop {
        let reported = client.reported();
        if predicate(&reported) {
            return reported;
        }
        if Instant::now() >= deadline {
            panic!("timed out after {timeout:?} waiting for {what}; last reported: {reported:#?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Lets `Reported` be matched on the state name the shadow carries.
pub trait StateName {
    fn state_is(&self, name: &str) -> bool;
}

impl StateName for Reported {
    fn state_is(&self, name: &str) -> bool {
        self.state.as_str() == name
    }
}

/// Everything the driver queues during `window`.
pub async fn collect(client: &Client, window: Duration) -> Vec<Sample> {
    let deadline = Instant::now() + window;
    let mut samples = Vec::new();
    while Instant::now() < deadline {
        samples.extend(client.drain(usize::MAX));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    samples.extend(client.drain(usize::MAX));
    samples
}

/// Telemetry settings matching [`Desired::new`]'s defaults.
pub fn telemetry_settings() -> TelemetrySettings {
    TelemetrySettings {
        topic: documents::telemetry_topic(THING),
        qos: 1,
        batch_max_items: 100,
        batch_max_bytes: 16_384,
        batch_max_age_ms: 2_000,
    }
}

/// Runs samples through the real batcher and returns the batches exactly as
/// they would be published: the JSON of §4.3.
pub fn encode(samples: Vec<Sample>) -> Vec<Value> {
    let mut batcher = Batcher::new(&telemetry_settings());
    let mut batches = Vec::new();
    for sample in samples {
        if let Some((batch, _)) = batcher.push(sample, 0) {
            batches.push(batch);
        }
    }
    batches.extend(batcher.flush(0).map(|(batch, _)| batch));
    batches
        .iter()
        .map(|b| {
            let bytes = serde_json::to_vec(b).expect("batches serialise");
            assert!(
                bytes.len() <= telemetry_settings().batch_max_bytes,
                "batch of {} B exceeds the budget",
                bytes.len()
            );
            serde_json::from_slice(&bytes).expect("batches are valid JSON")
        })
        .collect()
}

/// Flattens published batches into `{address: [row, …]}`.
pub fn rows_by_address(batches: &[Value]) -> HashMap<String, Vec<Vec<Value>>> {
    let mut out: HashMap<String, Vec<Vec<Value>>> = HashMap::new();
    for batch in batches {
        for row in batch["d"].as_array().expect("batch has rows") {
            let row = row.as_array().expect("row is an array").clone();
            let address = row[0]
                .as_str()
                .expect("row starts with the address")
                .to_string();
            out.entry(address).or_default().push(row);
        }
    }
    out
}

/// A TCP listener that accepts and immediately hangs up, counting attempts.
///
/// Stands in for a server that is down but whose host is up, which is what a
/// retry schedule is judged against: a tight loop would show hundreds.
pub struct CountingListener {
    pub port: u16,
    pub attempts: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl CountingListener {
    /// Binds `port` (0 for any), retrying briefly in case a server that has
    /// just stopped still holds it.
    pub async fn bind(port: u16) -> Self {
        let deadline = Instant::now() + secs(5);
        let listener = loop {
            match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await {
                Ok(l) => break l,
                Err(e) if Instant::now() < deadline => {
                    log::debug!("port {port} not free yet: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("could not bind port {port}: {e}"),
            }
        };
        let port = listener.local_addr().unwrap().port();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                drop(socket);
            }
        });
        Self {
            port,
            attempts,
            task,
        }
    }

    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    /// Stops listening and frees the port.
    pub async fn close(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

fn init_logging() {
    // `RUST_LOG=info cargo test …` shows the driver's own log lines, which is
    // the same view the device's serial console gives.
    let _ = env_logger::builder().is_test(true).try_init();
}
