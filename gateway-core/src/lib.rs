//! Pure, host-testable core of the ESP32 OPC UA gateway.
//!
//! Nothing in this crate may depend on `esp-idf-*`, the OPC UA stack, or any
//! I/O. Every module here is deterministic data transformation so that the
//! whole configuration/telemetry pipeline can be unit-tested with
//! `cargo test -p gateway-core` on the development host.
//!
//! Device-specific glue (OPC UA session, MQTT, NVS, OTA) lives in the firmware
//! binary crate and is kept behind narrow traits.

pub mod backoff;
pub mod batcher;
pub mod bundle;
pub mod codec;
pub mod diff;
pub mod health;
pub mod node;
pub mod plan;
pub mod queue;
pub mod settings;
pub mod shadow;
pub mod value;

/// Hard cap on the number of tags the firmware will accept in one bundle.
///
/// Derived from the memory budget in the requirements document (§5): each
/// MonitoredItem costs roughly 300–400 B of heap once the subscription state,
/// the address string and the last value are accounted for.
pub const MAX_TAGS: usize = 250;

/// Hard cap on the *serialised* size of a tag bundle.
///
/// Two independent budgets meet here:
///
/// * The dedicated `opcua` NVS partition is 52 KiB, of which roughly 40 KiB is
///   usable after page overhead — the bundle must fit with room for the
///   settings blob and NVS' copy-on-write headroom.
/// * The bundle arrives as a single retained MQTT message, and the ESP-IDF
///   MQTT client only reports the topic on the first chunk. Keeping the cap
///   below the client's input buffer guarantees single-chunk delivery.
///
/// The MQTT input buffer must therefore stay above this value. A 250-tag
/// bundle with realistic addresses is around 6 KB, so this leaves ~40 %
/// headroom.
pub const MAX_BUNDLE_BYTES: usize = 10 * 1024;
