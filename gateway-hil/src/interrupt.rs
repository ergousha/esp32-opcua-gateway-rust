//! Ctrl-C (and SIGTERM) as "stop the phases, then restore the device".
//!
//! Not `tokio::signal` on Unix. Tokio registers its handlers through
//! `signal-hook-registry`, which the workspace patches with a stub for ESP-IDF,
//! so there `ctrl_c()` fails at once with "registering signal handler
//! failed". A plain `sigaction` that raises a flag needs nothing from it.
//! Windows has no such detour, and tokio's console handler works.
//!
//! Only the first signal is caught. The second kills the runner the ordinary
//! way, for when the restore itself hangs; `--restore` finishes it later.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static CAUGHT: AtomicBool = AtomicBool::new(false);

/// Whether a signal has arrived since [`catch`].
pub fn caught() -> bool {
    CAUGHT.load(Ordering::SeqCst)
}

/// Resolves once a signal has arrived; never, if none does.
pub async fn wait() {
    while !caught() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Starts catching. Until then Ctrl-C kills the runner, which is right while
/// it has changed nothing.
#[cfg(unix)]
pub fn catch() -> std::io::Result<()> {
    extern "C" fn raise(_: libc::c_int) {
        CAUGHT.store(true, Ordering::SeqCst);
    }
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler only stores to an atomic, which is
        // async-signal-safe; the struct is fully initialised before use.
        let installed = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = raise as extern "C" fn(libc::c_int) as libc::sighandler_t;
            // One-shot: the next signal gets the default action again. And
            // system calls it lands in resume instead of failing.
            action.sa_flags = libc::SA_RESETHAND | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut()) == 0
        };
        if !installed {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Starts catching. Until then Ctrl-C kills the runner, which is right while
/// it has changed nothing.
#[cfg(windows)]
pub fn catch() -> std::io::Result<()> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    tokio::spawn(async move {
        if ctrl_c.recv().await.is_some() {
            CAUGHT.store(true, Ordering::SeqCst);
        }
        // Tokio's handler swallows every later Ctrl-C; keep the second fatal.
        if ctrl_c.recv().await.is_some() {
            std::process::exit(130);
        }
    });
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub fn catch() -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing tokio could not do here. Were the handler not in
    /// place, the signal would kill the test binary.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_first_sigint_is_caught_instead_of_killing_the_runner() {
        catch().unwrap();
        // SAFETY: `raise` is always safe to call; the handler is installed.
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        tokio::time::timeout(Duration::from_secs(5), wait())
            .await
            .expect("the signal raises the flag");
    }
}
