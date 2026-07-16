//! AWS IoT Fleet Provisioning by Claim istemcisi.
//!
//! Akis:
//!   1. Claim (bootstrap) sertifikasiyla baglan.
//!   2. CreateKeysAndCertificate: benzersiz cihaz sertifikasi + anahtar + sahiplik
//!      token'i al.
//!   3. RegisterThing: MAC + secret ile provisioning template'i tetikle. Lambda
//!      pre-provisioning hook DynamoDB'de dogrular; onaylanirsa thing olusur.
//!   4. Elde edilen kalici kimligi dondur (cagiran NVS'ye yazar).

use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::config;
use crate::device_id::{self, DeviceIdentity};
use crate::mqtt_util::{self, MqttEvent, QOS1};

const CREATE_ACCEPTED: &str = "$aws/certificates/create/json/accepted";
const CREATE_REJECTED: &str = "$aws/certificates/create/json/rejected";
const CREATE_PUBLISH: &str = "$aws/certificates/create/json";

/// Claim kimligiyle baglanip tam provisioning akisini yurutur.
pub fn run() -> Result<DeviceIdentity> {
    let (mac_colon, mac_plain) = device_id::mac_addr();
    let client_id = format!("claim-{mac_plain}");
    log::info!("Provisioning basliyor. MAC={mac_colon} client_id={client_id}");

    let mut session = mqtt_util::connect(
        &config::mqtt_url(),
        &client_id,
        &mqtt_util::Creds {
            root_ca: device_id::root_ca_pem(),
            client_cert: device_id::claim_cert_pem(),
            private_key: device_id::claim_key_pem(),
        },
    )?;

    wait_connected(&session.events).context("claim baglantisi kurulamadi")?;
    log::info!("Claim kimligiyle baglanildi.");

    // 1) CreateKeysAndCertificate
    session.client.subscribe(CREATE_ACCEPTED, QOS1)?;
    session.client.subscribe(CREATE_REJECTED, QOS1)?;
    sleep(Duration::from_millis(1000)); // SUBACK icin kisa bekleme

    session
        .client
        .publish(CREATE_PUBLISH, QOS1, false, b"{}")
        .context("create keys istegi gonderilemedi")?;
    log::info!("CreateKeysAndCertificate istegi gonderildi.");

    let create_resp = wait_response(&session.events, "certificates/create/json")?;
    let cert_pem = create_resp["certificatePem"]
        .as_str()
        .ok_or_else(|| anyhow!("certificatePem yok"))?
        .to_string();
    let priv_key = create_resp["privateKey"]
        .as_str()
        .ok_or_else(|| anyhow!("privateKey yok"))?
        .to_string();
    let ownership_token = create_resp["certificateOwnershipToken"]
        .as_str()
        .ok_or_else(|| anyhow!("certificateOwnershipToken yok"))?
        .to_string();
    log::info!("Benzersiz cihaz sertifikasi alindi.");

    // 2) RegisterThing
    let tmpl = config::PROVISIONING_TEMPLATE;
    let register_accepted = format!("$aws/provisioning-templates/{tmpl}/provision/json/accepted");
    let register_rejected = format!("$aws/provisioning-templates/{tmpl}/provision/json/rejected");
    let register_publish = format!("$aws/provisioning-templates/{tmpl}/provision/json");

    session.client.subscribe(&register_accepted, QOS1)?;
    session.client.subscribe(&register_rejected, QOS1)?;
    sleep(Duration::from_millis(1000));

    let register_body = json!({
        "certificateOwnershipToken": ownership_token,
        "parameters": {
            "SerialNumber": mac_plain,
            "MacAddress": mac_colon,
            "Secret": config::DEVICE_SECRET,
        }
    })
    .to_string();

    session
        .client
        .publish(&register_publish, QOS1, false, register_body.as_bytes())
        .context("RegisterThing istegi gonderilemedi")?;
    log::info!("RegisterThing istegi gonderildi (MAC + secret dogrulaniyor).");

    let register_resp = wait_response(&session.events, "provision/json")?;
    let thing_name = register_resp["thingName"]
        .as_str()
        .ok_or_else(|| anyhow!("thingName yok"))?
        .to_string();
    log::info!("Provisioning ONAYLANDI. thingName={thing_name}");

    Ok(DeviceIdentity {
        cert_pem,
        key_pem: priv_key,
        thing_name,
    })
}

/// Connected olayini bekler (timeout'lu).
fn wait_connected(events: &std::sync::mpsc::Receiver<MqttEvent>) -> Result<()> {
    let deadline = Duration::from_secs(30);
    loop {
        match events.recv_timeout(deadline) {
            Ok(MqttEvent::Connected) => return Ok(()),
            Ok(MqttEvent::Disconnected) => bail!("baglanti koptu (TLS/cert hatasi?)"),
            Ok(MqttEvent::Message { .. }) => continue,
            Err(_) => bail!("baglanti zaman asimi (endpoint/policy/cert kontrol edin)"),
        }
    }
}

/// `accepted` veya `rejected` yaniti gelene kadar bekler.
/// `marker` beklenen topic'in ortak parcasi (accepted/rejected ayrimini yapar).
fn wait_response(events: &std::sync::mpsc::Receiver<MqttEvent>, marker: &str) -> Result<Value> {
    let deadline = Duration::from_secs(30);
    loop {
        match events.recv_timeout(deadline) {
            Ok(MqttEvent::Message { topic, data }) if topic.contains(marker) => {
                let parsed: Value = serde_json::from_slice(&data)
                    .with_context(|| format!("yanit JSON parse edilemedi: {topic}"))?;
                if topic.ends_with("/rejected") {
                    bail!("provisioning REDDEDILDI: {parsed}");
                }
                return Ok(parsed);
            }
            Ok(MqttEvent::Message { .. }) => continue, // baska topic
            Ok(MqttEvent::Connected) => continue,
            Ok(MqttEvent::Disconnected) => bail!("yanit beklerken baglanti koptu"),
            Err(_) => bail!("yanit zaman asimi ({marker})"),
        }
    }
}
