#!/usr/bin/env python3
"""Proves the test server behaves the way the firmware expects, before the
firmware is ever involved.

This client does deliberately the same sequence `src/opcua/driver.rs` does:

  1. connect anonymously with SecurityPolicy None,
  2. read `Server_NamespaceArray` (i=2255) to resolve the namespace URI,
  3. create one subscription per distinct scan rate,
  4. create monitored items in chunks, tolerating per-item failures,
  5. collect notifications for a few seconds.

If this passes and the device still fails, the fault is in the firmware or the
network path — not in the harness. That separation is the only reason this file
exists.
"""

import argparse
import asyncio
import logging
from collections import defaultdict

from asyncua import Client, ua

from tags import MISSING_ADDRESSES, NAMESPACE_URI, TAGS, bundle_groups

_log = logging.getLogger("selfcheck")

#: Matches `gateway_core::plan::MAX_ITEMS_PER_REQUEST`.
CHUNK = 50


class Collector:
    def __init__(self) -> None:
        self.samples: dict[str, list] = defaultdict(list)
        self.handles: dict[int, str] = {}

    def datachange_notification(self, node, val, data) -> None:
        address = str(node.nodeid.Identifier)
        status = data.monitored_item.Value.StatusCode
        self.samples[address].append((val, status))


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--seconds", type=float, default=8.0)
    parser.add_argument(
        "--ns-fallback",
        type=int,
        default=99,
        help="deliberately wrong index, to prove URI resolution is doing the work",
    )
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO, format="%(levelname)-5s %(name)s: %(message)s")
    logging.getLogger("asyncua").setLevel(logging.ERROR)

    failures: list[str] = []
    async with Client(url=args.endpoint) as client:
        _log.info("connected to %s", args.endpoint)

        # 2. namespace resolution, exactly as gateway_core::node::resolve_namespace
        array_node = client.get_node(ua.NodeId(2255, 0))
        namespaces = await array_node.read_value()
        if NAMESPACE_URI in namespaces:
            ns = namespaces.index(NAMESPACE_URI)
            _log.info("namespace %r resolved to index %d", NAMESPACE_URI, ns)
        else:
            ns = args.ns_fallback
            failures.append(f"server did not publish {NAMESPACE_URI!r}; array={namespaces}")
            _log.error("namespace URI missing, falling back to %d", ns)

        collector = Collector()
        applied, failed = 0, []

        # 3 + 4. one subscription per scan rate, chunked item creation
        for group in bundle_groups(TAGS):
            rate = group["r"]
            sub = await client.create_subscription(float(rate), collector)
            for start in range(0, len(group["a"]), CHUNK):
                chunk = group["a"][start : start + CHUNK]
                nodes = [client.get_node(ua.NodeId(a, ns)) for a in chunk]
                results = await sub.subscribe_data_change(
                    nodes, queuesize=1, sampling_interval=float(rate)
                )
                # asyncua returns an int handle per success and a StatusCode
                # per failure, in request order — the same positional contract
                # `session.rs::create_items` relies on.
                for address, result in zip(chunk, results):
                    if isinstance(result, ua.StatusCode):
                        failed.append((address, result))
                    else:
                        applied += 1
            _log.info("subscription @ %d ms: %d addresses", rate, len(group["a"]))

        _log.info("monitored items: %d applied, %d failed", applied, len(failed))
        for address, status in failed:
            _log.info("  failed %-20s %s", address, status)

        expected_failures = set(MISSING_ADDRESSES)
        actual_failures = {a for a, _ in failed}
        if actual_failures != expected_failures:
            failures.append(
                f"expected exactly {sorted(expected_failures)} to fail, got {sorted(actual_failures)}"
            )

        _log.info("collecting notifications for %.1f s ...", args.seconds)
        await asyncio.sleep(args.seconds)

    # 5. every created tag must have produced at least one notification.
    silent = [
        t.address
        for t in TAGS
        if t.variant is not None and not collector.samples.get(t.address)
    ]
    if silent:
        failures.append(f"no notification for {silent}")

    for tag in TAGS:
        if tag.variant is None:
            continue
        got = collector.samples.get(tag.address, [])
        if not got:
            continue
        value, status = got[-1]
        _log.info(
            "  %-20s n=%-3d last=%-28r %s",
            tag.address,
            len(got),
            value if not isinstance(value, (bytes, list)) else type(value).__name__,
            "" if status.is_good() else status.name,
        )

    # The static tag is the report-by-exception canary: if the server is being
    # polled rather than reporting on change, it would have many samples.
    static = collector.samples.get("Line1.Static", [])
    if len(static) > 2:
        failures.append(
            f"Line1.Static reported {len(static)} times; the subscription is polling, "
            "not reporting by exception"
        )

    faulty = collector.samples.get("Line1.Faulty", [])
    if faulty and faulty[-1][1].is_good():
        failures.append("Line1.Faulty did not carry a Bad StatusCode")

    if failures:
        for f in failures:
            _log.error("FAIL %s", f)
        return 1
    _log.info("PASS - the server behaves as the firmware expects")
    return 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
