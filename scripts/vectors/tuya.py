#!/usr/bin/env python3
"""Vectors for the `tuya` integration.

    .venv-tuya/bin/python -I scripts/vectors/tuya.py
    cargo test -p ha-integrations tuya

Runs Home Assistant 2026.6.1's `tuya` integration on
tuya-device-sharing-sdk 0.2.8 and tuya-device-handlers 0.0.22, booted in a
temporary config directory, with the SDK's endpoint pointed at a fake Tuya
cloud on 127.0.0.1. The fake answers as the cloud does: it derives each
request's key from its `X-requestId` and the refresh token it last gave
out, checks the signature, decrypts the query and body, and encrypts its
replies. Its devices (a `cs` dehumidifier and a `cwwsq` pet feeder) are the
input, crates/ha-integrations/src/tuya/vectors/cloud.json, which the Rust
tests serve from their own fake. The MQTT client is replaced by a stand-in
that records what the SDK would subscribe to and lets the script hand
messages to `Manager.on_message`. Nothing reaches the network. It writes,
to crates/ha-integrations/src/tuya/vectors/:

- crypto.json: the SDK's `_secret_generating`, `_restful_sign`,
  `_aes_gcm_encrypt` (with its nonce fixed) and `_aex_gcm_decrypt` on
  fixed inputs;
- setup.json: after the entry is set up, its states, the device and
  entity registry rows HA made, the topics the SDK subscribes to and the
  requests it made, in order;
- pushes.json: the states after each push message in `PUSHES`;
- services.json: for each service call in `SERVICES`, the commands the
  SDK sent (decrypted) or the error HA raised.

Ids are written as placeholders (`<id>`), times as `<time>`. Run it twice:
the output must not change.

    .venv-tuya/bin/python -I scripts/vectors/tuya.py --serve HOST HTTP_PORT MQTT_PORT

serves the same fake cloud on HTTP_PORT and a broker that accepts,
subscribes and answers pings on MQTT_PORT (both on all addresses), until
stopped, with `access/config` pointing at tcp://HOST:MQTT_PORT. That is for
`scripts/perf.sh` runs with a Tuya entry: `PERF_CONFIG` holds an entry
whose endpoint is this server, so nothing reaches Tuya.
"""

import asyncio
import hashlib
import importlib.metadata
import json
import pathlib
import socket
import sys
import tempfile
import threading
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock

PINNED = {
    "homeassistant": "2026.6.1",
    "tuya-device-sharing-sdk": "0.2.8",
    "tuya-device-handlers": "0.0.22",
}
ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT = ROOT / "crates/ha-integrations/src/tuya/vectors"
CLOUD = json.loads((OUT / "cloud.json").read_text())

PUSHES = [
    # Local format, by dpId: humidity_indoor (16) falls.
    {"protocol": 4, "data": {"devId": "dev_dehumidifier", "status": [{"dpId": 16, "t": 1, "value": 60}]}},
    # By code: the tank empties, the feeder starts feeding.
    {"protocol": 4, "data": {"devId": "dev_dehumidifier", "status": [{"dpId": 20, "t": 2, "value": 0}]}},
    {"protocol": 4, "data": {"devId": "dev_feeder", "status": [{"code": "feed_state", "value": "feeding"}]}},
    # Offline, then back.
    {"protocol": 20, "data": {"bizCode": "offline", "bizData": {"devId": "dev_feeder"}}},
    {"protocol": 20, "data": {"bizCode": "online", "bizData": {"devId": "dev_feeder"}}},
]

SERVICES = [
    ("humidifier", "turn_off", {"entity_id": "humidifier.dry"}),
    ("humidifier", "set_humidity", {"entity_id": "humidifier.dry", "humidity": 60}),
    ("humidifier", "set_humidity", {"entity_id": "humidifier.dry", "humidity": 80}),
    ("humidifier", "set_mode", {"entity_id": "humidifier.dry", "mode": "auto"}),
    ("fan", "turn_on", {"entity_id": "fan.dry"}),
    ("fan", "set_percentage", {"entity_id": "fan.dry", "percentage": 50}),
    ("switch", "turn_on", {"entity_id": "switch.dry_child_lock"}),
    ("select", "select_option", {"entity_id": "select.dry_countdown", "option": "2h"}),
    ("select", "select_option", {"entity_id": "select.dry_countdown", "option": "9h"}),
    ("number", "set_value", {"entity_id": "number.feeder_feed", "value": 3}),
    ("number", "set_value", {"entity_id": "number.feeder_feed", "value": 25}),
]


def check_versions():
    for pkg, want in PINNED.items():
        have = importlib.metadata.version(pkg)
        if have != want:
            sys.exit(f"{pkg} is {have}; these vectors are made with {want}")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class Cloud:
    """The fake Tuya cloud: CLOUD's devices, keyed as the real one keys."""

    def __init__(self):
        self.refresh = "refresh1"
        self.seen = []
        self.lock = threading.Lock()

    def result(self, method, path):
        devices = {d["id"]: d for d in CLOUD["devices"]}
        if path == "/v1.0/m/life/users/homes":
            return CLOUD["homes"]
        if path == "/v1.0/m/life/ha/home/devices":
            return CLOUD["devices"]
        for dev in devices:
            if path == f"/v1.1/m/life/{dev}/specifications":
                return CLOUD["specifications"][dev]
            if path == f"/v1.0/m/life/devices/{dev}/status":
                return CLOUD["status"][dev]
            if path == f"/v1.0/m/life/ha/{dev}/dp-report-types":
                return CLOUD["report_types"].get(dev, [])
            if path == f"/v1.1/m/thing/{dev}/commands":
                return True
        # No tap-to-run scenes: `hars` has no Tuya scene entities yet.
        if path == "/v1.0/m/scene/ha/home/scenes":
            return []
        if path == "/v1.0/m/life/home-assistant/qrcode/versions":
            return True
        if path == "/v1.0/m/life/ha/access/config":
            return {**CLOUD["mqtt"], **({"url": self.mqtt_url} if getattr(self, "mqtt_url", None) else {})}
        if path.startswith("/v1.0/m/token/"):
            self.refresh = "refresh2"
            return {"expireTime": 7200, "uid": "uid", "accessToken": "access2", "refreshToken": "refresh2"}
        return None

    def handle(self, method, target, headers, body):
        from tuya_sharing.customerapi import _aex_gcm_decrypt, _restful_sign, _secret_generating
        import base64
        from cryptography.hazmat.primitives.ciphers.aead import AESGCM

        url = urllib.parse.urlsplit(target)
        rid = headers.get("X-requestId", "")
        with self.lock:
            refresh = self.refresh
        hash_key = hashlib.md5((rid + refresh).encode()).hexdigest()
        secret = _secret_generating(rid, "", hash_key)
        query = urllib.parse.parse_qs(url.query).get("encdata", [""])[0]
        body_enc = json.loads(body).get("encdata", "") if body else ""
        # Header names are case-insensitive (hars sends them in lower case).
        signed = {k: headers.get(k, "") for k in ("X-appKey", "X-requestId", "X-sid", "X-time", "X-token")}
        sign = _restful_sign(hash_key, query, body_enc, signed)
        assert sign == headers.get("X-sign"), "bad signature"

        def opened(enc):
            # The SDK sends b64(nonce) + b64(ciphertext); read it back.
            nonce, ct = base64.b64decode(enc[:16]), base64.b64decode(enc[16:])
            return json.loads(_aex_gcm_decrypt(base64.b64encode(nonce + ct).decode(), secret))

        self.seen.append({
            "method": method,
            "path": url.path,
            "query": opened(query) if query else None,
            "body": opened(body_enc) if body_enc else None,
        })
        result = self.result(method, url.path)
        nonce = b"ABCDEFGHJKMN"
        ct = AESGCM(secret.encode()).encrypt(nonce, json.dumps(result).encode(), None)
        return {"success": True, "t": 1, "result": base64.b64encode(nonce + ct).decode()}


def broker(port):
    """An MQTT 3.1.1 broker that publishes nothing: CONNACK, SUBACK, PINGRESP."""

    def client(conn):
        def read():
            first = conn.recv(1)
            if not first:
                return None, b""
            length, shift = 0, 0
            while True:
                b = conn.recv(1)[0]
                length |= (b & 0x7F) << shift
                if not b & 0x80:
                    break
                shift += 7
            body = b""
            while len(body) < length:
                body += conn.recv(length - len(body))
            return first[0], body

        with conn:
            while True:
                kind, body = read()
                if kind is None:
                    return
                if kind >> 4 == 1:
                    conn.sendall(bytes([0x20, 2, 0, 0]))
                elif kind >> 4 == 8:
                    conn.sendall(bytes([0x90, 3]) + body[:2] + b"\x00")
                elif kind >> 4 == 12:
                    conn.sendall(bytes([0xD0, 0]))
                elif kind >> 4 == 14:
                    return

    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("0.0.0.0", port))
    s.listen()
    while True:
        conn, _ = s.accept()
        threading.Thread(target=client, args=(conn,), daemon=True).start()


def serve(cloud, host="127.0.0.1", port=None):
    class Handler(BaseHTTPRequestHandler):
        def do(self, method):
            n = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(n).decode() if n else ""
            reply = json.dumps(cloud.handle(method, self.path, self.headers, body)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(reply)))
            self.end_headers()
            self.wfile.write(reply)

        def do_GET(self):
            self.do("GET")

        def do_POST(self):
            self.do("POST")

        def log_message(self, *args):
            pass

    port = port or free_port()
    server = ThreadingHTTPServer((host, port), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{port}", server


class FakeMQ:
    """`SharingMQ` without a broker: what it would subscribe to."""

    made = []

    def __init__(self, customer_api, owner_ids, device):
        from tuya_sharing.mq import SharingMQConfig

        self.owner_ids = owner_ids
        self.device = device
        self.mq_config = SharingMQConfig({"result": CLOUD["mqtt"]})
        self.listeners = []
        FakeMQ.made.append(self)

    def topics(self):
        from tuya_sharing.mq import SharingMQ

        out = [self.mq_config.owner_topic.format(ownerId=o) for o in self.owner_ids]
        out += [SharingMQ.subscribe_topic(self, d.id, d.support_local) for d in self.device]
        return out

    def start(self):
        pass

    def stop(self):
        pass

    def add_message_listener(self, listener):
        self.listeners.append(listener)

    def subscribe_device(self, dev_id, device):
        pass


def scrub(v):
    """Registry rows without the ids and times that change each run."""
    if isinstance(v, dict):
        out = {}
        for k, x in v.items():
            if k in ("id", "device_id", "config_entry_id", "created_at", "modified_at",
                     "primary_config_entry", "config_entries"):
                out[k] = "<id>" if x is not None and "_at" not in k else ("<time>" if x else x)
            else:
                out[k] = scrub(x)
        return out
    if isinstance(v, list):
        return [scrub(x) for x in v]
    return v


async def main():
    check_versions()
    from homeassistant import bootstrap, core, loader
    from homeassistant.config_entries import ConfigEntries, ConfigEntry
    from homeassistant.exceptions import HomeAssistantError
    from homeassistant.helpers import device_registry as dr, entity_registry as er
    from homeassistant.setup import async_setup_component
    import tuya_sharing.customerapi as capi

    cloud = Cloud()
    base, server = serve(cloud)
    tmp = pathlib.Path(tempfile.mkdtemp())
    hass = core.HomeAssistant(str(tmp))
    hass.config.skip_pip = True
    await hass.config.async_load()
    hass.config.time_zone = "Europe/London"
    loader.async_setup(hass)
    hass.config_entries = ConfigEntries(hass, {})
    await hass.config_entries.async_initialize()
    await bootstrap.async_load_base_functionality(hass)
    from homeassistant.auth import auth_manager_from_config

    # `camera` (a platform `tuya` loads) needs `http`, which needs auth.
    hass.auth = await auth_manager_from_config(hass, [{"type": "homeassistant"}], [])
    conf = {"http": {"server_port": free_port(), "server_host": "127.0.0.1"}}
    assert await async_setup_component(hass, "homeassistant", conf)
    assert await async_setup_component(hass, "http", conf)
    await hass.async_start()

    # A token issued now, good for an hour, so setup refreshes nothing.
    import time

    now_ms = int(time.time() * 1000)
    entry = ConfigEntry(
        domain="tuya", title="Tuya", version=1, minor_version=1, source="user",
        data={"endpoint": base, "terminal_id": "term", "user_code": "code",
              "token_info": {"t": now_ms, "expire_time": 3600, "uid": "uid",
                             "access_token": "access1", "refresh_token": "refresh1"}},
        options={}, unique_id=None, discovery_keys={}, subentries_data=None,
    )
    with mock.patch("tuya_sharing.manager.SharingMQ", FakeMQ):
        await hass.config_entries.async_add(entry)
        await hass.async_block_till_done()

        def states():
            return {s.entity_id: {"state": s.state, "attributes": dict(s.attributes)}
                    for s in sorted(hass.states.async_all(), key=lambda s: s.entity_id)
                    if s.entity_id.split(".")[0] != "sun" and s.domain not in ("person", "zone")}

        ereg, dreg = er.async_get(hass), dr.async_get(hass)
        keep = ("entity_id", "unique_id", "platform", "original_name", "translation_key",
                "original_device_class", "original_icon", "entity_category", "capabilities",
                "supported_features", "unit_of_measurement", "has_entity_name", "disabled_by")
        rows = sorted(ereg.entities.values(), key=lambda e: e.unique_id)

        def plain(v):
            return v.value if hasattr(v, "value") else v

        entities = [{k: plain(getattr(e, k)) for k in keep} for e in rows]
        devices = sorted(
            ({"identifiers": sorted(list(i) for i in d.identifiers), "manufacturer": d.manufacturer,
              "model": d.model, "model_id": d.model_id, "name": d.name}
             for d in dreg.devices.values()),
            key=lambda d: d["identifiers"],
        )
        setup = {
            "states": states(),
            "entities": entities,
            "devices": devices,
            "topics": FakeMQ.made[-1].topics(),
            "requests": [{"method": s["method"], "path": s["path"], "query": s["query"]} for s in cloud.seen],
        }
        cloud.seen.clear()

        mq = FakeMQ.made[-1]
        pushes = []
        for msg in PUSHES:
            for listener in mq.listeners:
                await hass.async_add_executor_job(listener, msg)
            await hass.async_block_till_done()
            pushes.append({"message": msg, "states": states()})

        services = []
        for domain, service, data in SERVICES:
            cloud.seen.clear()
            error = None
            try:
                await hass.services.async_call(domain, service, data, blocking=True)
            except HomeAssistantError as e:
                error = {"class": type(e).__name__, "message": str(e),
                         "translation_key": getattr(e, "translation_key", None),
                         "translation_placeholders": getattr(e, "translation_placeholders", None)}
            await hass.async_block_till_done()
            sent = [s["body"]["commands"] for s in cloud.seen if s["method"] == "POST"]
            services.append({"call": [domain, service, data], "sent": sent, "error": error})
            # The SDK's filter drops a repeat within 10 s: each call is new.
            await asyncio.sleep(0)

    # Fixed-input crypto.
    rid, refresh = "11111111-2222-4333-8444-555555555555", "REFRESH"
    hk = hashlib.md5((rid + refresh).encode()).hexdigest()
    sec = capi._secret_generating(rid, "", hk)
    headers = {"X-appKey": "HA_3y9q4ak7g4ephrvke", "X-requestId": rid, "X-sid": "", "X-time": "123", "X-token": "TOK"}
    with mock.patch.object(capi, "_random_nonce", lambda e=32: "ABCDEFGHJKMN"):
        enc = capi._aes_gcm_encrypt('{"homeId":"42"}', sec).decode()
    crypto = {
        "rid": rid, "refresh_token": refresh, "hash_key": hk, "secret": sec,
        "headers": headers, "query": "QQ", "body": "BB",
        "sign": capi._restful_sign(hk, "QQ", "BB", headers),
        "encrypt": {"plain": '{"homeId":"42"}', "nonce": "ABCDEFGHJKMN", "out": enc},
    }

    await hass.async_stop()
    server.shutdown()
    OUT.mkdir(parents=True, exist_ok=True)
    for name, data in (("crypto", crypto), ("setup", setup), ("pushes", pushes), ("services", services)):
        (OUT / f"{name}.json").write_text(json.dumps(scrub(data), indent=1, sort_keys=True, ensure_ascii=False) + "\n")
        print(f"wrote {OUT / name}.json")


def serve_forever(host, http_port, mqtt_port):
    cloud = Cloud()
    cloud.mqtt_url = f"tcp://{host}:{mqtt_port}"
    serve(cloud, "0.0.0.0", http_port)
    print(f"fake Tuya cloud on :{http_port}, broker on :{mqtt_port}", flush=True)
    broker(mqtt_port)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--serve"]:
        serve_forever(sys.argv[2], int(sys.argv[3]), int(sys.argv[4]))
    else:
        asyncio.run(main())
