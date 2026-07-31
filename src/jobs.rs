//! AWS IoT Jobs: firmware updates.
//!
//! Split out of the telemetry loop, where it had grown to ten levels of
//! nesting inside a `while let` inside a `loop`. Parsing is now a single
//! fallible function, so the interesting part — what happens when an update
//! fails — is visible instead of buried.

use anyhow::Result;

use crate::mqtt_util::{MqttTransport, QOS1};

/// The one job operation this firmware understands.
const OP_FIRMWARE_UPDATE: &str = "firmware_update";

/// Jobs topics for a single thing.
pub struct JobsClient {
    notify_next: String,
    next_accepted: String,
    next_get: String,
    thing_name: String,
}

impl JobsClient {
    /// Builds the topic set for `thing_name`.
    pub fn new(thing_name: &str) -> Self {
        Self {
            notify_next: format!("$aws/things/{thing_name}/jobs/notify-next"),
            next_accepted: format!("$aws/things/{thing_name}/jobs/$next/get/accepted"),
            next_get: format!("$aws/things/{thing_name}/jobs/$next/get"),
            thing_name: thing_name.to_string(),
        }
    }

    /// Subscribes and asks for any job that is already queued.
    pub fn start(&self, client: &mut impl MqttTransport) -> Result<()> {
        client.subscribe(&self.notify_next, QOS1)?;
        client.subscribe(&self.next_accepted, QOS1)?;
        client.publish(&self.next_get, QOS1, false, b"{}")?;
        Ok(())
    }

    /// True when `topic` belongs to this client.
    pub fn owns(&self, topic: &str) -> bool {
        topic == self.notify_next || topic == self.next_accepted
    }

    /// Handles one Jobs message.
    ///
    /// On a successful update this does not return: the device reboots into
    /// the new slot.
    pub fn handle(&self, topic: &str, payload: &[u8], client: &mut impl MqttTransport) {
        if topic == self.notify_next {
            log::info!("job notification; requesting details");
            let _ = client.publish(&self.next_get, QOS1, false, b"{}");
            return;
        }
        if topic != self.next_accepted {
            return;
        }

        let Some(job) = parse_firmware_job(payload) else {
            return;
        };
        self.run_update(&job, client);
    }

    fn run_update(&self, job: &FirmwareJob, client: &mut impl MqttTransport) {
        let update_topic = format!(
            "$aws/things/{}/jobs/{}/update",
            self.thing_name, job.job_id
        );
        log::info!("starting OTA job {}", job.job_id);
        let _ = client.publish(&update_topic, QOS1, false, br#"{"status":"IN_PROGRESS"}"#);

        match crate::ota::perform_ota(&job.url) {
            Ok(()) => {
                let _ = client.publish(&update_topic, QOS1, false, br#"{"status":"SUCCEEDED"}"#);
                // Give the broker a moment to actually put the status on the
                // wire; otherwise the job is stuck IN_PROGRESS forever even
                // though the device is running the new image.
                std::thread::sleep(std::time::Duration::from_millis(1500));
                log::info!("OTA complete; restarting");
                unsafe { esp_idf_svc::sys::esp_restart() };
            }
            Err(e) => {
                log::error!("OTA failed: {e:#}");
                let payload = serde_json::json!({
                    "status": "FAILED",
                    "statusDetails": { "reason": truncate(&format!("{e:#}"), 128) }
                });
                let _ = client.publish(
                    &update_topic,
                    QOS1,
                    false,
                    &serde_json::to_vec(&payload).unwrap_or_default(),
                );
            }
        }
    }
}

/// A firmware update job we are willing to execute.
struct FirmwareJob {
    job_id: String,
    url: String,
}

/// Extracts a firmware job from a `$next/get/accepted` payload.
///
/// Returns `None` for an empty queue or any job we do not implement — both are
/// normal, not errors.
fn parse_firmware_job(payload: &[u8]) -> Option<FirmwareJob> {
    let json: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let execution = json.get("execution")?;
    let job_id = execution.get("jobId")?.as_str()?;
    let doc = execution.get("jobDocument")?;

    let operation = doc.get("operation").and_then(|v| v.as_str()).unwrap_or("");
    if operation != OP_FIRMWARE_UPDATE {
        log::info!("ignoring job {job_id}: unsupported operation {operation:?}");
        return None;
    }

    let url = doc.get("download_url").and_then(|v| v.as_str())?;
    // Refuse plaintext downloads: an unauthenticated firmware image is a
    // remote code execution primitive.
    if !url.starts_with("https://") {
        log::error!("refusing OTA job {job_id}: download_url is not https");
        return None;
    }

    Some(FirmwareJob {
        job_id: job_id.to_string(),
        url: url.to_string(),
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
