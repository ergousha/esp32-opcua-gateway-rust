//! Thin wrapper for esp-idf-svc MQTT client: sets up mutual-TLS client
//! and routes events (Connected / Disconnected / Message) to an mpsc channel.
//!
//! Note: MqttClientConfiguration expects `X509<'static>` for certificates —
//! therefore, we upgrade runtime PEMs (from NVS) to 'static using Box::leak.
//! Since these identities live for the duration of the program, this leak is not a problem
//! in PoC (the device runs with a single identity).

use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use anyhow::{Context, Result};
use esp_idf_svc::mqtt::client::{
    EspMqttClient, EventPayload, MqttClientConfiguration, MqttProtocolVersion, QoS,
};
use esp_idf_svc::tls::X509;

/// Events transferred from callback to main task (data are owned copies).
#[derive(Debug)]
pub enum MqttEvent {
    Connected,
    Disconnected,
    Message { topic: String, data: Vec<u8> },
}

/// Holds the established client and the event channel together.
pub struct MqttSession {
    pub client: EspMqttClient<'static>,
    pub events: Receiver<MqttEvent>,
}

/// The only MQTT operations the rest of the firmware needs.
///
/// Keeping this narrow is what lets the shadow client, the Jobs handler and
/// the telemetry publisher be written against an interface instead of against
/// `EspMqttClient` directly.
pub trait MqttTransport {
    /// Publishes a payload. Errors are recoverable; the caller decides whether
    /// to retry or drop.
    fn publish(&mut self, topic: &str, qos: QoS, retain: bool, payload: &[u8]) -> Result<()>;
    /// Subscribes to a topic filter.
    fn subscribe(&mut self, topic: &str, qos: QoS) -> Result<()>;
    /// Drops a subscription. Needed because the tag-bundle topic is versioned:
    /// without this, every configuration bump would leak a subscription
    /// against the broker's per-connection limit.
    fn unsubscribe(&mut self, topic: &str) -> Result<()>;
}

impl MqttTransport for EspMqttClient<'static> {
    fn publish(&mut self, topic: &str, qos: QoS, retain: bool, payload: &[u8]) -> Result<()> {
        EspMqttClient::publish(self, topic, qos, retain, payload)
            .with_context(|| format!("publishing to {topic}"))?;
        Ok(())
    }

    fn subscribe(&mut self, topic: &str, qos: QoS) -> Result<()> {
        EspMqttClient::subscribe(self, topic, qos)
            .with_context(|| format!("subscribing to {topic}"))?;
        Ok(())
    }

    fn unsubscribe(&mut self, topic: &str) -> Result<()> {
        EspMqttClient::unsubscribe(self, topic)
            .with_context(|| format!("unsubscribing from {topic}"))?;
        Ok(())
    }
}

/// TLS credentials. Passed as a single reference: passing 5 separate &str arguments
/// was exceeding the register limit of fat-pointer arguments in xtensa, causing incorrect reads.
/// A struct reference prevents this.
pub struct Creds<'a> {
    pub root_ca: &'a str,
    pub client_cert: &'a str,
    pub private_key: &'a str,
}

/// Upgrades PEM string (possibly without null terminator) to 'static X509.
fn static_x509(pem: &str) -> X509<'static> {
    let mut bytes = pem.as_bytes().to_vec();
    if bytes.last() != Some(&0) {
        bytes.push(0);
    }
    let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    X509::pem_until_nul(leaked)
}

/// Sets up a client connecting to IoT Core over TLS using the given identity.
/// Connection is established asynchronously; caller should wait for `MqttEvent::Connected`.
pub fn connect(url: &str, client_id: &str, creds: &Creds) -> Result<MqttSession> {
    let root_ca = creds.root_ca;
    let client_cert = creds.client_cert;
    let private_key = creds.private_key;

    log::info!(
        "connect: ca_len={} cert_len={} key_len={} free_heap={}",
        root_ca.len(),
        client_cert.len(),
        private_key.len(),
        unsafe { esp_idf_svc::sys::esp_get_free_heap_size() }
    );

    let (tx, rx) = channel::<MqttEvent>();

    let conf = MqttClientConfiguration {
        client_id: Some(client_id),
        protocol_version: Some(MqttProtocolVersion::V3_1_1),
        keep_alive_interval: Some(Duration::from_secs(60)),
        // Must exceed `gateway_core::MAX_BUNDLE_BYTES`: the retained tag
        // bundle has to arrive in a single chunk, because esp-mqtt only
        // reports the topic on the first chunk of a fragmented message and we
        // would otherwise be unable to tell what the payload belongs to.
        // Also covers the provisioning create/accepted response (cert + key).
        buffer_size: 12 * 1024,
        out_buffer_size: 2048,
        server_certificate: Some(static_x509(root_ca)),
        client_certificate: Some(static_x509(client_cert)),
        private_key: Some(static_x509(private_key)),
        ..Default::default()
    };

    let client = EspMqttClient::new_cb(url, &conf, move |event| {
        match event.payload() {
            EventPayload::Connected(_) => {
                let _ = tx.send(MqttEvent::Connected);
            }
            EventPayload::Disconnected => {
                let _ = tx.send(MqttEvent::Disconnected);
            }
            EventPayload::Received {
                topic: Some(t),
                data,
                ..
            } => {
                // topic only comes in the first chunk; because buffer is large enough,
                // messages arrive in a single chunk.
                let _ = tx.send(MqttEvent::Message {
                    topic: t.to_string(),
                    data: data.to_vec(),
                });
            }
            _ => {}
        }
    })
    .context("Failed to set up MQTT client")?;

    Ok(MqttSession { client, events: rx })
}

pub const QOS1: QoS = QoS::AtLeastOnce;
