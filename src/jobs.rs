//! AWS IoT Jobs: firmware updates.
//!
//! The state machine is `gateway_core::jobs::client::JobsClient`, host-tested
//! there; this is what it runs on here: MQTT ([`Link`]), NVS
//! ([`JobStore`](crate::job_store)) and the OTA partitions
//! ([`Ota`]).

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use gateway_core::jobs::client::{self, Installer, Transport};

use crate::job_store::JobStore;
use crate::mqtt_util::{MqttEvent, MqttTransport, QOS1};

/// The Jobs client the telemetry loop drives.
pub type JobsClient = client::JobsClient<JobStore, Ota>;

/// How long the status that precedes a restart into a new image gets to reach
/// the wire. Losing it is not fatal: the next boot settles the job from the
/// NVS record.
pub const RESTART_GRACE: Duration = Duration::from_millis(1500);

/// The MQTT connection as the Jobs client sees it.
///
/// Waiting for an answer takes messages off the event channel; those that are
/// not the answer go to `backlog`, which the telemetry loop drains before it
/// reads the channel again, so nothing that arrives meanwhile is lost.
pub struct Link<'a, C> {
    pub client: &'a mut C,
    pub events: &'a Receiver<MqttEvent>,
    pub backlog: &'a mut VecDeque<MqttEvent>,
}

impl<C: MqttTransport> Transport for Link<'_, C> {
    fn publish(&mut self, topic: &str, payload: &[u8]) -> Result<(), String> {
        self.client
            .publish(topic, QOS1, false, payload)
            .map_err(|e| format!("{e:#}"))
    }

    fn subscribe(&mut self, topic: &str) -> Result<(), String> {
        self.client
            .subscribe(topic, QOS1)
            .map_err(|e| format!("{e:#}"))
    }

    fn unsubscribe(&mut self, topic: &str) -> Result<(), String> {
        self.client.unsubscribe(topic).map_err(|e| format!("{e:#}"))
    }

    fn wait_for(&mut self, topics: &[&str], timeout: Duration) -> Option<(String, Vec<u8>)> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(MqttEvent::Message { topic, data }) if topics.contains(&topic.as_str()) => {
                    return Some((topic, data));
                }
                Ok(other) => self.backlog.push_back(other),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

/// The OTA partitions.
pub struct Ota;

impl Installer for Ota {
    fn install(
        &mut self,
        url: &str,
        before_activate: &mut dyn FnMut(&str) -> Result<(), String>,
    ) -> Result<String, String> {
        crate::ota::perform_ota(url, |slot| {
            before_activate(slot).map_err(anyhow::Error::msg)
        })
        .map_err(|e| format!("{e:#}"))
    }
}
