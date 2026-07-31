#!/usr/bin/env python3
"""An OPC UA server that stands in for the PLC the gateway will really talk to.

Configured to match exactly what the firmware supports in phase 1
(docs/OPCUA_CLIENT_REQUIREMENTS.md §9): SecurityPolicy None, MessageSecurityMode
None, anonymous identity. That is not laziness — the device has no filesystem
and therefore no PKI, so a server demanding anything else could not be tested
against this firmware at all.

The node set comes from `tags.py`, which the cloud-side publisher reads too, so
the server and the tag bundle can never drift apart.

Run:
    .venv/bin/python opcua_test_server.py --host 0.0.0.0 --port 4855

Control (so the scenario runner can drive fault injection without a restart):
    a tiny line-oriented command channel on stdin —
      `fault on` / `fault off`   toggle the Bad StatusCode on Line1.Faulty
      `freeze`   / `thaw`        stop/resume value updates
      `quit`                     shut the server down
"""

import argparse
import asyncio
import datetime
import logging
import sys
import uuid

from asyncua import Server, ua

from tags import NAMESPACE_URI, SERVER_TAGS, TAGS, Tag

_log = logging.getLogger("test-server")

#: How often values are stepped. Half the fastest scan rate, so the 1 s
#: monitored items always have a fresh value to report and the report-by-
#: exception behaviour of `Line1.Static` stands out against it.
TICK_SECONDS = 0.5


def _variant_type(name: str) -> tuple[ua.VariantType, bool]:
    """Maps a `tags.py` variant name onto a `ua.VariantType` and array flag."""
    if name.endswith("Array"):
        return getattr(ua.VariantType, name[: -len("Array")]), True
    return getattr(ua.VariantType, name), False


def _to_ua(tag: Tag, value):
    """Wraps a Python value in the Variant the tag's node was declared with."""
    vtype, is_array = _variant_type(tag.variant)
    if vtype is ua.VariantType.Guid and isinstance(value, str):
        value = uuid.UUID(value)
    if vtype is ua.VariantType.ByteString and isinstance(value, (bytes, bytearray)):
        value = bytes(value)
    return ua.Variant(
        value,
        vtype,
        is_array=is_array if is_array else None,
    )


class TestServer:
    def __init__(self, host: str, port: int, path: str) -> None:
        self.endpoint = f"opc.tcp://{host}:{port}{path}"
        self.server = Server()
        self.nodes: dict[str, object] = {}
        self.ns_index: int = 0
        self.tick = 0
        self.frozen = False
        # Starts on so the very first sync already has a Bad-status tag to
        # report; the runner turns it off in the fault-injection phase.
        self.fault = True

    async def setup(self) -> None:
        await self.server.init()
        self.server.set_endpoint(self.endpoint)
        self.server.set_server_name("Ergousha OPC UA Test Server")

        # Phase 1 of the firmware is unsecured by explicit design decision (D2).
        # Anything else here would make the gateway refuse to connect.
        self.server.set_security_policy([ua.SecurityPolicyType.NoSecurity])
        self.server.set_identity_tokens([ua.AnonymousIdentityToken])

        self.ns_index = await self.server.register_namespace(NAMESPACE_URI)
        _log.info("namespace %r registered at index %d", NAMESPACE_URI, self.ns_index)

        objects = self.server.nodes.objects
        folder = await objects.add_folder(
            ua.NodeId("Plant", self.ns_index), "Plant"
        )

        for tag in SERVER_TAGS:
            vtype, is_array = _variant_type(tag.variant)
            node = await folder.add_variable(
                # A STRING NodeId, because that is what a real SCADA export
                # looks like and what the bundle's `id_type: "s"` produces.
                ua.NodeId(tag.address, self.ns_index),
                tag.address,
                _to_ua(tag, tag.initial),
                varianttype=vtype,
            )
            # Writable so a future phase can drive values from the cloud side;
            # the scenario itself only ever writes from the tick loop.
            await node.set_writable()
            self.nodes[tag.address] = node
            _log.info(
                "  ns=%d;s=%-20s %-12s %s",
                self.ns_index,
                tag.address,
                tag.variant,
                tag.proves,
            )

        for address in [t.address for t in TAGS if t.variant is None]:
            _log.info("  (not created) %-20s %s", address, "-> expected to fail")

    async def _write(self, tag: Tag, value, bad: bool) -> None:
        """Writes a value, optionally with a Bad StatusCode.

        `set_attribute_value` is used rather than `write_value` on purpose: the
        public write path validates the StatusCode and refuses to store a Bad
        one, which is precisely the case the gateway needs to see.
        """
        node = self.nodes[tag.address]
        status = ua.StatusCode(getattr(ua.StatusCodes, tag.bad_status)) if bad else ua.StatusCode()
        dv = ua.DataValue(
            Value=_to_ua(tag, value),
            StatusCode=status,
            SourceTimestamp=datetime.datetime.now(datetime.timezone.utc),
        )
        await self.server.write_attribute_value(
            node.nodeid, dv, ua.AttributeIds.Value
        )

    async def tick_loop(self) -> None:
        while True:
            await asyncio.sleep(TICK_SECONDS)
            if self.frozen:
                continue
            self.tick += 1
            for tag in SERVER_TAGS:
                bad = bool(tag.bad_status) and self.fault
                if tag.step is None and not bad:
                    # Static tags are written exactly once, at startup. Writing
                    # them every tick would mask whether the gateway is doing
                    # report-by-exception or dumb polling.
                    continue
                value = tag.step(self.tick) if tag.step else tag.initial
                try:
                    await self._write(tag, value, bad)
                except Exception as exc:  # pragma: no cover - diagnostics only
                    _log.warning("write %s failed: %s", tag.address, exc)

    async def command_loop(self) -> None:
        """Reads one-word commands from stdin so the runner can inject faults."""
        loop = asyncio.get_running_loop()
        reader = asyncio.StreamReader()
        try:
            await loop.connect_read_pipe(
                lambda: asyncio.StreamReaderProtocol(reader), sys.stdin
            )
        except (OSError, ValueError) as exc:
            # kqueue refuses /dev/null and regular files. Running without a
            # command channel is fine — fault injection simply is not available
            # — but silently dying and taking the server with it is not.
            _log.warning("no command channel on stdin (%s); running without it", exc)
            while True:
                await asyncio.sleep(3600)
        while True:
            line = await reader.readline()
            if not line:
                return
            cmd = line.decode().strip().lower()
            if cmd == "fault on":
                self.fault = True
            elif cmd == "fault off":
                self.fault = False
                # Clear the Bad status immediately rather than at the next tick.
                for tag in SERVER_TAGS:
                    if tag.bad_status:
                        await self._write(tag, 123.45, bad=False)
            elif cmd == "freeze":
                self.frozen = True
            elif cmd == "thaw":
                self.frozen = False
            elif cmd == "quit":
                _log.info("shutdown requested")
                raise SystemExit(0)
            else:
                _log.warning("unknown command %r", cmd)
                continue
            _log.info("command %r applied", cmd)

    async def run(self) -> None:
        await self.setup()
        async with self.server:
            _log.info("serving on %s", self.endpoint)
            _log.info("READY")  # the scenario runner waits for this line
            sys.stdout.flush()
            await asyncio.gather(self.tick_loop(), self.command_loop())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=4855)
    parser.add_argument("--path", default="/ergousha/test")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)-5s %(name)s: %(message)s",
    )
    # asyncua is extremely chatty at INFO and drowns out the test output.
    logging.getLogger("asyncua").setLevel(
        logging.INFO if args.verbose else logging.WARNING
    )

    server = TestServer(args.host, args.port, args.path)
    try:
        asyncio.run(server.run())
    except (KeyboardInterrupt, SystemExit):
        _log.info("stopped")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
