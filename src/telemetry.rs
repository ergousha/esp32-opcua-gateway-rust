//! Normal operation: device connects to IoT Core with its own (provisioned) identity
//! and listens to MQTT events in the background.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Result};

use crate::config;
use crate::device_id::{self, DeviceIdentity};
use crate::mqtt_util::{self, MqttEvent, QOS1};
use serde_json::Value;

/// Connects with device identity and enters infinite listening loop.
pub fn run(id: &DeviceIdentity) -> Result<()> {
    log::info!("Connection-only mode. thing={}", id.thing_name);

    let mut session = mqtt_util::connect(
        &config::mqtt_url(),
        &id.thing_name, // client_id == thingName (device policy limits it this way)
        &mqtt_util::Creds {
            root_ca: device_id::root_ca_pem(),
            client_cert: &id.cert_pem,
            private_key: &id.key_pem,
        },
    )?;

    // Wait for connection.
    loop {
        match session.events.recv_timeout(Duration::from_secs(30)) {
            Ok(MqttEvent::Connected) => break,
            Ok(MqttEvent::Disconnected) => bail!("device connection lost"),
            Ok(_) => continue,
            Err(_) => bail!("device connection timeout"),
        }
    }
    log::info!("Connected with device identity. Connection kept open.");

    // Mark current firmware as valid so it won't rollback
    if let Err(e) = crate::ota::mark_valid() {
        log::warn!("Failed to mark firmware as valid: {}", e);
    }

    // Subscribe to IoT Jobs topics
    let notify_topic = format!("$aws/things/{}/jobs/notify-next", id.thing_name);
    let accepted_topic = format!("$aws/things/{}/jobs/$next/get/accepted", id.thing_name);
    let get_topic = format!("$aws/things/{}/jobs/$next/get", id.thing_name);

    if let Err(e) = session.client.subscribe(&notify_topic, QOS1) {
        log::error!("Failed to subscribe to notify topic: {}", e);
    }
    if let Err(e) = session.client.subscribe(&accepted_topic, QOS1) {
        log::error!("Failed to subscribe to accepted topic: {}", e);
    }

    log::info!("Subscribed to AWS IoT Jobs topics.");

    // Request pending jobs right away
    let _ = session.client.publish(&get_topic, QOS1, false, b"{}");

    loop {
        // Drain incoming events (e.g. disconnect) in the background.
        while let Ok(ev) = session.events.try_recv() {
            match ev {
                MqttEvent::Disconnected => {
                    log::warn!("connection lost; esp-mqtt will reconnect automatically.");
                }
                MqttEvent::Message { topic, data } => {
                    if topic == notify_topic {
                        log::info!("Job notification received. Requesting job details...");
                        let _ = session.client.publish(&get_topic, QOS1, false, b"{}");
                    } else if topic == accepted_topic {
                        if let Ok(json) = serde_json::from_slice::<Value>(&data) {
                            if let Some(execution) = json.get("execution") {
                                if let Some(job_id) =
                                    execution.get("jobId").and_then(|v| v.as_str())
                                {
                                    if let Some(doc) = execution.get("jobDocument") {
                                        if doc.get("operation").and_then(|v| v.as_str())
                                            == Some("firmware_update")
                                        {
                                            if let Some(url) =
                                                doc.get("download_url").and_then(|v| v.as_str())
                                            {
                                                log::info!("Starting OTA job {}", job_id);
                                                let update_topic = format!(
                                                    "$aws/things/{}/jobs/{}/update",
                                                    id.thing_name, job_id
                                                );

                                                // Report IN_PROGRESS
                                                let _ = session.client.publish(
                                                    &update_topic,
                                                    QOS1,
                                                    false,
                                                    br#"{"status":"IN_PROGRESS"}"#,
                                                );

                                                match crate::ota::perform_ota(url) {
                                                    Ok(_) => {
                                                        log::info!(
                                                            "Reporting SUCCEEDED and rebooting..."
                                                        );
                                                        let _ = session.client.publish(
                                                            &update_topic,
                                                            QOS1,
                                                            false,
                                                            br#"{"status":"SUCCEEDED"}"#,
                                                        );
                                                        sleep(Duration::from_millis(1500));
                                                        unsafe {
                                                            esp_idf_svc::sys::esp_restart();
                                                        }
                                                    }
                                                    Err(e) => {
                                                        log::error!("OTA failed: {}", e);
                                                        let _ = session.client.publish(
                                                            &update_topic,
                                                            QOS1,
                                                            false,
                                                            br#"{"status":"FAILED"}"#,
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        sleep(Duration::from_millis(1000));
    }
}

/// Transition point between provisioning + operation (can be extended for commands/OTA in the future).
#[allow(dead_code)]
pub fn context_note() -> &'static str {
    "cmd/<thing>/* topics are allowed for subscription in device policy"
}
