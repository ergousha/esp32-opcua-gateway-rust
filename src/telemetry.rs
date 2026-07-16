//! Normal calisma: cihaz kendi (provisioned) kimligiyle IoT Core'a baglanip
//! periyodik telemetri yayinlar. IoT Rule bunu CloudWatch Logs'a dusurur.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::json;

use crate::config;
use crate::device_id::{self, DeviceIdentity};
use crate::mqtt_util::{self, MqttEvent, QOS1};

/// Cihaz kimligiyle baglanip sonsuz telemetri dongusune girer.
pub fn run(id: &DeviceIdentity) -> Result<()> {
    let topic = format!(
        "{}/{}/data",
        config::TELEMETRY_TOPIC_PREFIX,
        id.thing_name
    );
    log::info!(
        "Telemetri modu. thing={} topic={topic}",
        id.thing_name
    );

    let mut session = mqtt_util::connect(
        &config::mqtt_url(),
        &id.thing_name, // client_id == thingName (device policy boyle sinirliyor)
        &mqtt_util::Creds {
            root_ca: device_id::root_ca_pem(),
            client_cert: &id.cert_pem,
            private_key: &id.key_pem,
        },
    )?;

    // Baglantiyi bekle.
    loop {
        match session.events.recv_timeout(Duration::from_secs(30)) {
            Ok(MqttEvent::Connected) => break,
            Ok(MqttEvent::Disconnected) => bail!("cihaz baglantisi koptu"),
            Ok(_) => continue,
            Err(_) => bail!("cihaz baglanti zaman asimi"),
        }
    }
    log::info!("Cihaz kimligiyle baglanildi. Telemetri gonderiliyor.");

    let mut seq: u64 = 0;
    loop {
        seq += 1;
        let payload = json!({
            "thing": id.thing_name,
            "seq": seq,
            "uptime_ms": unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1000,
            "heap_free": unsafe { esp_idf_svc::sys::esp_get_free_heap_size() },
            // Gercek OPC UA/sensor verisi buraya baglanacak.
            "note": "opc-ua gateway telemetry placeholder",
        })
        .to_string();

        match session
            .client
            .publish(&topic, QOS1, false, payload.as_bytes())
        {
            Ok(_) => log::info!("telemetri gonderildi seq={seq}"),
            Err(e) => log::warn!("telemetri gonderilemedi: {e}"),
        }

        // Arka planda gelen olaylari ( or. disconnect) bosalt.
        while let Ok(ev) = session.events.try_recv() {
            if matches!(ev, MqttEvent::Disconnected) {
                log::warn!("baglanti koptu; esp-mqtt otomatik yeniden baglanacak.");
            }
        }

        sleep(Duration::from_millis(config::TELEMETRY_INTERVAL_MS as u64));
    }
}

/// Provisioning + telemetri arasindaki gecis noktasi (ileride komut/OTA icin
/// genisletilebilir). Su an sadece telemetri.
#[allow(dead_code)]
pub fn context_note() -> &'static str {
    "cmd/<thing>/* topic'leri device policy'de subscribe icin acik"
}
