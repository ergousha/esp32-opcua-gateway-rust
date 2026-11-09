//! Normal operation.
//!
//! One MQTT connection carries three planes:
//!
//! * **config** — the `opcua` named shadow plus the retained tag bundle
//!   ([`crate::shadow`]),
//! * **control** — AWS IoT Jobs, i.e. OTA ([`crate::jobs`]),
//! * **data** — batched OPC UA samples ([`publisher`]).
//!
//! The OPC UA client (`gateway-opcua`) runs on its own thread with its own
//! tokio runtime, and talks to this loop only through its [`Client`] handle —
//! a bounded sample queue and a command channel. That isolation is deliberate:
//! an unreachable PLC must never be able to stall the path that delivers a
//! firmware update.

pub mod publisher;

use std::ffi::CStr;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use gateway_core::health::DriverState;
use gateway_opcua::Client;

use crate::device_id::{self, DeviceIdentity};
use crate::jobs::JobsClient;
use crate::mqtt_util::{self, MqttEvent};
use crate::settings_store::SettingsStore;
use crate::shadow::ConfigPlane;
use crate::{config, ota};

use publisher::Publisher;

/// Samples moved out of the queue per loop turn. Caps the worst-case time this
/// loop spends away from the MQTT event channel.
const DRAIN_CHUNK: usize = 128;

/// Stack for the OPC UA thread.
///
/// Measured on hardware with [`StackProbe`]; see follow-up F3 in the
/// requirements. The connect path once peaked near 40 KiB because a spawned
/// ~16 KB future was moved by value (fixed in `gateway_opcua::session::open`).
const OPCUA_STACK_BYTES: usize = 40 * 1024;

/// FreeRTOS name of the OPC UA task; unnamed std threads all show as "pthread".
const OPCUA_TASK_NAME: &CStr = c"opcua";

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

    let opcua = start_opcua();

    let jobs = JobsClient::new(&id.thing_name);
    let mut config_plane = ConfigPlane::new(&id.thing_name, opcua.clone());
    let mut store = SettingsStore::new().context("opening the OPC UA settings store")?;

    config_plane.bootstrap(&store);
    jobs.start(&mut session.client)?;
    config_plane.start(&mut session.client)?;

    let mut publisher: Option<Publisher> = None;
    let mut last_tick = 0i64;
    let mut stack_probe = StackProbe::new();

    loop {
        match session
            .events
            .recv_timeout(Duration::from_millis(TICK_MS as u64))
        {
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
                    config_plane.handle(&topic, &data, &mut session.client, &mut store);
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
            publisher = Some(Publisher::new(&settings.telemetry));
        }

        if let Some(publisher) = publisher.as_mut() {
            publisher.ingest(opcua.drain(DRAIN_CHUNK), now);
            publisher.tick(&mut session.client, now);
        }

        update_counters(&opcua, publisher.as_ref());
        config_plane.report(&mut session.client, now);
        stack_probe.check();
    }
}

/// Logs how much of the OPC UA task's stack has never been touched, whenever
/// that reaches a new low.
struct StackProbe {
    task: esp_idf_svc::sys::TaskHandle_t,
    low: u32,
}

impl StackProbe {
    fn new() -> Self {
        // Null if the thread did not start; `check` is then a no-op.
        let task = unsafe { esp_idf_svc::sys::xTaskGetHandle(OPCUA_TASK_NAME.as_ptr()) };
        Self {
            task,
            low: u32::MAX,
        }
    }

    fn check(&mut self) {
        if self.task.is_null() {
            return;
        }
        // Sound: the task lives as long as `run` holds its `Client`.
        let free = unsafe { esp_idf_svc::sys::uxTaskGetStackHighWaterMark(self.task) };
        if free < self.low {
            self.low = free;
            log::info!("OPC UA stack headroom: {free} of {OPCUA_STACK_BYTES} B never used");
        }
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

/// Starts the OPC UA client on its own thread.
///
/// A dedicated thread rather than a task: `esp-mqtt` runs its callback on its
/// own task with a small stack, and the OPC UA stack needs far more room than
/// that callback can offer.
///
/// A client that cannot start does not take the device down with it: MQTT,
/// Jobs and OTA keep running so a fixed image can still be delivered, and the
/// failure is put in the shadow's `last_error` instead of surfacing only as an
/// absence of data (`docs/OPCUA_INTEGRATION_TEST.md` §8.2).
fn start_opcua() -> Client {
    let mut options = gateway_opcua::Options::new(env!("CARGO_PKG_VERSION"));
    // A per-device backoff seed keeps a whole fleet from reconnecting in
    // lockstep after a server restart.
    options.backoff_seed = backoff_seed();
    options.clock = Arc::new(now_ms);

    let (client, driver) = gateway_opcua::new(options);
    let started = register_eventfd().and_then(|()| {
        let named = ThreadSpawnConfiguration {
            name: Some(OPCUA_TASK_NAME),
            stack_size: OPCUA_STACK_BYTES,
            ..Default::default()
        };
        if let Err(e) = named.set() {
            log::warn!("could not name the OPC UA thread: {e}");
        }
        let heap = InternalHeap::now();
        let spawned = gateway_opcua::spawn_thread(driver, "opcua", OPCUA_STACK_BYTES)
            .map(|_| heap)
            .context("starting the OPC UA thread");
        // The configuration is per calling thread; later spawns must not inherit the name.
        if let Err(e) = ThreadSpawnConfiguration::default().set() {
            log::warn!("could not reset the thread spawn configuration: {e}");
        }
        spawned
    });
    match started {
        Ok(heap_before) => log_stack_placement(heap_before),
        Err(e) => {
            log::error!("OPC UA is unavailable: {e:#}");
            client.update_reported(|r| {
                r.state = DriverState::Error;
                r.set_error(format!("OPC UA did not start: {e:#}"));
            });
        }
    }
    client
}

/// Free internal heap, in total and as its largest block: what a thread stack
/// is carved from.
#[derive(Clone, Copy)]
struct InternalHeap {
    free: usize,
    largest_block: usize,
}

impl InternalHeap {
    fn now() -> Self {
        use esp_idf_svc::sys::{
            heap_caps_get_free_size, heap_caps_get_largest_free_block, MALLOC_CAP_INTERNAL,
        };

        // Safe: read-only queries of the allocator.
        unsafe {
            Self {
                free: heap_caps_get_free_size(MALLOC_CAP_INTERNAL),
                largest_block: heap_caps_get_largest_free_block(MALLOC_CAP_INTERNAL),
            }
        }
    }
}

/// Logs, once at boot, what stack the OPC UA task actually got, where it is,
/// and what starting the thread cost the internal heap.
///
/// The stack is a block of internal heap, and FreeRTOS only notices an
/// overflow by a changed canary at its low end, which the heap block below it
/// can overrun just as well. A 48 KiB stack that "overflowed" on the first
/// connect where 40 KiB did not is unexplained; this is the first thing to
/// look at when it is tried again.
fn log_stack_placement(before: InternalHeap) {
    use esp_idf_svc::sys::{heap_caps_get_allocated_size, pxTaskGetStackStart, xTaskGetHandle};

    let after = InternalHeap::now();
    // Null if the thread has already gone, or was not named.
    let task = unsafe { xTaskGetHandle(OPCUA_TASK_NAME.as_ptr()) };
    if task.is_null() {
        return;
    }
    // Sound: the task lives as long as its `Client`, held here, and FreeRTOS
    // allocated its stack as one heap block, as `heap_caps_get_allocated_size`
    // requires.
    let (start, size) = unsafe {
        let start = pxTaskGetStackStart(task);
        (start, heap_caps_get_allocated_size(start.cast()))
    };
    log::info!(
        "OPC UA stack: {size} B at {start:p}..{:p} ({OPCUA_STACK_BYTES} B requested); \
         internal heap {} -> {} B free, largest block {} -> {} B",
        start.wrapping_add(size),
        before.free,
        after.free,
        before.largest_block,
        after.largest_block,
    );
}

fn update_counters(opcua: &Client, publisher: Option<&Publisher>) {
    let (dropped, coalesced) = opcua.queue_counters();
    let dropped = dropped + publisher.map_or(0, |p| p.dropped_batches());
    opcua.update_reported(|r| {
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
