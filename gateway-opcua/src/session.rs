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
//!
//! And two that only showed up on hardware, both caused by async-opcua's own
//! reconnect loop running underneath the driver's (see
//! `docs/OPCUA_INTEGRATION_TEST.md` §8.10 and §8.11):
//!
//! * A lost connection was retried *inside* the event loop, so the join handle
//!   never finished and a dead server was reported as `running` for minutes.
//!   Library reconnects are now off: the event loop ends when the transport
//!   does, and the driver's jittered backoff is the one retry mechanism.
//! * `Session::disconnect` waits for a `Disconnected` state that the event loop
//!   only publishes from a *connected* transport. Mid-reconnect it never came,
//!   so disabling the gateway wedged the driver for good. Shutdown is now
//!   bounded, and skipped entirely for a session that is already gone.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_core::Stream;
use opcua_client::transport::TcpConnector;
use opcua_client::{ClientBuilder, DataChangeCallback, IdentityToken, Session, SessionEventLoop};
use opcua_types::{
    constants::SECURITY_POLICY_NONE_URI, AttributeId, EndpointDescription, ExtensionObject,
    MessageSecurityMode, MonitoredItemCreateRequest, MonitoringMode, MonitoringParameters, NodeId,
    QualifiedName, ReadValueId, StatusCode, TimestampsToReturn, UserTokenPolicy, Variant,
};
use tokio::task::JoinHandle;

use gateway_core::plan::PlannedItem;
use gateway_core::settings::InstanceSettings;

use crate::variant::{from_data_value, RawSample};
use crate::Clock;

/// NodeId of `Server_NamespaceArray`, used to resolve `ns_uri`.
const SERVER_NAMESPACE_ARRAY: u32 = 2255;

/// Callback invoked for every incoming data change.
pub type SampleSink = Arc<dyn Fn(RawSample) + Send + Sync>;

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
    event_loop: JoinHandle<StatusCode>,
    /// Set once the event loop has been observed to end.
    ended: Option<String>,
}

/// Message-size budget.
///
/// The stack allocates buffers from these numbers, so desktop defaults
/// (tens of megabytes) are not merely wasteful here, they fail to allocate.
///
/// Measured on hardware: with 64 KiB / 16 KiB the heap fell from ~232 KiB at
/// MQTT connect to under 9 KiB two seconds later, and the first
/// `ExtensionObject` decode aborted the process trying to build the generated
/// type table. 16 KiB across 8 KiB chunks still clears the ~15–25 KiB that
/// docs/OPCUA_CLIENT_REQUIREMENTS.md §D5 budgets for a 250-tag publish
/// response only if that response is chunked — which it is, because
/// `MAX_CHUNK_COUNT` chunks of `MAX_CHUNK_SIZE` bound the message, not one
/// contiguous buffer.
const MAX_MESSAGE_SIZE: usize = 16 * 1024;
const MAX_CHUNK_SIZE: usize = 8 * 1024;
const MAX_CHUNK_COUNT: usize = 4;
const MAX_ARRAY_LENGTH: usize = 1_024;
const MAX_STRING_LENGTH: usize = 8 * 1024;

/// Queue depth per monitored item.
///
/// 1 means "only the latest value matters"; anything larger multiplies by the
/// tag count and is the fastest way to exhaust heap on this device.
const ITEM_QUEUE_SIZE: u32 = 1;

/// Consecutive failed keep-alives after which the session is closed.
///
/// This is what notices a *silent* peer — a pulled cable, a hung PLC — where no
/// RST ever arrives and the socket would otherwise look healthy until TCP gives
/// up, which can take a quarter of an hour. One miss is tolerated so a single
/// slow read does not cost a full resubscribe.
const MAX_FAILED_KEEPALIVES: u64 = 2;

/// Bounds on the per-request timeout.
///
/// The timeout follows the configured keep-alive (twice it) because together
/// they set how long a silent peer goes unnoticed: roughly
/// `keepalive + (MAX_FAILED_KEEPALIVES + 1) × timeout`. That is about 70 s at
/// the default 10 s keep-alive and about 16 s at 1 s. The floor keeps a
/// 50-item `CreateMonitoredItems` on a slow PLC from timing out.
const MIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Longest a graceful `CloseSession` may take before the session is simply
/// abandoned. A disable must not wait on a server that may never answer.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Per-request timeout for a given keep-alive interval.
fn request_timeout(keepalive_ms: u32) -> Duration {
    (Duration::from_millis(keepalive_ms as u64) * 2).clamp(MIN_REQUEST_TIMEOUT, MAX_REQUEST_TIMEOUT)
}

/// Builds the client and starts the session's event loop.
///
/// Out of line and synchronous on purpose: the by-value builder chain needs a
/// large frame, which inlined into `connect` stayed live for the whole connect.
#[inline(never)]
fn open(
    instance: &InstanceSettings,
    session_name: &str,
    request_timeout: Duration,
) -> Result<(Arc<Session>, JoinHandle<StatusCode>)> {
    let mut client = ClientBuilder::new()
        .application_name("ESP32 OPC UA Gateway")
        .application_uri("urn:esp32-opcua-gateway")
        .product_uri("urn:esp32-opcua-gateway")
        .session_name(session_name)
        // No filesystem, no PKI, and no need for either at security None.
        // The library still insists on a PKI directory; on a host it would
        // otherwise create `./pki` in whatever directory the client runs
        // from. On the device the create fails harmlessly, as before.
        .create_sample_keypair(false)
        .pki_dir(std::env::temp_dir().join("gateway-opcua-pki"))
        .trust_server_certs(true)
        .verify_server_certs(false)
        // One attempt, no library-level retries: see the module docs.
        .session_retry_limit(0)
        .max_failed_keep_alive_count(MAX_FAILED_KEEPALIVES)
        .session_timeout(instance.session_timeout_ms)
        .keep_alive_interval(Duration::from_millis(instance.keepalive_ms as u64))
        .request_timeout(request_timeout)
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

    // The endpoint has to carry the Anonymous policy explicitly.
    // `EndpointDescription::from(&str)` leaves `user_identity_tokens`
    // empty, and because we deliberately skip discovery there is nothing
    // else to fill it in. ActivateSession then looks for the policy
    // matching the `IdentityToken::Anonymous` below, finds an empty list,
    // and fails with `BadSecurityPolicyRejected` — after CreateSession has
    // already succeeded, so the server looks reachable and the session
    // still never comes up.
    let endpoint = EndpointDescription::from((
        instance.endpoint.as_str(),
        SECURITY_POLICY_NONE_URI,
        MessageSecurityMode::None,
        UserTokenPolicy::anonymous(),
    ));
    let (session, event_loop) = client
        .connect_to_endpoint_directly(endpoint, IdentityToken::Anonymous)
        .with_context(|| format!("connecting to {}", instance.endpoint))?;

    Ok((session, spawn_event_loop(event_loop)))
}

/// Starts the session's event loop as a task.
///
/// Out of line for the same reason as `open`: the loop is built in a temporary
/// before it is boxed, and inlined, that temporary (~8 KB on the host) sat in
/// `open`'s frame under all of `connect_to_endpoint_directly`.
///
/// This is `SessionEventLoop::run` with the stream boxed. `run` builds the
/// ~8 KB stream inside its own future, so its poll frame reserves room for it
/// on every poll, including at the deepest point of a connect: 7.4 KiB of the
/// host's 42 KiB peak, used once.
#[inline(never)]
fn spawn_event_loop(event_loop: SessionEventLoop<TcpConnector>) -> JoinHandle<StatusCode> {
    // Boxed before spawning: tokio only boxes futures over 16 KiB itself, so
    // `event_loop.spawn()` moved the loop by value through several stack
    // frames (~65 KiB on the host) and overflowed the device.
    let mut events = Box::pin(event_loop.enter());
    tokio::task::spawn(async move {
        loop {
            match std::future::poll_fn(|cx| events.as_mut().poll_next(cx)).await {
                None => break StatusCode::Good,
                Some(Err(status)) => break status,
                Some(Ok(_)) => {}
            }
        }
    })
}

impl Connection {
    /// Opens a session against `instance`.
    ///
    /// Returns as soon as the secure channel and session are up; the caller is
    /// expected to synchronise subscriptions immediately afterwards. Never
    /// waits longer than one request timeout plus a margin, and never retries:
    /// retrying is the driver's job.
    pub async fn connect(instance: &InstanceSettings, session_name: &str) -> Result<Self> {
        // Phase 1 is unauthenticated and unencrypted by explicit configuration
        // (validated in `InstanceSettings::validate`). Say so on every connect
        // so it can never become an unnoticed default.
        log::warn!(
            "OPC UA link to {} is UNENCRYPTED and UNAUTHENTICATED (sec_policy=None, \
             sec_mode=None, anonymous identity). Treat the OT network as trusted.",
            instance.endpoint
        );

        let request_timeout = request_timeout(instance.keepalive_ms);
        let (session, mut event_loop) = open(instance, session_name, request_timeout)?;

        // `wait_for_connection` never returns if the event loop gives up, so
        // it has to be raced against the loop itself — and against a clock,
        // for a server that accepts TCP and then says nothing.
        let deadline = request_timeout * 2;
        tokio::select! {
            connected = session.wait_for_connection() => {
                if !connected {
                    event_loop.abort();
                    return Err(anyhow!("session to {} never came up", instance.endpoint));
                }
            }
            ended = &mut event_loop => {
                return Err(anyhow!(
                    "could not connect to {}: {}",
                    instance.endpoint,
                    describe_end(ended)
                ));
            }
            _ = tokio::time::sleep(deadline) => {
                event_loop.abort();
                return Err(anyhow!(
                    "timed out after {} s connecting to {}",
                    deadline.as_secs(),
                    instance.endpoint
                ));
            }
        }

        // From here on a lost transport must END the event loop rather than
        // be retried inside it; that is what lets the driver see the loss.
        session.disable_reconnects();

        log::info!("OPC UA session established with {}", instance.endpoint);
        Ok(Self {
            session,
            event_loop,
            ended: None,
        })
    }

    /// False once the session event loop has terminated, i.e. the session is
    /// unrecoverably gone and the driver must reconnect.
    pub fn is_alive(&self) -> bool {
        self.ended.is_none() && !self.event_loop.is_finished()
    }

    /// Resolves when the session is gone, with the reason.
    ///
    /// Cancel-safe, and returns immediately on every call after the first.
    pub async fn closed(&mut self) -> String {
        if let Some(reason) = &self.ended {
            return reason.clone();
        }
        let ended = (&mut self.event_loop).await;
        let reason = describe_end(ended);
        self.ended = Some(reason.clone());
        reason
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
        clock: Clock,
    ) -> Result<u32> {
        let interval = Duration::from_millis(publish_ms.max(1) as u64);

        // Keep-alive count is expressed in publishing intervals; ~10 s of
        // silence before a keep-alive, ~60 s before the server drops us.
        let keep_alive_count = (10_000 / publish_ms.max(1)).max(1);
        let lifetime_count = keep_alive_count * 3;

        let callback = DataChangeCallback::new(move |dv, item| {
            sink(from_data_value(item.client_handle(), &dv, clock()));
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

    /// Deletes subscriptions, together with their monitored items.
    ///
    /// Used on a live reconfiguration, before the replacement subscriptions
    /// are created: left in place, the old ones would keep delivering samples
    /// for tags the new configuration removed.
    pub async fn delete_subscriptions(&self, ids: &[u32]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let results = self
            .session
            .delete_subscriptions(ids)
            .await
            .map_err(|e| anyhow!("deleting {} subscriptions: {e}", ids.len()))?;
        for (id, status) in ids.iter().zip(results) {
            if !status.is_good() {
                // Not fatal: the server will expire it with the lifetime count.
                log::warn!("subscription {id} not deleted: {status}");
            }
        }
        Ok(())
    }

    /// Finds the items whose node the server does not have.
    ///
    /// Servers differ on an unknown NodeId in `CreateMonitoredItems`: most
    /// reject the item with `BadNodeIdUnknown`, but the specification lets a
    /// server accept it and deliver the Bad status in the first notification
    /// instead — `async-opcua-server` does exactly that. On such a server a
    /// typo'd address would count as *applied* and never appear in
    /// `failed_sample`. Reading the NodeClass first (a one-integer attribute,
    /// so the response stays tiny) makes "failed" mean the same thing on every
    /// server.
    ///
    /// Returns, per item and in order, the status that marks it missing. A
    /// failure of the Read itself is not fatal: every item is then left for
    /// the server to judge.
    pub async fn missing_nodes(&self, items: &[PlannedItem]) -> Vec<Option<u32>> {
        let mut reads = Vec::with_capacity(items.len());
        for item in items {
            let Ok(node_id) = NodeId::from_str(&item.tag.node_id) else {
                return vec![None; items.len()];
            };
            reads.push(ReadValueId {
                node_id,
                attribute_id: AttributeId::NodeClass as u32,
                index_range: Default::default(),
                data_encoding: QualifiedName::null(),
            });
        }

        let results = match self
            .session
            .read(&reads, TimestampsToReturn::Neither, 0.0)
            .await
        {
            Ok(results) if results.len() == items.len() => results,
            Ok(results) => {
                log::warn!(
                    "NodeClass check answered {} of {} reads; skipping it",
                    results.len(),
                    items.len()
                );
                return vec![None; items.len()];
            }
            Err(e) => {
                log::warn!("NodeClass check failed ({e}); skipping it");
                return vec![None; items.len()];
            }
        };
        results
            .into_iter()
            .map(|dv| match dv.status {
                Some(s)
                    if s == StatusCode::BadNodeIdUnknown || s == StatusCode::BadNodeIdInvalid =>
                {
                    Some(s.bits())
                }
                _ => None,
            })
            .collect()
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
        // pairing by index is what makes the handle mapping trustworthy. A
        // short response would silently drop the tail, so refuse it.
        if created.len() != items.len() {
            return Err(anyhow!(
                "server answered {} of {} monitored-item requests",
                created.len(),
                items.len()
            ));
        }
        Ok(created
            .into_iter()
            .map(|result| ItemOutcome {
                status: result.result.status_code.bits(),
            })
            .collect())
    }

    /// Closes the session and stops the event loop.
    ///
    /// Bounded: a server that has gone silent gets `SHUTDOWN_TIMEOUT` (3 s) to
    /// acknowledge `CloseSession`, then the session is dropped regardless.
    pub async fn shutdown(self) {
        if self.is_alive() {
            match tokio::time::timeout(SHUTDOWN_TIMEOUT, self.session.disconnect()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::debug!("OPC UA disconnect returned {e}"),
                Err(_) => log::warn!(
                    "OPC UA server did not acknowledge CloseSession within {} s; \
                     abandoning the session",
                    SHUTDOWN_TIMEOUT.as_secs()
                ),
            }
        }
        self.event_loop.abort();
    }
}

impl Drop for Connection {
    /// A safety net: a `Connection` dropped without [`Connection::shutdown`]
    /// would otherwise leave its event loop running detached, still holding
    /// the session and its subscriptions.
    fn drop(&mut self) {
        self.event_loop.abort();
    }
}

fn describe_end(ended: Result<StatusCode, tokio::task::JoinError>) -> String {
    match ended {
        Ok(status) if status.is_good() => "connection closed".to_string(),
        Ok(status) => status.to_string(),
        Err(e) if e.is_cancelled() => "event loop cancelled".to_string(),
        Err(e) => format!("event loop panicked: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_timeout_follows_keepalive_within_bounds() {
        assert_eq!(request_timeout(1_000), MIN_REQUEST_TIMEOUT);
        assert_eq!(request_timeout(4_000), Duration::from_secs(8));
        assert_eq!(request_timeout(10_000), MAX_REQUEST_TIMEOUT);
        assert_eq!(request_timeout(60_000), MAX_REQUEST_TIMEOUT);
    }
}
