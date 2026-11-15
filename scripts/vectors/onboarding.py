#!/usr/bin/env python3
"""Vectors for onboarding: HA's answers to every call the onboarding pages make.

    python3.14 -m venv .venv
    .venv/bin/pip install homeassistant==2026.6.1 aiohasupervisor==0.4.3 PyMetno==0.13.0
    .venv/bin/python -I scripts/vectors/onboarding.py

Boots Home Assistant 2026.6.1's core in a temporary config directory with
the components the onboarding pages talk to (`http`, `auth`,
`websocket_api`, `person`, `sun` (whose first start imports its config
entry, as `default_config` does), `onboarding`, `analytics` and `config`'s
`config/core/*` commands), then replays the requests in each scenario below
over real HTTP and WebSocket connections. It writes, to
crates/ha-api/src/onboarding/vectors/:

- `<scenario>.json`: each request and HA's reply (status, body, the
  WebSocket messages), and the `.storage` files after the scenario;
- values that are random or time-dependent (ids, codes, tokens, hashes,
  timestamps) are replaced by `<kind>` placeholders, as the Rust test does
  with hars's replies.

Nothing reaches the network: `whoami` (location detection) is answered by
a local server; met's forecast fetch is made to fail (its entry is created
and then waits to retry, as with no network); the other integrations HA
starts after the core config step (radio_browser, google_translate) fail at
their imports, as their libraries aren't installed (`skip_pip`); and
shopping_list needs nothing. Run it twice: the output must not change.
"""

import asyncio
import importlib.metadata
import json
import os
import pathlib
import re
import shutil
import socket
import sys
import tempfile
from unittest.mock import patch

PINNED = {
    "homeassistant": "2026.6.1",
    "voluptuous": "0.15.2",
    "aiohasupervisor": "0.4.3",
    "PyMetno": "0.13.0",
}
OUT = (
    pathlib.Path(__file__).resolve().parents[2]
    / "crates/ha-api/src/onboarding/vectors"
)
CLIENT = "http://127.0.0.1:8123/"
REDIRECT = "http://127.0.0.1:8123/?auth_callback=1"
USER = {
    "client_id": CLIENT,
    "name": "Test Name",
    "username": "test-user",
    "password": "test-pass",
    "language": "de",
}
# What `onboarding-core-config.ts` sends (a public city centre).
CORE = {
    "location_name": "Home",
    "latitude": 51.5,
    "longitude": -0.12,
    "elevation": 0,
    "unit_system": "metric",
    "time_zone": "Europe/London",
    "currency": "GBP",
    "country": "GB",
    "language": "en-GB",
}
WHOAMI = {
    "ip": "192.0.2.1",
    "country": "GB",
    "currency": "GBP",
    "region_code": "ENG",
    "region": "England",
    "city": "London",
    "postal_code": "SW1A",
    "timezone": "Europe/London",
    "latitude": "51.5",
    "longitude": "-0.12",
}
# Written with nothing random in them: compared byte for byte.
RAW = ["onboarding", "core.config", "core.analytics"]
STATES = ["person.test_name", "zone.home"]
STORES = [
    "onboarding",
    "core.config",
    "core.analytics",
    "core.area_registry",
    "person",
    "core.entity_registry",
    "core.config_entries",
    "auth",
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


# ----- placeholders -----

PATTERNS = [
    ("jwt", re.compile(r"^[\w-]{20,}\.[\w-]{20,}\.[\w-]{20,}$")),
    ("time", re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?\+00:00$")),
    ("ulid", re.compile(r"^[0-9A-HJKMNP-TV-Z]{26}$")),
    ("hex32", re.compile(r"^[0-9a-f]{32}$")),
    ("hex128", re.compile(r"^[0-9a-f]{128}$")),
]
TIME_KEYS = {"created_at", "modified_at", "last_used_at", "expire_at", "time_fired"}


def scrub(value, key=None):
    """Random and time-dependent values become `<kind>`."""
    if isinstance(value, dict):
        return {k: scrub(v, k) for k, v in value.items()}
    if isinstance(value, list):
        return [scrub(v) for v in value]
    if key in TIME_KEYS and value is not None:
        return "<time>"
    if key == "password":
        return "<hash>"
    if key == "context" and isinstance(value, dict):
        return value
    if isinstance(value, str):
        for kind, pattern in PATTERNS:
            if pattern.match(value):
                return f"<{kind}>"
        # sun's unique ids: `<entry id>-<sensor>`.
        if PATTERNS[2][1].match(value[:26]) and value[26:27] == "-":
            return "<ulid>" + value[26:]
    return value


def ordered(store):
    """Rows HA's setup tasks add in a racing order, sorted."""
    data = store.get("data", {})
    for key, by in (("entries", "domain"), ("entities", "entity_id")):
        if key in data:
            data[key].sort(key=lambda row: row[by])
    return store


class Run:
    """One HA instance on a config directory."""

    def __init__(self, config_dir, whoami_url):
        self.config_dir = config_dir
        self.whoami_url = whoami_url
        self.steps = []
        self.ws = None
        self.token = None
        self.code = None
        self.ws_id = 0

    async def start(self):
        from homeassistant import bootstrap, core, loader
        from homeassistant.auth import auth_manager_from_config
        from homeassistant.components.config import (
            config_entries as config_entries_ws,
            core as config_core,
        )
        from homeassistant.config_entries import ConfigEntries
        from homeassistant.setup import async_setup_component

        hass = self.hass = core.HomeAssistant(self.config_dir)
        hass.config.skip_pip = True
        await hass.config.async_load()
        loader.async_setup(hass)
        hass.config_entries = ConfigEntries(hass, {})
        await bootstrap.async_load_base_functionality(hass)
        hass.auth = await auth_manager_from_config(
            hass, [{"type": "homeassistant"}], []
        )
        self.port = free_port()
        http = {"server_port": self.port, "server_host": "127.0.0.1"}
        assert await async_setup_component(hass, "http", {"http": http})
        for domain in (
            "auth",
            "websocket_api",
            "person",
            "analytics",
            "sun",
            "onboarding",
        ):
            assert await async_setup_component(hass, domain, {}), domain
        # `config`'s sections the pages use (the rest needs `frontend`).
        config_core.async_setup(hass)
        config_entries_ws.async_setup(hass)
        await hass.async_start()
        await hass.async_block_till_done()
        import aiohttp

        self.session = aiohttp.ClientSession()

    async def stop(self):
        if self.ws is not None:
            await self.ws.close()
        await self.session.close()
        await self.hass.async_stop()

    # ----- HTTP -----

    async def http(self, method, path, body=None, auth=False, note=None):
        """`body`: a dict/list is sent as JSON, a str as it is."""
        headers = {}
        if auth:
            headers["Authorization"] = f"Bearer {self.token}"
        data = body if isinstance(body, str) or body is None else json.dumps(body)
        url = f"http://127.0.0.1:{self.port}{path}"
        async with self.session.request(method, url, data=data, headers=headers) as r:
            text = await r.text()
            try:
                reply = json.loads(text)
            except ValueError:
                reply = text
            step = {
                "http": {"method": method, "path": path, "auth": auth},
                "status": r.status,
                "body": scrub(reply),
                "content_type": r.headers.get("Content-Type", "").split(";")[0],
            }
            if body is not None:
                step["http"]["body"] = body
            if "WWW-Authenticate" in r.headers:
                step["www_authenticate"] = r.headers["WWW-Authenticate"].replace(
                    str(self.port), "<port>"
                )
            if note:
                step["note"] = note
            self.steps.append(step)
            return reply

    async def token_from(self, code, client_id=CLIENT):
        url = f"http://127.0.0.1:{self.port}/auth/token"
        form = {"grant_type": "authorization_code", "code": code, "client_id": client_id}
        async with self.session.post(url, data=form) as r:
            reply = await r.json()
        self.steps.append(
            {
                "token": {"client_id": client_id},
                "status": r.status,
                "body": scrub(reply),
            }
        )
        self.token = reply.get("access_token")
        return reply

    # ----- WebSocket -----

    async def connect(self):
        url = f"http://127.0.0.1:{self.port}/api/websocket"
        self.ws = await self.session.ws_connect(url)
        await self.ws.receive_json()
        await self.ws.send_json({"type": "auth", "access_token": self.token})
        assert (await self.ws.receive_json())["type"] == "auth_ok"

    async def call(self, msg, replies=1, note=None):
        """Send `msg` with the next id; record the next `replies` messages."""
        self.ws_id += 1
        sent = {"id": self.ws_id, **msg}
        await self.ws.send_json(sent)
        got = []
        for _ in range(replies):
            reply = await asyncio.wait_for(self.ws.receive_json(), 10)
            if msg["type"] == "config_entries/subscribe" and reply["type"] == "event":
                reply["event"].sort(key=lambda e: e["entry"]["domain"])
            if msg["type"] == "get_config":
                # A set in HA, and a temporary directory.
                reply["result"]["components"].sort()
                reply["result"]["config_dir"] = "<config_dir>"
            got.append(scrub(reply))
        step = {"ws": sent, "replies": got}
        if note:
            step["note"] = note
        self.steps.append(step)
        return got

    # ----- files and states -----

    def stores(self):
        out = {}
        for key in STORES:
            path = pathlib.Path(self.config_dir, ".storage", key)
            if path.exists():
                out[key] = ordered(scrub(json.loads(path.read_text())))
        return out

    def raw(self):
        """Files with nothing random in them, byte for byte."""
        out = {}
        for key in RAW:
            path = pathlib.Path(self.config_dir, ".storage", key)
            if path.exists():
                out[key] = path.read_text()
        return out

    def states(self):
        out = {}
        for entity_id in STATES:
            if (state := self.hass.states.get(entity_id)) is not None:
                d = state.as_dict()
                out[entity_id] = scrub(
                    {"state": d["state"], "attributes": dict(d["attributes"])}
                )
        return out


async def whoami_server(answer):
    """A local stand-in for services.home-assistant.io/whoami/v1."""
    from aiohttp import web

    async def handle(_request):
        return answer()

    app = web.Application()
    app.router.add_get("/whoami/v1", handle)
    runner = web.AppRunner(app)
    await runner.setup()
    port = free_port()
    await web.TCPSite(runner, "127.0.0.1", port).start()
    return runner, f"http://127.0.0.1:{port}/whoami/v1"


async def frontend(dir_, whoami):
    """The onboarding pages' calls, in order, with bad input along the way."""
    run = Run(dir_, whoami)
    await run.start()
    with patch("homeassistant.util.location.WHOAMI_URL", whoami):
        await run.http("GET", "/api/onboarding")
        await run.http("POST", "/api/onboarding/users", "{")
        await run.http("POST", "/api/onboarding/users", [])
        await run.http(
            "POST",
            "/api/onboarding/users",
            {k: v for k, v in USER.items() if k != "language"},
        )
        await run.http("POST", "/api/onboarding/users", {**USER, "name": 1})
        await run.http("POST", "/api/onboarding/users", {**USER, "x": 1})
        await run.http("POST", "/api/onboarding/core_config", note="no token")
        reply = await run.http("POST", "/api/onboarding/users", USER)
        await run.http("POST", "/api/onboarding/users", USER)
        await run.http("GET", "/api/onboarding")
        await run.http("GET", "/api/onboarding/installation_type")
        await run.token_from(reply["auth_code"])
        await run.connect()

        bad = [
            {"country": "XX"},
            {"country": 5},
            {"currency": "XXX"},
            {"language": "xx"},
            {"latitude": 91},
            {"latitude": "north"},
            {"longitude": -180.5},
            {"elevation": 1.5},
            {"elevation": "1"},
            {"radius": -1},
            {"radius": "12"},
            {"unit_system": "kelvin"},
            {"unit_system": 5},
            {"external_url": "http://example.com/path"},
            {"external_url": "ftp://example.com"},
            {"internal_url": "not a url"},
            {"location_name": 5},
            {"time_zone": 5},
            {"update_units": "yes"},
            {"foo": 1},
        ]
        for fields in bad:
            await run.call({"type": "config/core/update", **fields})
        await run.call(
            {"type": "config/core/update", "time_zone": "Mars/Phobos"},
            note="a bad time zone is an invalid_info error",
        )
        await run.call(
            {"type": "subscribe_events", "event_type": "core_config_updated"}
        )
        # The frontend sends lower and upper case, ints and floats.
        await run.call(
            {
                "type": "config/core/update",
                "unit_system": "Imperial",
                "latitude": 52,
                "radius": "150",
                "update_units": True,
            },
            replies=2,
        )
        await run.call({"type": "config/core/update", **CORE}, replies=2)
        await run.call(
            {
                "type": "config/core/update",
                "external_url": "https://example.com/",
                "internal_url": None,
            },
            replies=2,
        )
        await run.call({"type": "get_config"})
        await run.call({"type": "config/core/detect"}, note="whoami answers")
        await run.http("POST", "/api/onboarding/core_config", auth=True)
        await run.http("POST", "/api/onboarding/core_config", auth=True)
        await run.hass.async_block_till_done()

        await run.call({"type": "analytics"})
        await run.call({"type": "analytics/preferences", "preferences": {"base": "yes"}})
        await run.call({"type": "analytics/preferences", "preferences": {"x": True}})
        await run.call({"type": "analytics/preferences", "preferences": {}})
        await run.call(
            {"type": "analytics/preferences", "preferences": {"base": True, "usage": False}}
        )
        await run.call({"type": "analytics"})
        await run.http("POST", "/api/onboarding/analytics", auth=True)
        await run.http("POST", "/api/onboarding/analytics", auth=True)

        await run.call({"type": "config_entries/flow/subscribe"}, replies=2)
        await run.call(
            {"type": "config_entries/subscribe", "type_filter": ["device", "hub", "service"]},
            replies=2,
        )
        await run.http("POST", "/api/onboarding/integration/wait", {})
        await run.http("POST", "/api/onboarding/integration/wait", {"domain": "person"})
        await run.http("POST", "/api/onboarding/integration/wait", {"domain": "nope"})
        await run.http(
            "POST", "/api/onboarding/integration", {"client_id": CLIENT}, auth=True
        )
        reply = await run.http(
            "POST",
            "/api/onboarding/integration",
            {"client_id": CLIENT, "redirect_uri": REDIRECT},
            auth=True,
        )
        await run.http(
            "POST",
            "/api/onboarding/integration",
            {"client_id": CLIENT, "redirect_uri": REDIRECT},
            auth=True,
        )
        await run.http("GET", "/api/onboarding")
        await run.http("GET", "/api/onboarding/installation_type")
        await run.token_from(reply["auth_code"])
        await run.hass.async_block_till_done()
        states = run.states()
    await run.stop()
    return {
        "steps": run.steps,
        "states": states,
        "stores": run.stores(),
        "raw": run.raw(),
    }


async def bad_redirect(dir_, whoami):
    """The integration step is marked done before the redirect is checked."""
    run = Run(dir_, whoami)
    await run.start()
    reply = await run.http("POST", "/api/onboarding/users", {**USER, "language": "en"})
    await run.token_from(reply["auth_code"])
    await run.http(
        "POST",
        "/api/onboarding/integration",
        {"client_id": CLIENT, "redirect_uri": "http://example.com/"},
        auth=True,
    )
    await run.http(
        "POST",
        "/api/onboarding/integration",
        {"client_id": CLIENT, "redirect_uri": REDIRECT},
        auth=True,
    )
    await run.http("GET", "/api/onboarding")
    await run.connect()
    with patch("homeassistant.util.location.WHOAMI_URL", whoami):
        await run.call({"type": "config/core/detect"}, note="whoami fails")
    await run.stop()
    return {"steps": run.steps, "stores": run.stores()}


async def restarted(dir_, whoami):
    """After a restart with every step done, the views are gone."""
    run = Run(dir_, whoami)
    await run.start()
    await run.http("GET", "/api/onboarding")
    await run.http("GET", "/api/onboarding/installation_type")
    await run.http("POST", "/api/onboarding/users", USER)
    await run.http("POST", "/api/onboarding/integration/wait", {"domain": "person"})
    await run.stop()
    return {"steps": run.steps}


def write(name, data):
    OUT.mkdir(parents=True, exist_ok=True)
    text = json.dumps(data, indent=1, ensure_ascii=False) + "\n"
    (OUT / f"{name}.json").write_text(text)


async def main():
    from aiohttp import web

    check_versions()
    ok = {"value": True}

    def answer():
        if ok["value"]:
            return web.json_response(WHOAMI)
        return web.Response(status=503, text="down")

    async def no_forecast(_self):
        return False

    runner, whoami = await whoami_server(answer)
    offline = patch("metno.MetWeatherData.fetching_data", no_forecast)
    offline.start()
    tmp = pathlib.Path(tempfile.mkdtemp())
    try:
        write("frontend", await frontend(str(tmp / "a"), whoami))
        write("restarted", await restarted(str(tmp / "a"), whoami))
        ok["value"] = False
        write("bad_redirect", await bad_redirect(str(tmp / "b"), whoami))
    finally:
        offline.stop()
        await runner.cleanup()
        shutil.rmtree(tmp)


if __name__ == "__main__":
    asyncio.run(main())
    # CPython 3.14.7 can crash collecting HA's modules at shutdown, after
    # everything is written: skip the teardown.
    sys.stdout.flush()
    os._exit(0)
