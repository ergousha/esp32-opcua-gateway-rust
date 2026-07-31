//! Normal operation.
//!
//! One MQTT connection carries three planes:
//!
//! * **config** — the `opcua` named shadow plus the retained tag bundle
//!   ([`crate::shadow`]),
//! * **control** — AWS IoT Jobs, i.e. OTA ([`crate::jobs`]),
//! * **data** — batched OPC UA samples ([`publisher`]).
//!
//! The OPC UA stack runs on its own thread with its own tokio runtime, and
//! talks to this loop only through a bounded sample queue and a command
//! channel. That isolation is deliberate: an unreachable PLC must never be able
//! to stall the path that delivers a firmware update.

pub mod publisher;

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::device_id::{self, DeviceIdentity};
use crate::jobs::JobsClient;
use crate::mqtt_util::{self, MqttEvent};
use crate::opcua::{self, Shared};
use crate::settings_store::SettingsStore;
use crate::shadow::ConfigPlane;
use crate::{config, ota};

use publisher::Publisher;

/// Samples buffered between the OPC UA thread and this loop.
///
/// Two full sweeps of the 250-tag maximum. Larger buffers do not help: the
/// queue coalesces per address, so extra depth only adds staleness and heap.
const QUEUE_CAPACITY: usize = 500;

/// Samples moved out of the queue per loop turn. Caps the worst-case time this
/// loop spends away from the MQTT event channel.
const DRAIN_CHUNK: usize = 128;

/// Stack for the OPC UA thread.
///
/// The spike used 32 KiB, which does not cover the async-opcua chunk assembly
/// and tokio's task machinery on the same stack. This is generous rather than
/// measured; see follow-up F3 in the requirements.
const OPCUA_STACK_BYTES: usize = 40 * 1024;

/// How often the periodic work below runs, at most.
const TICK_MS: i64 = 50;

/// Connects with the device identity and runs until something unrecoverable
/// happens.
pub fn run(id: &DeviceIdentity) -> Result<()> {
    log::info!("starting gateway. thing={}", id.thing_name);

    let mut session = mqtt_util::connect(
        &config::mqtt_url(),
        // client_id == thingName; the device policy is written that way.
        &id.thing_name,
        &mqtt_util::Creds {
            root_ca: device_id::root_ca_pem(),
            client_cert: &id.cert_pem,
            private_key: &id.key_pem,
        },
    )?;

    wait_for_connection(&session)?;
    log::info!("connected to AWS IoT Core");

    // Only once we are on the network with a working identity is this image
    // worth keeping; before this point a rollback is the correct outcome.
    if let Err(e) = ota::mark_valid() {
        log::warn!("could not mark the firmware valid: {e:#}");
    }

    let shared = Arc::new(Shared::new(QUEUE_CAPACITY, env!("CARGO_PKG_VERSION")));
    let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();
    spawn_driver(Arc::clone(&shared), command_rx)?;

    let jobs = JobsClient::new(&id.thing_name);
    let mut config_plane = ConfigPlane::new(&id.thing_name, commands);
    let mut store = SettingsStore::new()
        .context("opening the OPC UA settings store")?;

    config_plane.bootstrap(&store);
    jobs.start(&mut session.client)?;
    config_plane.start(&mut session.client)?;

    let mut publisher: Option<Publisher> = None;
    let mut last_tick = 0i64;

    loop {
        match session.events.recv_timeout(Duration::from_millis(TICK_MS as u64)) {
            Ok(MqttEvent::Connected) => {
                // Subscriptions do not survive a dropped session, and the
                // shadow response is not retained, so both planes restart.
                log::info!("MQTT (re)connected; restoring subscriptions");
                if let Err(e) = jobs.start(&mut session.client) {
                    log::error!("could not restore Jobs subscriptions: {e:#}");
                }
                if let Err(e) = config_plane.start(&mut session.client) {
                    log::error!("could not restore shadow subscriptions: {e:#}");
                }
            }
            Ok(MqttEvent::Disconnected) => {
                log::warn!("MQTT connection lost; esp-mqtt will reconnect");
            }
            Ok(MqttEvent::Message { topic, data }) => {
                if jobs.owns(&topic) {
                    jobs.handle(&topic, &data, &mut session.client);
                } else {
                    config_plane.handle(
                        &topic,
                        &data,
                        &mut session.client,
                        &mut store,
                        &shared,
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                bail!("MQTT event channel closed");
            }
        }

        let now = now_ms();
        if now.saturating_sub(last_tick) < TICK_MS {
            continue;
        }
        last_tick = now;

        if let Some(settings) = config_plane.take_fresh_settings() {
            // Flush under the old policy first, otherwise samples collected
            // for the previous version go out stamped with the new one.
            if let Some(mut old) = publisher.take() {
                old.flush(&mut session.client, now);
            }
            log::info!(
                "telemetry -> {} (qos {}, <={} items, <={} B, <={} ms)",
                settings.telemetry.topic,
                settings.telemetry.qos,
                settings.telemetry.batch_max_items,
                settings.telemetry.batch_max_bytes,
                settings.telemetry.batch_max_age_ms
            );
            publisher = Some(Publisher::new(&settings.telemetry, settings.cfg.v));
        }

        if let Some(publisher) = publisher.as_mut() {
            let samples = shared.with_queue(|q| q.drain(DRAIN_CHUNK));
            publisher.ingest(samples, now);
            publisher.tick(&mut session.client, now);
        }

        update_counters(&shared, publisher.as_ref());
        config_plane.report(&mut session.client, &shared, now);
    }
}

fn wait_for_connection(session: &mqtt_util::MqttSession) -> Result<()> {
    loop {
        match session.events.recv_timeout(Duration::from_secs(30)) {
            Ok(MqttEvent::Connected) => return Ok(()),
            Ok(MqttEvent::Disconnected) => bail!("connection lost before it was established"),
            Ok(_) => continue,
            Err(_) => bail!("timed out connecting to AWS IoT Core"),
        }
    }
}

/// Registers ESP-IDF's eventfd VFS driver, which Tokio's I/O driver requires.
///
/// Tokio wakes its reactor through an `eventfd`. On ESP-IDF that syscall is not
/// available until the eventfd VFS has been registered: `vfs_eventfd.c` returns
/// `EACCES` while its VFS id is still -1, so without this call
/// `Runtime::build()` fails with "Permission denied (os error 13)" and the whole
/// OPC UA thread dies before it ever opens a session — silently, because the
/// MQTT/OTA path keeps running.
///
/// Two descriptors: one for the current-thread runtime's reactor waker, plus a
/// spare so a future second runtime does not reintroduce the same failure.
fn register_eventfd() -> Result<()> {
    use esp_idf_svc::sys::{esp, esp_vfs_eventfd_config_t, esp_vfs_eventfd_register};

    let config = esp_vfs_eventfd_config_t { max_fds: 2 };
    // Safe: `config` outlives the call, and the driver copies what it needs.
    esp!(unsafe { esp_vfs_eventfd_register(&config) })
        .context("registering the eventfd VFS driver for Tokio")
}

/// Starts the OPC UA thread.
///
/// A dedicated thread rather than a task: `esp-mqtt` runs its callback on its
/// own task with a small stack, and the OPC UA stack needs far more room than
/// that callback can offer.
fn spawn_driver(
    shared: Arc<Shared>,
    commands: tokio::sync::mpsc::UnboundedReceiver<opcua::Command>,
) -> Result<()> {
    register_eventfd()?;

    std::thread::Builder::new()
        .name("opcua".into())
        .stack_size(OPCUA_STACK_BYTES)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("could not start the OPC UA runtime: {e}");
                    return;
                }
            };
            let now = Arc::new(now_ms) as opcua::session::NowFn;
            // A per-device backoff seed keeps a whole fleet from reconnecting
            // in lockstep after a server restart.
            let seed = backoff_seed();
            runtime.block_on(opcua::driver::run(shared, commands, now, seed));
            log::warn!("OPC UA driver exited");
        })
        .context("spawning the OPC UA thread")?;
    Ok(())
}

fn update_counters(shared: &Shared, publisher: Option<&Publisher>) {
    let (dropped, coalesced) = shared.with_queue(|q| (q.dropped(), q.coalesced()));
    let dropped = dropped + publisher.map_or(0, |p| p.dropped_batches());
    shared.with_reported(|r| {
        r.dropped = dropped;
        r.coalesced = coalesced;
        r.uptime_s = uptime_s();
    });
}

/// Wall-clock milliseconds since the epoch.
///
/// Only used as a fallback: a sample is stamped with the server's source or
/// server timestamp whenever the server provides one, because the device has
/// no reliable clock of its own.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn uptime_s() -> u64 {
    (unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1_000_000) as u64
}

fn backoff_seed() -> u32 {
    let mut mac = [0u8; 6];
    unsafe {
        esp_idf_svc::sys::esp_read_mac(
            mac.as_mut_ptr(),
            esp_idf_svc::sys::esp_mac_type_t_ESP_MAC_WIFI_STA,
        );
    }
    u32::from_le_bytes([mac[2], mac[3], mac[4], mac[5]])
}
