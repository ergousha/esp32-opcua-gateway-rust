//! The device side: flashing, the serial monitor, and USB resets.
//!
//! `std::process` rather than `tokio::process` on purpose. The workspace
//! patches `signal-hook-registry` with a stub (signals do not exist on
//! ESP-IDF), and tokio's child reaper on macOS is built on SIGCHLD — so
//! `tokio::process` cannot wait for a child here. Blocking calls run on
//! `spawn_blocking` instead.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;

use crate::report::info;

/// Absolute path to `espflash`.
///
/// `~/export-esp.sh` exports the xtensa toolchain but not `~/.cargo/bin`,
/// where `cargo install espflash` puts it, so a bare name fails after the
/// build — for a reason the error does not name.
pub fn espflash() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("espflash"))
            .find(|candidate| candidate.is_file())
    }) {
        return Ok(path);
    }
    let fallback = std::env::var_os("HOME")
        .map(|home| Path::new(&home).join(".cargo/bin/espflash"))
        .filter(|p| p.is_file());
    fallback.ok_or_else(|| {
        anyhow!(
            "espflash not found on PATH or at ~/.cargo/bin/espflash; install it with \
             `cargo install espflash`"
        )
    })
}

/// Waits for the USB-serial node to reappear and hold still after a reset.
///
/// An ESP32-S3 on its built-in USB-JTAG re-enumerates on every chip reset:
/// the node disappears and comes back, possibly as a new inode. A monitor
/// attached in that window holds a handle that never delivers a byte, which on
/// the log is indistinguishable from a device that booted and said nothing.
pub fn wait_for_port(port: &str, timeout: Duration, settle: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut last: Option<u64> = None;
    let mut stable_since: Option<Instant> = None;
    while Instant::now() < deadline {
        match std::fs::metadata(port) {
            Err(_) => (last, stable_since) = (None, None),
            Ok(meta) if Some(meta.ino()) != last => {
                (last, stable_since) = (Some(meta.ino()), Some(Instant::now()));
            }
            Ok(_) => {
                if stable_since.is_some_and(|t| t.elapsed() >= settle) {
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

/// Flashes `elf` into the `ota_0` slot.
pub async fn flash(port: &str, elf: &Path, repo: &Path) -> Result<()> {
    let mut cmd = Command::new(espflash()?);
    cmd.args(["flash", "--port", port])
        .args(["--partition-table", "partitions.csv"])
        .args(["--target-app-partition", "ota_0", "--non-interactive"])
        .arg(elf)
        .current_dir(repo);
    println!("    $ {cmd:?}");
    let status = tokio::task::spawn_blocking(move || cmd.status())
        .await?
        .context("running espflash flash")?;
    if !status.success() {
        bail!("espflash flash failed with {status}");
    }
    Ok(())
}

/// Resets the chip over USB. Returns espflash's stderr on failure.
pub async fn reset(port: &str, repo: &Path) -> Result<std::result::Result<(), String>> {
    let mut cmd = Command::new(espflash()?);
    cmd.args(["reset", "--port", port]).current_dir(repo);
    let out = tokio::task::spawn_blocking(move || cmd.output())
        .await?
        .context("running espflash reset")?;
    if out.status.success() {
        Ok(Ok(()))
    } else {
        Ok(Err(String::from_utf8_lossy(&out.stderr).into_owned()))
    }
}

/// Captures the device's serial log for the whole run.
///
/// One long-lived monitor rather than one per phase: the port is exclusive,
/// and re-attaching resets the chip, which would destroy exactly the
/// continuity later phases assert on. Only a run that just flashed attaches
/// *with* a reset — flashing already reset the chip while nothing was
/// listening, so one more reset is the only way to see a boot from its first
/// line. Every other attach keeps `--no-reset`, so the boot under inspection
/// is the one the phase caused.
pub struct SerialMonitor {
    pub port: String,
    log: PathBuf,
    elf: PathBuf,
    repo: PathBuf,
    child: Option<Child>,
}

impl SerialMonitor {
    pub fn new(port: &str, log: PathBuf, elf: PathBuf, repo: PathBuf) -> Self {
        Self {
            port: port.to_string(),
            log,
            elf,
            repo,
            child: None,
        }
    }

    pub async fn start(&mut self, reset: bool) -> Result<()> {
        let port = self.port.clone();
        let settled = tokio::task::spawn_blocking(move || {
            wait_for_port(&port, Duration::from_secs(30), Duration::from_millis(1_500))
        })
        .await?;
        if !settled {
            bail!(
                "{} did not settle after the chip reset; the monitor would have attached \
                 to a node that delivers nothing",
                self.port
            );
        }

        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)?;
        let mut cmd = Command::new(espflash()?);
        cmd.args(["monitor", "--port", &self.port, "--non-interactive"]);
        if !reset {
            cmd.arg("--no-reset");
        }
        cmd.arg("--elf")
            .arg(&self.elf)
            .current_dir(&self.repo)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        self.child = Some(cmd.spawn().context("starting espflash monitor")?);
        tokio::time::sleep(Duration::from_secs(2)).await;

        let how = if reset {
            "with a reset, to capture the boot"
        } else {
            "without resetting"
        };
        info(format!(
            "serial monitor attached to {} {how} -> {}",
            self.port,
            self.log.display()
        ));
        Ok(())
    }

    /// Current size of the log, for reading only what a phase produced.
    pub fn mark(&self) -> u64 {
        std::fs::metadata(&self.log).map(|m| m.len()).unwrap_or(0)
    }

    pub fn read_since(&self, offset: u64) -> String {
        let mut text = Vec::new();
        if let Ok(mut file) = File::open(&self.log) {
            if file.seek(SeekFrom::Start(offset)).is_ok() {
                let _ = file.read_to_end(&mut text);
            }
        }
        String::from_utf8_lossy(&text).into_owned()
    }

    /// Waits for `pattern` to appear after `offset`.
    pub async fn wait_for(
        &self,
        pattern: &Regex,
        offset: u64,
        timeout: Duration,
    ) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(m) = pattern.find(&self.read_since(offset)) {
                return Some(m.as_str().to_string());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        None
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for SerialMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}
