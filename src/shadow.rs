//! The configuration plane: AWS IoT Device Shadow plus the out-of-band tag
//! bundle.
//!
//! Settings arrive on the `opcua` named shadow, which AWS caps at 8 KB. A
//! 250-tag list does not fit, and echoing it back in `reported` would not fit
//! twice over. So the shadow carries only a pointer —
//! `cfg {v, n, sha256, topic}` — and the tag list itself arrives as a
//! *retained* MQTT message on the topic named there.
//!
//! The digest is what keeps the two planes honest: a bundle is applied only
//! when its SHA-256 and version both match what the shadow asked for. There is
//! no ordering requirement between the two messages and no way to end up
//! running a tag list the cloud did not sanction.

use anyhow::Result;
use esp_idf_svc::mqtt::client::QoS;

use gateway_core::bundle;
use gateway_core::health::DriverState;
use gateway_core::settings::DesiredSettings;
use gateway_core::shadow::{self, ShadowTopics};
use tokio::sync::mpsc::UnboundedSender;

use crate::mqtt_util::{MqttTransport, QOS1};
use crate::opcua::{AppliedConfig, Command, Shared};
use crate::settings_store::SettingsStore;

/// Minimum spacing between `reported` updates.
///
/// AWS allows 20 shadow operations/s; one every 30 s is plenty for health, and
/// the state-change path below reports immediately anyway.
const REPORT_INTERVAL_MS: i64 = 30_000;

/// Drives the shadow conversation and hands validated configs to the driver.
pub struct ConfigPlane {
    topics: ShadowTopics,
    commands: UnboundedSender<Command>,
    /// Validated settings waiting for a bundle that matches `cfg.sha256`.
    pending: Option<DesiredSettings>,
    /// Bundle topic we are currently subscribed to, if any.
    subscribed_bundle_topic: Option<String>,
    /// Version currently applied to the driver.
    applied_version: u32,
    /// Set on every apply so the caller can rebuild its publisher.
    fresh_settings: Option<DesiredSettings>,
    last_report_ms: i64,
    last_reported_state: Option<DriverState>,
}

impl ConfigPlane {
    /// Creates the config plane for `thing_name`.
    pub fn new(thing_name: &str, commands: UnboundedSender<Command>) -> Self {
        Self {
            topics: ShadowTopics::new(thing_name),
            commands,
            pending: None,
            subscribed_bundle_topic: None,
            applied_version: 0,
            fresh_settings: None,
            last_report_ms: 0,
            last_reported_state: None,
        }
    }

    /// Subscribes to the shadow topics and asks for the current document.
    ///
    /// Subscribing before publishing `/get` is mandatory: the response is not
    /// retained, so a late subscriber simply never sees it. Safe to call again
    /// after a reconnect — MQTT subscriptions do not survive a dropped session.
    pub fn start(&mut self, client: &mut impl MqttTransport) -> Result<()> {
        for topic in self.topics.subscriptions() {
            client.subscribe(topic, QOS1)?;
        }
        if let Some(topic) = self.subscribed_bundle_topic.clone() {
            client.subscribe(&topic, QOS1)?;
        }
        client.publish(&self.topics.get, QOS1, false, b"")?;
        log::info!("requested {} ", self.topics.get);
        Ok(())
    }

    /// Applies the configuration cached in NVS, if any.
    ///
    /// Done before the cloud answers so a device that boots without a WAN link
    /// still collects data.
    pub fn bootstrap(&mut self, store: &SettingsStore) {
        let Some(cached) = store.load() else {
            log::info!("no cached OPC UA configuration; waiting for the shadow");
            return;
        };
        let settings = cached.settings;
        match bundle::parse_and_verify(
            &cached.bundle,
            &settings.cfg,
            settings.instance.ns,
            settings.instance.id_type,
        ) {
            Ok(tags) => {
                log::info!(
                    "booting with cached OPC UA config v{} ({} tags)",
                    settings.cfg.v,
                    tags.len()
                );
                self.dispatch(settings, tags);
            }
            Err(e) => log::warn!("cached bundle rejected: {e}"),
        }
    }

    /// Handles one MQTT message. Never fails the caller: a bad document is
    /// logged and reported, not fatal.
    pub fn handle(
        &mut self,
        topic: &str,
        payload: &[u8],
        client: &mut impl MqttTransport,
        store: &mut SettingsStore,
        shared: &Shared,
    ) {
        if topic == self.topics.get_accepted {
            self.on_desired(payload, client, store, shared);
        } else if topic == self.topics.get_rejected {
            let err = shadow::parse_rejected(payload);
            // A 404 simply means nobody has configured this device yet.
            log::warn!("shadow get rejected: {err}");
        } else if topic == self.topics.update_delta {
            match shadow::is_relevant_delta(payload) {
                Ok((version, true)) => {
                    log::info!("shadow delta v{version}; re-reading the document");
                    let _ = client.publish(&self.topics.get, QOS1, false, b"");
                }
                Ok((_, false)) => {}
                Err(e) => log::warn!("unparseable shadow delta: {e}"),
            }
        } else if topic == self.topics.update_rejected {
            log::warn!("shadow update rejected: {}", shadow::parse_rejected(payload));
        } else if self.subscribed_bundle_topic.as_deref() == Some(topic) {
            self.on_bundle(payload, store, shared);
        }
    }

    fn on_desired(
        &mut self,
        payload: &[u8],
        client: &mut impl MqttTransport,
        store: &mut SettingsStore,
        shared: &Shared,
    ) {
        let doc = match shadow::parse_get_accepted(payload) {
            Ok(doc) => doc,
            Err(e) => {
                log::warn!("shadow document unusable: {e}");
                shared.with_reported(|r| r.set_error(e.to_string()));
                return;
            }
        };

        if let Err(e) = doc.settings.validate() {
            // Hard rejection. Notably this is where a non-`None` security
            // policy is refused rather than silently downgraded.
            log::error!("shadow v{} rejected: {e}", doc.version);
            shared.with_reported(|r| r.set_error(e.to_string()));
            return;
        }

        if !doc.settings.enabled {
            log::info!("shadow v{}: OPC UA disabled", doc.version);
            self.pending = None;
            let _ = self.commands.send(Command::Disable);
            return;
        }

        if doc.settings.cfg.v == self.applied_version {
            // Idempotent re-delivery of the version we are already running.
            log::debug!("shadow v{}: config v{} already applied", doc.version, self.applied_version);
            return;
        }

        log::info!(
            "shadow v{}: config v{} with {} tags from {}",
            doc.version,
            doc.settings.cfg.v,
            doc.settings.cfg.n,
            doc.settings.cfg.topic
        );

        // The cached bundle may already be the one this config points at, in
        // which case nothing needs to come over the wire.
        if let Some(cached) = store.load() {
            if cached.settings.cfg.sha256.eq_ignore_ascii_case(&doc.settings.cfg.sha256) {
                if let Ok(tags) = bundle::parse_and_verify(
                    &cached.bundle,
                    &doc.settings.cfg,
                    doc.settings.instance.ns,
                    doc.settings.instance.id_type,
                ) {
                    log::info!("cached bundle already matches config v{}", doc.settings.cfg.v);
                    let settings = doc.settings.clone();
                    let _ = store.save(&settings, &cached.bundle);
                    self.dispatch(settings, tags);
                    return;
                }
            }
        }

        self.subscribe_bundle(&doc.settings.cfg.topic, client);
        self.pending = Some(doc.settings);
    }

    fn subscribe_bundle(&mut self, topic: &str, client: &mut impl MqttTransport) {
        if self.subscribed_bundle_topic.as_deref() == Some(topic) {
            return;
        }
        if let Some(old) = self.subscribed_bundle_topic.take() {
            if let Err(e) = client.unsubscribe(&old) {
                log::warn!("could not unsubscribe from {old}: {e:#}");
            }
        }
        match client.subscribe(topic, QoS::AtLeastOnce) {
            Ok(()) => {
                log::info!("waiting for the retained tag bundle on {topic}");
                self.subscribed_bundle_topic = Some(topic.to_string());
            }
            Err(e) => log::error!("could not subscribe to {topic}: {e:#}"),
        }
    }

    fn on_bundle(&mut self, payload: &[u8], store: &mut SettingsStore, shared: &Shared) {
        let Some(settings) = self.pending.clone() else {
            // A retained bundle can arrive before, or long after, the shadow.
            log::debug!("tag bundle arrived with no pending configuration");
            return;
        };

        let tags = match bundle::parse_and_verify(
            payload,
            &settings.cfg,
            settings.instance.ns,
            settings.instance.id_type,
        ) {
            Ok(tags) => tags,
            Err(e) => {
                log::error!("tag bundle rejected: {e}");
                shared.with_reported(|r| r.set_error(e.to_string()));
                return;
            }
        };

        if let Err(e) = store.save(&settings, payload) {
            // Not fatal: we can still run, we just will not survive a reboot
            // without the cloud.
            log::warn!("could not cache OPC UA config: {e:#}");
        }

        self.pending = None;
        self.dispatch(settings, tags);
    }

    fn dispatch(&mut self, settings: DesiredSettings, tags: Vec<gateway_core::bundle::TagSpec>) {
        self.applied_version = settings.cfg.v;
        self.fresh_settings = Some(settings.clone());
        let config = AppliedConfig { settings, tags };
        if self.commands.send(Command::Apply(Box::new(config))).is_err() {
            log::error!("OPC UA task is gone; configuration not applied");
        }
    }

    /// Yields the settings once after each apply, so the telemetry publisher
    /// can be rebuilt against the new batching policy and `cfg.v`.
    pub fn take_fresh_settings(&mut self) -> Option<DesiredSettings> {
        self.fresh_settings.take()
    }

    /// Publishes `reported` when the state changed or the interval elapsed.
    pub fn report(&mut self, client: &mut impl MqttTransport, shared: &Shared, now_ms: i64) {
        let (payload, state) = shared.with_reported(|r| {
            r.free_heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
            (shadow::encode_reported(r), r.state)
        });

        let state_changed = self.last_reported_state != Some(state);
        let due = now_ms.saturating_sub(self.last_report_ms) >= REPORT_INTERVAL_MS;
        if !state_changed && !due {
            return;
        }

        match client.publish(&self.topics.update, QOS1, false, &payload) {
            Ok(()) => {
                self.last_report_ms = now_ms;
                self.last_reported_state = Some(state);
            }
            Err(e) => log::warn!("could not report shadow state: {e:#}"),
        }
    }
}
