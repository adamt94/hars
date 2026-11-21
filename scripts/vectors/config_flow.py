#!/usr/bin/env python3
"""Vectors for config flows: adding SwitchBot Cloud from the UI.

    .venv/bin/python -I scripts/vectors/config_flow.py
    cargo test -p ha-api config_entries && cargo test -p ha-integrations flow

Boots Home Assistant 2026.6.1's core in a temporary config directory with
`http`, `auth`, `websocket_api` and `config` (its `config_entries` views and
commands: `ConfigManagerFlowIndexView`, `ConfigManagerFlowResourceView`,
`ConfigManagerAvailableFlowView`, `ConfigManagerEntryResourceView`), an admin
and a non-admin user, and `switchbot_cloud` with switchbot-api 2.11.0, whose
`_API_HOST` points at a server on 127.0.0.1 that answers with the recorded
responses in crates/ha-integrations/src/switchbot_cloud/vectors/ (or, when a
step says so, with an error). Each scenario runs on a fresh instance and
sends its steps over real HTTP and a real WebSocket. It writes

- crates/ha-api/src/config_entries/vectors/flow.json: for each scenario its
  steps (`http`: a request and the reply's status, content type and body;
  `ws`: a message and its reply; `api`: how the SwitchBot API answers from
  here on; `handlers`: `flow_handlers`, of which only the integrations in
  `KNOWN` are kept), the events each `config_entries/subscribe` and
  `config_entries/flow/subscribe` subscription received after each step, and
  at the end `core.config_entries`'s entries;
- crates/ha-integrations/src/switchbot_cloud/vectors/fresh_setup.json: the
  device and entity registry rows and the states HA has after the first
  scenario's entry is set up, which is what a fresh set-up must create;
  two_accounts.json: the same after the `duplicate` scenario, where a
  second account lists the same devices; retry.json: the entry's changes
  (`config_entries/subscribe`) in the `retry_recovers` scenario, which
  waits 6.5 s of real time for HA's retry; disabled_account.json: the
  entries and registry rows as stored after an account is disabled, and
  after a second account that lists the same devices is added;
- crates/ha-api/src/ws/vectors/integration_descriptions.json:
  `integration/descriptions`' entries for the integrations in `KNOWN`
  (looked up inside their brand), and the reply's other keys.

Ids are written as placeholders in order of appearance (`<flow:1>`,
`<entry:1>`), a device's registry id as `<device:IDENTIFIER>`, other
registry ids `<id>`, the webhook id HA adds to the entry's data
`<webhook>`, times `<time>` (ISO text) and `<ts>` (seconds). A step may
name a device by its placeholder. Registry rows
are sorted (HA sets the platforms up side by side, in an order that changes
from run to run), and so are the fields of a refused form's `errors`, the
handler lists and a device's `config_entries`, `identifiers` and
`connections` (sets in HA). Nothing reaches the network. Run it twice: the output
must not change.
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

PINNED = {"homeassistant": "2026.6.1", "aiohttp": "3.13.5", "switchbot-api": "2.11.0",
          "voluptuous": "0.15.2", "voluptuous-serialize": "2.7.0"}
ROOT = pathlib.Path(__file__).resolve().parents[2]
API_OUT = ROOT / "crates/ha-api/src/config_entries/vectors"
WS_OUT = ROOT / "crates/ha-api/src/ws/vectors"
SWITCHBOT = ROOT / "crates/ha-integrations/src/switchbot_cloud/vectors"
CLIENT = "http://127.0.0.1:8123/"
FLOW = "/api/config/config_entries/flow"
ENTRY = "/api/config/config_entries/entry"
# The integrations hars runs: what the vectors keep of HA's long lists.
KNOWN = ["hue", "lifx", "met", "mobile_app", "sun", "switchbot_cloud"]
BOT, HUB = "AABBCC000001", "AABBCC000002"
GOOD = {"api_token": "token-one", "api_key": "secret-one"}
OTHER = {"api_token": "token-two", "api_key": "secret-two"}


def http(method, path, body=None, user="admin"):
    """`body`: a value sent as JSON, or a str sent as it is."""
    step = {"http": {"method": method, "path": path, "user": user}}
    if body is not None:
        step["http"]["body"] = body
    return step


def ws(msg, user="admin"):
    return {"ws": {"user": user, "msg": msg}}


def api(mode):
    """How the SwitchBot API answers from here on (see `FakeApi`)."""
    return {"api": mode}


def handlers(query=""):
    return {"handlers": query}


def snapshot(name):
    """Keep the entries and the registries as stored now, under `name`."""
    return {"snapshot": name}


def user_disables(device, entity):
    """The user disables a device (by its SwitchBot id) and an entity."""
    return {"user_disables": {"device": device, "entity": entity}}


def wait(seconds):
    """Let real time pass (HA retries a setup about 5 s after it failed)."""
    return {"wait": seconds}


START = http("POST", FLOW, {"handler": "switchbot_cloud", "show_advanced_options": False})
ENTRIES = ws({"type": "config_entries/get"})
SUBSCRIBE = [("entries", {"type": "config_entries/subscribe"}),
             ("flows", {"type": "config_entries/flow/subscribe"})]

# (name, what it shows, subscriptions [(name, message)], steps)
SCENARIOS = [
    (
        "add",
        "The frontend's Add integration dialog, start to finish: the form, the "
        "credentials, the entry set up before the reply.",
        SUBSCRIBE + [("hubs", {"type": "config_entries/subscribe", "type_filter": ["hub"]}),
                     ("helpers", {"type": "config_entries/subscribe",
                                  "type_filter": ["helper"]})],
        [ws({"type": "config_entries/flow/progress"}), ENTRIES, handlers(),
         handlers("?type=integration"), handlers("?type=helper"), handlers("?type=hub"),
         START,
         http("GET", FLOW + "/<flow:1>"), ws({"type": "config_entries/flow/progress"}),
         http("POST", FLOW + "/<flow:1>", GOOD), http("GET", FLOW + "/<flow:1>"),
         http("DELETE", FLOW + "/<flow:1>"), ENTRIES,
         ws({"type": "config_entries/get", "domain": "switchbot_cloud"}),
         ws({"type": "config_entries/get", "domain": "hue"}),
         ws({"type": "config_entries/get", "type_filter": ["hub"]}),
         ws({"type": "config_entries/get", "type_filter": "hub"}),
         ws({"type": "config_entries/get", "type_filter": ["helper"]}),
         ws({"type": "config_entries/get", "type_filter": ["device", "service"]}),
         ws({"type": "config_entries/get", "type_filter": [5]}),
         ws({"type": "config_entries/get", "domain": 5}),
         ws({"type": "config_entries/get", "extra": 1})],
    ),
    (
        "bad_credentials",
        "A token the API refuses shows the form again with `invalid_auth`; the same "
        "flow then takes the right one.",
        SUBSCRIBE,
        [START, api("forbidden"), http("POST", FLOW + "/<flow:1>", GOOD),
         http("GET", FLOW + "/<flow:1>"), api("ok"), http("POST", FLOW + "/<flow:1>", GOOD)],
    ),
    (
        "cannot_connect",
        "What the API's failures show: an error status, an error `statusCode` and a "
        "refused connection are `cannot_connect`; a body that isn't JSON or has no "
        "lists is `unknown`.",
        SUBSCRIBE,
        [START, api("http500"), http("POST", FLOW + "/<flow:1>", GOOD),
         api("status190"), http("POST", FLOW + "/<flow:1>", GOOD),
         api("refused"), http("POST", FLOW + "/<flow:1>", GOOD),
         api("not_json"), http("POST", FLOW + "/<flow:1>", GOOD),
         api("no_lists"), http("POST", FLOW + "/<flow:1>", GOOD),
         api("offline"), http("POST", FLOW + "/<flow:1>", GOOD),
         api("ok"), http("POST", FLOW + "/<flow:1>", GOOD)],
    ),
    (
        "duplicate",
        "The account's token is the entry's unique id: a second flow with it aborts, "
        "one with another token makes a second entry.",
        SUBSCRIBE,
        [START, http("POST", FLOW + "/<flow:1>", GOOD), START,
         http("POST", FLOW + "/<flow:2>", {**GOOD, "api_key": "another-secret"}),
         http("GET", FLOW + "/<flow:2>"), START, http("POST", FLOW + "/<flow:3>", OTHER),
         ENTRIES],
    ),
    (
        "two_flows",
        "Two flows in progress for one integration: both stay until each is "
        "answered.",
        SUBSCRIBE,
        [START, START, http("POST", FLOW + "/<flow:1>", GOOD),
         http("GET", FLOW + "/<flow:2>"), http("POST", FLOW + "/<flow:2>", GOOD),
         http("GET", FLOW + "/<flow:2>")],
    ),
    (
        "invalid_input",
        "What the form's schema refuses, and bodies that aren't an object.",
        [],
        [START, http("POST", FLOW + "/<flow:1>", {"api_key": "x"}),
         http("POST", FLOW + "/<flow:1>", {"api_token": "x"}),
         http("POST", FLOW + "/<flow:1>", {"api_token": 5, "api_key": "x"}),
         http("POST", FLOW + "/<flow:1>", {"api_token": "x", "api_key": None}),
         http("POST", FLOW + "/<flow:1>", {**GOOD, "extra": 1}),
         http("POST", FLOW + "/<flow:1>", {**GOOD, "extra": 1, "more": 2}),
         http("POST", FLOW + "/<flow:1>", {"api_token": 5, "api_key": "x", "extra": 1}),
         http("POST", FLOW + "/<flow:1>", "[]"), http("POST", FLOW + "/<flow:1>", "5"),
         http("POST", FLOW + "/<flow:1>", '"text"'), http("POST", FLOW + "/<flow:1>", "null"),
         http("POST", FLOW + "/<flow:1>", "{not json"), http("POST", FLOW + "/<flow:1>", ""),
         http("GET", FLOW + "/<flow:1>")],
    ),
    (
        "init_errors",
        "Starting a flow for what has none, and flows that don't exist.",
        [],
        [http("POST", FLOW, {"handler": "nope"}), http("POST", FLOW, {"handler": "api"}),
         http("POST", FLOW, {}), http("POST", FLOW, {"handler": 5}),
         http("POST", FLOW, {"handler": None}), http("POST", FLOW, "[]"),
         http("POST", FLOW, "{not json"), http("POST", FLOW, ""),
         http("GET", FLOW), http("DELETE", FLOW),
         http("GET", FLOW + "/nope"), http("POST", FLOW + "/nope", {}),
         http("DELETE", FLOW + "/nope"),
         http("POST", FLOW, {"handler": "switchbot_cloud", "entry_id": "nope"}),
         http("POST", FLOW, {"handler": "switchbot_cloud", "entry_id": 5}),
         http("POST", FLOW, {"handler": "switchbot_cloud", "entry_id": None}),
         http("POST", FLOW, {"handler": "switchbot_cloud", "entry_id": []}),
         http("POST", FLOW, {"handler": "switchbot_cloud", "entry_id": ""}),
         http("DELETE", FLOW + "/<flow:1>"),
         http("POST", FLOW, {"handler": ["switchbot_cloud"]}),
         http("PUT", FLOW + "/nope", {}),
         ws({"type": "config_entries/flow/progress"})],
    ),
    (
        "abandon",
        "Closing the dialog deletes the flow.",
        SUBSCRIBE,
        [START, http("DELETE", FLOW + "/<flow:1>"), http("GET", FLOW + "/<flow:1>"),
         http("POST", FLOW + "/<flow:1>", GOOD), http("DELETE", FLOW + "/<flow:1>"), ENTRIES],
    ),
    (
        "who_may",
        "What needs an admin, and what a signed-in user or nobody may do.",
        [("entries", {"type": "config_entries/subscribe"})],
        [START, http("POST", FLOW, {"handler": "switchbot_cloud"}, user="user"),
         http("POST", FLOW, {"handler": "switchbot_cloud"}, user=None),
         http("GET", FLOW + "/<flow:1>", user="user"),
         http("GET", FLOW + "/<flow:1>", user=None),
         http("POST", FLOW + "/<flow:1>", GOOD, user="user"),
         http("POST", FLOW + "/<flow:1>", GOOD, user=None),
         http("DELETE", FLOW + "/<flow:1>", user=None),
         http("GET", FLOW + "/<flow:1>"),
         http("DELETE", FLOW + "/<flow:1>", user="user"),
         http("GET", FLOW + "/<flow:1>"),
         http("GET", FLOW + "_handlers", user="user"),
         http("GET", FLOW + "_handlers", user=None),
         START, http("POST", FLOW + "/<flow:2>", GOOD),
         ws({"type": "config_entries/flow/progress"}, user="user"),
         ws({"type": "config_entries/flow/subscribe"}, user="user"),
         ws({"type": "integration/descriptions"}, user="user"),
         ws({"type": "config_entries/get"}, user="user"),
         ws({"type": "config_entries/subscribe"}, user="user"),
         http("DELETE", ENTRY + "/<entry:1>", user="user"),
         http("DELETE", ENTRY + "/<entry:1>", user=None), ENTRIES],
    ),
    (
        "setup_not_ready",
        "The flow's check passes but the entry's setup can't read a device: the "
        "entry is made and retried.",
        SUBSCRIBE,
        [START, api("status_down"), http("POST", FLOW + "/<flow:1>", GOOD), ENTRIES],
    ),
    (
        "head",
        "HEAD on the flow and entry URLs: none of the views has it, whoever asks.",
        [],
        [START]
        + [http("HEAD", path, user=user)
           for path in (FLOW, FLOW + "/<flow:1>", FLOW + "/nope", FLOW + "_handlers",
                        ENTRY + "/nope")
           for user in ("admin", "user", None)]
        + [http("GET", FLOW + "/<flow:1>")],
    ),
    (
        "remove_while_retrying",
        "Removing an entry whose setup waits to be tried again: nothing to unload.",
        SUBSCRIBE,
        [START, api("status_down"), http("POST", FLOW + "/<flow:1>", GOOD),
         http("DELETE", ENTRY + "/<entry:1>"), ENTRIES],
    ),
    (
        "retry_recovers",
        "The retry of a setup that wasn't ready, once the API answers: written to "
        "switchbot_cloud/vectors/retry.json as the entry's changes, not to flow.json.",
        SUBSCRIBE,
        [START, api("status_down"), http("POST", FLOW + "/<flow:1>", GOOD), api("ok"),
         wait(6.5), ENTRIES],
    ),
    (
        "disabled_account",
        "An account whose Bot and whose Hub's humidity sensor the user disabled, "
        "then the account itself disabled (its other devices and entities with it), "
        "then a second account that lists the same devices: they become the second "
        "account's, enabled, but for what the user disabled. Written to switchbot_cloud/vectors/disabled_account.json "
        "as the stores before and after, not to flow.json (hars has no "
        "`config_entries/disable` yet).",
        [],
        [START, http("POST", FLOW + "/<flow:1>", GOOD),
         user_disables(BOT, "sensor.hub_2_9d_humidity"),
         ws({"type": "config_entries/disable", "entry_id": "<entry:1>",
             "disabled_by": "user"}),
         snapshot("before"), START, http("POST", FLOW + "/<flow:2>", OTHER)],
    ),
    (
        "remove",
        "Removing the entry: its devices and entities go with it.",
        SUBSCRIBE,
        [START, http("POST", FLOW + "/<flow:1>", GOOD),
         http("DELETE", ENTRY + "/nope"), http("DELETE", ENTRY + "/<entry:1>"),
         http("DELETE", ENTRY + "/<entry:1>"), ENTRIES,
         START, http("POST", FLOW + "/<flow:2>", GOOD), ENTRIES],
    ),
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


class FakeApi:
    """The SwitchBot API on 127.0.0.1, answering with the recorded bodies.

    `mode`: `ok`; `forbidden` (HTTP 403); `http500` (an error status with a
    JSON body); `status190` (HTTP 200 with an error `statusCode`);
    `offline` (`statusCode` 161); `not_json` (an HTML page); `no_lists`
    (a `body` without the device lists); `status_down` (the device list
    answers, a device's status is an error); `refused` (nothing listens:
    handled by pointing the library at a closed port).
    """

    def __init__(self):
        self.mode = "ok"
        self.port = free_port()
        self.closed = free_port()
        self.runner = None

    async def start(self):
        from aiohttp import web

        app = web.Application()
        app.router.add_route("*", "/{tail:.*}", self.handle)
        self.runner = web.AppRunner(app)
        await self.runner.setup()
        await web.TCPSite(self.runner, "127.0.0.1", self.port).start()

    async def stop(self):
        await self.runner.cleanup()

    def host(self):
        port = self.closed if self.mode == "refused" else self.port
        return f"http://127.0.0.1:{port}"

    async def handle(self, request):
        from aiohttp import web

        def reply(text, status=200):
            return web.Response(text=text, status=status, content_type="application/json")

        error = '{"statusCode":190,"body":{},"message":"error"}'
        path = request.path
        mode = self.mode
        if mode == "forbidden":
            return reply('{"message":"Unauthorized"}', 403)
        if mode == "http500":
            return reply('{"message":"Internal server error"}', 500)
        if mode == "status190":
            return reply(error)
        if mode == "offline":
            return reply('{"statusCode":161,"body":{},"message":"offline"}')
        if mode == "not_json":
            return web.Response(text="<html>down</html>", content_type="text/html")
        if mode == "no_lists":
            return reply('{"statusCode":100,"body":{},"message":"success"}')
        if path == "/v1.1/devices":
            return reply((SWITCHBOT / "devices.json").read_text())
        if path.endswith("/status"):
            if mode == "status_down":
                return reply(error)
            name = {BOT: "Bot", HUB: "Hub_2"}.get(path.split("/")[3])
            if name:
                return reply((SWITCHBOT / f"status_{name}.json").read_text())
        if path == "/v1.1/webhook/queryWebhook":
            return reply('{"statusCode":100,"body":{"urls":[]},"message":"success"}')
        if path.startswith("/v1.1/webhook/"):
            return reply('{"statusCode":100,"body":{},"message":"success"}')
        return reply(error, 404)


ULID = re.compile(r"\b[0-9A-HJKMNP-TV-Z]{26}\b")
HEX32 = re.compile(r"\b[0-9a-f]{32}\b")
HEX64 = re.compile(r"\b[0-9a-f]{64}\b")
ISO = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?\+00:00")


class Ids:
    """Flow and entry ids, numbered as they first appear."""

    def __init__(self):
        self.flows = {}
        self.entries = {}
        self.devices = {}

    def learn(self, value):
        """Take the ids out of a flow's reply or an entry."""
        if isinstance(value, dict):
            if isinstance(value.get("flow_id"), str):
                self.flows.setdefault(value["flow_id"], f"<flow:{len(self.flows) + 1}>")
            if isinstance(value.get("entry_id"), str):
                self.entries.setdefault(value["entry_id"],
                                        f"<entry:{len(self.entries) + 1}>")
            for v in value.values():
                self.learn(v)
        elif isinstance(value, list):
            for v in value:
                self.learn(v)

    def real(self, text):
        """A step's path or message with the real ids."""
        for table in (self.flows, self.entries, self.devices):
            for real, name in table.items():
                text = text.replace(name, real)
        return text

    def text(self, text):
        for table in (self.flows, self.entries, self.devices):
            for real, name in table.items():
                text = text.replace(real, name)
        return ISO.sub("<time>", HEX64.sub("<webhook>", HEX32.sub("<id>", text)))

    def plain(self, value, key=None):
        if isinstance(value, dict):
            return {self.text(k): self.plain(v, k) for k, v in value.items()}
        if isinstance(value, (list, tuple)):
            items = [self.plain(v) for v in value]
            # A device's entries, identifiers and connections are sets in HA.
            sets = ("config_entries", "identifiers", "connections")
            return sorted(items) if key in sets else items
        if isinstance(value, float) and key in ("created_at", "modified_at"):
            return "<ts>"
        if isinstance(value, str):
            return self.text(value)
        return value


def event_of(message):
    """A subscription's event: a `subscribe_events` one cut down to its
    type and data, a state to its text and attributes (the rest is times
    and context ids)."""
    event = message["event"]
    if not isinstance(event, dict) or "event_type" not in event:
        return event
    data = event["data"]
    if event["event_type"] == "state_changed":
        def state(s):
            return s and {"state": s["state"], "attributes": s["attributes"]}

        data = {"entity_id": data["entity_id"], "old_state": state(data["old_state"]),
                "new_state": state(data["new_state"])}
    return {"event_type": event["event_type"], "data": data}


class SwitchBot:
    """What `run` talks to instead of SwitchBot's cloud: `FakeApi`, which
    the library is pointed at. Another integration's generator passes its
    own (`hue_flow.py`): `domain`, whose devices get `<device:…>` names,
    `start`, `stop`, `set(mode)` for an `api` step, and `take()`, the
    requests it got since the last step (or `None` to record none)."""

    domain = "switchbot_cloud"

    def __init__(self):
        self.fake = FakeApi()

    async def start(self):
        await self.fake.start()
        self.set("ok")

    async def stop(self):
        await self.fake.stop()

    def set(self, mode):
        import switchbot_api

        self.fake.mode = mode
        switchbot_api._API_HOST = self.fake.host()  # noqa: SLF001

    def take(self):
        return None


async def run(subscriptions, steps, backend=None):
    from homeassistant import bootstrap, core, loader
    from homeassistant.auth import auth_manager_from_config
    from homeassistant.config_entries import ConfigEntries
    from homeassistant.helpers import (area_registry as ar, device_registry as dr,
                                       entity_registry as er)
    from homeassistant.setup import async_setup_component
    import aiohttp

    fake = backend or SwitchBot()
    await fake.start()
    tmp = pathlib.Path(tempfile.mkdtemp())
    ids = Ids()
    try:
        hass = core.HomeAssistant(str(tmp))
        hass.config.skip_pip = True
        await hass.config.async_load()
        loader.async_setup(hass)
        hass.config_entries = ConfigEntries(hass, {})
        await hass.config_entries.async_initialize()
        await bootstrap.async_load_base_functionality(hass)
        hass.auth = await auth_manager_from_config(hass, [{"type": "homeassistant"}], [])
        port = free_port()
        conf = {"http": {"server_port": port, "server_host": "127.0.0.1"}}
        assert await async_setup_component(hass, "homeassistant", conf)
        assert await async_setup_component(hass, "http", conf)
        for domain in ("auth", "api", "websocket_api", "config"):
            assert await async_setup_component(hass, domain, conf), domain
        await hass.async_start()
        await hass.async_block_till_done()
        tokens = {}
        for name, group in (("admin", "system-admin"), ("user", "system-users")):
            user = await hass.auth.async_create_user(name, group_ids=[group])
            refresh = await hass.auth.async_create_refresh_token(user, CLIENT)
            tokens[name] = hass.auth.async_create_access_token(refresh)
        out = []
        snapshots = {}
        async with aiohttp.ClientSession() as session:

            async def connect(who):
                sock = await session.ws_connect(f"http://127.0.0.1:{port}/api/websocket")
                await sock.receive_json()
                await sock.send_json({"type": "auth", "access_token": tokens[who]})
                assert (await sock.receive_json())["type"] == "auth_ok"
                return sock

            sockets = {who: await connect(who) for who in tokens}
            next_id = dict.fromkeys(tokens, 1)

            async def call(who, msg):
                """A command's reply; anything before it is unexpected."""
                sent = {"id": next_id[who], **msg}
                next_id[who] += 1
                await sockets[who].send_json(sent)
                replies = []
                while True:
                    reply = await asyncio.wait_for(sockets[who].receive_json(), 10)
                    replies.append(reply)
                    if reply.get("type") in ("result", "pong"):
                        return replies

            # Each subscription on a socket of its own: what it first
            # sends, then what each step makes it send.
            subs = []
            opened = []
            for name, msg in subscriptions:
                sock = await connect("admin")
                await sock.send_json({"id": 1, **msg})
                # `subscribe_events` answers with its result alone.
                count = 1 if msg["type"] == "subscribe_events" else 2
                first = []
                while len(first) < count:
                    first.append(await asyncio.wait_for(sock.receive_json(), 10))
                subs.append((name, sock, [2]))
                opened.append({"sub": name, "msg": msg, "first": first})

            async def drain():
                """What the subscriptions got: everything before a pong."""
                for _ in range(3):
                    await hass.async_block_till_done()
                got = []
                for name, sock, counter in subs:
                    await sock.send_json({"id": counter[0], "type": "ping"})
                    counter[0] += 1
                    while True:
                        m = await asyncio.wait_for(sock.receive_json(), 10)
                        if m.get("type") == "pong":
                            break
                        ids.learn(m)
                        got.append({"sub": name, "event": event_of(m)})
                # The devices there are now, for the steps that name one.
                name_devices()
                return got

            def name_devices():
                """A device's registry id as `<device:IDENTIFIER>` (the
                first of its identifiers, sorted: a set in HA)."""
                for device in dr.async_get(hass).devices.values():
                    idents = sorted(i for d, i in device.identifiers if d == fake.domain)
                    if idents:
                        ids.devices[device.id] = f"<device:{idents[0]}>"

            from homeassistant.helpers.json import json_dumps as dumps

            def stores():
                def stored(registry):
                    return json.loads(dumps(registry._data_to_save()))  # noqa: SLF001

                return {
                    "entries": stored(hass.config_entries)["entries"],
                    "devices": sorted(stored(dr.async_get(hass))["devices"],
                                      key=lambda d: json.dumps(sorted(d["identifiers"]))),
                    "entities": sorted(stored(er.async_get(hass))["entities"],
                                       key=lambda e: e["entity_id"]),
                }

            for step in steps:
                if "user_disables" in step:
                    devices, entities = dr.async_get(hass), er.async_get(hass)
                    device = devices.async_get_device(
                        identifiers={("switchbot_cloud", step["user_disables"]["device"])})
                    devices.async_update_device(
                        device.id, disabled_by=dr.DeviceEntryDisabler.USER)
                    entities.async_update_entity(
                        step["user_disables"]["entity"],
                        disabled_by=er.RegistryEntryDisabler.USER)
                    for _ in range(3):
                        await hass.async_block_till_done()
                    continue
                if "snapshot" in step:
                    for _ in range(3):
                        await hass.async_block_till_done()
                    snapshots[step["snapshot"]] = stores()
                    continue
                if "api" in step:
                    fake.set(step["api"])
                    out.append(step)
                    continue
                if "wait" in step:
                    await asyncio.sleep(step["wait"])
                    out.append({**step, "events": await drain()})
                    continue
                if "handlers" in step:
                    url = f"http://127.0.0.1:{port}{FLOW}_handlers{step['handlers']}"
                    headers = {"Authorization": f"Bearer {tokens['admin']}"}
                    async with session.get(url, headers=headers) as r:
                        # A type HA has no list for is a 500.
                        listed = await r.json() if r.status == 200 else []
                        out.append({**step, "status": r.status,
                                    "known": sorted(set(listed) & set(KNOWN)),
                                    "has_more": len(listed) > len(KNOWN)})
                    continue
                if "ws" in step:
                    who, msg = step["ws"]["user"], step["ws"]["msg"]
                    replies = await call(who, json.loads(ids.real(json.dumps(msg))))
                    ids.learn(replies)
                    for reply in replies:
                        reply.pop("id")
                    result = {**step, "replies": replies}
                    # A subscription a step opens is closed again.
                    if msg["type"].endswith("subscribe") and replies[-1].get("success"):
                        await call(who, {"type": "unsubscribe_events",
                                         "subscription": next_id[who] - 1})
                else:
                    request = step["http"]
                    headers = {}
                    if request["user"] is not None:
                        headers["Authorization"] = f"Bearer {tokens[request['user']]}"
                    body = request.get("body")
                    data = None
                    if body is not None:
                        data = body if isinstance(body, str) else json.dumps(body)
                        request = {k: v for k, v in request.items() if k != "body"}
                        request["sent"] = data
                    url = f"http://127.0.0.1:{port}{ids.real(request['path'])}"
                    async with session.request(request["method"], url, data=data,
                                               headers=headers,
                                               skip_auto_headers=["Content-Type"]) as r:
                        text = await r.text()
                        result = {"http": request, "status": r.status,
                                  "content_type": r.headers.get("Content-Type"),
                                  "allow": r.headers.get("Allow")}
                    try:
                        result["json"] = json.loads(text)
                        ids.learn(result["json"])
                        errors = result["json"].get("errors") if r.status == 400 else None
                        if errors:
                            # HA names the missing keys in an order that
                            # changes from run to run (voluptuous walks a set).
                            result["json"]["errors"] = dict(sorted(errors.items()))
                    except ValueError:
                        result["text"] = text
                    if request["path"].endswith("_handlers") and r.status == 200:
                        # A set in HA, and long: as a `handlers` step keeps it.
                        listed = result.pop("json")
                        result["known"] = sorted(set(listed) & set(KNOWN))
                        result["has_more"] = len(listed) > len(KNOWN)
                result["events"] = await drain()
                requests = fake.take()
                if requests is not None:
                    result["requests"] = requests
                out.append(result)
            for sock in [*sockets.values(), *(s for _, s, _ in subs)]:
                await sock.close()
        for _ in range(3):
            await hass.async_block_till_done()
        # Through JSON first: the stores' rows are HA's own objects.
        from homeassistant.helpers.json import json_dumps

        def stored(registry):
            return json.loads(json_dumps(registry._data_to_save()))  # noqa: SLF001

        devices = dr.async_get(hass)
        entities = er.async_get(hass)
        name_devices()
        states = {
            s.entity_id: {"state": s.state, "attributes": dict(s.attributes)}
            for s in sorted(hass.states.async_all(), key=lambda s: s.entity_id)
            if entities.async_get(s.entity_id)
        }
        result = {
            "snapshots": snapshots,
            "subscriptions": opened,
            "steps": out,
            "entries": stored(hass.config_entries)["entries"],
            "devices": sorted(stored(devices)["devices"],
                              key=lambda d: json.dumps(sorted(d["identifiers"]))),
            "entities": sorted(stored(entities)["entities"], key=lambda e: e["entity_id"]),
            "states": json.loads(json_dumps(states)),
            "areas": stored(ar.async_get(hass))["areas"],
            "deleted_devices": len(stored(devices)["deleted_devices"]),
            "deleted_entities": len(stored(entities)["deleted_entities"]),
        }
        result = ids.plain(result)
        await hass.async_stop(force=True)
    finally:
        await fake.stop()
        shutil.rmtree(tmp)
    return result


async def descriptions():
    """`integration/descriptions`, cut down to the integrations hars runs."""
    from homeassistant import core, loader

    tmp = pathlib.Path(tempfile.mkdtemp())
    try:
        hass = core.HomeAssistant(str(tmp))
        loader.async_setup(hass)
        hass.config.skip_pip = True
        reply = await loader.async_get_integration_descriptions(hass)
        await hass.async_stop(force=True)
    finally:
        shutil.rmtree(tmp)
    found = {}
    for domain, item in reply["core"]["integration"].items():
        if domain in KNOWN and "integration_type" in item:
            found[domain] = {"brand": None, "description": item}
        for inner, description in item.get("integrations", {}).items():
            if inner in KNOWN:
                found[inner] = {"brand": domain, "description": description}
    return {
        "keys": {k: sorted(v) for k, v in reply.items()},
        "custom": reply["custom"],
        "known": dict(sorted(found.items())),
        "helpers_known": sorted(set(reply["core"]["helper"]) & set(KNOWN)),
        "translated_name_known": sorted(set(reply["core"]["translated_name"]) & set(KNOWN)),
    }


def write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=1, ensure_ascii=False) + "\n")


async def main():
    check_versions()
    scenarios = []
    for name, doc, subscriptions, steps in SCENARIOS:
        result = await run(subscriptions, json.loads(json.dumps(steps)))
        rows = {k: result.pop(k) for k in ("devices", "entities", "states")}
        result.pop("areas")
        if name == "add":
            write(SWITCHBOT / "fresh_setup.json", rows)
        if name == "duplicate":
            # Two accounts that list the same devices.
            write(SWITCHBOT / "two_accounts.json", rows)
        if name == "disabled_account":
            write(SWITCHBOT / "disabled_account.json",
                  {"doc": doc, "before": result["snapshots"]["before"],
                   "after": {"entries": result["entries"], **rows}})
            continue
        result.pop("snapshots")
        if name == "retry_recovers":
            changes = [
                {"type": change["type"], "state": change["entry"]["state"],
                 "reason": change["entry"]["reason"],
                 "supports_unload": change["entry"]["supports_unload"]}
                for step in result["steps"] for event in step.get("events", [])
                if event["sub"] == "entries" for change in event["event"]
            ]
            write(SWITCHBOT / "retry.json", {"doc": doc, "changes": changes})
            continue
        scenarios.append({"name": name, "doc": doc, **result})
    write(API_OUT / "flow.json", {"scenarios": scenarios})
    write(WS_OUT / "integration_descriptions.json", await descriptions())


if __name__ == "__main__":
    asyncio.run(main())
    # CPython 3.14.7 can crash collecting HA's modules at shutdown, after
    # everything is written: skip the teardown.
    sys.stdout.flush()
    os._exit(0)
