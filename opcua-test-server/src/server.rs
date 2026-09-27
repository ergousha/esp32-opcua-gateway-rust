//! The OPC UA server that stands in for the PLC.
//!
//! Built on `async-opcua-server`, the server half of the very library the
//! gateway's client uses, and configured to match exactly what the firmware
//! supports in phase 1: SecurityPolicy `None`, MessageSecurityMode `None`,
//! anonymous identity (decision D2 in `docs/OPCUA_CLIENT_REQUIREMENTS.md`).
//! That is not laziness: the device has no key store, so a server demanding
//! anything else could not be tested against this firmware at all.
//!
//! The node set comes from [`crate::catalogue`], which the cloud-side documents
//! read too, so the server and the tag bundle can never drift apart.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use opcua_nodes::AccessLevel;
use opcua_server::authenticator::{AuthManager, DefaultAuthenticator, UserToken};
use opcua_server::diagnostics::NamespaceMetadata;
use opcua_server::node_manager::memory::{simple_node_manager, SimpleNodeManager};
use opcua_server::{ServerBuilder, ServerEndpoint, ServerHandle, ANONYMOUS_USER_TOKEN_ID};
use opcua_types::{
    DataValue, DateTime, Error, NodeId, ObjectId, StatusCode, UserTokenPolicy, Variant,
};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::catalogue::{self, Tag, FAULT_CLEARED_VALUE, NAMESPACE_URI};

/// Endpoint path the server listens under.
pub const DEFAULT_PATH: &str = "/ergousha/test";

/// Port the standalone server uses when none is given (async-opcua's default).
pub const DEFAULT_PORT: u16 = 4855;

/// How often values are stepped.
///
/// Half the fastest scan rate, so the 1 s monitored items always have a fresh
/// value and the report-by-exception behaviour of `Line1.Static` stands out
/// against them.
pub const DEFAULT_TICK: Duration = Duration::from_millis(500);

/// How to run a [`TestServer`].
#[derive(Debug, Clone)]
pub struct Options {
    /// Address to bind. Loopback for host tests — which is also what keeps a
    /// host firewall out of the picture — and `0.0.0.0` when a device on the
    /// LAN has to reach the server.
    pub bind: IpAddr,
    /// TCP port; 0 picks a free one.
    pub port: u16,
    /// Endpoint path.
    pub path: String,
    /// Host name in the endpoint URL. Defaults to the bind address, which is
    /// wrong for `0.0.0.0`; the gateway never follows discovery, so this only
    /// matters for [`TestServer::endpoint`].
    pub advertised_host: Option<String>,
    /// Value update interval.
    pub tick: Duration,
    /// Whether `Line1.Faulty` starts with its Bad StatusCode.
    pub fault: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            path: DEFAULT_PATH.to_string(),
            advertised_host: None,
            tick: DEFAULT_TICK,
            // On from the start, so the very first sync already has a
            // Bad-status tag to report.
            fault: true,
        }
    }
}

impl Options {
    /// Loopback on an ephemeral port: what host tests want.
    pub fn loopback() -> Self {
        Self::default()
    }

    fn host(&self) -> String {
        self.advertised_host
            .clone()
            .unwrap_or_else(|| self.bind.to_string())
    }
}

/// Fault-injection switches, shared with the tick task.
#[derive(Debug, Default)]
struct Control {
    fault: AtomicBool,
    frozen: AtomicBool,
    changed: Notify,
}

/// A running test server.
///
/// Dropping it without [`TestServer::stop`] aborts the tasks, which is enough
/// for a test that is failing anyway.
pub struct TestServer {
    options: Options,
    handle: ServerHandle,
    manager: Arc<SimpleNodeManager>,
    server_task: Option<JoinHandle<()>>,
    tick_task: JoinHandle<()>,
    control: Arc<Control>,
    activations: Arc<AtomicU64>,
    ns: u16,
}

impl TestServer {
    /// Builds the address space and starts serving.
    ///
    /// Returns once the listener is bound, so a client may connect immediately.
    pub async fn start(options: Options) -> Result<Self> {
        let listener = TcpListener::bind(SocketAddr::new(options.bind, options.port))
            .await
            .with_context(|| format!("binding {}:{}", options.bind, options.port))?;
        let port = listener.local_addr()?.port();
        let options = Options { port, ..options };

        let activations = Arc::new(AtomicU64::new(0));
        let (server, handle) = ServerBuilder::new()
            .application_name("Ergousha OPC UA Test Server")
            .application_uri("urn:ergousha:opcua-test-server")
            .product_uri("urn:ergousha:opcua-test-server")
            .host(options.host())
            .port(port)
            // The endpoint descriptions are built from host + port + path.
            .add_endpoint(
                "none",
                ServerEndpoint::new_none(&options.path, &[ANONYMOUS_USER_TOKEN_ID.to_string()]),
            )
            .discovery_urls(vec![options.path.clone()])
            // No certificates at security None; keep the PKI directory the
            // library insists on out of the working tree.
            .create_sample_keypair(false)
            .pki_dir(pki_dir())
            .with_authenticator(Arc::new(CountingAuthenticator {
                inner: DefaultAuthenticator::new(Default::default()),
                activations: Arc::clone(&activations),
            }))
            .with_node_manager(simple_node_manager(
                NamespaceMetadata {
                    namespace_uri: NAMESPACE_URI.to_string(),
                    ..Default::default()
                },
                "ergousha-test",
            ))
            .build()
            .map_err(|e| anyhow!("invalid server configuration: {e}"))?;

        let manager = handle
            .node_managers()
            .get_of_type::<SimpleNodeManager>()
            .ok_or_else(|| anyhow!("simple node manager missing"))?;
        let ns = handle
            .get_namespace_index(NAMESPACE_URI)
            .ok_or_else(|| anyhow!("{NAMESPACE_URI} was not registered"))?;

        let control = Arc::new(Control::default());
        control.fault.store(options.fault, Ordering::SeqCst);
        populate(&manager, ns, options.fault);

        let server_task = tokio::spawn(async move {
            if let Err(e) = server.run_with(listener).await {
                log::error!("OPC UA test server stopped: {e}");
            }
        });
        let tick_task = tokio::spawn(tick_loop(
            Arc::clone(&manager),
            handle.clone(),
            Arc::clone(&control),
            ns,
            options.tick,
        ));

        let server = Self {
            options,
            handle,
            manager,
            server_task: Some(server_task),
            tick_task,
            control,
            activations,
            ns,
        };
        log::info!(
            "OPC UA test server on {} (namespace {NAMESPACE_URI:?} = ns {ns})",
            server.endpoint()
        );
        Ok(server)
    }

    /// `opc.tcp://host:port/path`.
    pub fn endpoint(&self) -> String {
        format!(
            "opc.tcp://{}:{}{}",
            self.options.host(),
            self.options.port,
            self.options.path
        )
    }

    /// The port actually bound.
    pub fn port(&self) -> u16 {
        self.options.port
    }

    /// Index [`NAMESPACE_URI`] landed on.
    pub fn namespace_index(&self) -> u16 {
        self.ns
    }

    /// Sessions activated since this server started.
    ///
    /// A live reconfiguration must not reconnect, and this is how a test tells
    /// "resynchronised the subscription" from "tore the session down".
    pub fn sessions_activated(&self) -> u64 {
        self.activations.load(Ordering::SeqCst)
    }

    /// Sets or clears the Bad StatusCode on every tag that declares one.
    pub fn set_fault(&self, on: bool) {
        self.control.fault.store(on, Ordering::SeqCst);
        self.control.changed.notify_one();
    }

    /// Stops (or resumes) value updates.
    pub fn set_frozen(&self, frozen: bool) {
        self.control.frozen.store(frozen, Ordering::SeqCst);
    }

    /// Writes one value, as a PLC program would.
    pub fn write(&self, address: &str, value: Variant) -> Result<()> {
        let node = NodeId::new(self.ns, address);
        let dv = data_value(value, StatusCode::Good);
        self.manager
            .set_values(self.handle.subscriptions(), [(&node, None, dv)].into_iter())
            .map_err(|s| anyhow!("writing {address}: {s}"))
    }

    /// Stops serving and closes every connection.
    ///
    /// Returns the options with the port that was actually bound, so the same
    /// endpoint can be brought back with [`TestServer::start`] — which is the
    /// only honest way to test that a gateway reconnects on its own.
    pub async fn stop(mut self) -> Options {
        self.shutdown().await;
        log::info!("OPC UA test server on port {} stopped", self.options.port);
        self.options.clone()
    }

    async fn shutdown(&mut self) {
        self.tick_task.abort();
        self.handle.cancel();
        if let Some(task) = self.server_task.take() {
            if tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .is_err()
            {
                log::warn!("OPC UA test server did not stop within 5 s");
            }
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.tick_task.abort();
        self.handle.cancel();
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
    }
}

fn pki_dir() -> PathBuf {
    std::env::temp_dir().join("ergousha-opcua-test-server-pki")
}

fn data_value(value: Variant, status: StatusCode) -> DataValue {
    let now = DateTime::now();
    DataValue {
        value: Some(value),
        status: Some(status),
        source_timestamp: Some(now),
        server_timestamp: Some(now),
        ..Default::default()
    }
}

/// Creates the `Plant` folder and one variable per present tag.
fn populate(manager: &SimpleNodeManager, ns: u16, fault: bool) {
    use opcua_nodes::Variable;

    let mut space = manager.address_space().write();
    let folder = NodeId::new(ns, "Plant");
    space.add_folder(
        &folder,
        "Plant",
        "Plant",
        &NodeId::from(ObjectId::ObjectsFolder),
    );

    for tag in catalogue::present() {
        // A STRING NodeId, because that is what a real SCADA export looks like
        // and what the bundle's `id_type: "s"` produces.
        let node = NodeId::new(ns, tag.address);
        let mut variable = Variable::new(&node, tag.address, tag.address, (tag.value)(0));
        if let (Some(bad), true) = (tag.bad_status, fault) {
            variable.set_data_value(data_value((tag.value)(0), bad));
        }
        space.add_variables(vec![variable], &folder);
        log::debug!("  ns={ns};s={:<20} {}", tag.address, tag.proves);
    }
    for address in catalogue::missing_addresses() {
        log::debug!("  (not created) {address:<20} -> expected to fail");
    }
}

/// Steps every changing tag and applies fault transitions.
async fn tick_loop(
    manager: Arc<SimpleNodeManager>,
    handle: ServerHandle,
    control: Arc<Control>,
    ns: u16,
    period: Duration,
) {
    let changing: Vec<(NodeId, Tag)> = catalogue::present()
        .into_iter()
        .filter(|t| t.changes)
        .map(|t| (NodeId::new(ns, t.address), t))
        .collect();
    let faulty: Vec<(NodeId, Tag)> = catalogue::present()
        .into_iter()
        .filter(|t| t.bad_status.is_some())
        .map(|t| (NodeId::new(ns, t.address), t))
        .collect();

    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await; // the first tick is immediate
    let mut tick = 0u64;
    let mut fault = control.fault.load(Ordering::SeqCst);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if control.frozen.load(Ordering::SeqCst) {
                    continue;
                }
                tick += 1;
                let updates = changing
                    .iter()
                    .map(|(node, tag)| (node, None, data_value((tag.value)(tick), StatusCode::Good)));
                if let Err(e) = manager.set_values(handle.subscriptions(), updates) {
                    log::warn!("value update failed: {e}");
                }
            }
            _ = control.changed.notified() => {
                let now = control.fault.load(Ordering::SeqCst);
                if now == fault {
                    continue;
                }
                fault = now;
                // Written only on a transition: the server would not notify an
                // identical value and status anyway, and rewriting it every
                // tick would hide whether the gateway reports by exception.
                let updates = faulty.iter().map(|(node, tag)| {
                    let dv = match (fault, tag.bad_status) {
                        (true, Some(bad)) => data_value((tag.value)(0), bad),
                        _ => data_value(Variant::Double(FAULT_CLEARED_VALUE), StatusCode::Good),
                    };
                    (node, None, dv)
                });
                if let Err(e) = manager.set_values(handle.subscriptions(), updates) {
                    log::warn!("fault update failed: {e}");
                }
                log::info!("fault {}", if fault { "injected" } else { "cleared" });
            }
        }
    }
}

/// The default authenticator, counting anonymous activations.
struct CountingAuthenticator {
    inner: DefaultAuthenticator,
    activations: Arc<AtomicU64>,
}

#[async_trait]
impl AuthManager for CountingAuthenticator {
    async fn authenticate_anonymous_token(&self, endpoint: &ServerEndpoint) -> Result<(), Error> {
        self.inner.authenticate_anonymous_token(endpoint).await?;
        self.activations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn user_token_policies(&self, endpoint: &ServerEndpoint) -> Vec<UserTokenPolicy> {
        self.inner.user_token_policies(endpoint)
    }

    fn effective_user_access_level(
        &self,
        token: &UserToken,
        user_access_level: AccessLevel,
        node_id: &NodeId,
    ) -> AccessLevel {
        self.inner
            .effective_user_access_level(token, user_access_level, node_id)
    }
}
