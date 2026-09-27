//! The OPC UA driver: a state machine that keeps the server subscription in
//! sync with whatever the cloud last asked for.
//!
//! ```text
//!            ┌──────┐  config arrives   ┌────────────┐  session up  ┌─────────┐
//!            │ Idle │──────────────────▶│ Connecting │─────────────▶│ Syncing │
//!            └──────┘                   └────────────┘              └─────────┘
//!                ▲                            ▲                          │
//!    disable     │                            │ backoff elapsed          │ items created
//!                │                       ┌─────────┐                     ▼
//!                └───────────────────────│  Error  │◀───────────────┌─────────┐
//!                                        └─────────┘  session lost  │ Running │
//!                                                                   └─────────┘
//! ```
//!
//! Design constraints:
//!
//! * This task must never panic the process and must never block the MQTT/OTA
//!   task. Every fallible step is captured into `reported.last_error` and
//!   turned into a backoff, never a `panic!` or an early `return`.
//! * Reconnects use exponential backoff with jitter (1 s → 60 s). The spike's
//!   flat 5 s retry turns a fleet into a synchronised connection storm the
//!   moment a server comes back. This is the *only* retry loop: the library's
//!   own is switched off (see `session.rs`), because two of them fighting is
//!   what hid dead servers and wedged disables on hardware.
//! * Steady state is event-driven: the driver sleeps until a command arrives or
//!   the session ends, and reacts to either at once.
//! * A newer configuration always wins over whatever is in flight.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use gateway_core::backoff::Backoff;
use gateway_core::batcher::Sample;
use gateway_core::bundle::TagSpec;
use gateway_core::diff;
use gateway_core::health::{DriverState, FailedTag};
use gateway_core::node::resolve_namespace;
use gateway_core::plan::{self, MAX_ITEMS_PER_REQUEST};
use gateway_core::settings::InstanceSettings;
use gateway_core::value::is_good;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::session::{Connection, SampleSink};
use crate::{AppliedConfig, Clock, Command, Shared};

/// Runs the driver until `commands` is closed.
pub(crate) async fn run(
    shared: Arc<Shared>,
    mut commands: UnboundedReceiver<Command>,
    clock: Clock,
    backoff_seed: u32,
) {
    let mut backoff = Backoff::default_schedule(backoff_seed);
    let mut desired: Option<AppliedConfig> = None;
    let mut running: Option<Session> = None;

    loop {
        let Some(config) = desired.clone() else {
            if let Some(session) = running.take() {
                session.close().await;
            }
            set_state(&shared, DriverState::Idle);
            match commands.recv().await {
                Some(cmd) => {
                    apply(cmd, &mut desired, &mut commands);
                    backoff.reset();
                    continue;
                }
                None => return,
            }
        };

        let outcome = match running.take() {
            Some(session) => resync(&shared, session, &config, &clock).await,
            None => connect_and_sync(&shared, &config, &clock).await,
        };

        let failure = match outcome {
            Ok(mut session) => {
                backoff.reset();
                set_state(&shared, DriverState::Running);

                tokio::select! {
                    cmd = commands.recv() => match cmd {
                        Some(cmd) => {
                            apply(cmd, &mut desired, &mut commands);
                            running = Some(session);
                            continue;
                        }
                        None => {
                            session.close().await;
                            return;
                        }
                    },
                    reason = session.connection.closed() => {
                        session.close().await;
                        anyhow!("session lost: {reason}")
                    }
                }
            }
            Err(e) => e,
        };

        let delay = backoff.next_delay_ms();
        log::error!(
            "OPC UA failure (attempt {}): {failure:#}; retrying in {delay} ms",
            backoff.attempt()
        );
        set_state(&shared, DriverState::Error);
        shared.with_reported(|r| r.set_error(format!("{failure:#}")));

        // Sleeping in a `select!` keeps a config change from waiting out a
        // 60 s backoff.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
            cmd = commands.recv() => match cmd {
                Some(cmd) => {
                    apply(cmd, &mut desired, &mut commands);
                    backoff.reset();
                }
                None => return,
            }
        }
    }
}

/// Records `cmd`, then any commands queued behind it: only the last one counts.
fn apply(
    cmd: Command,
    desired: &mut Option<AppliedConfig>,
    commands: &mut UnboundedReceiver<Command>,
) {
    let mut next = Some(cmd);
    while let Some(cmd) = next {
        match cmd {
            Command::Apply(config) => *desired = Some(*config),
            Command::Disable => {
                log::info!("OPC UA disabled by configuration");
                *desired = None;
            }
        }
        next = commands.try_recv().ok();
    }
}

/// A connected session together with what it currently realises.
struct Session {
    connection: Connection,
    /// Connection parameters the session was opened with.
    instance: InstanceSettings,
    /// Config version the subscriptions belong to.
    cfg_version: u32,
    synced: Synced,
}

/// The subscriptions created for one configuration.
struct Synced {
    /// The configuration's tags, as given — before any namespace renumbering,
    /// so that re-applying an identical configuration diffs as empty.
    tags: Vec<TagSpec>,
    /// Subscription ids, so a reconfiguration can delete them.
    subscriptions: Vec<u32>,
    /// Cleared when these subscriptions are retired; their sink checks it, so
    /// a notification already in flight cannot land under the new config.
    live: Arc<AtomicBool>,
}

impl Session {
    /// Ends the session, bounded (see [`Connection::shutdown`]).
    async fn close(self) {
        self.synced.live.store(false, Ordering::SeqCst);
        self.connection.shutdown().await;
    }
}

fn set_state(shared: &Shared, state: DriverState) {
    shared.with_reported(|r| {
        if r.state != state {
            log::info!("OPC UA driver: {} -> {}", r.state.as_str(), state.as_str());
            r.state = state;
        }
        if state == DriverState::Running {
            r.last_error = None;
        }
    });
}

/// Builds the sink that turns notifications into queued samples.
///
/// The handle-to-address map is captured by the closure, which is why a
/// resync always rebuilds the subscriptions rather than mutating them: a stale
/// map would misattribute values to the wrong tag, the worst failure mode a
/// gateway has.
fn make_sink(
    shared: Arc<Shared>,
    handles: HashMap<u32, String>,
    cfg_v: u32,
    live: Arc<AtomicBool>,
) -> SampleSink {
    Arc::new(move |raw| {
        if !live.load(Ordering::SeqCst) {
            return;
        }
        let Some(address) = handles.get(&raw.client_handle) else {
            // Can only happen if the server echoes a handle we never sent.
            log::debug!(
                "dropping notification for unknown handle {}",
                raw.client_handle
            );
            return;
        };
        shared.with_queue(|q| {
            q.push(Sample {
                address: address.clone(),
                ts_ms: raw.ts_ms,
                value: raw.value,
                status: raw.status,
                cfg_v,
            })
        });
    })
}

async fn connect_and_sync(
    shared: &Arc<Shared>,
    config: &AppliedConfig,
    clock: &Clock,
) -> anyhow::Result<Session> {
    set_state(shared, DriverState::Connecting);

    let session_name = format!("esp32-gw-v{}", config.settings.cfg.v);
    let connection = Connection::connect(&config.settings.instance, &session_name).await?;

    match sync(shared, &connection, config, clock).await {
        Ok(synced) => Ok(Session {
            connection,
            instance: config.settings.instance.clone(),
            cfg_version: config.settings.cfg.v,
            synced,
        }),
        Err(e) => {
            // Never leave an event loop running behind a failed sync.
            connection.shutdown().await;
            Err(e)
        }
    }
}

async fn resync(
    shared: &Arc<Shared>,
    session: Session,
    config: &AppliedConfig,
    clock: &Clock,
) -> anyhow::Result<Session> {
    if session.instance != config.settings.instance {
        // A different endpoint (or session parameters) is a different
        // session; resynchronising the old one would keep reading the old PLC.
        log::info!(
            "config v{} -> v{}: connection settings changed; reconnecting",
            session.cfg_version,
            config.settings.cfg.v
        );
        session.close().await;
        return connect_and_sync(shared, config, clock).await;
    }

    let plan = diff::diff(&session.synced.tags, &config.tags);
    if plan.is_empty() && session.cfg_version == config.settings.cfg.v {
        return Ok(session);
    }

    log::info!(
        "config v{} -> v{}: {} add, {} modify, {} remove",
        session.cfg_version,
        config.settings.cfg.v,
        plan.add.len(),
        plan.modify.len(),
        plan.remove.len()
    );

    // Subscriptions are rebuilt wholesale rather than edited in place. At 250
    // tags the extra service calls cost under a second, and it removes the
    // entire class of bugs where the handle map and the server's monitored
    // items drift apart. The old ones are deleted first: left in place they
    // keep reporting removed tags, and on the device they leak heap with every
    // reconfiguration.
    let Session {
        connection, synced, ..
    } = session;
    synced.live.store(false, Ordering::SeqCst);
    let rebuilt = match connection.delete_subscriptions(&synced.subscriptions).await {
        Ok(()) => sync(shared, &connection, config, clock).await,
        Err(e) => Err(e),
    };
    match rebuilt {
        Ok(synced) => Ok(Session {
            connection,
            instance: config.settings.instance.clone(),
            cfg_version: config.settings.cfg.v,
            synced,
        }),
        Err(e) => {
            connection.shutdown().await;
            Err(e)
        }
    }
}

/// Creates every subscription and monitored item for `config`.
async fn sync(
    shared: &Arc<Shared>,
    connection: &Connection,
    config: &AppliedConfig,
    clock: &Clock,
) -> anyhow::Result<Synced> {
    set_state(shared, DriverState::Syncing);

    let instance = &config.settings.instance;
    let namespaces = match connection.namespace_array().await {
        Ok(ns) => ns,
        Err(e) => {
            // Not fatal: without the array we simply use the literal index.
            log::warn!(
                "could not read NamespaceArray ({e:#}); using ns={}",
                instance.ns
            );
            Vec::new()
        }
    };
    let (ns, resolved) = resolve_namespace(instance.ns_uri.as_deref(), &namespaces, instance.ns);
    if instance.ns_uri.is_some() && !resolved {
        log::warn!(
            "ns_uri {:?} not published by the server; falling back to ns={ns}",
            instance.ns_uri
        );
    }

    // `TagSpec.node_id` was rendered with the settings' literal `ns`; if the
    // URI resolved to a different index the NodeIds must be re-rendered.
    let tags = if ns == instance.ns {
        config.tags.clone()
    } else {
        renumber(&config.tags, instance.ns, ns)
    };

    let plans = plan::plan(&tags, MAX_ITEMS_PER_REQUEST);
    let handles: HashMap<u32, String> = plans
        .iter()
        .flat_map(|p| p.chunks.iter().flatten())
        .map(|i| (i.client_handle, i.tag.address.clone()))
        .collect();
    let live = Arc::new(AtomicBool::new(true));
    let sink = make_sink(
        Arc::clone(shared),
        handles,
        config.settings.cfg.v,
        Arc::clone(&live),
    );

    let mut applied = 0usize;
    let mut failures: Vec<FailedTag> = Vec::new();
    let mut subscriptions = Vec::with_capacity(plans.len());

    for subscription in &plans {
        let subscription_id = connection
            .create_subscription(
                subscription.scan_rate_ms,
                Arc::clone(&sink),
                Arc::clone(clock),
            )
            .await?;
        subscriptions.push(subscription_id);

        for chunk in &subscription.chunks {
            // Addresses the server does not have are failed here, whatever the
            // server's own policy on subscribing to them (see `missing_nodes`).
            let missing = connection.missing_nodes(chunk).await;
            let mut present = Vec::with_capacity(chunk.len());
            for (item, missing) in chunk.iter().zip(missing) {
                match missing {
                    Some(status) => failures.push(FailedTag {
                        a: item.tag.address.clone(),
                        s: status,
                    }),
                    None => present.push(item.clone()),
                }
            }
            if present.is_empty() {
                continue;
            }

            let outcomes = connection.create_items(subscription_id, &present).await?;
            for (item, outcome) in present.iter().zip(outcomes) {
                if is_good(outcome.status) {
                    applied += 1;
                } else {
                    failures.push(FailedTag {
                        a: item.tag.address.clone(),
                        s: outcome.status,
                    });
                }
            }
        }
    }

    if applied == 0 && !tags.is_empty() {
        // Every single tag failed: almost certainly a wrong namespace or
        // id_type, which is worth surfacing as an error rather than as a
        // "running" state with no data.
        anyhow::bail!(
            "all {} monitored items were rejected (first status {:#010x})",
            tags.len(),
            failures.first().map(|f| f.s).unwrap_or(0)
        );
    }

    log::info!(
        "OPC UA synced: {} subscriptions, {applied} items applied, {} failed",
        plans.len(),
        failures.len()
    );

    shared.with_reported(|r| {
        r.cfg_v = config.settings.cfg.v;
        r.srv_publish_ms = instance.publish_ms;
        r.set_sync_outcome(applied, failures);
    });

    Ok(Synced {
        tags: config.tags.clone(),
        subscriptions,
        live,
    })
}

/// Re-renders NodeIds after `ns_uri` resolved to a different namespace index.
fn renumber(tags: &[TagSpec], from: u16, to: u16) -> Vec<TagSpec> {
    let old = format!("ns={from};");
    let new = format!("ns={to};");
    tags.iter()
        .map(|t| TagSpec {
            node_id: t.node_id.replacen(&old, &new, 1),
            ..t.clone()
        })
        .collect()
}
