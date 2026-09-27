//! The gateway's OPC UA client, as a self-contained package.
//!
//! ```text
//!   config plane ── Client::apply / disable ──▶ Driver ──▶ Connection ──▶ PLC
//!                                               (state      (async-opcua
//!                                                machine)    session)
//!   telemetry    ◀── Client::drain ◀── SampleQueue ◀── notifications
//!   health       ◀── Client::reported ◀── Reported
//! ```
//!
//! Nothing here depends on the device. The firmware supplies what only it can —
//! a wall clock, a per-device backoff seed, the thread to run on — and in
//! exchange gets a [`Client`] handle it can call from any thread. The same code
//! runs unchanged on the development host, which is how it is tested: the
//! integration tests in `tests/` drive it against a real OPC UA server from the
//! same library (`opcua-test-server`) over loopback, and
//! `examples/local_gateway.rs` runs the whole client against any endpoint from
//! a laptop.
//!
//! Plugging it in:
//!
//! ```no_run
//! # fn config() -> gateway_opcua::AppliedConfig { unimplemented!() }
//! let (client, driver) = gateway_opcua::new(gateway_opcua::Options::new("1.2.3"));
//! gateway_opcua::spawn_thread(driver, "opcua", 40 * 1024)?;
//!
//! client.apply(config())?;          // from the config plane
//! let samples = client.drain(128);  // from the telemetry loop
//! let health = client.reported();   // for the shadow
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! All the decision logic (what to subscribe to, how to chunk it, how long to
//! back off, how to encode a value) lives in `gateway-core` and is unit-tested
//! on its own. This crate is the effectful shell around it.

mod driver;
pub mod session;
pub mod variant;

use std::fmt;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use gateway_core::batcher::Sample;
use gateway_core::bundle::{self, BundleError, TagSpec};
use gateway_core::health::Reported;
use gateway_core::queue::SampleQueue;
use gateway_core::settings::{DesiredSettings, SettingsError};

/// Wall clock in ms since the Unix epoch.
///
/// Only a fallback: a sample is stamped with the server's source or server
/// timestamp whenever there is one, because an edge device has no reliable
/// clock of its own.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Samples buffered between the driver and the telemetry publisher by default.
///
/// Two full sweeps of the 250-tag maximum. Larger buffers do not help: the
/// queue coalesces per address, so extra depth only adds staleness and heap.
pub const DEFAULT_QUEUE_CAPACITY: usize = 500;

/// A validated configuration ready to be applied to the OPC UA server.
#[derive(Debug, Clone)]
pub struct AppliedConfig {
    /// Validated shadow settings.
    pub settings: DesiredSettings,
    /// Verified and expanded tag list.
    pub tags: Vec<TagSpec>,
}

/// Why a configuration was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigError {
    /// The settings document failed validation.
    Settings(SettingsError),
    /// The bundle did not match the settings or was malformed.
    Bundle(BundleError),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Transparent on purpose: these strings land in `reported.last_error`
        // and operators (and the HIL harness) match on them.
        match self {
            ConfigError::Settings(e) => e.fmt(f),
            ConfigError::Bundle(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ConfigError {}

impl AppliedConfig {
    /// Validates `settings` and verifies `bundle` against them.
    ///
    /// The one gate every configuration passes through, whether it came from
    /// the cloud, from flash, or from a file on a laptop: a bundle is accepted
    /// only when its version *and* SHA-256 match what the settings asked for.
    pub fn new(settings: DesiredSettings, bundle: &[u8]) -> Result<Self, ConfigError> {
        settings.validate().map_err(ConfigError::Settings)?;
        let tags = bundle::parse_and_verify(
            bundle,
            &settings.cfg,
            settings.instance.ns,
            settings.instance.id_type,
        )
        .map_err(ConfigError::Bundle)?;
        Ok(Self { settings, tags })
    }
}

/// How to run the client.
#[derive(Clone)]
pub struct Options {
    /// Capacity of the sample queue.
    pub queue_capacity: usize,
    /// Reported as `fw` in the health snapshot.
    pub firmware_version: String,
    /// Jitter seed for reconnect backoff. Must differ per device, or a whole
    /// fleet reconnects in lockstep after a server restart.
    pub backoff_seed: u32,
    /// Fallback timestamp source.
    pub clock: Clock,
}

impl Options {
    /// Defaults: the system clock, and a backoff seed from it.
    ///
    /// The firmware overrides `backoff_seed` with one derived from the MAC,
    /// because a fleet that booted together has near-identical clocks.
    pub fn new(firmware_version: impl Into<String>) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        Self {
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            firmware_version: firmware_version.into(),
            backoff_seed: nanos,
            clock: Arc::new(system_clock_ms),
        }
    }
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("queue_capacity", &self.queue_capacity)
            .field("firmware_version", &self.firmware_version)
            .field("backoff_seed", &self.backoff_seed)
            .finish_non_exhaustive()
    }
}

/// Milliseconds since the Unix epoch from the system clock.
pub fn system_clock_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Creates a client handle and the driver it controls.
///
/// Nothing happens until the [`Driver`] is run, on [`spawn_thread`] or on any
/// tokio runtime with the time and I/O drivers enabled.
pub fn new(options: Options) -> (Client, Driver) {
    let shared = Arc::new(Shared::new(
        options.queue_capacity,
        &options.firmware_version,
    ));
    let (commands, command_rx) = unbounded_channel();
    let client = Client {
        shared: Arc::clone(&shared),
        commands,
    };
    let driver = Driver {
        shared,
        commands: command_rx,
        clock: options.clock,
        backoff_seed: options.backoff_seed,
    };
    (client, driver)
}

/// The driver has stopped, so the command could not be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverGone;

impl fmt::Display for DriverGone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the OPC UA driver has stopped")
    }
}

impl std::error::Error for DriverGone {}

/// Handle to a running client. Cheap to clone; usable from any thread.
#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
    commands: UnboundedSender<Command>,
}

impl Client {
    /// Applies a configuration, reconciling against whatever is running.
    ///
    /// Returns at once; the outcome shows up in [`Client::reported`].
    pub fn apply(&self, config: AppliedConfig) -> Result<(), DriverGone> {
        self.commands
            .send(Command::Apply(Box::new(config)))
            .map_err(|_| DriverGone)
    }

    /// Tears the session down and idles (`desired.enabled == false`).
    pub fn disable(&self) -> Result<(), DriverGone> {
        self.commands.send(Command::Disable).map_err(|_| DriverGone)
    }

    /// Moves up to `max` queued samples out, oldest first.
    pub fn drain(&self, max: usize) -> Vec<Sample> {
        self.shared.with_queue(|q| q.drain(max))
    }

    /// Snapshot of the health report.
    pub fn reported(&self) -> Reported {
        self.shared.with_reported(|r| r.clone())
    }

    /// Updates the health report in place, e.g. with device-only fields such
    /// as free heap and uptime.
    pub fn update_reported<R>(&self, f: impl FnOnce(&mut Reported) -> R) -> R {
        self.shared.with_reported(f)
    }

    /// Overflow counters of the sample queue: `(dropped, coalesced)`.
    pub fn queue_counters(&self) -> (u64, u64) {
        self.shared.with_queue(|q| (q.dropped(), q.coalesced()))
    }

    /// False once the driver has stopped.
    pub fn is_running(&self) -> bool {
        !self.commands.is_closed()
    }
}

/// The OPC UA side: a state machine that keeps the server subscription in
/// sync with the last configuration applied through the [`Client`].
pub struct Driver {
    shared: Arc<Shared>,
    commands: UnboundedReceiver<Command>,
    clock: Clock,
    backoff_seed: u32,
}

impl Driver {
    /// Runs until every [`Client`] handle has been dropped.
    ///
    /// Needs a tokio runtime with the time and I/O drivers enabled. Never
    /// panics and never returns early on an OPC UA failure: every fallible step
    /// becomes `reported.last_error` plus a backoff.
    pub async fn run(self) {
        driver::run(self.shared, self.commands, self.clock, self.backoff_seed).await;
    }
}

/// Runs the driver on a dedicated OS thread with its own current-thread
/// runtime.
///
/// A dedicated thread because on the device the MQTT client runs its callback
/// on a small-stack task, and the OPC UA stack needs far more room than that.
///
/// Returns only once the runtime is up, so a runtime that cannot be built is an
/// error here rather than a thread that dies silently. On ESP-IDF that is not
/// hypothetical: without the eventfd VFS registered first, `Runtime::build()`
/// fails with EACCES, and nothing but the absence of data would ever say so
/// (`docs/OPCUA_INTEGRATION_TEST.md` §8.2).
pub fn spawn_thread(
    driver: Driver,
    name: &str,
    stack_bytes: usize,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let (ready_tx, ready_rx) = mpsc::sync_channel::<std::io::Result<()>>(1);
    let handle = std::thread::Builder::new()
        .name(name.into())
        .stack_size(stack_bytes)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            runtime.block_on(driver.run());
            log::warn!("OPC UA driver exited");
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(std::io::Error::other("OPC UA thread exited during startup")),
    }
}

/// Claims the OPC UA type table up front.
///
/// It is ~9 kB in one block, built lazily on the first `ExtensionObject`
/// decode. Deferred, that decode lands when TLS and the session hold most of
/// the heap, and on a small device the allocation aborts the process instead of
/// failing softly. Call this early at boot; on a host it is merely harmless.
pub fn preload_types() {
    opcua_types::generated::types::preload_types();
}

/// Instruction sent from a [`Client`] to the [`Driver`].
#[derive(Debug)]
enum Command {
    /// Apply a new configuration, reconciling against whatever is running.
    Apply(Box<AppliedConfig>),
    /// Tear down the session and idle.
    Disable,
}

/// State shared between the driver and the client handles.
///
/// Two mutexes rather than one so a slow health update never blocks the
/// notification callback, which runs on the OPC UA event loop.
#[derive(Debug)]
struct Shared {
    /// Bounded, coalescing sample queue drained by the publisher.
    queue: Mutex<SampleQueue>,
    /// Health snapshot mirrored into the shadow's `reported`.
    reported: Mutex<Reported>,
}

impl Shared {
    fn new(capacity: usize, fw: &str) -> Self {
        Self {
            queue: Mutex::new(SampleQueue::new(capacity)),
            reported: Mutex::new(Reported::new(fw)),
        }
    }

    /// Applies `f` to the health snapshot, ignoring lock poisoning.
    ///
    /// A panic in another thread must not take telemetry down with it; the
    /// worst case is a slightly stale report.
    fn with_reported<R>(&self, f: impl FnOnce(&mut Reported) -> R) -> R {
        let mut guard = match self.reported.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }

    /// Applies `f` to the sample queue, ignoring lock poisoning.
    fn with_queue<R>(&self, f: impl FnOnce(&mut SampleQueue) -> R) -> R {
        let mut guard = match self.queue.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }
}
