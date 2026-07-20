//! AWS IoT Fleet Provisioning by Claim client.
//!
//! Flow:
//!   1. Connect using Claim (bootstrap) certificate.
//!   2. CreateKeysAndCertificate: get unique device certificate + key + ownership token.
//!   3. RegisterThing: trigger provisioning template with MAC + secret. Lambda
//!      pre-provisioning hook validates against DynamoDB; if approved, thing is created.
//!   4. Return the obtained persistent identity (caller writes it to NVS).

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

/// Connects with the claim identity and executes the complete provisioning flow.
pub fn run() -> Result<DeviceIdentity> {
    let (mac_colon, mac_plain) = device_id::mac_addr();
    let client_id = format!("claim-{mac_plain}");
    log::info!("Provisioning starting. MAC={mac_colon} client_id={client_id}");

    let mut session = mqtt_util::connect(
        &config::mqtt_url(),
        &client_id,
        &mqtt_util::Creds {
            root_ca: device_id::root_ca_pem(),
            client_cert: device_id::claim_cert_pem(),
            private_key: device_id::claim_key_pem(),
        },
    )?;

    wait_connected(&session.events).context("claim connection failed")?;
    log::info!("Connected with claim identity.");

    // 1) CreateKeysAndCertificate
    session.client.subscribe(CREATE_ACCEPTED, QOS1)?;
    session.client.subscribe(CREATE_REJECTED, QOS1)?;
    sleep(Duration::from_millis(1000)); // Short wait for SUBACK

    session
        .client
        .publish(CREATE_PUBLISH, QOS1, false, b"{}")
        .context("failed to send create keys request")?;
    log::info!("CreateKeysAndCertificate request sent.");

    let create_resp = wait_response(&session.events, "certificates/create/json")?;
    let cert_pem = create_resp["certificatePem"]
        .as_str()
        .ok_or_else(|| anyhow!("certificatePem missing"))?
        .to_string();
    let priv_key = create_resp["privateKey"]
        .as_str()
        .ok_or_else(|| anyhow!("privateKey missing"))?
        .to_string();
    let ownership_token = create_resp["certificateOwnershipToken"]
        .as_str()
        .ok_or_else(|| anyhow!("certificateOwnershipToken missing"))?
        .to_string();
    log::info!("Unique device certificate received.");

    // 2) RegisterThing
    let tmpl = config::CONFIG.provisioning_template;
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
        .context("failed to send RegisterThing request")?;
    log::info!("RegisterThing request sent (validating MAC + secret).");

    let register_resp = wait_response(&session.events, "provision/json")?;
    let thing_name = register_resp["thingName"]
        .as_str()
        .ok_or_else(|| anyhow!("thingName missing"))?
        .to_string();
    log::info!("Provisioning APPROVED. thingName={thing_name}");

    Ok(DeviceIdentity {
        cert_pem,
        key_pem: priv_key,
        thing_name,
    })
}

/// Waits for the Connected event (with timeout).
fn wait_connected(events: &std::sync::mpsc::Receiver<MqttEvent>) -> Result<()> {
    let deadline = Duration::from_secs(30);
    loop {
        match events.recv_timeout(deadline) {
            Ok(MqttEvent::Connected) => return Ok(()),
            Ok(MqttEvent::Disconnected) => bail!("connection lost (TLS/cert error?)"),
            Ok(MqttEvent::Message { .. }) => continue,
            Err(_) => bail!("connection timeout (check endpoint/policy/certs)"),
        }
    }
}

/// Waits until `accepted` or `rejected` response arrives.
/// `marker` is a common part of the expected topic (distinguishes accepted/rejected).
fn wait_response(events: &std::sync::mpsc::Receiver<MqttEvent>, marker: &str) -> Result<Value> {
    let deadline = Duration::from_secs(30);
    loop {
        match events.recv_timeout(deadline) {
            Ok(MqttEvent::Message { topic, data }) if topic.contains(marker) => {
                let parsed: Value = serde_json::from_slice(&data)
                    .with_context(|| format!("failed to parse JSON response: {topic}"))?;
                if topic.ends_with("/rejected") {
                    bail!("provisioning REJECTED: {parsed}");
                }
                return Ok(parsed);
            }
            Ok(MqttEvent::Message { .. }) => continue, // other topic
            Ok(MqttEvent::Connected) => continue,
            Ok(MqttEvent::Disconnected) => bail!("connection lost while waiting for response"),
            Err(_) => bail!("response timeout ({marker})"),
        }
    }
}
