//! Normal operation: device connects to IoT Core with its own (provisioned) identity
//! and listens to MQTT events in the background.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Result};

use crate::config;
use crate::device_id::{self, DeviceIdentity};
use crate::mqtt_util::{self, MqttEvent};

/// Connects with device identity and enters infinite listening loop.
pub fn run(id: &DeviceIdentity) -> Result<()> {
    log::info!(
        "Connection-only mode. thing={}",
        id.thing_name
    );

    let session = mqtt_util::connect(
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

    loop {
        // Drain incoming events (e.g. disconnect) in the background.
        while let Ok(ev) = session.events.try_recv() {
            if matches!(ev, MqttEvent::Disconnected) {
                log::warn!("connection lost; esp-mqtt will reconnect automatically.");
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
