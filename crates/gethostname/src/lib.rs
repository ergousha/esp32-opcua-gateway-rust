//! Stub gethostname for ESP32 target.

use std::ffi::OsString;

pub fn gethostname() -> OsString {
    OsString::from("esp32-opcua-gateway")
}
