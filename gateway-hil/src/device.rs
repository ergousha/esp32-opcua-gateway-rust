//! The device side: flashing, the serial monitor, and USB resets.
//!
//! `std::process` rather than `tokio::process` on purpose. The workspace
//! patches `signal-hook-registry` with a stub (signals do not exist on
//! ESP-IDF), and tokio's child reaper on macOS is built on SIGCHLD — so
//! `tokio::process` cannot wait for a child here. Blocking calls run on
//! `spawn_blocking` instead.
//!
//! Runs on macOS, Linux and Windows. Only port detection differs: see
//! [`wait_for_port`].

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
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
    let name = format!("espflash{}", std::env::consts::EXE_SUFFIX);
    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(&name))
            .find(|candidate| candidate.is_file())
    }) {
        return Ok(path);
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let fallback = home
        .map(|home| Path::new(&home).join(".cargo").join("bin").join(&name))
        .filter(|p| p.is_file());
    fallback.ok_or_else(|| {
        anyhow!(
            "{name} not found on PATH or in ~/.cargo/bin; install it with \
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
#[cfg(unix)]
pub fn wait_for_port(port: &str, timeout: Duration, settle: Duration) -> bool {
    use std::os::unix::fs::MetadataExt;

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

/// Windows: a COM port is not a file with an inode, and probing it by opening
/// it can drive DTR/RTS — which on this chip's USB-JTAG is a reset line. So
/// wait out the re-enumeration instead; espflash reports a port that never
/// came back.
#[cfg(not(unix))]
pub fn wait_for_port(_port: &str, timeout: Duration, settle: Duration) -> bool {
    std::thread::sleep((settle * 2).min(timeout));
    true
}

/// The bootloader the build of `elf` produced: the firmware's `build.rs`
/// copies it to `<target-dir>/bootloader.bin`, and the ELF is
/// `<target-dir>/<triple>/<profile>/<name>`.
pub fn bootloader_for(elf: &Path) -> Option<PathBuf> {
    elf.ancestors()
        .nth(3)
        .map(|target_dir| target_dir.join("bootloader.bin"))
}

/// Flashes `elf` into the `ota_0` slot, with the bootloader its build
/// produced.
///
/// Never espflash's bundled bootloader, which it writes when not given one:
/// that one is built without `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`, so a bad
/// OTA image boot-loops instead of being rolled back (issue #22). OTA never
/// replaces the bootloader, so the unit would keep it.
pub async fn flash(port: &str, elf: &Path, repo: &Path) -> Result<()> {
    let bootloader = bootloader_for(elf)
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            anyhow!(
                "no bootloader.bin in the target dir of {}; the firmware build writes it \
                 there (build.rs), so rebuild with `cargo build --release`",
                elf.display()
            )
        })?;
    let mut cmd = Command::new(espflash()?);
    cmd.args(["flash", "--port", port])
        .arg("--bootloader")
        .arg(&bootloader)
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

/// Reads `len` bytes of flash from `offset`. Needs the port: detach the
/// monitor first. Resets the chip, into the ROM loader and back out.
pub async fn read_flash(port: &str, offset: u32, len: u32) -> Result<Vec<u8>> {
    let file = std::env::temp_dir().join(format!(
        "gateway-hil-flash-{}-{offset:#x}.bin",
        std::process::id()
    ));
    let mut cmd = Command::new(espflash()?);
    cmd.args(["read-flash", "--port", port, "--non-interactive"])
        .arg(format!("{offset:#x}"))
        .arg(format!("{len:#x}"))
        .arg(&file)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let output = tokio::task::spawn_blocking(move || cmd.output())
        .await?
        .context("running espflash read-flash")?;
    if !output.status.success() {
        bail!(
            "espflash read-flash failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let data = std::fs::read(&file).with_context(|| format!("reading {}", file.display()));
    let _ = std::fs::remove_file(&file);
    data
}

/// Captures the device's serial log for the whole run.
///
/// One long-lived monitor rather than one per phase: the port is exclusive,
/// and every attach reboots the chip, which would destroy exactly the
/// continuity later phases assert on.
///
/// **Every attach resets the chip, deliberately.** `espflash monitor` cannot
/// attach to running firmware: it first connects to the ROM loader, which
/// means resetting the chip into download mode. Its default then hard-resets
/// the chip so the firmware boots, and the log starts at the first boot line.
/// `--no-reset` only skips that second reset (it is `--after no-reset`): the
/// chip stays in the bootloader and the firmware never runs again, while the
/// monitor shows `Using flash stub` and then nothing. That was the harness
/// defect behind §9.8; see `docs/OPCUA_INTEGRATION_TEST.md` §8.20. A reboot is
/// therefore just a re-attach ([`SerialMonitor::restart`]).
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

    /// Attaches, rebooting the chip so its log is captured from the first line.
    pub async fn start(&mut self) -> Result<()> {
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
        // The ELF only decodes backtraces; a host that did not build the
        // firmware can still monitor without it.
        if self.elf.is_file() {
            cmd.arg("--elf").arg(&self.elf);
        }
        cmd.current_dir(&self.repo)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        self.child = Some(cmd.spawn().context("starting espflash monitor")?);
        tokio::time::sleep(Duration::from_secs(2)).await;

        info(format!(
            "serial monitor attached to {} (the chip reboots) -> {}",
            self.port,
            self.log.display()
        ));
        Ok(())
    }

    /// Reboots the chip by re-attaching (see the type docs).
    pub async fn restart(&mut self) -> Result<()> {
        self.stop();
        tokio::time::sleep(Duration::from_secs(1)).await;
        self.start().await
    }

    /// The firmware ELF the log is decoded against.
    pub fn elf(&self) -> &Path {
        &self.elf
    }

    /// Reads flash with the monitor detached, then re-attaches, which reboots
    /// the chip like [`SerialMonitor::restart`].
    pub async fn read_flash(&mut self, offset: u32, len: u32) -> Result<Vec<u8>> {
        self.stop();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let data = read_flash(&self.port, offset, len).await;
        self.start().await?;
        data
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
