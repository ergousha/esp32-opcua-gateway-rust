# Changelog

## [0.4.0](https://github.com/ergousha/esp32-opcua-gateway-rust/compare/v0.3.0...v0.4.0) (2026-11-11)


### Features

* move the cloud link onto the platform device SDK; transitional WiFi move ([c4dbb8a](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/c4dbb8ab6180f12a09657530d0a56a74da0f3551))


### Bug Fixes

* device SDK with the connection fix and p256 0.14 ([b176156](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/b176156615a5da5bf64c5081d542df1abe71e2af))
* device SDK without the empty-publish crash ([e293ba5](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/e293ba571a9dd6e512e3c6aa7525980e2084fa6b))
* OTA on the gateway: free heap, MQTT paused, WiFi retries ([06f4a40](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/06f4a409058084cda4e694fe63dab53ec62af6db))
* WiFi from NVS only; SDK session and heap fixes; release version ([866a1ff](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/866a1ff2241eed6f796f8b46d2b65ed5b765e753))

## [0.3.0](https://github.com/ergousha/esp32-opcua-gateway-rust/compare/v0.2.1...v0.3.0) (2026-11-09)


### Features

* share the Jobs protocol as iot-jobs, refuse other products' images ([caef958](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/caef958914cec0117dbe8f323b8a0b3fcb26d1d6))

## [0.2.1](https://github.com/ergousha/esp32-opcua-gateway-rust/compare/v0.2.0...v0.2.1) (2026-11-09)


### Bug Fixes

* flash the project's bootloader, which has OTA rollback ([db27d64](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/db27d647fa45c2e47d7e7924deaf167b063a3f78)), closes [#22](https://github.com/ergousha/esp32-opcua-gateway-rust/issues/22)
* **hil:** confirm a restore to an idle device, treat test-namespace configs as leftovers, harden signal handling ([be120df](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/be120df2893c1b62133c78c7336009a050dfbab1))
* **ota:** host-test the Jobs client; retry a never-activated update, cap crashing downloads, honour cancellation, reject refused jobs ([c937521](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/c937521c7650ccf83e7ad75029d5baf6d805f2b1))
* **ota:** treat TerminalStateReached as a cancelled job before activation ([db015be](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/db015be7ec91f78f750012102643aacc9c42ae75))

## [0.2.0](https://github.com/ergousha/esp32-opcua-gateway-rust/compare/v0.1.0...v0.2.0) (2026-11-09)


### Features

* **firmware:** log where the OPC UA stack is and what it cost the heap ([b53c496](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/b53c4969551925d4db8c8406ab71ccc47dceb436))


### Bug Fixes

* **deps:** update rustls and ctutils for new RustSec advisories ([1ea9a0d](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/1ea9a0d791a8c82c12f655d6c4710922008ad36d))
* **deps:** update rustls-webpki for RUSTSEC-2026-0382 ([04acf42](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/04acf42b4c6d89d88adec979a2e78311f5bb1ffc))
* **firmware:** restart an unverified OTA image that cannot reach AWS IoT ([ec51a26](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/ec51a269e72b146ef47b8e569c8472989404279a))
* **hil:** match the restored endpoint as a whole URL; harden the snapshot file ([5d988d8](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/5d988d806ef5336e2182ff108addeb77176a0d83))
* **hil:** put the device back the way the run found it ([d343793](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/d343793848d42b670ae660a6a34ffdb0dd05f4e6))
* **opcua:** box the driver future before the thread reports ready ([69aad32](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/69aad3285b3d97bfb3678cd9e37ba8d9d942d09d))
* **ota:** report a job's outcome from the image that runs after the reboot ([b322e2b](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/b322e2b771a4902763d9c3abc5ee681be32df7ea))
* **ota:** stop re-settling a refused job; give a fresh image time to connect ([11154ab](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/11154ab6be084eecfbd4dce094e7595d05f0e03f))


### Performance Improvements

* **opcua:** box the driver future before block_on ([c21e566](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/c21e566b0eb6178fd48a768248a23a4a7a0080bd))
* **opcua:** poll the session event loop's stream from our own task ([85d3ced](https://github.com/ergousha/esp32-opcua-gateway-rust/commit/85d3cede18d10b1b67e8d58069e5da980e632d42))

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
