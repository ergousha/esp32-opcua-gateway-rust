//! Normal operation.
//!
//! One MQTT connection carries three planes:
//!
//! * **config** — the `opcua` named shadow plus the retained tag bundle
//!   ([`crate::shadow`]),
//! * **control** — AWS IoT Jobs (OTA) and the `platform` shadow, which the
//!   platform SDK's [`Session`] handles itself,
//! * **data** — batched OPC UA samples ([`publisher`]).
//!
//! The OPC UA client (`gateway-opcua`) runs on its own thread with its own
//! tokio runtime, and talks to this loop only through its [`Client`] handle —
//! a bounded sample queue and a command channel. That isolation is deliberate:
//! an unreachable PLC must never be able to stall the path that delivers a
//! firmware update.

pub mod publisher;

use std::ffi::CStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use device_sdk::{Event, Session};
use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use gateway_core::health::DriverState;
use gateway_opcua::Client;

use crate::settings_store::SettingsStore;
use crate::shadow::ConfigPlane;

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

/// Runs the gateway on `session` until something unrecoverable happens.
///
/// The session handles Jobs (OTA) and the `platform` shadow itself; what
/// reaches this loop is the gateway's own: the `opcua` shadow and the tag
/// bundle.
pub fn run(mut session: Session) -> Result<()> {
    log::info!("starting gateway. thing={}", session.thing_name());

    let opcua = start_opcua();
    let mut config_plane = ConfigPlane::new(session.thing_name(), opcua.clone());
    let mut store = SettingsStore::new().context("opening the OPC UA settings store")?;

    config_plane.bootstrap(&store);
    config_plane.start(&mut session.client())?;

    let mut publisher: Option<Publisher> = None;
    let mut last_tick = 0i64;
    let mut stack_probe = StackProbe::new();
    let mut telemetry_on = false;

    loop {
        match session.poll(Duration::from_millis(TICK_MS as u64)) {
            Some(Event::Connected) => {
                // Subscriptions do not survive a dropped session, and the
                // shadow response is not retained, so the plane restarts.
                if let Err(e) = config_plane.start(&mut session.client()) {
                    log::error!("could not restore shadow subscriptions: {e:#}");
                }
            }
            Some(Event::Disconnected) => {
                log::warn!("MQTT connection lost; the platform SDK reconnects");
            }
            Some(Event::Message { topic, data }) => {
                config_plane.handle(&topic, &data, &mut session.client(), &mut store);
            }
            Some(Event::Restart) => device_sdk::restart_into_update(),
            None => {}
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
                old.flush(&mut session.client(), now);
            }
            let telemetry = settings.telemetry.effective();
            log::info!(
                "telemetry -> {} (qos {}, <={} items, <={} B, <={} ms)",
                telemetry.topic,
                telemetry.qos,
                telemetry.batch_max_items,
                telemetry.batch_max_bytes,
                telemetry.batch_max_age_ms
            );
            publisher = Some(Publisher::new(&telemetry));
        }

        // Telemetry is opt-in: the platform switches it on (`platform` shadow,
        // telemetry.enabled) only when a cloud rule consumes it. Off, samples
        // are drained and dropped, so the queue never backs up.
        if session.telemetry_enabled() != telemetry_on {
            telemetry_on = session.telemetry_enabled();
            log::info!("telemetry {}", if telemetry_on { "on" } else { "off" });
        }
        let samples = opcua.drain(DRAIN_CHUNK);
        if let (true, Some(publisher)) = (telemetry_on, publisher.as_mut()) {
            publisher.ingest(samples, now);
            publisher.tick(&mut session.client(), now);
        }

        update_counters(&opcua, publisher.as_ref());
        config_plane.report(&mut session.client(), now);
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
    let mut options = gateway_opcua::Options::new(env!("FW_VERSION"));
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
