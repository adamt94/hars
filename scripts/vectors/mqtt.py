#!/usr/bin/env python3
"""Vectors for `mqtt`: the client, discovery of sensors and binary sensors,
and the `mqtt.publish` action, as Home Assistant 2026.6.1 runs them.

    python3.14 -m venv .venv-mqtt
    .venv-mqtt/bin/pip install homeassistant==2026.6.1 paho-mqtt==2.1.0 \
        aiohasupervisor==0.4.3
    .venv-mqtt/bin/python -I scripts/vectors/mqtt.py          # everything
    .venv-mqtt/bin/python -I scripts/vectors/mqtt.py tables   # tables.rs only
    cargo test -p ha-integrations mqtt

(`aiohasupervisor` because `mqtt`'s config flow imports `hassio`.)

Home Assistant 2026.6.1's `components/mqtt` on paho-mqtt 2.1.0 is started
with one `mqtt` config entry against a fake broker on 127.0.0.1 (in this
script: it speaks MQTT 3.1.1 and 5, answers every packet as mosquitto
would, keeps retained messages and records every packet Home Assistant
sends it). Nothing else is reached. Each scenario is a list of steps (the
broker publishes, drops the connection, time passes in real time, an
action is called) and, after each step, what Home Assistant did: the
packets it sent the broker (decoded), the states of the `mqtt` entities
that changed, and the WARNING and ERROR lines `mqtt` logged. At the end:
the entry as stored and the `mqtt` rows of the entity and device
registries.

The discovery payloads are what Zigbee2MQTT 2.6 publishes for an Aqara
temperature and humidity sensor (WSDCGQ11LM), an Aqara door and window
sensor (MCCGQ11LM) and its bridge, what Tasmota's legacy discovery
(`SetOption19 1`) publishes for a DS18B20 sensor and its status sensor, and
one device-based discovery message (`<prefix>/device/<id>/config`) of a
made-up weather station; addresses, names and serial numbers are made up.

It writes, in crates/ha-integrations/src/mqtt/vectors/:

- `v5.json`: an entry as 2026.6.1's config flow makes it (version 2.1,
  `protocol` 5) on a broker that speaks MQTT 5;
- `v311.json`: an entry from an older release (version 1.1, everything in
  `data`, no `protocol`: 3.1.1, a client id, keepalive, discovery prefix,
  birth and will of its own) on a broker that refuses MQTT 5: the entry is
  migrated to 1.2, the try at MQTT 5 fails, and 3.1.1 is used;

and the tables `hars` compiles in, from 2026.6.1's sources:

- `crates/ha-integrations/src/mqtt/tables.rs`: `abbreviations.py`'s
  three tables, `SUPPORTED_COMPONENTS`, and the English names of the
  `sensor` and `binary_sensor` device classes (what an unnamed entity is
  called, from those components' `strings.json`), and the device classes
  each accepts.

Packet ids, subscription identifiers, client ids and times are kept as Home
Assistant sent them (client ids are random: written as `<random>`); the
Rust test numbers ids by order of appearance on both sides. A step's
SUBSCRIBEs of plain topics, what the broker sent and the log lines are
sorted: HA's order there follows its tasks and Python's set order. Run it
twice: only times in `at` may change.
"""

import asyncio
import importlib.metadata
import json
import logging
import os
import pathlib
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
from types import MappingProxyType

PINNED = {"homeassistant": "2026.6.1", "paho-mqtt": "2.1.0", "aiohasupervisor": "0.4.3"}
ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT = ROOT / "crates/ha-integrations/src/mqtt/vectors"
TABLES = ROOT / "crates/ha-integrations/src/mqtt/tables.rs"


def check_versions():
    for pkg, want in PINNED.items():
        have = importlib.metadata.version(pkg)
        if have != want:
            sys.exit(f"{pkg} is {have}; these vectors are made with {want}")


def write(name, data):
    (OUT / name).write_text(json.dumps(data, indent=1, ensure_ascii=False) + "\n")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


# ---------------------------------------------------------------- the wire


def varint(n):
    out = bytearray()
    while True:
        b = n % 128
        n //= 128
        out.append(b | (0x80 if n else 0))
        if not n:
            return bytes(out)


def read_varint(buf, i):
    mult, value = 1, 0
    while True:
        b = buf[i]
        i += 1
        value += (b & 0x7F) * mult
        mult *= 128
        if not b & 0x80:
            return value, i


def str16(s):
    b = s.encode() if isinstance(s, str) else s
    return struct.pack("!H", len(b)) + b


def read16(buf, i):
    (n,) = struct.unpack_from("!H", buf, i)
    return bytes(buf[i + 2:i + 2 + n]), i + 2 + n


def props_decode(kind, buf, i):
    """MQTT 5 properties at `i`: as paho reads them, by paho's names."""
    from paho.mqtt.packettypes import PacketTypes
    from paho.mqtt.properties import Properties

    props = Properties(getattr(PacketTypes, kind))
    _, length = props.unpack(buf[i:])
    out = {}
    for name in props.names:
        compressed = name.replace(" ", "")
        if hasattr(props, compressed):
            out[compressed] = getattr(props, compressed)
    return out, i + length


def props_pack(kind, **values):
    from paho.mqtt.packettypes import PacketTypes
    from paho.mqtt.properties import Properties

    props = Properties(getattr(PacketTypes, kind))
    for k, v in values.items():
        setattr(props, k, v)
    return props.pack()


def text(b):
    try:
        return b.decode()
    except UnicodeDecodeError:
        return {"hex": b.hex()}


def decode(first, body, v5):
    """One packet Home Assistant sent, as data."""
    kind = first >> 4
    flags = first & 0x0F
    i = 0
    if kind == 1:
        name, i = read16(body, i)
        level, cflags, keepalive = struct.unpack_from("!BBH", body, i)
        i += 4
        out = {"type": "CONNECT", "protocol": name.decode(), "level": level,
               "clean": bool(cflags & 0x02), "keepalive": keepalive}
        is_v5 = level == 5
        if is_v5:
            out["properties"], i = props_decode("CONNECT", body, i)
        client_id, i = read16(body, i)
        out["client_id"] = client_id.decode()
        if cflags & 0x04:
            will = {"qos": (cflags >> 3) & 3, "retain": bool(cflags & 0x20)}
            if is_v5:
                will["properties"], i = props_decode("WILLMESSAGE", body, i)
            topic, i = read16(body, i)
            payload, i = read16(body, i)
            will |= {"topic": topic.decode(), "payload": text(payload)}
            out["will"] = will
        if cflags & 0x80:
            user, i = read16(body, i)
            out["username"] = user.decode()
        if cflags & 0x40:
            password, i = read16(body, i)
            out["password"] = password.decode()
        return out
    if kind == 3:
        qos = (flags >> 1) & 3
        topic, i = read16(body, i)
        out = {"type": "PUBLISH", "topic": topic.decode(), "qos": qos,
               "retain": bool(flags & 1), "dup": bool(flags & 8)}
        if qos:
            (out["packet_id"],) = struct.unpack_from("!H", body, i)
            i += 2
        if v5:
            out["properties"], i = props_decode("PUBLISH", body, i)
        out["payload"] = text(bytes(body[i:]))
        return out
    if kind in (4, 5, 6, 7):
        names = {4: "PUBACK", 5: "PUBREC", 6: "PUBREL", 7: "PUBCOMP"}
        (packet_id,) = struct.unpack_from("!H", body, 0)
        out = {"type": names[kind], "packet_id": packet_id}
        if len(body) > 2:
            out["reason"] = body[2]
        return out
    if kind == 8:
        (packet_id,) = struct.unpack_from("!H", body, 0)
        i = 2
        out = {"type": "SUBSCRIBE", "packet_id": packet_id}
        if v5:
            out["properties"], i = props_decode("SUBSCRIBE", body, i)
        topics = []
        while i < len(body):
            topic, i = read16(body, i)
            topics.append([topic.decode(), body[i]])
            i += 1
        out["topics"] = topics
        return out
    if kind == 10:
        (packet_id,) = struct.unpack_from("!H", body, 0)
        i = 2
        out = {"type": "UNSUBSCRIBE", "packet_id": packet_id}
        if v5:
            out["properties"], i = props_decode("UNSUBSCRIBE", body, i)
        topics = []
        while i < len(body):
            topic, i = read16(body, i)
            topics.append(topic.decode())
        out["topics"] = topics
        return out
    if kind == 12:
        return {"type": "PINGREQ"}
    if kind == 14:
        out = {"type": "DISCONNECT"}
        if body:
            out["reason"] = body[0]
        return out
    return {"type": f"unknown {kind}", "hex": bytes(body).hex()}


def matches(filter_, topic):
    f, t = filter_.split("/"), topic.split("/")
    for i, part in enumerate(f):
        if part == "#":
            return not (i == 0 and t[0].startswith("$"))
        if i >= len(t):
            return False
        if part == "+":
            if i == 0 and t[0].startswith("$"):
                return False
            continue
        if part != t[i]:
            return False
    return len(f) == len(t)


class Conn:
    def __init__(self, broker, index, reader, writer):
        self.broker, self.index = broker, index
        self.reader, self.writer = reader, writer
        self.v5 = False
        self.subs = {}  # filter -> (qos, subscription identifier)
        self.next_id = 0
        self.connected = False

    def send(self, data):
        if not self.writer.is_closing():
            self.writer.write(data)

    def packet_id(self):
        self.next_id = self.next_id % 65535 + 1
        return self.next_id

    def deliver(self, topic, payload, qos, retain, ids):
        self.broker.delivered.append({"conn": self.index, "topic": topic,
                                      "payload": text(payload), "qos": qos,
                                      "retain": retain, "ids": ids})
        props = b""
        if self.v5:
            props = props_pack("PUBLISH", SubscriptionIdentifier=ids) if ids else b"\x00"
        body = str16(topic)
        if qos:
            body += struct.pack("!H", self.packet_id())
        body += props + payload
        self.send(bytes([0x30 | (qos << 1) | int(retain)]) + varint(len(body)) + body)


class Broker:
    """A broker on 127.0.0.1 that records what it is sent."""

    def __init__(self, v5=True):
        self.v5_ok = v5
        self.port = free_port()
        self.conns = []
        self.retained = {}
        self.sent = []  # (connection index, seconds since start, packet)
        self.delivered = []  # what Home Assistant was sent
        self.start = time.monotonic()
        self.server = None

    async def run(self):
        self.server = await asyncio.start_server(self.handle, "127.0.0.1", self.port)

    async def close(self):
        for c in self.conns:
            c.writer.close()
        self.server.close()

    def live(self):
        return [c for c in self.conns if c.connected and not c.writer.is_closing()]

    def take(self):
        sent, self.sent = self.sent, []
        return sent

    def received(self):
        delivered, self.delivered = self.delivered, []
        return delivered

    async def handle(self, reader, writer):
        conn = Conn(self, len(self.conns), reader, writer)
        self.conns.append(conn)
        try:
            while True:
                first = (await reader.readexactly(1))[0]
                length, mult = 0, 1
                while True:
                    b = (await reader.readexactly(1))[0]
                    length += (b & 0x7F) * mult
                    mult *= 128
                    if not b & 0x80:
                        break
                body = await reader.readexactly(length)
                if first >> 4 == 1:
                    conn.v5 = body[6] == 5
                packet = decode(first, body, conn.v5)
                self.sent.append((conn.index, round(time.monotonic() - self.start, 1), packet))
                if not self.answer(conn, packet):
                    break
        except (asyncio.IncompleteReadError, ConnectionError):
            pass
        conn.connected = False
        writer.close()

    def answer(self, conn, p):
        t = p["type"]
        if t == "CONNECT":
            if conn.v5 and not self.v5_ok:
                # A 3.1.1 broker: "unacceptable protocol version", then gone.
                conn.send(b"\x20\x02\x00\x01")
                return False
            if conn.v5:
                props = props_pack("CONNACK", TopicAliasMaximum=10, ReceiveMaximum=20,
                                   RetainAvailable=1, MaximumQoS=2,
                                   WildcardSubscriptionAvailable=1,
                                   SubscriptionIdentifierAvailable=1,
                                   SharedSubscriptionAvailable=1)
                body = b"\x00\x00" + props
            else:
                body = b"\x00\x00"
            conn.send(b"\x20" + varint(len(body)) + body)
            conn.connected = True
        elif t == "SUBSCRIBE":
            ids = p.get("properties", {}).get("SubscriptionIdentifier", [])
            codes = bytes(opt & 3 for _, opt in p["topics"])
            body = struct.pack("!H", p["packet_id"]) + (b"\x00" if conn.v5 else b"") + codes
            conn.send(b"\x90" + varint(len(body)) + body)
            for topic, opt in p["topics"]:
                conn.subs[topic] = (opt & 3, ids[0] if ids else None)
            for topic, opt in p["topics"]:
                for rtopic, (payload, rqos) in list(self.retained.items()):
                    if matches(topic, rtopic):
                        conn.deliver(rtopic, payload, min(rqos, opt & 3), True,
                                     [ids[0]] if ids else [])
        elif t == "UNSUBSCRIBE":
            body = struct.pack("!H", p["packet_id"])
            if conn.v5:
                body += b"\x00" + b"\x00" * len(p["topics"])
            conn.send(b"\xb0" + varint(len(body)) + body)
            for topic in p["topics"]:
                conn.subs.pop(topic, None)
        elif t == "PUBLISH":
            payload = p["payload"].encode() if isinstance(p["payload"], str) else b""
            if p["qos"] == 1:
                conn.send(b"\x40\x02" + struct.pack("!H", p["packet_id"]))
            elif p["qos"] == 2:
                conn.send(b"\x50\x02" + struct.pack("!H", p["packet_id"]))
            self.route(p["topic"], payload, p["qos"], p["retain"])
        elif t == "PUBREL":
            conn.send(b"\x70\x02" + struct.pack("!H", p["packet_id"]))
        elif t == "PINGREQ":
            conn.send(b"\xd0\x00")
        elif t == "DISCONNECT":
            return False
        return True

    def route(self, topic, payload, qos, retain):
        if retain:
            if payload:
                self.retained[topic] = (payload, qos)
            else:
                self.retained.pop(topic, None)
        for conn in self.live():
            got = [(q, i) for f, (q, i) in conn.subs.items() if matches(f, topic)]
            if not got:
                continue
            conn.deliver(topic, payload, min(qos, max(q for q, _ in got)), False,
                         sorted(i for _, i in got if i is not None))

    def drop(self):
        for conn in self.live():
            conn.writer.close()


# ---------------------------------------------------------------- inputs

Z2M = "zigbee2mqtt"
BRIDGE = "0x00124b00a1b2c3d4"
THERMO = "0x00158d00a1b2c3e5"
DOOR = "0x00158d00a1b2c3f6"
Z2M_ORIGIN = {"name": "Zigbee2MQTT", "sw": "2.6.3", "url": "https://www.zigbee2mqtt.io"}


def z2m_availability(name):
    return [{"topic": f"{Z2M}/bridge/state", "value_template": "{{ value_json.state }}"},
            {"topic": f"{Z2M}/{name}/availability",
             "value_template": "{{ value_json.state }}"}]


def z2m_device(ieee, name, model, model_id, manufacturer, sw):
    return {"identifiers": [f"zigbee2mqtt_{ieee}"], "manufacturer": manufacturer,
            "model": model, "model_id": model_id, "name": name, "sw_version": sw,
            "via_device": f"zigbee2mqtt_bridge_{BRIDGE}"}


THERMO_DEVICE = z2m_device(THERMO, "Living room sensor", "Temperature and humidity sensor",
                           "WSDCGQ11LM", "Aqara", "3000-0001")
DOOR_DEVICE = z2m_device(DOOR, "Front door", "Door and window sensor", "MCCGQ11LM", "Aqara",
                         "3000-0001")


def z2m_sensor(ieee, device, key, **extra):
    return {"availability": z2m_availability(device["name"]), "availability_mode": "all",
            "device": device, "origin": Z2M_ORIGIN,
            "state_topic": f"{Z2M}/{device['name']}",
            "unique_id": f"{ieee}_{key}_zigbee2mqtt",
            "value_template": f"{{{{ value_json.{key} }}}}"} | extra


DISCOVERY_Z2M = [
    (f"homeassistant/binary_sensor/1221051039810110150109113116116_{BRIDGE}"
     "/connection_state/config",
     {"device": {"hw_version": "zStack3x0 20240710", "identifiers":
                 [f"zigbee2mqtt_bridge_{BRIDGE}"], "manufacturer": "Zigbee2MQTT",
                 "model": "Bridge", "name": "Zigbee2MQTT Bridge", "sw_version": "2.6.3",
                 "configuration_url": "http://192.0.2.20:8080/#/settings"},
      "origin": Z2M_ORIGIN, "device_class": "connectivity", "entity_category": "diagnostic",
      "name": "Connection state", "payload_on": "online", "payload_off": "offline",
      "state_topic": f"{Z2M}/bridge/state", "value_template": "{{ value_json.state }}",
      "unique_id": f"bridge_{BRIDGE}_connection_state_zigbee2mqtt",
      "default_entity_id": "binary_sensor.zigbee2mqtt_bridge_connection_state"}),
    (f"homeassistant/sensor/{THERMO}/temperature/config",
     z2m_sensor(THERMO, THERMO_DEVICE, "temperature", device_class="temperature",
                enabled_by_default=True, state_class="measurement",
                unit_of_measurement="°C",
                default_entity_id="sensor.living_room_sensor_temperature")),
    (f"homeassistant/sensor/{THERMO}/humidity/config",
     z2m_sensor(THERMO, THERMO_DEVICE, "humidity", device_class="humidity",
                enabled_by_default=True, state_class="measurement",
                unit_of_measurement="%")),
    (f"homeassistant/sensor/{THERMO}/pressure/config",
     z2m_sensor(THERMO, THERMO_DEVICE, "pressure", device_class="atmospheric_pressure",
                enabled_by_default=True, state_class="measurement",
                unit_of_measurement="hPa")),
    (f"homeassistant/sensor/{THERMO}/battery/config",
     z2m_sensor(THERMO, THERMO_DEVICE, "battery", device_class="battery",
                enabled_by_default=True, entity_category="diagnostic",
                state_class="measurement", unit_of_measurement="%")),
    (f"homeassistant/sensor/{THERMO}/voltage/config",
     z2m_sensor(THERMO, THERMO_DEVICE, "voltage", device_class="voltage",
                enabled_by_default=False, entity_category="diagnostic",
                state_class="measurement", unit_of_measurement="mV")),
    (f"homeassistant/sensor/{THERMO}/linkquality/config",
     z2m_sensor(THERMO, THERMO_DEVICE, "linkquality", enabled_by_default=False,
                entity_category="diagnostic", icon="mdi:signal", name="Linkquality",
                state_class="measurement", unit_of_measurement="lqi")),
    (f"homeassistant/binary_sensor/{DOOR}/contact/config",
     z2m_sensor(DOOR, DOOR_DEVICE, "contact", device_class="door", payload_off=True,
                payload_on=False)),
    (f"homeassistant/sensor/{DOOR}/battery/config",
     z2m_sensor(DOOR, DOOR_DEVICE, "battery", device_class="battery",
                enabled_by_default=True, entity_category="diagnostic",
                state_class="measurement", unit_of_measurement="%")),
]

TASMOTA = "A1B2C3"
TASMOTA_TOPIC = "tasmota_A1B2C3"
DISCOVERY_TASMOTA = [
    (f"homeassistant/sensor/{TASMOTA}_status/config",
     {"name": "Tasmota status", "stat_t": f"tele/{TASMOTA_TOPIC}/HASS_STATE",
      "avty_t": f"tele/{TASMOTA_TOPIC}/LWT", "pl_avail": "Online", "pl_not_avail": "Offline",
      "json_attr_t": f"tele/{TASMOTA_TOPIC}/HASS_STATE", "unit_of_meas": "%",
      "val_tpl": "{{value_json['RSSI']}}", "ic": "mdi:information-outline",
      "uniq_id": f"{TASMOTA}_status",
      "dev": {"ids": [TASMOTA], "name": "Tasmota", "mdl": "Sonoff Basic", "sw": "9.1.0(tasmota)",
              "mf": "Tasmota", "cns": [["mac", "02:00:00:A1:B2:C3"]]}}),
    (f"homeassistant/sensor/{TASMOTA}_DS18B20_Temperature/config",
     {"name": "Tasmota DS18B20 Temperature", "stat_t": f"tele/{TASMOTA_TOPIC}/SENSOR",
      "avty_t": f"tele/{TASMOTA_TOPIC}/LWT", "pl_avail": "Online", "pl_not_avail": "Offline",
      "uniq_id": f"{TASMOTA}_DS18B20_Temperature", "dev": {"ids": [TASMOTA]},
      "unit_of_meas": "°C", "dev_cla": "temperature", "frc_upd": True,
      "val_tpl": "{{value_json['DS18B20'].Temperature}}"}),
]

STATION = "acme_ws01"
STATION_CONFIG = {
    "dev": {"ids": STATION, "name": "Weather station", "mf": "Acme", "mdl": "WS-01",
            "sw": "1.0", "hw": "rev2", "sn": "WS01-0001"},
    "o": {"name": "acme2mqtt", "sw": "0.1", "url": "https://example.com/support"},
    "cmps": {
        "temp": {"p": "sensor", "dev_cla": "temperature", "unit_of_meas": "°C",
                 "stat_cla": "measurement", "val_tpl": "{{ value_json.temperature }}",
                 "uniq_id": "acme_ws01_temp", "exp_aft": 120},
        "rain": {"p": "binary_sensor", "dev_cla": "moisture", "name": "Rain",
                 "val_tpl": "{{ value_json.rain }}", "uniq_id": "acme_ws01_rain",
                 "off_dly": 30},
        "uptime": {"p": "sensor", "name": None, "stat_t": "acme/ws01/uptime",
                   "uniq_id": "acme_ws01_uptime", "ent_cat": "diagnostic"},
    },
    "stat_t": "acme/ws01/state", "avty_t": "acme/ws01/availability", "qos": 1,
}


def j(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def publish(topic, payload, retain=False, qos=0):
    return {"do": "publish", "topic": topic, "payload": payload, "retain": retain, "qos": qos}


def scenario_v5():
    thermo = f"{Z2M}/Living room sensor"
    door = f"{Z2M}/Front door"
    steps = [publish(t, j(c), retain=True) for t, c in DISCOVERY_Z2M + DISCOVERY_TASMOTA]
    steps += [
        publish(f"{Z2M}/bridge/state", j({"state": "online"}), retain=True),
        publish(f"{thermo}/availability", j({"state": "online"}), retain=True),
        publish(f"{door}/availability", j({"state": "online"}), retain=True),
        publish(f"tele/{TASMOTA_TOPIC}/LWT", "Online", retain=True),
    ]
    start = {"do": "start"}
    after = [
        publish(thermo, j({"battery": 91, "humidity": 52.84, "linkquality": 120,
                           "power_outage_count": 3, "pressure": 1013.4,
                           "temperature": 21.37, "voltage": 2995})),
        publish(door, j({"battery": 100, "contact": True, "linkquality": 87,
                         "voltage": 3025})),
        publish(door, j({"battery": 100, "contact": False, "linkquality": 87,
                         "voltage": 3025})),
        # A message without the key: the template renders "", which is
        # ignored.
        publish(thermo, j({"linkquality": 116})),
        publish(f"tele/{TASMOTA_TOPIC}/SENSOR",
                j({"Time": "2026-06-01T12:00:00", "DS18B20": {"Id": "01144A0CB2AA",
                                                            "Temperature": 19.4},
                   "TempUnit": "C"})),
        publish(f"tele/{TASMOTA_TOPIC}/HASS_STATE",
                j({"Version": "9.1.0(tasmota)", "BuildDateTime": "2020-11-23T09:00:00",
                   "Module or Template": "Sonoff Basic", "RestartReason": "Software/System restart",
                   "Uptime": "0T00:05:12", "Hostname": "tasmota-A1B2C3-0451",
                   "IPAddress": "192.0.2.30", "RSSI": "100", "Signal (dBm)": "-48",
                   "WiFi LinkCount": 1, "WiFi Downtime": "0T00:00:03",
                   "MqttCount": 1, "LoadAvg": 19})),
        # Force update: the same value again is written again.
        publish(f"tele/{TASMOTA_TOPIC}/SENSOR",
                j({"Time": "2026-06-01T12:00:00", "DS18B20": {"Id": "01144A0CB2AA",
                                                            "Temperature": 19.4},
                   "TempUnit": "C"})),
        # The device goes away, then comes back.
        publish(f"{thermo}/availability", j({"state": "offline"}), retain=True),
        publish(f"{thermo}/availability", j({"state": "online"}), retain=True),
        publish(f"tele/{TASMOTA_TOPIC}/LWT", "Offline", retain=True),
        publish(f"tele/{TASMOTA_TOPIC}/LWT", "Online", retain=True),
        # A non-numeric value for a numeric sensor.
        publish(thermo, j({"temperature": "warm"})),
        # Device-based discovery.
        publish(f"homeassistant/device/{STATION}/config", j(STATION_CONFIG), retain=True),
        publish("acme/ws01/availability", "online", retain=True),
        publish("acme/ws01/state", j({"temperature": 12.5, "rain": "ON"}), qos=1),
        publish("acme/ws01/uptime", "3600"),
        {"do": "wait", "seconds": 31},  # off_delay 30 s
        {"do": "wait", "seconds": 91},  # expire_after 120 s
        # An updated config: the unit and name change.
        publish(f"homeassistant/sensor/{THERMO}/humidity/config",
                j(z2m_sensor(THERMO, THERMO_DEVICE, "humidity", device_class="humidity",
                             enabled_by_default=True, state_class="measurement",
                             unit_of_measurement="%", name="Relative humidity")),
                retain=True),
        # The same config again: ignored.
        publish(f"homeassistant/sensor/{THERMO}/humidity/config",
                j(z2m_sensor(THERMO, THERMO_DEVICE, "humidity", device_class="humidity",
                             enabled_by_default=True, state_class="measurement",
                             unit_of_measurement="%", name="Relative humidity")),
                retain=True),
        # A component removed from the device's config.
        publish(f"homeassistant/device/{STATION}/config",
                j(STATION_CONFIG | {"cmps": STATION_CONFIG["cmps"] | {"rain": {"p": "binary_sensor"}}}),
                retain=True),
        # An empty config: the entity and its row are removed.
        publish(f"homeassistant/sensor/{DOOR}/battery/config", "", retain=True),
        {"do": "service", "service": "publish",
         "data": {"topic": "hars/test", "payload": "hello"}},
        {"do": "service", "service": "publish",
         "data": {"topic": "hars/test", "payload": "{\"on\": true}", "qos": "1",
                  "retain": True}},
        {"do": "service", "service": "publish",
         "data": {"topic": "hars/test", "payload": "b'\\x01\\x02'", "evaluate_payload": True,
                  "qos": 2}},
        {"do": "service", "service": "publish",
         "data": {"topic": "hars/test", "payload": 21.5}},
        {"do": "service", "service": "publish", "data": {"topic": "hars/empty"}},
        {"do": "service", "service": "publish",
         "data": {"topic": "hars/expiry", "payload": "x",
                  "message_expiry_interval": {"minutes": 1, "seconds": 5}}},
        {"do": "service", "service": "publish", "data": {"topic": "hars/+", "payload": "x"}},
        {"do": "service", "service": "publish",
         "data": {"topic": "hars/test", "payload": "x", "qos": 3}},
        # The broker goes away; Home Assistant comes back.
        # Home Assistant tries again 10 s after the connection is lost,
        # then every 10 s; the birth message follows 5 s of quiet.
        {"do": "drop"},
        {"do": "wait", "seconds": 17},
        publish(thermo, j({"temperature": 21.9})),
    ]
    entry = {"data": {"broker": "127.0.0.1", "port": "<port>", "username": "hars",
                      "password": "fake-password", "protocol": "5"},
             "options": {}, "version": 2, "minor_version": 1, "title": "127.0.0.1"}
    return entry, steps, start, after


def scenario_v311():
    entry = {"data": {"broker": "127.0.0.1", "port": "<port>", "client_id": "hars-test",
                      "keepalive": 15, "discovery": True, "discovery_prefix": "hass",
                      "birth_message": {"topic": "hass/status", "payload": "up", "qos": 1,
                                        "retain": True},
                      "will_message": {"topic": "hass/status", "payload": "down", "qos": 1,
                                       "retain": True}},
             "options": {}, "version": 1, "minor_version": 1, "title": "127.0.0.1"}
    steps = [publish(
        f"hass/sensor/{TASMOTA}_DS18B20_Temperature/config",
        j(DISCOVERY_TASMOTA[1][1]), retain=True)]
    after = [
        publish(f"tele/{TASMOTA_TOPIC}/LWT", "Online", retain=True),
        publish(f"tele/{TASMOTA_TOPIC}/SENSOR", j({"DS18B20": {"Temperature": 18.9}})),
        {"do": "wait", "seconds": 33},  # a keepalive of 15 s: PINGREQs
        {"do": "service", "service": "publish",
         "data": {"topic": "hars/expiry", "payload": "x",
                  "message_expiry_interval": {"seconds": 5}}},
    ]
    return entry, steps, {"do": "start"}, after


# ---------------------------------------------------------------- Home Assistant


class Logs(logging.Handler):
    def __init__(self):
        super().__init__(logging.WARNING)
        self.lines = []

    def emit(self, record):
        if record.name.startswith("homeassistant.components.mqtt") or \
                record.name.startswith("homeassistant.components.sensor") or \
                record.name.startswith("homeassistant.helpers.template"):
            self.lines.append([record.levelname, record.name, record.getMessage()])

    def take(self):
        lines, self.lines = self.lines, []
        return lines


async def settle(hass):
    for _ in range(3):
        await hass.async_block_till_done()
        await asyncio.sleep(0.05)


async def boot(tmp):
    from homeassistant import bootstrap, core, loader
    from homeassistant.auth import auth_manager_from_config
    from homeassistant.config_entries import ConfigEntries
    from homeassistant.setup import async_setup_component

    (tmp / "configuration.yaml").write_text("")
    hass = core.HomeAssistant(str(tmp))
    hass.config.skip_pip = True
    await hass.config.async_load()
    loader.async_setup(hass)
    hass.config_entries = ConfigEntries(hass, {})
    await hass.config_entries.async_initialize()
    await bootstrap.async_load_base_functionality(hass)
    hass.auth = await auth_manager_from_config(hass, [{"type": "homeassistant"}], [])
    conf = {"http": {"server_port": free_port(), "server_host": "127.0.0.1"}}
    for domain in ("homeassistant", "http"):
        assert await async_setup_component(hass, domain, conf), domain
    return hass


def mqtt_states(hass, previous):
    from homeassistant.helpers import entity_registry as er
    from homeassistant.helpers.json import json_dumps

    entities = er.async_get(hass)
    out = {}
    now = {}
    for s in hass.states.async_all():
        row = entities.async_get(s.entity_id)
        if s.domain not in ("sensor", "binary_sensor"):
            continue
        if row is not None and row.platform != "mqtt":
            continue
        value = json.loads(json_dumps({"state": s.state, "attributes": dict(s.attributes),
                                       "last_changed": s.last_changed.isoformat(),
                                       "last_updated": s.last_updated.isoformat()}))
        now[s.entity_id] = value
    for entity_id in sorted(set(now) | set(previous)):
        if entity_id not in now:
            out[entity_id] = None
        elif previous.get(entity_id) != now[entity_id]:
            old = previous.get(entity_id)
            value = {"state": now[entity_id]["state"],
                     "attributes": now[entity_id]["attributes"]}
            # Whether the write moved last_changed / last_updated.
            if old is not None:
                value["changed"] = old["last_changed"] != now[entity_id]["last_changed"]
                value["updated"] = old["last_updated"] != now[entity_id]["last_updated"]
            out[entity_id] = value
    return out, now


def rows(hass, entry_id, port):
    from homeassistant.helpers import device_registry as dr
    from homeassistant.helpers import entity_registry as er

    devices = dr.async_get(hass)
    entities = er.async_get(hass)
    names = {}
    out_devices = []
    # Named in the order of their identifiers: HA makes them in the order
    # its tasks run.
    ours = sorted((d for d in devices.devices.values() if entry_id in d.config_entries),
                  key=lambda d: sorted(list(i) for i in d.identifiers))
    for d in ours:
        names[d.id] = f"<device {len(names) + 1}>"
    for d in ours:
        out_devices.append({
            "id": names[d.id],
            "identifiers": sorted(list(i) for i in d.identifiers),
            "connections": sorted(list(c) for c in d.connections),
            "manufacturer": d.manufacturer, "model": d.model, "model_id": d.model_id,
            "name": d.name, "name_by_user": d.name_by_user, "sw_version": d.sw_version,
            "hw_version": d.hw_version, "serial_number": d.serial_number,
            "configuration_url": d.configuration_url,
            "via_device_id": names.get(d.via_device_id, d.via_device_id),
            "entry_type": d.entry_type, "area_id": d.area_id,
            "primary_config_entry": "<entry>" if d.primary_config_entry == entry_id else
            d.primary_config_entry,
        })
    out_entities = []
    for e in entities.entities.values():
        if e.platform != "mqtt":
            continue
        out_entities.append({
            "entity_id": e.entity_id, "unique_id": e.unique_id, "platform": e.platform,
            "config_entry_id": "<entry>" if e.config_entry_id == entry_id else
            e.config_entry_id,
            "device_id": names.get(e.device_id, e.device_id),
            "disabled_by": e.disabled_by, "hidden_by": e.hidden_by,
            "entity_category": e.entity_category, "has_entity_name": e.has_entity_name,
            "original_name": e.original_name, "original_device_class": e.original_device_class,
            "original_icon": e.original_icon, "object_id_base": e.object_id_base,
            "suggested_object_id": e.suggested_object_id,
            "capabilities": e.capabilities, "supported_features": e.supported_features,
            "unit_of_measurement": e.unit_of_measurement, "options": dict(e.options),
            "translation_key": e.translation_key,
        })
    out_entities.sort(key=lambda e: e["entity_id"])
    return {"devices": out_devices, "entities": out_entities,
            "deleted_entities": sorted(
                [d.domain, d.platform, d.unique_id] for d in entities.deleted_entities.values()
                if d.platform == "mqtt")}


def clean_packets(sent, port, entry):
    out = []
    for conn, at, p in sent:
        if p["type"] == "CONNECT" and "client_id" not in entry["data"]:
            assert len(p["client_id"]) == 22, p
            p["client_id"] = "<random>"
        # The plain topics of one SUBSCRIBE are in the order HA's tasks
        # subscribed them, which changes from run to run: sorted.
        if p["type"] == "SUBSCRIBE" and not any("+" in t or "#" in t for t, _ in p["topics"]):
            p["topics"].sort()
        out.append({"conn": conn, "at": at, "packet": p})
    return out


def steady(record):
    """What the broker sent HA, and HA's log lines, in an order of their own:
    HA's comes from its tasks and from Python's set order (a message's
    subscriptions), which change from run to run."""
    record["received"].sort(key=lambda r: json.dumps(r, sort_keys=True))
    record["log"].sort()
    return record


async def run(name, make):
    from homeassistant.config_entries import ConfigEntry

    entry_in, before, start, after = make()
    broker = Broker(v5=name == "v5")
    await broker.run()
    logs = Logs()
    logging.getLogger().addHandler(logs)
    logging.getLogger().setLevel(logging.INFO)
    tmp = pathlib.Path(tempfile.mkdtemp())
    out_steps = []
    try:
        # Retained messages the broker has before Home Assistant connects.
        for step in before:
            broker.route(step["topic"], step["payload"].encode(), step["qos"], step["retain"])
        hass = await boot(tmp)
        data = {k: (broker.port if v == "<port>" else v) for k, v in entry_in["data"].items()}
        entry = ConfigEntry(data=data, discovery_keys=MappingProxyType({}), domain="mqtt",
                            minor_version=entry_in["minor_version"],
                            options=entry_in["options"], source="user",
                            subentries_data=None, title=entry_in["title"],
                            unique_id=None, version=entry_in["version"])
        await hass.config_entries.async_add(entry)
        await settle(hass)
        states = {}
        changed, states = mqtt_states(hass, states)
        out_steps.append(steady({"do": "setup", "received": broker.received(),
                                 "sent": clean_packets(broker.take(), broker.port, entry_in),
                                 "states": changed, "log": logs.take(),
                                 "entry_state": entry.state.value}))
        await hass.async_start()
        # The birth message waits for start, then for discovery to be
        # quiet for 5 s.
        await asyncio.sleep(7)
        await settle(hass)
        changed, states = mqtt_states(hass, states)
        out_steps.append(steady({"do": "start", "waited": 7, "received": broker.received(),
                                 "sent": clean_packets(broker.take(), broker.port, entry_in),
                                 "states": changed, "log": logs.take()}))
        for step in after:
            record = dict(step)
            if step["do"] == "publish":
                payload = step["payload"].encode()
                broker.route(step["topic"], payload, step["qos"], step["retain"])
                await asyncio.sleep(0.3)
            elif step["do"] == "wait":
                await asyncio.sleep(step["seconds"])
            elif step["do"] == "drop":
                broker.drop()
                await asyncio.sleep(0.3)
            elif step["do"] == "service":
                try:
                    await hass.services.async_call("mqtt", step["service"], step["data"],
                                                   blocking=True)
                    record["result"] = "ok"
                except Exception as err:  # noqa: BLE001
                    record["result"] = {"error": type(err).__name__, "message": str(err)}
                await asyncio.sleep(0.3)
            await settle(hass)
            changed, states = mqtt_states(hass, states)
            record |= {"received": broker.received(),
                       "sent": clean_packets(broker.take(), broker.port, entry_in),
                       "states": changed, "log": logs.take()}
            out_steps.append(steady(record))
        stored = hass.config_entries.async_get_entry(entry.entry_id)
        stored_entry = {"data": {k: ("<port>" if k == "port" else v)
                                 for k, v in stored.data.items()},
                        "options": dict(stored.options), "version": stored.version,
                        "minor_version": stored.minor_version}
        registry = rows(hass, entry.entry_id, broker.port)
        await hass.async_stop(force=True)
        await broker.close()
    finally:
        logging.getLogger().removeHandler(logs)
        shutil.rmtree(tmp, ignore_errors=True)
    for step in out_steps:
        for line in step.get("log", []):
            line[2] = line[2].replace(str(broker.port), "<port>")
    return {"entry": entry_in, "retained_before": before, "steps": out_steps,
            "stored_entry": stored_entry, **registry}


# ---------------------------------------------------------------- tables


def tables():
    from homeassistant.components.binary_sensor import BinarySensorDeviceClass
    from homeassistant.components.mqtt import abbreviations, const
    from homeassistant.components.sensor import SensorDeviceClass
    import homeassistant.components.binary_sensor as bs
    import homeassistant.components.sensor as se

    def strings(module):
        path = pathlib.Path(module.__file__).parent / "translations" / "en.json"
        return json.loads(path.read_text())["entity_component"]

    def table(name, pairs, doc):
        lines = [f"/// {doc}", f"pub const {name}: &[(&str, &str)] = &["]
        lines += [f"    ({json.dumps(k, ensure_ascii=False)}, {json.dumps(v, ensure_ascii=False)}),"
                  for k, v in pairs]
        return lines + ["];", ""]

    def names(module, classes):
        found = strings(module)
        return [(c.value, found[c.value]["name"]) for c in classes
                if c.value in found and "name" in found[c.value]]

    out = ["//! Generated by `scripts/vectors/mqtt.py` from Home Assistant 2026.6.1:",
           "//! `components/mqtt/abbreviations.py` and `const.py`, and the English names",
           "//! of the `sensor` and `binary_sensor` device classes. Don't edit by hand.",
           ""]
    out += table("ABBREVIATIONS", sorted(abbreviations.ABBREVIATIONS.items()),
                 "`ABBREVIATIONS`, sorted by abbreviation.")
    out += table("DEVICE_ABBREVIATIONS", sorted(abbreviations.DEVICE_ABBREVIATIONS.items()),
                 "`DEVICE_ABBREVIATIONS`, sorted by abbreviation.")
    out += table("ORIGIN_ABBREVIATIONS", sorted(abbreviations.ORIGIN_ABBREVIATIONS.items()),
                 "`ORIGIN_ABBREVIATIONS`, sorted by abbreviation.")
    out += ["/// `SUPPORTED_COMPONENTS`, in its order (the discovery subscriptions'",
            "/// order).", "pub const SUPPORTED_COMPONENTS: &[&str] = &["]
    out += [f"    {json.dumps(c)}," for c in const.SUPPORTED_COMPONENTS]
    out += ["];", ""]
    out += table("SENSOR_NAMES", sorted(names(se, SensorDeviceClass)),
                 "A `sensor` device class and its English name, sorted by class.")
    out += table("BINARY_SENSOR_NAMES", sorted(names(bs, BinarySensorDeviceClass)),
                 "A `binary_sensor` device class and its English name, sorted by class.")
    out += ["/// `SensorDeviceClass`'s values, sorted.", "pub const SENSOR_CLASSES: &[&str] = &["]
    out += [f"    {json.dumps(c.value)}," for c in sorted(SensorDeviceClass, key=lambda c: c.value)]
    out += ["];", "", "/// `BinarySensorDeviceClass`'s values, sorted.",
            "pub const BINARY_SENSOR_CLASSES: &[&str] = &["]
    out += [f"    {json.dumps(c.value)}," for c in
            sorted(BinarySensorDeviceClass, key=lambda c: c.value)]
    out += ["];", "", "/// `SensorStateClass`'s values, sorted.",
            "pub const STATE_CLASSES: &[&str] = &["]
    out += [f"    {json.dumps(c.value)}," for c in
            sorted(se.SensorStateClass, key=lambda c: c.value)]
    out += ["];", "", "/// `DEVICE_CLASS_UNITS`: a class and the units it allows (`\"\"` for",
            "/// none), sorted.", "pub const DEVICE_CLASS_UNITS: &[(&str, &[&str])] = &["]
    for dc, units in sorted(se.DEVICE_CLASS_UNITS.items(), key=lambda kv: kv[0].value):
        listed = sorted("" if u is None else str(u) for u in units)
        inner = ", ".join(json.dumps(u, ensure_ascii=False) for u in listed)
        out.append(f"    ({json.dumps(dc.value)}, &[{inner}]),")
    out += ["];", "", "/// `STATE_CLASS_UNITS`, as [`DEVICE_CLASS_UNITS`].",
            "pub const STATE_CLASS_UNITS: &[(&str, &[&str])] = &["]
    for sc, units in sorted(se.STATE_CLASS_UNITS.items(), key=lambda kv: kv[0].value):
        listed = sorted("" if u is None else str(u) for u in units)
        inner = ", ".join(json.dumps(u, ensure_ascii=False) for u in listed)
        out.append(f"    ({json.dumps(sc.value)}, &[{inner}]),")
    out += ["];", ""]
    out += table("AMBIGUOUS_UNITS", sorted((k, str(v)) for k, v in se.AMBIGUOUS_UNITS.items()),
                 "`AMBIGUOUS_UNITS`: a unit and the one it is written as, sorted.")
    # `_get_adjusted_display_precision` with no suggested precision: the
    # precision HA works out for a device class and unit.
    from homeassistant.components.sensor import _calculate_precision_from_ratio
    from homeassistant.components.sensor.const import (DEFAULT_PRECISION_LIMIT,
                                                       UNIT_CONVERTERS, UNITS_PRECISION)

    rows = []
    for dc, (base_unit, base) in UNITS_PRECISION.items():
        converter = UNIT_CONVERTERS.get(dc)
        units = sorted(str(u) for u in (converter.VALID_UNITS if converter else ()) if u)
        for unit in sorted(set(units) | {str(base_unit)}):
            precision = (base if str(base_unit) == unit else
                         _calculate_precision_from_ratio(dc, base_unit, unit, base))
            if precision is None:
                continue
            rows.append((dc.value, unit, min(precision, base + DEFAULT_PRECISION_LIMIT)))
    out += ["/// `_get_adjusted_display_precision` with no suggested precision and no",
            "/// unit conversion: a device class, a unit and the precision, sorted.",
            "pub const UNIT_PRECISION: &[(&str, &str, u8)] = &["]
    out += [f"    ({json.dumps(dc)}, {json.dumps(u, ensure_ascii=False)}, {p}),"
            for dc, u, p in sorted(rows)]
    out += ["];", ""]
    out += ["/// `NON_NUMERIC_DEVICE_CLASSES`, sorted.",
            "pub const NON_NUMERIC_SENSOR_CLASSES: &[&str] = &["]
    out += [f"    {json.dumps(c.value)}," for c in
            sorted(se.NON_NUMERIC_DEVICE_CLASSES, key=lambda c: c.value)]
    out += ["];", ""]
    TABLES.write_text("\n".join(out))
    # As `cargo fmt` leaves it (the toolchain's rustfmt, from
    # `rust-toolchain.toml`).
    subprocess.run(["rustfmt", "--edition", "2024", str(TABLES)], check=True, cwd=ROOT)


async def main():
    check_versions()
    OUT.mkdir(parents=True, exist_ok=True)
    tables()
    only = sys.argv[1:]
    if only == ["tables"]:
        return
    for name, make in (("v5", scenario_v5), ("v311", scenario_v311)):
        if only and name not in only:
            continue
        print(f"scenario {name}", flush=True)
        write(f"{name}.json", await run(name, make))


if __name__ == "__main__":
    asyncio.run(main())
    # paho's and HA's threads can crash the interpreter's shutdown once
    # everything is written: leave at once.
    sys.stdout.flush()
    os._exit(0)
