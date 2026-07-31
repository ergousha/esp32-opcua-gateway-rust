//! What the device reports back into the `opcua` named shadow.
//!
//! `reported` is deliberately small and bounded. Echoing the applied tag list
//! back would be the obvious thing to do and is exactly what blows the 8 KB
//! AWS IoT shadow limit at 250 tags — so we report counts, a truncated sample
//! of failures, and enough health to diagnose the rest.

use serde::{Deserialize, Serialize};

/// How many failed tags are named in `reported`. Enough to diagnose a bad
/// address; small enough that a wholesale failure still fits in the shadow.
pub const FAILED_SAMPLE_LEN: usize = 5;

/// Coarse driver state, mirrored into the shadow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverState {
    /// Disabled by configuration, or no configuration yet.
    Idle,
    /// Establishing the OPC UA session.
    Connecting,
    /// Session up, creating subscriptions and monitored items.
    Syncing,
    /// Steady state: notifications flowing.
    Running,
    /// Backing off after a failure. `last_error` says why.
    Error,
}

impl DriverState {
    /// Lowercase name, matching the shadow encoding.
    pub fn as_str(self) -> &'static str {
        match self {
            DriverState::Idle => "idle",
            DriverState::Connecting => "connecting",
            DriverState::Syncing => "syncing",
            DriverState::Running => "running",
            DriverState::Error => "error",
        }
    }
}

/// One failed tag, named so the operator can fix the bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailedTag {
    /// Bare address from the bundle.
    pub a: String,
    /// OPC UA StatusCode returned by `CreateMonitoredItems`.
    pub s: u32,
}

/// The `state.reported` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reported {
    /// Firmware version.
    pub fw: String,
    /// Applied configuration version, or 0 if none has been applied.
    pub cfg_v: u32,
    /// Current driver state.
    pub state: DriverState,
    /// MonitoredItems the server accepted.
    pub applied: usize,
    /// MonitoredItems the server rejected.
    pub failed: usize,
    /// Up to [`FAILED_SAMPLE_LEN`] of the rejected tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_sample: Vec<FailedTag>,
    /// Publishing interval the server actually revised to. A server that
    /// silently slows the subscription down is otherwise invisible.
    pub srv_publish_ms: u32,
    /// Samples the queue had to discard because the publisher fell behind.
    pub dropped: u64,
    /// Samples superseded by a newer value for the same tag under overflow.
    pub coalesced: u64,
    /// Last error, truncated. `None` clears the field in the shadow.
    pub last_error: Option<String>,
    /// Seconds since boot.
    pub uptime_s: u64,
    /// Free heap in bytes; the number that decides whether 250 tags is safe.
    pub free_heap: u32,
}

/// Longest error string put into the shadow.
pub const MAX_ERROR_LEN: usize = 160;

impl Reported {
    /// Creates a report for a device that has not applied anything yet.
    pub fn new(fw: &str) -> Self {
        Self {
            fw: fw.to_string(),
            cfg_v: 0,
            state: DriverState::Idle,
            applied: 0,
            failed: 0,
            failed_sample: Vec::new(),
            srv_publish_ms: 0,
            dropped: 0,
            coalesced: 0,
            last_error: None,
            uptime_s: 0,
            free_heap: 0,
        }
    }

    /// Records an error, truncated on a character boundary so the shadow stays
    /// small and the JSON stays valid.
    pub fn set_error(&mut self, message: impl AsRef<str>) {
        let message = message.as_ref();
        let end = message
            .char_indices()
            .map(|(i, c)| i + c.len_utf8())
            .take_while(|i| *i <= MAX_ERROR_LEN)
            .last()
            .unwrap_or(0);
        self.last_error = Some(message[..end].to_string());
    }

    /// Records the outcome of a sync, truncating the failure sample.
    pub fn set_sync_outcome(&mut self, applied: usize, failures: Vec<FailedTag>) {
        self.applied = applied;
        self.failed = failures.len();
        self.failed_sample = failures.into_iter().take(FAILED_SAMPLE_LEN).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_names_match_the_shadow_encoding() {
        assert_eq!(
            serde_json::to_string(&DriverState::Running).unwrap(),
            "\"running\""
        );
        for s in [
            DriverState::Idle,
            DriverState::Connecting,
            DriverState::Syncing,
            DriverState::Running,
            DriverState::Error,
        ] {
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{}\"", s.as_str())
            );
        }
    }

    #[test]
    fn errors_are_truncated_without_splitting_a_character() {
        let mut r = Reported::new("0.0.1");
        r.set_error("é".repeat(200));
        let err = r.last_error.unwrap();
        assert!(err.len() <= MAX_ERROR_LEN);
        assert!(err.chars().all(|c| c == 'é'));
    }

    #[test]
    fn short_errors_are_kept_intact() {
        let mut r = Reported::new("0.0.1");
        r.set_error("BadNodeIdUnknown");
        assert_eq!(r.last_error.as_deref(), Some("BadNodeIdUnknown"));
    }

    #[test]
    fn failure_sample_is_bounded() {
        let mut r = Reported::new("0.0.1");
        let failures: Vec<_> = (0..100)
            .map(|i| FailedTag { a: format!("tag{i}"), s: 0x8034_0000 })
            .collect();
        r.set_sync_outcome(150, failures);
        assert_eq!(r.applied, 150);
        assert_eq!(r.failed, 100);
        assert_eq!(r.failed_sample.len(), FAILED_SAMPLE_LEN);
    }

    /// The whole point of not echoing the tag list: worst-case `reported` has
    /// to leave room for `desired` inside the 8 KB shadow document.
    #[test]
    fn worst_case_report_stays_far_below_the_shadow_limit() {
        let mut r = Reported::new("0.0.1");
        r.state = DriverState::Error;
        r.set_error("x".repeat(500));
        r.set_sync_outcome(
            248,
            (0..250)
                .map(|i| FailedTag { a: format!("Chan1.Dev1.Tag{i:04}"), s: 0x8034_0000 })
                .collect(),
        );
        let encoded = serde_json::to_vec(&r).unwrap();
        assert!(encoded.len() < 1024, "reported is {} B", encoded.len());
    }

    #[test]
    fn report_round_trips() {
        let mut r = Reported::new("0.0.1");
        r.cfg_v = 7;
        r.state = DriverState::Running;
        r.set_sync_outcome(248, vec![FailedTag { a: "bad".into(), s: 1 }]);
        let back: Reported = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);
    }
}
