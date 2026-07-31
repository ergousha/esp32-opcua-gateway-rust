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
//!   moment a server comes back.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gateway_core::backoff::Backoff;
use gateway_core::batcher::Sample;
use gateway_core::bundle::TagSpec;
use gateway_core::diff;
use gateway_core::health::{DriverState, FailedTag};
use gateway_core::node::resolve_namespace;
use gateway_core::plan::{self, MAX_ITEMS_PER_REQUEST};
use gateway_core::value::is_good;
use tokio::sync::mpsc::UnboundedReceiver;

use super::session::{Connection, NowFn, SampleSink};
use super::{AppliedConfig, Command, Shared};

/// How often the running state is re-checked for a dead session.
const LIVENESS_POLL: Duration = Duration::from_millis(500);

/// Runs the driver until `commands` is closed. Never returns otherwise.
pub async fn run(
    shared: Arc<Shared>,
    mut commands: UnboundedReceiver<Command>,
    now: NowFn,
    backoff_seed: u32,
) {
    let mut backoff = Backoff::default_schedule(backoff_seed);
    let mut desired: Option<AppliedConfig> = None;
    let mut running: Option<Session> = None;

    loop {
        // A newer configuration always wins over whatever is in flight.
        match next_command(&mut commands, running.is_some() || desired.is_some()).await {
            Some(Command::Apply(config)) => {
                desired = Some(*config);
                backoff.reset();
            }
            Some(Command::Disable) => {
                log::info!("OPC UA disabled by configuration");
                desired = None;
                if let Some(session) = running.take() {
                    session.connection.shutdown().await;
                }
                set_state(&shared, DriverState::Idle);
                continue;
            }
            None if commands.is_closed() => return,
            None => {}
        }

        let Some(config) = desired.clone() else {
            set_state(&shared, DriverState::Idle);
            continue;
        };

        // Drop a session whose event loop has died before doing anything else.
        if running.as_ref().is_some_and(|s| !s.connection.is_alive()) {
            log::warn!("OPC UA session lost; reconnecting");
            running = None;
            shared.with_reported(|r| r.set_error("session lost"));
        }

        let outcome = match running.take() {
            Some(session) => resync(&shared, session, &config, &now).await,
            None => connect_and_sync(&shared, &config, &now).await,
        };

        match outcome {
            Ok(session) => {
                running = Some(session);
                backoff.reset();
                set_state(&shared, DriverState::Running);
                tokio::time::sleep(LIVENESS_POLL).await;
            }
            Err(e) => {
                let delay = backoff.next_delay_ms();
                log::error!(
                    "OPC UA sync failed (attempt {}): {e:#}; retrying in {} ms",
                    backoff.attempt(),
                    delay
                );
                shared.with_reported(|r| {
                    r.state = DriverState::Error;
                    r.set_error(format!("{e:#}"));
                });
                // Sleeping in a `select!` keeps a config change from waiting
                // out a 60 s backoff.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                    cmd = commands.recv() => {
                        if let Some(cmd) = cmd {
                            apply_command(cmd, &mut desired, &mut running).await;
                            backoff.reset();
                        }
                    }
                }
            }
        }
    }
}

/// Waits for a command when idle, polls without blocking when there is work.
async fn next_command(
    commands: &mut UnboundedReceiver<Command>,
    have_work: bool,
) -> Option<Command> {
    if have_work {
        commands.try_recv().ok()
    } else {
        commands.recv().await
    }
}

async fn apply_command(
    cmd: Command,
    desired: &mut Option<AppliedConfig>,
    running: &mut Option<Session>,
) {
    match cmd {
        Command::Apply(config) => *desired = Some(*config),
        Command::Disable => {
            *desired = None;
            if let Some(session) = running.take() {
                session.connection.shutdown().await;
            }
        }
    }
}

/// A connected session together with the tag set it currently realises.
struct Session {
    connection: Connection,
    /// Tags actually created on the server, in bundle order.
    tags: Vec<TagSpec>,
    /// Config version these tags came from.
    cfg_version: u32,
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
/// resync always rebuilds the subscription rather than mutating it: a stale
/// map would misattribute values to the wrong tag, the worst failure mode a
/// gateway has.
fn make_sink(shared: Arc<Shared>, handles: HashMap<u32, String>) -> SampleSink {
    Arc::new(move |raw| {
        let Some(address) = handles.get(&raw.client_handle) else {
            // Can only happen if the server echoes a handle we never sent.
            log::debug!("dropping notification for unknown handle {}", raw.client_handle);
            return;
        };
        shared.with_queue(|q| {
            q.push(Sample {
                address: address.clone(),
                ts_ms: raw.ts_ms,
                value: raw.value.clone(),
                status: raw.status,
            })
        });
    })
}

async fn connect_and_sync(
    shared: &Arc<Shared>,
    config: &AppliedConfig,
    now: &NowFn,
) -> anyhow::Result<Session> {
    set_state(shared, DriverState::Connecting);

    let session_name = format!("esp32-gw-v{}", config.settings.cfg.v);
    let connection = Connection::connect(&config.settings.instance, &session_name).await?;

    match sync(shared, &connection, config, now, &[]).await {
        Ok(tags) => Ok(Session {
            connection,
            tags,
            cfg_version: config.settings.cfg.v,
        }),
        Err(e) => {
            // Do not leak the event loop task on a failed sync.
            connection.shutdown().await;
            Err(e)
        }
    }
}

async fn resync(
    shared: &Arc<Shared>,
    mut session: Session,
    config: &AppliedConfig,
    now: &NowFn,
) -> anyhow::Result<Session> {
    let plan = diff::diff(&session.tags, &config.tags);
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
    // items drift apart.
    let tags = sync(shared, &session.connection, config, now, &session.tags).await?;
    session.tags = tags;
    session.cfg_version = config.settings.cfg.v;
    Ok(session)
}

/// Creates every subscription and monitored item for `config`.
async fn sync(
    shared: &Arc<Shared>,
    connection: &Connection,
    config: &AppliedConfig,
    now: &NowFn,
    previous: &[TagSpec],
) -> anyhow::Result<Vec<TagSpec>> {
    set_state(shared, DriverState::Syncing);

    // Samples buffered under the previous configuration would be published
    // with the new `cfg.v` and silently mis-attributed.
    if !previous.is_empty() {
        shared.with_queue(|q| q.clear());
    }

    let instance = &config.settings.instance;
    let namespaces = match connection.namespace_array().await {
        Ok(ns) => ns,
        Err(e) => {
            // Not fatal: without the array we simply use the literal index.
            log::warn!("could not read NamespaceArray ({e:#}); using ns={}", instance.ns);
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
    let sink = make_sink(Arc::clone(shared), handles);

    let mut applied = 0usize;
    let mut failures: Vec<FailedTag> = Vec::new();

    for subscription in &plans {
        let subscription_id = connection
            .create_subscription(subscription.scan_rate_ms, Arc::clone(&sink), Arc::clone(now))
            .await?;

        for chunk in &subscription.chunks {
            let outcomes = connection.create_items(subscription_id, chunk).await?;
            for (item, outcome) in chunk.iter().zip(outcomes) {
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

    Ok(tags)
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
