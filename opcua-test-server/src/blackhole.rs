//! A TCP relay that can go silent without closing anything.
//!
//! Killing the server is the easy failure: the socket closes and the client
//! hears about it at once. The failure that actually strands gateways in the
//! field is the silent one — a pulled cable, a dead switch, a PLC that hangs
//! with its TCP stack still up. No RST ever arrives, so the only way to notice
//! is the session keep-alive. This relay reproduces that on loopback: while
//! silent it accepts and reads everything and forwards nothing.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// A relay from a loopback port to `target`.
pub struct Blackhole {
    port: u16,
    silent: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Blackhole {
    /// Starts relaying from an ephemeral loopback port to `target`.
    pub async fn start(target: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("binding the relay")?;
        let port = listener.local_addr()?.port();
        let silent = Arc::new(AtomicBool::new(false));

        let flag = Arc::clone(&silent);
        let task = tokio::spawn(async move {
            while let Ok((inbound, _)) = listener.accept().await {
                let flag = Arc::clone(&flag);
                tokio::spawn(async move {
                    let Ok(outbound) = TcpStream::connect(target).await else {
                        return;
                    };
                    let (ri, wi) = inbound.into_split();
                    let (ro, wo) = outbound.into_split();
                    tokio::join!(pump(ri, wo, Arc::clone(&flag)), pump(ro, wi, flag));
                });
            }
        });

        Ok(Self { port, silent, task })
    }

    /// Loopback port clients should dial instead of the target.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// While silent, bytes are swallowed in both directions and no connection
    /// is closed.
    pub fn set_silent(&self, silent: bool) {
        self.silent.store(silent, Ordering::SeqCst);
    }
}

impl Drop for Blackhole {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn pump(
    mut from: impl AsyncRead + Unpin,
    mut to: impl AsyncWrite + Unpin,
    silent: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if silent.load(Ordering::SeqCst) {
            continue;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
}
