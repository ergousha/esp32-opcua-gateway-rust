#!/usr/bin/env python3
"""End-to-end integration scenario for the ESP32-S3 OPC UA gateway.

Drives a real device against a real OPC UA server and a real AWS IoT account,
and asserts on the three things that can actually be observed from outside the
firmware:

  * the `reported` block of the `opcua` named shadow (state, counts, errors),
  * the batched telemetry that reaches CloudWatch via the `dt/+/opcua` IoT rule,
  * the device's own serial log.

Phases run in order and share state, because that is how the device works: the
reconfiguration phase is only meaningful once the happy path has been applied.
Each phase is individually selectable with `--phases` for re-running a single
failure without repeating a 20-minute run.

    .venv/bin/python run_scenario.py --thing 28848553144F \\
        --server-host 192.168.50.28 --port /dev/cu.usbmodem21401 --flash
"""

from __future__ import annotations

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Optional

import cloud
from cloud import Cloud, build_bundle, desired_document, rows_by_address
from tags import (
    ENCODING_PREDICATES,
    FAST_MS,
    MISSING_ADDRESSES,
    NAMESPACE_URI,
    SLOW_MS,
    TAGS,
)

HERE = Path(__file__).resolve().parent
REPO = HERE.parent

# The firmware reports `state` from `gateway_core::health::DriverState`.
RUNNING, IDLE, ERROR = "running", "idle", "error"


# ---------------------------------------------------------------------------
# result plumbing
# ---------------------------------------------------------------------------


@dataclass
class Check:
    name: str
    ok: bool
    detail: str = ""


@dataclass
class PhaseResult:
    name: str
    checks: list[Check] = field(default_factory=list)
    skipped: bool = False
    note: str = ""

    @property
    def ok(self) -> bool:
        return self.skipped or all(c.ok for c in self.checks)

    def check(self, name: str, ok: bool, detail: str = "") -> Check:
        c = Check(name, bool(ok), detail)
        self.checks.append(c)
        status = "PASS" if c.ok else "FAIL"
        line = f"    [{status}] {name}"
        if detail:
            line += f" — {detail}"
        print(line, flush=True)
        return c


def banner(text: str) -> None:
    print(f"\n{'=' * 78}\n{text}\n{'=' * 78}", flush=True)


def info(text: str) -> None:
    print(f"    · {text}", flush=True)


# ---------------------------------------------------------------------------
# external processes
# ---------------------------------------------------------------------------


class OpcuaServer:
    """The test OPC UA server as a child process.

    Owned by the runner rather than started by hand so that the resilience
    phase can kill it mid-session and bring it back — which is the only honest
    way to test reconnect/backoff.
    """

    def __init__(self, host: str, port: int, path: str, log: Path) -> None:
        self.host, self.port, self.path = host, port, path
        self.log = log
        self.proc: Optional[subprocess.Popen] = None

    @property
    def endpoint(self) -> str:
        return f"opc.tcp://{self.host}:{self.port}{self.path}"

    def start(self) -> None:
        if self.proc and self.proc.poll() is None:
            return
        # Only look for READY *after* this point in the log. The file is
        # appended to across restarts, so scanning the whole tail would match
        # the previous run's marker and report a dead server as ready — exactly
        # the wrong answer in the `server_down` phase.
        offset = self.log.stat().st_size if self.log.exists() else 0
        handle = open(self.log, "ab")
        self.proc = subprocess.Popen(
            [
                sys.executable,
                str(HERE / "opcua_test_server.py"),
                "--host",
                "0.0.0.0",
                "--port",
                str(self.port),
                "--path",
                self.path,
            ],
            cwd=HERE,
            stdin=subprocess.PIPE,
            stdout=handle,
            stderr=subprocess.STDOUT,
        )
        # Wait for the READY marker rather than sleeping a guessed interval.
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if self.log.exists() and b"READY" in self.log.read_bytes()[offset:]:
                info(f"OPC UA server up on {self.endpoint}")
                return
            if self.proc.poll() is not None:
                raise RuntimeError(f"server exited early; see {self.log}")
            time.sleep(0.5)
        raise RuntimeError("OPC UA server did not become ready")

    def command(self, cmd: str) -> None:
        if self.proc and self.proc.stdin:
            self.proc.stdin.write(f"{cmd}\n".encode())
            self.proc.stdin.flush()

    def stop(self) -> None:
        if not self.proc or self.proc.poll() is not None:
            return
        self.proc.send_signal(signal.SIGKILL)  # abrupt on purpose: simulates a PLC drop
        self.proc.wait(timeout=10)
        info("OPC UA server killed")


class SerialMonitor:
    """Captures the device's serial log for the whole run.

    One long-lived monitor rather than one per phase: the USB-serial port is
    exclusive, and re-attaching resets the chip, which would destroy exactly the
    continuity the later phases assert on.
    """

    def __init__(self, port: str, log: Path) -> None:
        self.port, self.log = port, log
        self.proc: Optional[subprocess.Popen] = None

    def start(self) -> None:
        handle = open(self.log, "ab")
        env = dict(os.environ)
        self.proc = subprocess.Popen(
            [
                "espflash",
                "monitor",
                "--port",
                self.port,
                "--non-interactive",
                "--no-reset",
                "--elf",
                str(REPO / "target/xtensa-esp32s3-espidf/release/esp32-opcua-gateway"),
            ],
            cwd=REPO,
            stdin=subprocess.DEVNULL,
            stdout=handle,
            stderr=subprocess.STDOUT,
            env=env,
        )
        time.sleep(2)
        info(f"serial monitor attached to {self.port} -> {self.log.name}")

    def mark(self) -> int:
        """Current size of the log, for reading only what a phase produced."""
        return self.log.stat().st_size if self.log.exists() else 0

    def read_since(self, offset: int) -> str:
        if not self.log.exists():
            return ""
        with open(self.log, "rb") as fh:
            fh.seek(offset)
            return fh.read().decode("utf-8", errors="replace")

    def wait_for(self, pattern: str, offset: int, timeout_s: float) -> Optional[str]:
        rx = re.compile(pattern)
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            m = rx.search(self.read_since(offset))
            if m:
                return m.group(0)
            time.sleep(1.0)
        return None

    def stop(self) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()


# ---------------------------------------------------------------------------
# shared context
# ---------------------------------------------------------------------------


@dataclass
class Ctx:
    cloud: Cloud
    server: OpcuaServer
    monitor: Optional[SerialMonitor]
    thing: str
    #: config version currently expected to be applied on the device
    applied_version: int = 0
    #: unix ms marking the start of the run, for CloudWatch queries
    t0_ms: int = 0
    timeout: float = 180.0

    def publish_config(self, version: int, tags, **kw) -> cloud.Bundle:
        bundle = build_bundle(self.thing, version, tags)
        desired = desired_document(
            self.thing, bundle, self.server.endpoint, **kw
        )
        self.cloud.publish_bundle(bundle)
        self.cloud.update_desired(desired)
        info(
            f"published config v{version}: {bundle.count} tags, "
            f"{len(bundle.payload)} B bundle, sha256={bundle.sha256[:12]}…"
        )
        return bundle

    def expect_state(
        self, phase: PhaseResult, predicate, label: str, timeout: Optional[float] = None
    ) -> dict:
        ok, reported = self.cloud.wait_for_reported(
            predicate,
            timeout_s=timeout or self.timeout,
            on_poll=lambda r: info(
                f"reported: state={r.get('state')} cfg_v={r.get('cfg_v')} "
                f"applied={r.get('applied')} failed={r.get('failed')} "
                f"heap={r.get('free_heap')} err={r.get('last_error')}"
            ),
        )
        phase.check(label, ok, json.dumps(reported, sort_keys=True)[:300])
        return reported


# ---------------------------------------------------------------------------
# phases
# ---------------------------------------------------------------------------

PHASES: dict[str, Callable[[Ctx], PhaseResult]] = {}


def phase(name: str):
    def wrap(fn):
        PHASES[name] = fn
        return fn

    return wrap


@phase("preflight")
def preflight(ctx: Ctx) -> PhaseResult:
    """Server behaves as the firmware expects, before the device is involved."""
    r = PhaseResult("preflight")
    ctx.server.start()

    proc = subprocess.run(
        [
            sys.executable,
            str(HERE / "selfcheck_client.py"),
            "--endpoint",
            ctx.server.endpoint,
            "--seconds",
            "6",
        ],
        cwd=HERE,
        capture_output=True,
        text=True,
        timeout=120,
    )
    r.check(
        "OPC UA server passes the firmware-equivalent self-check",
        proc.returncode == 0,
        proc.stdout.strip().splitlines()[-1] if proc.stdout else proc.stderr[-200:],
    )

    bundle = build_bundle(ctx.thing, 1, TAGS)
    r.check(
        "tag bundle fits the firmware's NVS budget",
        len(bundle.payload) <= cloud.MAX_BUNDLE_BYTES,
        f"{len(bundle.payload)} B <= {cloud.MAX_BUNDLE_BYTES} B",
    )
    r.check(
        "shadow desired document fits the 8 KB AWS IoT limit",
        len(json.dumps(desired_document(ctx.thing, bundle, ctx.server.endpoint))) < 8192,
        f"{len(json.dumps(desired_document(ctx.thing, bundle, ctx.server.endpoint)))} B",
    )
    return r


@phase("provision")
def provision(ctx: Ctx) -> PhaseResult:
    """Publish config v1 on both planes and confirm the device applies it."""
    r = PhaseResult("provision")
    ctx.server.start()

    mark = ctx.monitor.mark() if ctx.monitor else 0
    ctx.publish_config(1, TAGS)
    ctx.applied_version = 1

    expected_applied = len([t for t in TAGS if t.variant is not None])
    reported = ctx.expect_state(
        r,
        lambda x: x.get("state") == RUNNING and x.get("cfg_v") == 1,
        "driver reaches state=running with cfg_v=1",
        timeout=240,
    )

    r.check(
        f"{expected_applied} monitored items applied",
        reported.get("applied") == expected_applied,
        f"applied={reported.get('applied')} expected={expected_applied}",
    )
    r.check(
        "exactly one tag failed (the deliberately absent one)",
        reported.get("failed") == len(MISSING_ADDRESSES),
        f"failed={reported.get('failed')}",
    )
    sample = reported.get("failed_sample") or []
    r.check(
        "the failed tag is named in failed_sample",
        set(sample) == set(MISSING_ADDRESSES),
        f"failed_sample={sample}",
    )
    r.check(
        "one bad NodeId did not take the other tags down (finding A4)",
        reported.get("applied", 0) > 0 and reported.get("state") == RUNNING,
        "",
    )
    r.check(
        "free heap reported and above 40 KB",
        (reported.get("free_heap") or 0) > 40_000,
        f"free_heap={reported.get('free_heap')}",
    )

    if ctx.monitor:
        log = ctx.monitor.read_since(mark)
        r.check(
            "device logged the unencrypted-link warning on connect (NFR §7)",
            "UNENCRYPTED and UNAUTHENTICATED" in log,
            "",
        )
        r.check(
            "device logged two subscriptions (one per scan rate)",
            bool(re.search(r"OPC UA synced: 2 subscriptions", log)),
            (re.search(r"OPC UA synced:[^\n]*", log) or [""])[0]
            if re.search(r"OPC UA synced:[^\n]*", log)
            else "not found",
        )
    return r


@phase("telemetry")
def telemetry(ctx: Ctx) -> PhaseResult:
    """Assert on the actual bytes that reached AWS, per value type."""
    r = PhaseResult("telemetry")
    start = int(time.time() * 1000) - 120_000
    info("waiting for batches to land in CloudWatch (the IoT rule hop lags)…")
    batches = ctx.cloud.wait_for_telemetry(start, minimum=3, timeout_s=180)
    r.check("telemetry batches arrived", len(batches) >= 3, f"{len(batches)} batches")
    if not batches:
        return r

    r.check(
        "batches are stamped with the applied config version",
        all(b.get("v") == ctx.applied_version for b in batches),
        f"versions={sorted({b.get('v') for b in batches})}",
    )

    rows = rows_by_address(batches)
    info(f"addresses seen: {len(rows)} — {sorted(rows)}")

    r.check(
        "the absent tag never produced a sample",
        all(a not in rows for a in MISSING_ADDRESSES),
        "",
    )

    def last(address):
        got = rows.get(address)
        return got[-1] if got else None

    # --- per-type encoding assertions (§4.3) ------------------------------
    # Driven by the catalogue's `expects`, so a new tag brings its own
    # assertion instead of needing a second edit here.
    for tag in TAGS:
        predicate = ENCODING_PREDICATES.get(tag.expects)
        if predicate is None:
            continue
        row = last(tag.address)
        value = row[2] if row else None
        # The Bad-status tag legitimately carries a null value.
        if tag.bad_status and value is None:
            continue
        r.check(
            f"{tag.address}: {tag.proves}",
            bool(row) and predicate(value),
            f"= {json.dumps(value)[:80]}",
        )

    # Float widening: the server holds f32 1.1/1.2/…; a naive `as f64` would
    # surface 1.100000023841858. `variant.rs::shortest_f32` must not.
    pressure = last("Line1.Pressure")
    if pressure:
        v = pressure[2]
        r.check(
            "Float is widened via its shortest decimal form, not `as f64`",
            isinstance(v, (int, float)) and len(repr(float(v))) <= 5,
            f"Line1.Pressure = {v!r}",
        )

    # Status code only present when NOT Good, as the 4th element.
    good_row = last("Line1.Temp")
    r.check(
        "a Good status is omitted from the row (3 elements)",
        bool(good_row) and len(good_row) == 3,
        f"len={len(good_row) if good_row else 0}",
    )
    faulty = last("Line1.Faulty")
    r.check(
        "a Bad status travels as the 4th element (BadDeviceFailure=0x808B0000)",
        bool(faulty) and len(faulty) == 4 and faulty[3] == 0x808B0000,
        f"Line1.Faulty row = {json.dumps(faulty)}",
    )

    # Report-by-exception: a tag whose value never changes must not repeat.
    static_n = len(rows.get("Line1.Static", []))
    fast_n = len(rows.get("Line1.Temp", []))
    r.check(
        "an unchanging tag is not re-reported every scan (subscription, not polling)",
        static_n <= 2 and fast_n > static_n,
        f"Line1.Static n={static_n} vs Line1.Temp n={fast_n}",
    )

    # The slow group must be sampled roughly 5x less often than the fast one.
    slow_n = len(rows.get("Line2.Level", []))
    r.check(
        "the 5 s group reports less often than the 1 s group",
        slow_n < fast_n if fast_n else False,
        f"Line2.Level n={slow_n} vs Line1.Temp n={fast_n}",
    )

    r.check(
        "no batch exceeded the configured byte budget",
        all(len(json.dumps(b, separators=(",", ":"))) <= 16384 for b in batches),
        f"max={max(len(json.dumps(b, separators=(',', ':'))) for b in batches)} B",
    )
    return r


@phase("reconfig")
def reconfig(ctx: Ctx) -> PhaseResult:
    """A new tag set must be applied live, without a reboot or a reflash."""
    r = PhaseResult("reconfig")
    ctx.server.start()

    # Drop the slow group and the absent tag; move Line1.Temp to the slow rate.
    subset = [t for t in TAGS if t.variant is not None and not t.address.startswith("Line2.")]
    for t in subset:
        if t.address == "Line1.Temp":
            t.scan_rate_ms = SLOW_MS

    mark = ctx.monitor.mark() if ctx.monitor else 0
    ctx.publish_config(2, subset)
    ctx.applied_version = 2

    reported = ctx.expect_state(
        r,
        lambda x: x.get("cfg_v") == 2 and x.get("state") == RUNNING,
        "device applied config v2 live",
        timeout=240,
    )
    r.check(
        "every tag in v2 applied and none failed",
        reported.get("applied") == len(subset) and reported.get("failed") == 0,
        f"applied={reported.get('applied')} failed={reported.get('failed')} expected={len(subset)}",
    )

    if ctx.monitor:
        log = ctx.monitor.read_since(mark)
        r.check(
            "device logged the config transition without reconnecting",
            "config v1 -> v2" in log,
            (re.search(r"config v1 -> v2[^\n]*", log) or [""])[0]
            if re.search(r"config v1 -> v2[^\n]*", log)
            else "not found",
        )

    start = int(time.time() * 1000)
    batches = ctx.cloud.wait_for_telemetry(start, minimum=2, timeout_s=180)
    r.check(
        "telemetry is re-stamped with v2 and drops the removed tags",
        bool(batches)
        and all(b.get("v") == 2 for b in batches)
        and not any(
            a.startswith("Line2.") for a in rows_by_address(batches)
        ),
        f"{len(batches)} batches, versions={sorted({b.get('v') for b in batches})}, "
        f"addresses={sorted(rows_by_address(batches))}",
    )

    # restore for later phases
    for t in TAGS:
        if t.address == "Line1.Temp":
            t.scan_rate_ms = FAST_MS
    return r


@phase("ns_uri")
def ns_uri(ctx: Ctx) -> PhaseResult:
    """A wrong namespace index must be rescued by the namespace URI."""
    r = PhaseResult("ns_uri")
    ctx.server.start()
    subset = [t for t in TAGS if t.variant is not None]

    mark = ctx.monitor.mark() if ctx.monitor else 0
    # ns=99 is deliberately wrong. Without URI resolution every single item
    # would be rejected and the driver would bail with "all items rejected".
    ctx.publish_config(3, subset, ns=99, ns_uri=NAMESPACE_URI)
    ctx.applied_version = 3

    reported = ctx.expect_state(
        r,
        lambda x: x.get("cfg_v") == 3 and x.get("state") == RUNNING,
        "device recovered the right namespace from ns_uri despite ns=99",
        timeout=240,
    )
    r.check(
        "all items applied under the resolved namespace",
        reported.get("applied") == len(subset),
        f"applied={reported.get('applied')} expected={len(subset)}",
    )
    if ctx.monitor:
        log = ctx.monitor.read_since(mark)
        r.check(
            "device did NOT log a namespace fallback",
            "not published by the server" not in log,
            "",
        )
    return r


@phase("reject_security")
def reject_security(ctx: Ctx) -> PhaseResult:
    """A non-`None` security policy must be refused, never downgraded."""
    r = PhaseResult("reject_security")
    subset = [t for t in TAGS if t.variant is not None]

    ctx.publish_config(4, subset, sec_policy="Basic256Sha256", sec_mode="SignAndEncrypt")

    ok, reported = ctx.cloud.wait_for_reported(
        lambda x: bool(x.get("last_error"))
        and "Basic256Sha256" in str(x.get("last_error")),
        timeout_s=120,
        on_poll=lambda x: info(f"reported: last_error={x.get('last_error')} cfg_v={x.get('cfg_v')}"),
    )
    r.check(
        "device refused the secured config with an explicit error",
        ok,
        f"last_error={reported.get('last_error')}",
    )
    r.check(
        "device did NOT silently downgrade — it kept running the previous config",
        reported.get("cfg_v") == 3,
        f"cfg_v={reported.get('cfg_v')} (expected 3)",
    )
    ctx.cloud.clear_retained(f"cmd/{ctx.thing}/opcua/tags/v4")
    return r


@phase("reject_digest")
def reject_digest(ctx: Ctx) -> PhaseResult:
    """A bundle whose SHA-256 does not match the shadow must be refused."""
    r = PhaseResult("reject_digest")
    subset = [t for t in TAGS if t.variant is not None]

    bogus = "0" * 64
    ctx.publish_config(5, subset, sha256_override=bogus)

    ok, reported = ctx.cloud.wait_for_reported(
        lambda x: "sha256" in str(x.get("last_error", "")).lower(),
        timeout_s=120,
        on_poll=lambda x: info(f"reported: last_error={x.get('last_error')} cfg_v={x.get('cfg_v')}"),
    )
    r.check(
        "device refused the bundle on digest mismatch",
        ok,
        f"last_error={reported.get('last_error')}",
    )
    r.check(
        "device kept running the last known-good config",
        reported.get("cfg_v") == 3,
        f"cfg_v={reported.get('cfg_v')}",
    )
    ctx.cloud.clear_retained(f"cmd/{ctx.thing}/opcua/tags/v5")
    return r


@phase("server_down")
def server_down(ctx: Ctx) -> PhaseResult:
    """Kill the PLC mid-session; the gateway must back off and recover itself."""
    r = PhaseResult("server_down")

    # Get back to a known-good config first (the negative phases left v4/v5
    # rejected, so the device is still on v3).
    subset = [t for t in TAGS if t.variant is not None]
    ctx.publish_config(6, subset)
    ctx.applied_version = 6
    ctx.expect_state(
        r,
        lambda x: x.get("cfg_v") == 6 and x.get("state") == RUNNING,
        "baseline config v6 running before the fault",
        timeout=240,
    )

    mark = ctx.monitor.mark() if ctx.monitor else 0
    ctx.server.stop()

    ok, reported = ctx.cloud.wait_for_reported(
        lambda x: x.get("state") == ERROR,
        timeout_s=180,
        on_poll=lambda x: info(f"reported: state={x.get('state')} err={x.get('last_error')}"),
    )
    r.check("device noticed the server was gone", ok, f"state={reported.get('state')}")

    if ctx.monitor:
        log = ctx.monitor.read_since(mark)
        delays = [int(m) for m in re.findall(r"retrying in (\d+) ms", log)]
        r.check(
            "reconnect uses backoff, not a tight loop",
            bool(delays) and max(delays) >= 1000,
            f"retry delays observed: {delays[:8]}",
        )
        r.check(
            "the MQTT/OTA path stayed alive while OPC UA was down",
            "MQTT event channel closed" not in log and "panicked" not in log,
            "",
        )

    info("restarting the OPC UA server…")
    ctx.server.start()
    ok, reported = ctx.cloud.wait_for_reported(
        lambda x: x.get("state") == RUNNING,
        timeout_s=240,
        on_poll=lambda x: info(f"reported: state={x.get('state')} applied={x.get('applied')}"),
    )
    r.check("device reconnected on its own once the server came back", ok,
            f"state={reported.get('state')} applied={reported.get('applied')}")
    r.check(
        "all items re-created after the reconnect",
        reported.get("applied") == len(subset),
        f"applied={reported.get('applied')} expected={len(subset)}",
    )
    return r


@phase("disable")
def disable(ctx: Ctx) -> PhaseResult:
    """`enabled: false` must stop the driver without deleting the config."""
    r = PhaseResult("disable")
    subset = [t for t in TAGS if t.variant is not None]

    ctx.publish_config(6, subset, enabled=False)
    ok, reported = ctx.cloud.wait_for_reported(
        lambda x: x.get("state") == IDLE,
        timeout_s=180,
        on_poll=lambda x: info(f"reported: state={x.get('state')}"),
    )
    r.check("driver went idle on enabled=false", ok, f"state={reported.get('state')}")

    # The IoT rule -> CloudWatch hop lags by a few seconds, so batches published
    # just BEFORE the disable would otherwise land inside the observation
    # window and read as "still publishing". Let the pipeline drain first.
    info("letting the CloudWatch pipeline drain before measuring silence…")
    time.sleep(30)
    start = int(time.time() * 1000)
    time.sleep(45)
    quiet = ctx.cloud.telemetry_since(start)
    r.check("telemetry stopped while disabled", len(quiet) == 0, f"{len(quiet)} batches in 45 s")

    # Re-enable under a fresh version so the device treats it as a new config.
    ctx.publish_config(7, subset, enabled=True)
    ctx.applied_version = 7
    ctx.expect_state(
        r,
        lambda x: x.get("state") == RUNNING and x.get("cfg_v") == 7,
        "driver resumed on enabled=true",
        timeout=240,
    )
    return r


@phase("reboot")
def reboot(ctx: Ctx) -> PhaseResult:
    """After a power cycle the cached NVS bundle must be applied before the cloud answers."""
    r = PhaseResult("reboot")
    if not ctx.monitor:
        r.skipped = True
        r.note = "needs the serial monitor"
        return r

    ctx.server.start()
    mark = ctx.monitor.mark()

    # The serial port is exclusive, so the monitor has to let go before
    # `espflash reset` can drive DTR/RTS. Re-attach with --no-reset afterwards
    # so the boot we are about to inspect is the one the reset caused, not a
    # second one triggered by re-attaching.
    info("resetting the device over USB…")
    ctx.monitor.stop()
    time.sleep(1)
    reset = subprocess.run(
        ["espflash", "reset", "--port", ctx.monitor.port],
        cwd=REPO,
        capture_output=True,
        timeout=60,
    )
    ctx.monitor.start()
    r.check(
        "device reset over USB",
        reset.returncode == 0,
        reset.stderr.decode(errors="replace")[-160:] if reset.returncode else "",
    )

    hit = ctx.monitor.wait_for(
        r"booting with cached OPC UA config v\d+ \(\d+ tags\)", mark, timeout_s=120
    )
    r.check(
        "device applied the NVS-cached config at boot, before the shadow replied",
        bool(hit),
        hit or "not logged",
    )

    ctx.expect_state(
        r,
        lambda x: x.get("state") == RUNNING and x.get("cfg_v") == ctx.applied_version,
        f"device is running again on cfg_v={ctx.applied_version} after the reboot",
        timeout=300,
    )
    return r


# ---------------------------------------------------------------------------
# driver
# ---------------------------------------------------------------------------


def flash(port: str) -> None:
    banner("FLASHING FIRMWARE")
    cmd = [
        "espflash",
        "flash",
        "--port",
        port,
        "--partition-table",
        "partitions.csv",
        "--target-app-partition",
        "ota_0",
        "--non-interactive",
        str(REPO / "target/xtensa-esp32s3-espidf/release/esp32-opcua-gateway"),
    ]
    print("    $ " + " ".join(cmd), flush=True)
    proc = subprocess.run(cmd, cwd=REPO, timeout=900)
    if proc.returncode != 0:
        raise SystemExit(f"espflash failed with {proc.returncode}")


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--thing", required=True)
    p.add_argument("--server-host", required=True, help="LAN IP the DEVICE can reach")
    p.add_argument("--server-port", type=int, default=4855)
    p.add_argument("--server-path", default="/ergousha/test")
    p.add_argument("--port", help="serial port of the device")
    p.add_argument("--flash", action="store_true")
    p.add_argument("--phases", help="comma-separated subset of phases to run")
    p.add_argument("--region", default="eu-central-1")
    p.add_argument("--artifacts", default=str(HERE / "artifacts"))
    p.add_argument(
        "--cleanup",
        action="store_true",
        help="clear the retained tag bundles this run published",
    )
    args = p.parse_args()

    artifacts = Path(args.artifacts)
    artifacts.mkdir(parents=True, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    server_log = artifacts / f"opcua-server-{stamp}.log"
    serial_log = artifacts / f"device-serial-{stamp}.log"

    if args.flash:
        flash(args.port)

    server = OpcuaServer(args.server_host, args.server_port, args.server_path, server_log)
    monitor = SerialMonitor(args.port, serial_log) if args.port else None

    ctx = Ctx(
        cloud=Cloud(args.thing, args.region),
        server=server,
        monitor=monitor,
        thing=args.thing,
        t0_ms=int(time.time() * 1000),
    )

    selected = args.phases.split(",") if args.phases else list(PHASES)
    unknown = [s for s in selected if s not in PHASES]
    if unknown:
        raise SystemExit(f"unknown phases: {unknown}; known: {list(PHASES)}")

    results: list[PhaseResult] = []
    try:
        if monitor:
            monitor.start()
        for name in selected:
            banner(f"PHASE: {name}")
            try:
                results.append(PHASES[name](ctx))
            except Exception as exc:  # a phase blowing up is a failure, not a crash
                r = PhaseResult(name)
                r.check("phase completed without raising", False, f"{type(exc).__name__}: {exc}")
                results.append(r)
    finally:
        server.stop()
        if monitor:
            monitor.stop()
        if args.cleanup:
            # Retained messages outlive the test. Left behind, a later boot
            # would pull a tag bundle from a run nobody remembers.
            for version in range(1, 16):
                ctx.cloud.clear_retained(f"cmd/{args.thing}/opcua/tags/v{version}")
            info("cleared retained tag bundles")

    banner("SUMMARY")
    total = passed = 0
    for r in results:
        if r.skipped:
            print(f"  SKIP  {r.name} — {r.note}")
            continue
        n_ok = sum(1 for c in r.checks if c.ok)
        total += len(r.checks)
        passed += n_ok
        flag = "PASS" if r.ok else "FAIL"
        print(f"  {flag}  {r.name}: {n_ok}/{len(r.checks)}")
        for c in r.checks:
            if not c.ok:
                print(f"          ✗ {c.name} — {c.detail}")
    print(f"\n  {passed}/{total} checks passed")
    print(f"  serial log : {serial_log}")
    print(f"  server log : {server_log}")

    summary = {
        "thing": args.thing,
        "endpoint": server.endpoint,
        "phases": [
            {
                "name": r.name,
                "skipped": r.skipped,
                "checks": [{"name": c.name, "ok": c.ok, "detail": c.detail} for c in r.checks],
            }
            for r in results
        ],
        "passed": passed,
        "total": total,
    }
    (artifacts / f"summary-{stamp}.json").write_text(json.dumps(summary, indent=2))
    return 0 if passed == total else 1


if __name__ == "__main__":
    raise SystemExit(main())
