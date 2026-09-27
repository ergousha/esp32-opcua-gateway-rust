//! Test fixture for the gateway's OPC UA client.
//!
//! * [`TestServer`] — an OPC UA server built on `async-opcua-server`, the server
//!   half of the same library the gateway's client is built on. It stands in
//!   for the PLC.
//! * [`catalogue`] — the tag set the server exposes. The single source of
//!   truth: the server builds its nodes from it, and [`documents`] builds the
//!   tag bundle from it.
//! * [`documents`] — the cloud side: the tag bundle and the shadow's
//!   `state.desired`, exactly as AWS would deliver them.
//! * [`Blackhole`] — a TCP relay that can go silent, for the failure a killed
//!   server does not reproduce.
//!
//! Two ways to use it:
//!
//! * **In-process, over loopback**, from `gateway-opcua`'s tests and its
//!   `local_gateway` example. Client and server share one process on
//!   `127.0.0.1`, so no device, no LAN and no host firewall is involved.
//! * **Standalone** (`src/main.rs`), bound to a LAN address, for a real device
//!   to dial — see `docs/OPCUA_INTEGRATION_TEST.md`.

pub mod blackhole;
pub mod catalogue;
pub mod documents;
mod server;

pub use blackhole::Blackhole;
pub use server::{Options, TestServer, DEFAULT_PATH, DEFAULT_PORT, DEFAULT_TICK};
