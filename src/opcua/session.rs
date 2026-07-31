//! The OPC UA session: connection, subscription creation, monitored items.
//!
//! Differences from the original spike, all of which were defects:
//!
//! * `connect_to_endpoint_id` takes a *key into the client config*, not a URL.
//!   Passing the URL there could only ever fail. We connect directly to the
//!   configured endpoint instead, which also avoids the discovery round trip —
//!   servers behind NAT or Docker routinely advertise unreachable hostnames.
//! * `create_sample_keypair(true)` made the client generate an RSA keypair and
//!   write it to a PKI directory. There is no filesystem mounted on this
//!   device, and RSA keygen on an ESP32-S3 takes tens of seconds.
//! * The `SessionEventLoop` join handle was dropped, so a dead session was
//!   indistinguishable from a healthy one.
//! * Message/chunk limits were left at desktop defaults, which at 250 tags
//!   allocate far more than the device's free heap.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use opcua_client::{ClientBuilder, DataChangeCallback, IdentityToken, Session};
use opcua_types::{
    EndpointDescription, ExtensionObject, MonitoredItemCreateRequest, MonitoringMode,
    MonitoringParameters, NodeId, ReadValueId, TimestampsToReturn, Variant,
};
use tokio::task::JoinHandle;

use gateway_core::plan::PlannedItem;
use gateway_core::settings::InstanceSettings;

use super::variant::{from_data_value, RawSample};

/// NodeId of `Server_NamespaceArray`, used to resolve `ns_uri`.
const SERVER_NAMESPACE_ARRAY: u32 = 2255;

/// Callback invoked for every incoming data change.
pub type SampleSink = Arc<dyn Fn(RawSample) + Send + Sync>;

/// Wall clock in ms, used as the fallback timestamp for values the server did
/// not stamp.
pub type NowFn = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Result of one `CreateMonitoredItems` entry.
///
/// Positional: the service contract guarantees results come back in request
/// order, so the caller pairs an outcome with the tag it belongs to by index.
#[derive(Debug, Clone, Copy)]
pub struct ItemOutcome {
    /// StatusCode the server returned.
    pub status: u32,
}

/// A live OPC UA session plus the task driving it.
pub struct Connection {
    session: Arc<Session>,
    event_loop: JoinHandle<opcua_types::StatusCode>,
}

/// Message-size budget.
///
/// The stack allocates buffers from these numbers, so desktop defaults
/// (tens of megabytes) are not merely wasteful here, they fail to allocate.
/// 64 KiB total across 16 KiB chunks comfortably carries a 50-item
/// `CreateMonitoredItems` response and a full publish response.
const MAX_MESSAGE_SIZE: usize = 64 * 1024;
const MAX_CHUNK_SIZE: usize = 16 * 1024;
const MAX_CHUNK_COUNT: usize = 8;
const MAX_ARRAY_LENGTH: usize = 1_024;
const MAX_STRING_LENGTH: usize = 8 * 1024;

/// Queue depth per monitored item.
///
/// 1 means "only the latest value matters"; anything larger multiplies by the
/// tag count and is the fastest way to exhaust heap on this device.
const ITEM_QUEUE_SIZE: u32 = 1;

impl Connection {
    /// Opens a session against `instance`.
    ///
    /// Returns as soon as the secure channel and session are up; the caller is
    /// expected to synchronise subscriptions immediately afterwards.
    pub async fn connect(instance: &InstanceSettings, session_name: &str) -> Result<Self> {
        // Phase 1 is unauthenticated and unencrypted by explicit configuration
        // (validated in `InstanceSettings::validate`). Say so on every connect
        // so it can never become an unnoticed default.
        log::warn!(
            "OPC UA link to {} is UNENCRYPTED and UNAUTHENTICATED (sec_policy=None, \
             sec_mode=None, anonymous identity). Treat the OT network as trusted.",
            instance.endpoint
        );

        let mut client = ClientBuilder::new()
            .application_name("ESP32 OPC UA Gateway")
            .application_uri("urn:esp32-opcua-gateway")
            .product_uri("urn:esp32-opcua-gateway")
            .session_name(session_name)
            // No filesystem, no PKI, and no need for either at security None.
            .create_sample_keypair(false)
            .trust_server_certs(true)
            .verify_server_certs(false)
            .session_timeout(instance.session_timeout_ms)
            .keep_alive_interval(Duration::from_millis(instance.keepalive_ms as u64))
            .request_timeout(Duration::from_secs(20))
            .publish_timeout(Duration::from_secs(30))
            .max_message_size(MAX_MESSAGE_SIZE)
            .max_chunk_size(MAX_CHUNK_SIZE)
            .max_incoming_chunk_size(MAX_CHUNK_SIZE)
            .max_chunk_count(MAX_CHUNK_COUNT)
            .max_array_length(MAX_ARRAY_LENGTH)
            .max_string_length(MAX_STRING_LENGTH)
            .max_byte_string_length(MAX_STRING_LENGTH)
            .recreate_monitored_items_chunk(gateway_core::plan::MAX_ITEMS_PER_REQUEST)
            .client()
            .map_err(|errors| anyhow!("invalid OPC UA client configuration: {errors:?}"))?;

        let endpoint = EndpointDescription::from(instance.endpoint.as_str());
        let (session, event_loop) = client
            .connect_to_endpoint_directly(endpoint, IdentityToken::Anonymous)
            .with_context(|| format!("connecting to {}", instance.endpoint))?;

        let event_loop = event_loop.spawn();

        if !session.wait_for_connection().await {
            event_loop.abort();
            return Err(anyhow!("session to {} never came up", instance.endpoint));
        }

        log::info!("OPC UA session established with {}", instance.endpoint);
        Ok(Self {
            session,
            event_loop,
        })
    }

    /// False once the session event loop has terminated, i.e. the session is
    /// unrecoverably gone and the driver must reconnect.
    pub fn is_alive(&self) -> bool {
        !self.event_loop.is_finished()
    }

    /// Reads the server's NamespaceArray so `ns_uri` can be resolved to an index.
    pub async fn namespace_array(&self) -> Result<Vec<String>> {
        let node = NodeId::new(0u16, SERVER_NAMESPACE_ARRAY);
        let results = self
            .session
            .read(&[ReadValueId::from(node)], TimestampsToReturn::Neither, 0.0)
            .await
            .map_err(|e| anyhow!("reading NamespaceArray: {e}"))?;

        let Some(dv) = results.into_iter().next() else {
            return Err(anyhow!("NamespaceArray read returned no results"));
        };
        match dv.value {
            Some(Variant::Array(arr)) => Ok(arr
                .values
                .iter()
                .map(|v| match v {
                    Variant::String(s) => s.as_ref().to_string(),
                    other => format!("{other:?}"),
                })
                .collect()),
            other => Err(anyhow!("NamespaceArray had unexpected type: {other:?}")),
        }
    }

    /// Creates one subscription and wires its notifications into `sink`.
    pub async fn create_subscription(
        &self,
        publish_ms: u32,
        sink: SampleSink,
        now: NowFn,
    ) -> Result<u32> {
        let interval = Duration::from_millis(publish_ms.max(1) as u64);

        // Keep-alive count is expressed in publishing intervals; ~10 s of
        // silence before a keep-alive, ~60 s before the server drops us.
        let keep_alive_count = (10_000 / publish_ms.max(1)).max(1);
        let lifetime_count = keep_alive_count * 3;

        let callback = DataChangeCallback::new(move |dv, item| {
            sink(from_data_value(item.client_handle(), &dv, now()));
        });

        let id = self
            .session
            .create_subscription(
                interval,
                lifetime_count,
                keep_alive_count,
                // Bound the size of a single publish response; the stack will
                // send more responses rather than one oversized one.
                gateway_core::plan::MAX_ITEMS_PER_REQUEST as u32,
                0,
                true,
                callback,
            )
            .await
            .map_err(|e| anyhow!("creating subscription at {publish_ms} ms: {e}"))?;

        Ok(id)
    }

    /// Creates one chunk of monitored items.
    ///
    /// Per-item failures are returned rather than raised: one bad address out
    /// of 250 must not stop the other 249 from reporting.
    pub async fn create_items(
        &self,
        subscription_id: u32,
        items: &[PlannedItem],
    ) -> Result<Vec<ItemOutcome>> {
        let mut requests = Vec::with_capacity(items.len());
        for item in items {
            let node_id = NodeId::from_str(&item.tag.node_id)
                .map_err(|e| anyhow!("unparseable NodeId {}: {e:?}", item.tag.node_id))?;
            requests.push(MonitoredItemCreateRequest {
                item_to_monitor: ReadValueId::from(node_id),
                monitoring_mode: MonitoringMode::Reporting,
                requested_parameters: MonitoringParameters {
                    client_handle: item.client_handle,
                    sampling_interval: item.tag.scan_rate_ms as f64,
                    // Deadband filtering is a phase-2 concern; the bundle
                    // carries the value but the server is not asked for it yet.
                    filter: ExtensionObject::null(),
                    queue_size: ITEM_QUEUE_SIZE,
                    discard_oldest: true,
                },
            });
        }

        let created = self
            .session
            .create_monitored_items(subscription_id, TimestampsToReturn::Both, requests)
            .await
            .map_err(|e| anyhow!("creating {} monitored items: {e}", items.len()))?;

        // The service contract guarantees result order matches request order;
        // pairing by index is what makes the handle mapping trustworthy.
        Ok(items
            .iter()
            .zip(created.into_iter())
            .map(|(_, result)| ItemOutcome {
                status: result.result.status_code.bits(),
            })
            .collect())
    }

    /// Closes the session and stops the event loop.
    pub async fn shutdown(self) {
        if let Err(e) = self.session.disconnect().await {
            log::debug!("OPC UA disconnect returned {e}");
        }
        self.event_loop.abort();
    }
}
