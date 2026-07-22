//! Stub signal-hook-registry implementation for ESP-IDF / embedded target.

pub type SigId = usize;
pub const FORBIDDEN: &[std::os::raw::c_int] = &[];

pub unsafe fn register(
    _signal: std::os::raw::c_int,
    _action: impl Fn() + Send + Sync + 'static,
) -> Result<SigId, std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "signals not supported on espidf",
    ))
}

pub unsafe fn register_signal_unchecked(
    _signal: std::os::raw::c_int,
    _action: impl Fn() + Send + Sync + 'static,
) -> Result<SigId, std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "signals not supported on espidf",
    ))
}

pub fn unregister(_id: SigId) -> bool {
    false
}
