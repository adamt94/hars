#!/usr/bin/env python3
"""Vectors for adding a LIFX bulb from the UI.

    .venv/bin/python -I scripts/vectors/lifx_flow.py
    cargo test -p ha-api config_entries && cargo test -p ha-integrations lifx

Runs on `config_flow.py`'s Home Assistant 2026.6.1 (its `run`) with `lifx`
and aiolifx 1.2.1. Nothing reaches the network: every UDP endpoint HA opens
goes through this script. One to a made-up bulb address (TEST-NET-1,
`192.0.2.x`) is connected to a fake bulb on 127.0.0.1 instead, and what the
fake sends back arrives as if from the bulb's address. A discovery socket
is bound to 127.0.0.1, and its broadcast goes to a fake that has each bulb
that answers discovery answer from its own socket. An endpoint to anything
else, or a datagram to any other address, stops the script. Every datagram
HA sends is recorded as it was sent: where to, its type, whether it was
tagged, its target, its flags, its sequence number and its payload (not its
source id, which aiolifx picks at random). The coordinator's polls after
an entry's set-up (untagged `LightGet`s, every 10 s) are left out: when they
fall depends on how long a step took.

The fake bulbs (serials, addresses, labels and groups are made up):

- `192.0.2.20`: a LIFX Color (product 91), firmware 3.70, "Desk Lamp" in
  "Office". `192.0.2.25` reaches the same bulb (it moved).
- `192.0.2.21`: a LIFX Mini White (product 51: one colour temperature),
  firmware 2.77, "Hall Light" in "Hallway".
- `192.0.2.23`: a LIFX Switch (product 70), which HA's flow refuses.
- `192.0.2.24`: nothing answers there.
- `192.0.2.26`: a bulb that answers discovery and then nothing more.

An `api` step changes them from then on (see `Lifx.set`).

HA's `lifx` also scans the network itself, 5 s after it is set up and every
15 minutes, and starts a discovery flow for each bulb it finds. `hars` has
no such flows (it starts none by itself), so that scan is put off past the
end of each run (`DISCOVERY_COOLDOWN` and `DISCOVERY_INTERVAL`); the scan the
user's flow makes is HA's own.

It writes
- crates/ha-api/src/config_entries/vectors/lifx_flow.json: for each scenario
  its steps, as `config_flow.py` writes them, each with the datagrams HA
  sent during it, and for the scenarios that add a bulb the registry rows
  and states;
- crates/ha-integrations/src/lifx/vectors/fresh_setup.json: the entry, the
  device and entity rows, the areas and the states HA has after the colour
  bulb is added by its address (`colour`), and after the white bulb is
  picked from discovery (`white`).

Placeholders are `config_flow.py`'s. Run it twice: the output must not
change.
"""

import asyncio
import importlib.metadata
import json
import os
import pathlib
import struct
import sys

# After the standard library: this directory has an `http.py` of its own.
sys.path.append(str(pathlib.Path(__file__).resolve().parent))

import config_flow as cf  # noqa: E402

OUT = cf.API_OUT / "lifx_flow.json"
LIFX = cf.ROOT / "crates/ha-integrations/src/lifx/vectors"
AIOLIFX = "1.2.1"
PORT = 56700
BROADCAST = "255.255.255.255"

COLOUR, WHITE, SWITCH, SILENT, ALIAS, GHOST = (
    "192.0.2.20", "192.0.2.21", "192.0.2.23", "192.0.2.24", "192.0.2.25", "192.0.2.26")
# A bulb's MAC address is its serial, one up from firmware 3.70: no fake
# bulb's serial is another's MAC.
COLOUR_SERIAL = "d0:73:d5:00:00:20"
WHITE_SERIAL = "d0:73:d5:00:00:41"
GHOST_SERIAL = "d0:73:d5:00:00:26"
OTHER_SERIAL = "d0:73:d5:00:00:2f"

# Message types.
GET_SERVICE, STATE_SERVICE = 2, 3
GET_HOST_FIRMWARE, STATE_HOST_FIRMWARE = 14, 15
GET_WIFI_INFO, STATE_WIFI_INFO = 16, 17
GET_LABEL, STATE_LABEL = 23, 25
GET_VERSION, STATE_VERSION = 32, 33
ACK = 45
GET_GROUP, STATE_GROUP = 51, 53
LIGHT_GET, LIGHT_STATE = 101, 107
LIGHT_SET_POWER, LIGHT_SET_WAVEFORM_OPTIONAL = 117, 119


def serial_bytes(serial):
    return bytes(int(b, 16) for b in serial.split(":"))


def frame(source, target, seq, ty, payload, res=False):
    """A LIFX message as a bulb sends it: untagged, from its serial."""
    size = 36 + len(payload)
    head = struct.pack("<HHI", size, 0x1400, source)
    head += target + b"\0\0" + b"\0" * 6 + bytes([int(res), seq]) + b"\0" * 8
    head += struct.pack("<HH", ty, 0)
    return head + payload


def decode(data):
    size, proto, source = struct.unpack_from("<HHI", data)
    target = data[8:14]
    flags, seq = data[22], data[23]
    ty = struct.unpack_from("<H", data, 32)[0]
    return {"size": size, "tagged": bool(proto & 0x2000), "source": source,
            "target": ":".join(f"{b:02x}" for b in target),
            "ack": bool(flags & 2), "res": bool(flags & 1), "seq": seq, "type": ty,
            "payload": data[36:size].hex()}


def text(value, size):
    return value.encode().ljust(size, b"\0")


class Bulb:
    """A fake bulb's state and how it answers."""

    def __init__(self, ip, serial, label, group, product, firmware, discovery=True,
                 silent=False, unicast=True):
        self.ip = ip
        self.serial = serial
        self.label = label
        self.group = group
        self.product = product
        self.firmware = firmware
        self.discovery = discovery
        self.silent = silent
        # Whether it answers what is sent to its address.
        self.unicast = unicast
        self.power = 65535
        self.color = [0, 0, 39321, 2700]
        # After it has told its label, it answers as another bulb.
        self.swaps = False
        self.swapped = False
        self.transport = None
        self.port = None

    def reply(self, msg):
        """What the bulb sends back to `msg`: a list of datagrams."""
        if self.silent:
            return []
        serial = serial_bytes(OTHER_SERIAL if self.swapped else self.serial)
        source, seq, ty = msg["source"], msg["seq"], msg["type"]
        payload = bytes.fromhex(msg["payload"])
        out = []

        def state(kind, body):
            out.append(frame(source, serial, seq, kind, body))

        if msg["ack"]:
            out.append(frame(source, serial, seq, ACK, b""))
        major, minor = self.firmware
        if ty == GET_SERVICE:
            state(STATE_SERVICE, struct.pack("<BI", 1, PORT))
        elif ty == GET_HOST_FIRMWARE:
            state(STATE_HOST_FIRMWARE, struct.pack("<QQI", 0, 0, major << 16 | minor))
        elif ty == GET_WIFI_INFO:
            state(STATE_WIFI_INFO, struct.pack("<fIIh", 1e-5, 0, 0, 0))
        elif ty == GET_LABEL:
            state(STATE_LABEL, text(self.label, 32))
            if self.swaps:
                self.swapped = True
        elif ty == GET_VERSION:
            state(STATE_VERSION, struct.pack("<III", 1, self.product, 0))
        elif ty == GET_GROUP:
            state(STATE_GROUP, b"\x11" * 16 + text(self.group, 32) + struct.pack("<Q", 0))
        elif ty == LIGHT_GET:
            body = struct.pack("<HHHHhH", *self.color, 0, self.power)
            state(LIGHT_STATE, body + text(self.label, 32) + struct.pack("<Q", 0))
        elif ty == LIGHT_SET_POWER:
            self.power = struct.unpack_from("<H", payload)[0]
        elif ty == LIGHT_SET_WAVEFORM_OPTIONAL:
            color = list(struct.unpack_from("<HHHH", payload, 2))
            sets = payload[17:21]
            self.color = [c if s else old for c, s, old in zip(color, sets, self.color)]
        return out


class Lifx:
    """The bulbs and the discovery fake, and the UDP HA sent to them."""

    domain = "lifx"

    def __init__(self):
        self.bulbs = {}
        self.requests = []
        self.transports = []
        self.discovery = None
        self.original = None
        self.make_bulbs()

    def make_bulbs(self):
        colour = Bulb(COLOUR, COLOUR_SERIAL, "Desk Lamp", "Office", 91, (3, 70))
        for bulb in (
            colour,
            Bulb(WHITE, WHITE_SERIAL, "Hall Light", "Hallway", 51, (2, 77)),
            Bulb(SWITCH, "d0:73:d5:00:00:23", "Wall Switch", "Office", 70, (3, 70),
                 discovery=False),
            Bulb(SILENT, "d0:73:d5:00:00:24", "Nobody", "Office", 91, (3, 70),
                 discovery=False, silent=True),
            Bulb(GHOST, GHOST_SERIAL, "Ghost", "Attic", 91, (3, 70), unicast=False),
        ):
            self.bulbs[bulb.ip] = bulb
        self.bulbs[ALIAS] = colour
        self.discovering = [COLOUR, WHITE]

    def set(self, mode):
        """`discovery_none` (no bulb answers discovery), `discovery_ghost`
        (only the bulb that then says nothing more answers), `colour_swaps`
        (once the colour bulb has told its label, another bulb answers
        there), `colour_moves` (the colour bulb answers discovery from its
        new address only)."""
        if mode == "discovery_none":
            self.discovering = []
        elif mode == "discovery_ghost":
            self.discovering = [GHOST]
        elif mode == "colour_swaps":
            self.bulbs[COLOUR].swaps = True
        elif mode == "ok":
            pass
        else:
            sys.exit(f"no mode {mode}")

    def take(self):
        taken, self.requests = self.requests, []
        return taken

    def note(self, to, data):
        msg = decode(data)
        # The coordinator's polls: when they fall depends on the steps.
        if msg["type"] == LIGHT_GET and not msg["tagged"]:
            return
        self.requests.append({"to": to, **{k: v for k, v in msg.items()
                                           if k not in ("source", "size")}})

    async def start(self):
        import homeassistant.components.lifx as component
        from datetime import timedelta

        # HA's own scan, which starts discovery flows: past the run's end.
        component.DISCOVERY_COOLDOWN = 10**7
        component.DISCOVERY_INTERVAL = timedelta(days=3650)

        loop = asyncio.get_running_loop()
        self.original = original = asyncio.BaseEventLoop.create_datagram_endpoint
        fake = self

        class Answering(asyncio.DatagramProtocol):
            def __init__(self, bulb):
                self.bulb = bulb

            def connection_made(self, transport):
                self.bulb.transport = transport

            def datagram_received(self, data, addr):
                if not self.bulb.unicast:
                    return
                for out in self.bulb.reply(decode(data)):
                    self.bulb.transport.sendto(out, addr)

        class Discovery(asyncio.DatagramProtocol):
            def connection_made(self, transport):
                self.transport = transport

            def datagram_received(self, data, addr):
                msg = decode(data)
                for ip in fake.discovering:
                    bulb = fake.bulbs[ip]
                    for out in Bulb.reply(bulb, msg):
                        bulb.transport.sendto(out, addr)

        seen = set()
        for bulb in self.bulbs.values():
            if id(bulb) in seen:
                continue
            seen.add(id(bulb))
            transport, _ = await original(loop, lambda b=bulb: Answering(b),
                                          local_addr=("127.0.0.1", 0))
            bulb.port = transport.get_extra_info("sockname")[1]
            self.transports.append(transport)
        transport, self.discovery = await original(loop, Discovery,
                                                   local_addr=("127.0.0.1", 0))
        self.transports.append(transport)
        self.discovery_port = transport.get_extra_info("sockname")[1]
        by_port = {b.port: ip for ip, b in self.bulbs.items() if ip != ALIAS}

        class Recording:
            """HA's transport: what it sends is noted and goes to a fake."""

            def __init__(self, transport, peer):
                self.transport = transport
                self.peer = peer

            def sendto(self, data, addr=None):
                if addr is None:
                    fake.note(self.peer, data)
                    return self.transport.sendto(data)
                if addr != (BROADCAST, PORT):
                    sys.exit(f"HA sent a datagram to {addr}")
                fake.note("broadcast", data)
                self.transport.sendto(data, ("127.0.0.1", fake.discovery_port))

            def __getattr__(self, name):
                return getattr(self.transport, name)

        class Mapped(asyncio.DatagramProtocol):
            """HA's protocol: what the fakes send seems to come from the bulb."""

            def __init__(self, inner, peer):
                self.inner = inner
                self.peer = peer
                self.recording = None

            def connection_made(self, transport):
                self.recording = Recording(transport, self.peer)
                self.inner.connection_made(self.recording)

            def datagram_received(self, data, addr):
                ip = by_port.get(addr[1], self.peer)
                self.inner.datagram_received(data, (ip, PORT))

            def error_received(self, exc):
                self.inner.error_received(exc)

            def connection_lost(self, exc):
                self.inner.connection_lost(exc)

        async def endpoint(loop, factory, local_addr=None, remote_addr=None, **kwargs):
            if remote_addr is not None:
                host, port = remote_addr[0], remote_addr[1]
                bulb = fake.bulbs.get(host)
                if bulb is None or port != PORT:
                    sys.exit(f"HA opened an endpoint to {remote_addr}")
                kwargs.pop("family", None)
                mapped = None

                def wrap():
                    nonlocal mapped
                    mapped = Mapped(factory(), host)
                    return mapped

                await original(loop, wrap, remote_addr=("127.0.0.1", bulb.port), **kwargs)
                return mapped.recording, mapped.inner
            if local_addr is None:
                sys.exit("HA opened an endpoint with no address")
            mapped = None

            def wrap():
                nonlocal mapped
                mapped = Mapped(factory(), None)
                return mapped

            await original(loop, wrap, local_addr=("127.0.0.1", 0), **kwargs)
            return mapped.recording, mapped.inner

        asyncio.BaseEventLoop.create_datagram_endpoint = endpoint

    async def stop(self):
        asyncio.BaseEventLoop.create_datagram_endpoint = self.original
        for transport in self.transports:
            transport.close()


START = cf.http("POST", cf.FLOW, {"handler": "lifx", "show_advanced_options": False})
FLOW1 = f"{cf.FLOW}/<flow:1>"
TYPED = cf.http("POST", FLOW1, {"host": COLOUR})
ADD = [START, TYPED]


def flow(n):
    return f"{cf.FLOW}/<flow:{n}>"


SCENARIOS = [
    ("add",
     "The colour bulb's address is typed in: the entry is made and set up.",
     cf.SUBSCRIBE,
     [cf.ENTRIES, START, cf.http("GET", FLOW1), cf.ws({"type": "config_entries/flow/progress"}),
      TYPED, cf.ENTRIES, cf.http("GET", FLOW1)]),
    ("pick",
     "No address: discovery lists the colour and the white bulb, the white one"
     " is picked, after what the form refuses.",
     cf.SUBSCRIBE,
     [START, cf.http("POST", FLOW1, {}),
      cf.http("POST", FLOW1, {"device": "nobody"}),
      cf.http("POST", FLOW1, {}),
      cf.http("POST", FLOW1, {"device": WHITE_SERIAL, "host": WHITE}),
      cf.http("POST", FLOW1, {"device": WHITE_SERIAL}), cf.ENTRIES]),
    ("pick_none",
     "No address, and no bulb answers discovery.",
     cf.SUBSCRIBE,
     [cf.api("discovery_none"), START, cf.http("POST", FLOW1, {"host": ""})]),
    ("typed_fails",
     "An address where nothing answers, then a LIFX Switch, then what the form"
     " refuses.",
     cf.SUBSCRIBE,
     [START, cf.http("POST", FLOW1, {"host": SILENT}),
      cf.http("POST", FLOW1, {"host": SWITCH}),
      cf.http("POST", FLOW1, {"host": 5}),
      cf.http("POST", FLOW1, {"host": COLOUR, "port": 1}),
      cf.http("DELETE", FLOW1)]),
    ("pick_gone",
     "A bulb that answers discovery and then nothing more is picked.",
     cf.SUBSCRIBE,
     [cf.api("discovery_ghost"), START, cf.http("POST", FLOW1, {}),
      cf.http("POST", FLOW1, {"device": GHOST_SERIAL})]),
    ("configured",
     "With the colour bulb set up: its address again; discovery, which leaves"
     " it out; then its new address, which moves the entry there.",
     cf.SUBSCRIBE,
     [*ADD, START, cf.http("POST", flow(2), {"host": COLOUR}),
      START, cf.http("POST", flow(3), {}), cf.http("DELETE", flow(3)),
      START, cf.http("POST", flow(4), {"host": ALIAS}), cf.ENTRIES]),
    ("not_ready",
     "Once the flow has the colour bulb's label, another bulb answers at its"
     " address: the entry waits for a retry, and is removed.",
     cf.SUBSCRIBE,
     [START, cf.api("colour_swaps"), TYPED, cf.ENTRIES,
      cf.http("DELETE", f"{cf.ENTRY}/<entry:1>"), cf.ENTRIES]),
    ("remove",
     "A set-up bulb is removed: its entry, device, entities and states go.",
     cf.SUBSCRIBE,
     [*ADD, cf.http("DELETE", f"{cf.ENTRY}/<entry:1>"), cf.ENTRIES]),
]


async def main():
    cf.check_versions()
    found = importlib.metadata.version("aiolifx")
    if found != AIOLIFX:
        sys.exit(f"aiolifx is {found}, the vectors are pinned to {AIOLIFX}")
    scenarios = []
    fresh = {}
    for name, doc, subscriptions, steps in SCENARIOS:
        result = await cf.run(subscriptions, json.loads(json.dumps(steps)), Lifx())
        result.pop("snapshots")
        rows = {k: result.pop(k) for k in ("devices", "entities", "states", "areas")}
        if name in ("add", "pick"):
            fresh["colour" if name == "add" else "white"] = {
                "entries": result["entries"], **rows}
            result.update(rows)
        scenarios.append({"name": name, "doc": doc, **result})
    cf.write(LIFX / "fresh_setup.json", fresh)
    cf.write(OUT, {"scenarios": scenarios})


if __name__ == "__main__":
    asyncio.run(main())
    # As `config_flow.py`: skip CPython's teardown.
    sys.stdout.flush()
    os._exit(0)
