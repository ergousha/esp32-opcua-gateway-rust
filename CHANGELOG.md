# Changelog

## [0.1.0](https://github.com/ergousha/esp32-opcua-gateway-rust/compare/v0.0.1...v0.1.0) (2026-09-27)


### Features

* **firmware:** name the OPC UA task and log its stack headroom ([26c969d](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/26c969d4f6960dffb746d949f683fb143e94597d))
* **opcua:** add OPC UA client integration ([18ce4d4](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/18ce4d4b01d53937cbe7921465099e9a476ddd62))
* **tools:** port seed_device.py to Rust ([3d1ffe4](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/3d1ffe488a2304ce61e6e33812a06abb8c6871ff))


### Bug Fixes

* **build:** anchor the partition-table glob so rebuilds stop failing ([0812508](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/0812508aa77b41693437ff56a14f504e5c1afb96))
* **deps:** make the vendored Tokio signal driver inert on ESP-IDF ([d9e0ac0](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/d9e0ac0ad19460cce4be0c223d4834427b1aa70e))
* **firmware:** restart instead of hanging when WiFi fails at boot ([a743482](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/a743482dc8c42a4d21f9b8b62fdedca0d0df5a3d))
* **hil:** fail phases the device crashed in; stop a stale reject_digest pass ([83005a5](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/83005a568b433fdc550fe9a8f6e65a9e76d85590))
* **hil:** stop the monitor freezing the chip; add --offline mode and Windows support ([50eef4e](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/50eef4e945cf93b40603227561ce6745df9623ca))
* **main:** catch wifi start error and sleep to prevent double panic drop of eth SpiDriver ([16f88a3](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/16f88a39b749f30b888eab8bdee179c7b5f4d483))
* **opcua:** box the session event loop before spawning it ([214e53a](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/214e53a2a081d5f987c88fddf4a16494ac440f4a))
* **opcua:** register the eventfd VFS before building the Tokio runtime ([e889e00](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/e889e003da9d2b46847cc71d0daa4a9547244a2c))
* **release:** build a firmware image that can connect, and keep its secrets private ([b9ef7e9](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/b9ef7e9649ef4d8c8b4a322eb01ab650a69b0e56))
