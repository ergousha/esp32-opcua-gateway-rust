//! The MQTT operations the shadow client and the telemetry publisher need, on
//! the platform SDK's client. Every publish passes the SDK's limiter.

use anyhow::Result;
use esp_idf_svc::mqtt::client::QoS;

/// The only MQTT operations the rest of the firmware needs.
///
/// Keeping this narrow is what lets the shadow client and the telemetry
/// publisher be written against an interface instead of a concrete client.
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

impl MqttTransport for &device_sdk::Mqtt {
    fn publish(&mut self, topic: &str, qos: QoS, retain: bool, payload: &[u8]) -> Result<()> {
        let qos = match qos {
            QoS::AtMostOnce => 0,
            _ => 1,
        };
        self.publish_with(topic, payload, qos, retain)
    }

    // Subscriptions are always QoS 1: AWS IoT has no QoS 2.
    fn subscribe(&mut self, topic: &str, _qos: QoS) -> Result<()> {
        device_sdk::Mqtt::subscribe(self, topic)
    }

    fn unsubscribe(&mut self, topic: &str) -> Result<()> {
        device_sdk::Mqtt::unsubscribe(self, topic)
    }
}

pub const QOS1: QoS = QoS::AtLeastOnce;
