//! OPC UA client: session management, subscription synchronisation and the
//! reconnect state machine.
//!
//! All the decision logic (what to subscribe to, how to chunk it, how long to
//! back off, how to encode a value) lives in `gateway-core` and is unit-tested
//! on the host. This module is the effectful shell around it.

pub mod driver;
pub mod session;
pub mod variant;

use std::sync::Mutex;

use gateway_core::bundle::TagSpec;
use gateway_core::health::Reported;
use gateway_core::queue::SampleQueue;
use gateway_core::settings::DesiredSettings;

/// A validated configuration ready to be applied to the OPC UA server.
#[derive(Debug, Clone)]
pub struct AppliedConfig {
    /// Validated shadow settings.
    pub settings: DesiredSettings,
    /// Verified and expanded tag list.
    pub tags: Vec<TagSpec>,
}

/// Instruction sent from the MQTT task to the OPC UA task.
#[derive(Debug)]
pub enum Command {
    /// Apply a new configuration, reconciling against whatever is running.
    Apply(Box<AppliedConfig>),
    /// Tear down the session and idle (`desired.enabled == false`).
    Disable,
}

/// State shared between the OPC UA task and the MQTT task.
///
/// Two mutexes rather than one so a slow shadow update never blocks the
/// notification callback, which runs on the OPC UA event loop.
#[derive(Debug)]
pub struct Shared {
    /// Bounded, coalescing sample queue drained by the publisher.
    pub queue: Mutex<SampleQueue>,
    /// Health snapshot mirrored into the shadow's `reported`.
    pub reported: Mutex<Reported>,
}

impl Shared {
    /// Creates shared state with a queue of `capacity` samples.
    pub fn new(capacity: usize, fw: &str) -> Self {
        Self {
            queue: Mutex::new(SampleQueue::new(capacity)),
            reported: Mutex::new(Reported::new(fw)),
        }
    }

    /// Applies `f` to the health snapshot, ignoring lock poisoning.
    ///
    /// A panic in another thread must not take telemetry down with it; the
    /// worst case is a slightly stale report.
    pub fn with_reported<R>(&self, f: impl FnOnce(&mut Reported) -> R) -> R {
        let mut guard = match self.reported.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }

    /// Applies `f` to the sample queue, ignoring lock poisoning.
    pub fn with_queue<R>(&self, f: impl FnOnce(&mut SampleQueue) -> R) -> R {
        let mut guard = match self.queue.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }
}
