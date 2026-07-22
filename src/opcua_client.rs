//! OPC UA Client module for ESP32-S3 Gateway.
//!
//! Connects to an OPC UA Server over TCP (`opc.tcp://`), reads configured
//! NodeIDs periodically, and forwards telemetry payloads to the AWS IoT MQTT task.

use std::sync::mpsc::Sender;
use std::time::Duration;
use anyhow::{anyhow, Result};
use log::{error, info, warn};

#[derive(Debug, Clone, serde::Serialize)]
pub struct OpcUaTelemetry {
    pub node_id: String,
    pub value: String,
    pub status: String,
    pub timestamp_ms: u64,
}

/// Configuration options for OPC UA Client connection.
#[derive(Debug, Clone)]
pub struct OpcUaClientConfig {
    pub endpoint_url: String,
    pub poll_interval: Duration,
    pub node_ids: Vec<String>,
}

impl Default for OpcUaClientConfig {
    fn default() -> Self {
        Self {
            endpoint_url: "opc.tcp://127.0.0.1:4840".to_string(),
            poll_interval: Duration::from_secs(5),
            node_ids: vec![
                "ns=2;s=Temperature".to_string(),
                "ns=2;s=Pressure".to_string(),
            ],
        }
    }
}

/// Main entry point for OPC UA worker background task.
/// Run on a dedicated thread or async runtime.
pub fn spawn_opcua_worker(
    config: OpcUaClientConfig,
    tx: Sender<OpcUaTelemetry>,
) -> Result<std::thread::JoinHandle<()>> {
    info!(
        "Spawning OPC UA Client worker task. Endpoint: {}",
        config.endpoint_url
    );

    let handle = std::thread::Builder::new()
        .name("opcua_client".to_string())
        .stack_size(32 * 1024) // 32KB stack for ESP32
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    error!("Failed to create tokio runtime for OPC UA client: {:?}", e);
                    return;
                }
            };

            rt.block_on(async move {
                opcua_loop(config, tx).await;
            });
        })?;

    Ok(handle)
}

/// Reconnecting infinite OPC UA polling loop.
async fn opcua_loop(config: OpcUaClientConfig, tx: Sender<OpcUaTelemetry>) {
    loop {
        info!("Connecting to OPC UA Server: {}", config.endpoint_url);

        match run_opcua_session(&config, &tx).await {
            Ok(_) => {
                warn!("OPC UA session ended cleanly. Retrying in 5 seconds...");
            }
            Err(e) => {
                error!("OPC UA session error: {:?}. Retrying in 5 seconds...", e);
            }
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Establishes session and executes polling loop.
async fn run_opcua_session(
    config: &OpcUaClientConfig,
    tx: &Sender<OpcUaTelemetry>,
) -> Result<()> {
    use opcua_client::ClientBuilder;
    use opcua_types::{NodeId, ReadValueId, TimestampsToReturn};
    use std::str::FromStr;

    let mut client = ClientBuilder::new()
        .application_name("ESP32-OPCUA-Gateway")
        .application_uri("urn:esp32-opcua-gateway")
        .product_uri("urn:esp32-opcua-gateway")
        .trust_server_certs(true)
        .create_sample_keypair(true)
        .client()
        .map_err(|e| anyhow!("Failed to build OPC UA Client: {:?}", e))?;

    let (session, session_loop) = client
        .connect_to_endpoint_id(config.endpoint_url.as_str())
        .await
        .map_err(|e| anyhow!("Failed to connect to OPC UA endpoint: {:?}", e))?;

    tokio::spawn(session_loop.run());

    info!("Successfully established OPC UA Session with {}", config.endpoint_url);

    let node_ids: Vec<NodeId> = config
        .node_ids
        .iter()
        .filter_map(|id_str| NodeId::from_str(id_str).ok())
        .collect();

    if node_ids.is_empty() && !config.node_ids.is_empty() {
        warn!("None of the provided NodeID strings parsed successfully into NodeId!");
    }

    loop {
        if !node_ids.is_empty() {
            let read_nodes: Vec<ReadValueId> = node_ids
                .iter()
                .cloned()
                .map(ReadValueId::from)
                .collect();

            match session.read(&read_nodes, TimestampsToReturn::Both, 0.0).await {
                Ok(results) => {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;

                    for (node_id, res) in node_ids.iter().zip(results.into_iter()) {
                        let val_str = match res.value {
                            Some(ref v) => format!("{:?}", v),
                            None => "null".to_string(),
                        };
                        let status_str = match res.status {
                            Some(ref s) => format!("{:?}", s),
                            None => "Good".to_string(),
                        };

                        let telem = OpcUaTelemetry {
                            node_id: node_id.to_string(),
                            value: val_str,
                            status: status_str,
                            timestamp_ms: now_ms,
                        };

                        info!("OPC UA Node: {} = {}", telem.node_id, telem.value);

                        if let Err(e) = tx.send(telem) {
                            error!("Failed to queue telemetry to MQTT task: {:?}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("Error reading OPC UA nodes: {:?}", e);
                    return Err(anyhow!("Read error: {:?}", e));
                }
            }
        }

        tokio::time::sleep(config.poll_interval).await;
    }
}
