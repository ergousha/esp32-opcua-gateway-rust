//! AWS IoT Jobs: firmware updates.
//!
//! Split out of the telemetry loop, where it had grown to ten levels of
//! nesting inside a `while let` inside a `loop`. Parsing and every decision
//! about an outcome live in `gateway_core::jobs`, where they are unit-tested;
//! this is the part that talks MQTT, NVS and the OTA partitions.
//!
//! An update is reported SUCCEEDED by the image it installed, never by the one
//! that downloaded it: until the new image has reached AWS IoT and marked
//! itself valid, the bootloader can still roll it back. So a boot that
//! installs an update ends with the execution IN_PROGRESS and the job recorded
//! in NVS ([`JobStore`]), and the next boot *settles* it: SUCCEEDED from the
//! new image, FAILED "rolled back" from the previous one.
//!
//! Until the Jobs service has acknowledged that outcome, no other job is
//! taken. In particular the same execution, which the service offers again at
//! boot because it is still IN_PROGRESS, is never downloaded a second time.
//! A job for the version already running is settled the same way, without a
//! download.

use std::time::{Duration, Instant};

use anyhow::Result;
use gateway_core::jobs::{self, BootFacts, FirmwareJob, NextJob, Outcome, PendingUpdate};

use crate::job_store::JobStore;
use crate::mqtt_util::{MqttTransport, QOS1};

/// Version of this image, as the shadow reports it (`fw`).
const RUNNING_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long an outcome may go unacknowledged before it is sent again.
///
/// The Jobs service answers in well under a second. Silence means the request
/// or the answer was lost, or the request was throttled; a reconnect also
/// sends it again.
const SETTLE_RETRY: Duration = Duration::from_secs(30);

/// Jobs topics and state for a single thing.
pub struct JobsClient {
    notify_next: String,
    next_accepted: String,
    next_get: String,
    thing_name: String,
    store: JobStore,
    boot: BootFacts,
    /// The outcome this boot is reporting, until the Jobs service takes it.
    settling: Option<Settling>,
}

/// A job outcome on its way to the Jobs service.
struct Settling {
    job_id: String,
    update: String,
    accepted: String,
    rejected: String,
    payload: Vec<u8>,
    sent_at: Option<Instant>,
}

impl Settling {
    /// Subscribes to the answer, then publishes the outcome.
    ///
    /// The subscriptions are renewed on every send, which costs nothing when
    /// they already exist: a SUBSCRIBE lost on the way would otherwise leave
    /// the answer unheard, and the Jobs plane waiting for it until the next
    /// reconnect.
    fn send(&mut self, client: &mut impl MqttTransport) {
        self.sent_at = Some(Instant::now());
        for topic in [&self.accepted, &self.rejected] {
            if let Err(e) = client.subscribe(topic, QOS1) {
                log::warn!("could not subscribe to {topic}: {e:#}");
            }
        }
        if let Err(e) = client.publish(&self.update, QOS1, false, &self.payload) {
            log::warn!("could not report OTA job {}: {e:#}", self.job_id);
        }
    }
}

impl JobsClient {
    /// Builds the topic set for `thing_name`.
    ///
    /// `boot` is what this boot knows about its own image, taken after
    /// `ota::mark_valid`. A job left pending by the previous boot is settled
    /// against it once [`Self::start`] runs.
    pub fn new(thing_name: &str, store: JobStore, boot: BootFacts) -> Self {
        let mut client = Self {
            notify_next: format!("$aws/things/{thing_name}/jobs/notify-next"),
            next_accepted: format!("$aws/things/{thing_name}/jobs/$next/get/accepted"),
            next_get: format!("$aws/things/{thing_name}/jobs/$next/get"),
            thing_name: thing_name.to_string(),
            store,
            boot,
            settling: None,
        };
        if let Some(pending) = client.store.load() {
            client.settle(pending);
        }
        client
    }

    /// Subscribes, then reports the outcome still owed, or else asks for any
    /// job that is already queued. Safe to call again after a reconnect.
    pub fn start(&mut self, client: &mut impl MqttTransport) -> Result<()> {
        client.subscribe(&self.notify_next, QOS1)?;
        client.subscribe(&self.next_accepted, QOS1)?;
        if let Some(settling) = self.settling.as_mut() {
            settling.send(client);
            return Ok(());
        }
        client.publish(&self.next_get, QOS1, false, b"{}")?;
        Ok(())
    }

    /// Sends an outcome again if the Jobs service has not acknowledged it.
    pub fn tick(&mut self, client: &mut impl MqttTransport) {
        let Some(settling) = self.settling.as_mut() else {
            return;
        };
        if settling
            .sent_at
            .is_some_and(|at| at.elapsed() < SETTLE_RETRY)
        {
            return;
        }
        log::warn!(
            "OTA job {}: outcome not acknowledged; sending it again",
            settling.job_id
        );
        settling.send(client);
    }

    /// True when `topic` belongs to this client.
    pub fn owns(&self, topic: &str) -> bool {
        topic == self.notify_next
            || topic == self.next_accepted
            || self
                .settling
                .as_ref()
                .is_some_and(|s| topic == s.accepted || topic == s.rejected)
    }

    /// Handles one Jobs message.
    ///
    /// On a successful download this does not return: the device reboots into
    /// the new slot.
    pub fn handle(&mut self, topic: &str, payload: &[u8], client: &mut impl MqttTransport) {
        if let Some(settling) = &self.settling {
            if topic == settling.accepted {
                log::info!("OTA job {}: outcome recorded", settling.job_id);
                self.finish_settling(client);
            } else if topic == settling.rejected {
                let rejection = jobs::parse_rejection(payload);
                if rejection.is_transient() {
                    log::warn!(
                        "OTA job {}: outcome not taken ({rejection}); will retry",
                        settling.job_id
                    );
                    return;
                }
                // The execution has already ended: cancelled, timed out, or
                // settled by an earlier attempt whose answer was lost. There
                // is nothing left to report, and retrying would never end.
                log::warn!(
                    "OTA job {}: outcome rejected ({rejection}); dropping it",
                    settling.job_id
                );
                self.finish_settling(client);
            } else {
                // Everything else waits until the outcome is on record: the
                // service keeps offering this same execution while it is
                // IN_PROGRESS, and a queued job would start a second update on
                // top of one nobody has confirmed.
                log::debug!("ignoring {topic} while OTA job {} settles", settling.job_id);
            }
            return;
        }

        if topic == self.notify_next {
            // The notification carries a copy of the job document, but only
            // the `$next/get` answer is used: that is where AWS IoT fills in
            // the presigned download URL.
            log::info!("job notification; requesting details");
            let _ = client.publish(&self.next_get, QOS1, false, b"{}");
            return;
        }
        if topic != self.next_accepted {
            return;
        }

        match jobs::parse_next(payload) {
            Ok(NextJob::Idle) => {}
            Ok(NextJob::Unsupported { job_id, operation }) => {
                log::info!("ignoring job {job_id}: unsupported operation {operation:?}");
            }
            Ok(NextJob::Refused { job_id, reason }) => {
                log::error!("refusing OTA job {job_id}: {reason}");
            }
            Ok(NextJob::Install(job)) if jobs::is_already_running(&job, &self.boot) => {
                log::info!(
                    "OTA job {}: {} is already running; reporting SUCCEEDED without a download",
                    job.job_id,
                    self.boot.running_version
                );
                self.report(&job.job_id, jobs::already_running(&self.boot).encode());
                self.send_report(client);
            }
            Ok(NextJob::Install(job)) => self.run_update(&job, client),
            Ok(NextJob::Settle(pending)) => {
                // Installed and rebooted for, but not in NVS: the record was
                // lost. The status details the reboot left behind stand in.
                log::warn!(
                    "OTA job {} is IN_PROGRESS after its reboot with no record on the device; settling it",
                    pending.job_id
                );
                self.settle(pending);
                self.send_report(client);
            }
            Err(e) => log::warn!("unusable job description: {e}"),
        }
    }

    /// Decides the outcome of `pending` and queues it for [`Self::start`].
    fn settle(&mut self, pending: PendingUpdate) {
        let outcome = jobs::settle(&pending, &self.boot);
        match &outcome {
            Outcome::Succeeded => log::info!(
                "OTA job {}: {} is running from {}; reporting SUCCEEDED",
                pending.job_id,
                self.boot.running_version,
                self.boot.running_slot
            ),
            Outcome::Failed { reason, detail } => log::warn!(
                "OTA job {}: {reason} ({detail}); reporting FAILED",
                pending.job_id
            ),
        }
        let payload = jobs::settled(&pending, &self.boot, &outcome).encode();
        self.report(&pending.job_id, payload);
    }

    /// Queues a terminal status for `job_id`; nothing else is taken on until
    /// the Jobs service has answered it.
    fn report(&mut self, job_id: &str, payload: Vec<u8>) {
        let update = self.update_topic(job_id);
        self.settling = Some(Settling {
            job_id: job_id.to_string(),
            accepted: format!("{update}/accepted"),
            rejected: format!("{update}/rejected"),
            update,
            payload,
            sent_at: None,
        });
    }

    /// Sends the queued status now rather than on the next [`Self::start`].
    fn send_report(&mut self, client: &mut impl MqttTransport) {
        if let Some(settling) = self.settling.as_mut() {
            settling.send(client);
        }
    }

    /// Forgets the settled job and goes back to taking work.
    fn finish_settling(&mut self, client: &mut impl MqttTransport) {
        let Some(settling) = self.settling.take() else {
            return;
        };
        if let Err(e) = self.store.clear() {
            // Harmless: the next boot reports again and is told it is done.
            log::warn!("{e:#}");
        }
        for topic in [&settling.accepted, &settling.rejected] {
            if let Err(e) = client.unsubscribe(topic) {
                log::warn!("could not unsubscribe from {topic}: {e:#}");
            }
        }
        // Whatever was queued behind this job can run now.
        let _ = client.publish(&self.next_get, QOS1, false, b"{}");
    }

    fn update_topic(&self, job_id: &str) -> String {
        format!("$aws/things/{}/jobs/{job_id}/update", self.thing_name)
    }

    fn run_update(&mut self, job: &FirmwareJob, client: &mut impl MqttTransport) {
        let update_topic = self.update_topic(&job.job_id);
        log::info!(
            "starting OTA job {} ({RUNNING_VERSION} -> {:?})",
            job.job_id,
            job.target_version
        );
        let _ = client.publish(
            &update_topic,
            QOS1,
            false,
            &jobs::downloading(job, RUNNING_VERSION).encode(),
        );

        let store = &self.store;
        let installed = crate::ota::perform_ota(&job.url, |slot| {
            // Before activation, so the new image never boots without it.
            store.save(&job.pending(RUNNING_VERSION, slot))
        });

        match installed {
            Ok(slot) => {
                let pending = job.pending(RUNNING_VERSION, &slot);
                let _ = client.publish(
                    &update_topic,
                    QOS1,
                    false,
                    &jobs::rebooting(&pending).encode(),
                );
                // Give the broker a moment to put the status on the wire.
                // Losing it is no longer fatal: the next boot settles the job
                // from the NVS record, not from this message.
                std::thread::sleep(Duration::from_millis(1500));
                log::info!("OTA image in {slot}; restarting into it");
                unsafe { esp_idf_svc::sys::esp_restart() };
            }
            Err(e) => {
                log::error!("OTA failed: {e:#}");
                // The running image is still the boot image; a record written
                // just before a failed activation must not outlive it.
                if let Err(e) = self.store.clear() {
                    log::warn!("{e:#}");
                }
                let _ = client.publish(
                    &update_topic,
                    QOS1,
                    false,
                    &jobs::install_failed(job, &format!("{e:#}")).encode(),
                );
            }
        }
    }
}
