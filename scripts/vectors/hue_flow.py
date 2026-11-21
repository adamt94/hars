#!/usr/bin/env python3
"""Vectors for adding a Philips Hue bridge from the UI.

    .venv/bin/python -I scripts/vectors/hue_flow.py
    cargo test -p ha-api config_entries && cargo test -p ha-integrations hue

Runs on `config_flow.py`'s Home Assistant 2026.6.1 (its `run`) with `hue`
and aiohue 4.8.1. Nothing reaches a bridge or discovery.meethue.com: every
request aiohue makes is redirected to two servers on 127.0.0.1 and recorded
as it was made (its method and URL, its JSON body, whether it carried the
application key).

- The bridge, at the made-up address `192.0.2.10`: `GET /api/config` (the
  v1 description every bridge gives), `POST /api` (a new application key:
  error 101 until the link button is "pressed"), `GET /clip/v2/resource`
  (403 without the key, else the recorded resources in
  crates/ha-integrations/src/hue/vectors/resource.json), the event stream
  (opened and left silent) and `PUT`s (accepted).
- Discovery: `GET /` lists that bridge.

An `api` step changes how they answer from then on (see `Hue.set`).

It writes
- crates/ha-api/src/config_entries/vectors/hue_flow.json: for each scenario
  its steps, as `config_flow.py` writes them, each with the requests the
  bridge and discovery got during it (but for the event stream's, which
  aiohue opens whenever its task gets to it), and for the first scenario
  the rows and states below;
- crates/ha-integrations/src/hue/vectors/fresh_setup.json: the entry, the
  device and entity registry rows and the states HA has after the first
  scenario's entry is set up: what a fresh set-up must create.

Placeholders are `config_flow.py`'s. A light group's `hue_scenes`, `lights`
and `entity_id` attributes are sorted (sets in HA). The application key the fake bridge
gives is not a secret. Run it twice: the output must not change.
"""

import asyncio
import importlib.metadata
import json
import os
import pathlib
import sys

# After the standard library: this directory has an `http.py` of its own.
sys.path.append(str(pathlib.Path(__file__).resolve().parent))

import config_flow as cf  # noqa: E402

OUT = cf.API_OUT / "hue_flow.json"
AIOHUE = "4.8.1"
HUE = cf.ROOT / "crates/ha-integrations/src/hue/vectors"
# TEST-NET-1: an address no bridge has.
HOST = "192.0.2.10"
OTHER = "192.0.2.11"
BRIDGE_ID = "C42996FFFE000001"
KEY = "fake-hue-application-key-0000000000000001"
DISCOVERY = "https://discovery.meethue.com/"


class Hue:
    """The bridge and the discovery service, and aiohttp sent to them."""

    domain = "hue"

    def __init__(self):
        self.pressed = False
        self.bridge = "ok"
        self.discovery = "one"
        self.requests = []
        self.runners = []
        self.ports = {}
        self.original = None
        # The resources as JSON text, if not the recorded ones
        # (`hue_rows.py` adds to them).
        self.resources = None

    def set(self, mode):
        """`pressed` / `not_pressed`: the link button. `bridge_ok`,
        `bridge_down` (nothing listens), `bridge_not_hue` (`/api/config`
        without a bridge id), `bridge_v1` (no v2 API), `bridge_broken`
        (`POST /api` answers 500), `key_refused` (the resources answer 403
        to the key too), `resources_down` (the resources' connection is
        cut). `discovery_one`, `discovery_none` (an empty list),
        `discovery_down` (nothing listens), `discovery_slow` (answers
        after 6 s, past HA's 5), `discovery_two` (also a bridge that isn't
        there)."""
        if mode in ("pressed", "not_pressed"):
            self.pressed = mode == "pressed"
        elif mode.startswith("discovery_"):
            self.discovery = mode.removeprefix("discovery_")
        elif mode == "ok":
            self.pressed, self.bridge, self.discovery = False, "ok", "one"
        else:
            self.bridge = mode.removeprefix("bridge_")

    def take(self):
        taken, self.requests = self.requests, []
        return taken

    async def start(self):
        import aiohttp
        from aiohttp import web

        for name, handler in (("bridge", self.serve_bridge),
                              ("discovery", self.serve_discovery)):
            app = web.Application()
            app.router.add_route("*", "/{tail:.*}", handler)
            # The event stream is held open: don't wait for it at the end.
            runner = web.AppRunner(app, shutdown_timeout=0.1)
            await runner.setup()
            self.ports[name] = cf.free_port()
            await web.TCPSite(runner, "127.0.0.1", self.ports[name]).start()
            self.runners.append(runner)
        self.ports["closed"] = cf.free_port()
        self.original = original = aiohttp.ClientSession._request  # noqa: SLF001
        fake = self

        async def request(session, method, url, **kwargs):
            target = fake.redirect(method, str(url), kwargs)
            return await original(session, method, target or url, **kwargs)

        aiohttp.ClientSession._request = request  # noqa: SLF001

    async def stop(self):
        import aiohttp

        aiohttp.ClientSession._request = self.original  # noqa: SLF001
        for runner in self.runners:
            await runner.cleanup()

    def redirect(self, method, url, kwargs):
        """Where a request aiohue made really goes, having noted it."""
        local = "http://127.0.0.1:{}"
        if url == DISCOVERY:
            port = "closed" if self.discovery == "down" else "discovery"
            self.note(method, url, kwargs)
            return local.format(self.ports[port]) + "/"
        for scheme in ("http", "https"):
            for host in (HOST, OTHER):
                base = f"{scheme}://{host}/"
                if not url.startswith(base):
                    continue
                path = url.removeprefix(base)
                if not path.startswith("eventstream"):
                    self.note(method, url, kwargs)
                down = host == OTHER or self.bridge == "down"
                port = "closed" if down else "bridge"
                return local.format(self.ports[port]) + "/" + path
        return None

    def note(self, method, url, kwargs):
        headers = kwargs.get("headers") or {}
        request = {"method": method, "url": url}
        if kwargs.get("json") is not None:
            request["json"] = kwargs["json"]
        if any(k.lower() == "hue-application-key" for k in headers):
            request["key"] = True
        self.requests.append(request)

    async def serve_discovery(self, request):
        from aiohttp import web

        if self.discovery == "slow":
            await asyncio.sleep(6)
        found = []
        if self.discovery in ("one", "two", "slow"):
            found.append({"id": BRIDGE_ID.lower(), "internalipaddress": HOST, "port": 443})
        if self.discovery == "two":
            found.append({"id": "c42996fffe000002", "internalipaddress": OTHER,
                          "port": 443})
        return web.json_response(found)

    async def serve_bridge(self, request):
        from aiohttp import web

        path, method = request.path, request.method
        key = request.headers.get("hue-application-key")
        if path == "/api/config":
            config = {"name": "Hue Bridge", "datastoreversion": "172",
                      "swversion": "1971060010", "apiversion": "1.71.0",
                      "mac": "c4:29:96:00:00:01", "bridgeid": BRIDGE_ID,
                      "factorynew": False, "replacesbridgeid": None,
                      "modelid": "BSB003", "starterkitid": ""}
            if self.bridge == "not_hue":
                del config["bridgeid"]
            return web.json_response(config)
        if path == "/api" and method == "POST":
            if self.bridge == "broken":
                return web.Response(status=500, text="broken")
            if not self.pressed:
                return web.json_response([{"error": {
                    "type": 101, "address": "", "description": "link button not pressed"}}])
            return web.json_response([{"success": {"username": KEY,
                                                    "clientkey": "FAKECLIENTKEY"}}])
        if path == "/clip/v2/resource" and method == "GET":
            if self.bridge == "v1":
                return web.Response(status=404, text="Not found")
            if key != KEY or self.bridge == "key_refused":
                return web.json_response(
                    {"errors": [{"description": "unauthorized user"}], "data": []},
                    status=403)
            if self.bridge == "resources_down":
                request.transport.close()
                return web.Response()
            return web.Response(
                text=self.resources or (HUE / "resource.json").read_text(),
                content_type="application/json")
        if path == "/eventstream/clip/v2":
            stream = web.StreamResponse(headers={"Content-Type": "text/event-stream"})
            await stream.prepare(request)
            await stream.write(b": hi\n\n")
            try:
                await asyncio.sleep(3600)
            except asyncio.CancelledError, ConnectionResetError:
                pass
            return stream
        if method == "PUT":
            return web.json_response({"errors": [], "data": []})
        return web.json_response({"errors": [{"description": "not found"}], "data": []},
                                 status=404)


START = cf.http("POST", cf.FLOW, {"handler": "hue", "show_advanced_options": False})
FLOW1 = f"{cf.FLOW}/<flow:1>"
BRIDGE = BRIDGE_ID.lower()[:6] + BRIDGE_ID.lower()[-6:]
PICK = cf.http("POST", FLOW1, {"id": BRIDGE})
PRESS = cf.http("POST", FLOW1, {})
ADD = [START, PICK, cf.api("pressed"), PRESS]

SCENARIOS = [
    ("add",
     "Discovery finds the bridge, it is picked, the link button isn't pressed"
     " the first time and is the second: the entry is made and set up.",
     cf.SUBSCRIBE,
     [cf.ENTRIES, cf.ws({"type": "config_entries/flow/progress"}), START,
      cf.http("GET", FLOW1), PICK, cf.http("GET", FLOW1), PRESS, PRESS,
      cf.api("pressed"), PRESS, cf.ENTRIES,
      cf.ws({"type": "config_entries/flow/progress"}), cf.http("GET", FLOW1)]),
    ("manual",
     "Discovery finds nothing, so the host is asked for: one where nothing"
     " listens, one that isn't a Hue bridge, then the bridge.",
     cf.SUBSCRIBE,
     [cf.api("discovery_none"), START,
      cf.http("POST", FLOW1, {"host": OTHER}),
      START, cf.api("bridge_not_hue"),
      cf.http("POST", f"{cf.FLOW}/<flow:2>", {"host": HOST}),
      cf.api("bridge_ok"), START,
      cf.http("POST", f"{cf.FLOW}/<flow:3>", {"host": HOST}),
      cf.api("pressed"), cf.http("POST", f"{cf.FLOW}/<flow:3>", {}), cf.ENTRIES]),
    ("pick_manual",
     "Discovery lists the bridge and one that isn't there (left out); manual"
     " entry is picked instead, and what a form refuses.",
     cf.SUBSCRIBE,
     [cf.api("discovery_two"), START,
      cf.http("POST", FLOW1, {"id": "nobody"}),
      cf.http("POST", FLOW1, {}),
      cf.http("POST", FLOW1, {"id": 5}),
      cf.http("POST", FLOW1, {"id": BRIDGE, "host": HOST}),
      cf.http("POST", FLOW1, {"id": "manual"}),
      cf.http("POST", FLOW1, {}),
      cf.http("POST", FLOW1, {"host": 5}),
      cf.http("POST", FLOW1, {"host": HOST, "id": BRIDGE}),
      cf.http("POST", FLOW1, {"host": HOST}),
      cf.http("POST", FLOW1, {"anything": "goes"}),
      cf.http("DELETE", FLOW1)]),
    ("discovery_fails",
     "Discovery can't be reached, then answers too late (HA gives it 5 s):"
     " the host is asked for.",
     cf.SUBSCRIBE,
     [cf.api("discovery_down"), START, cf.http("DELETE", FLOW1),
      cf.api("discovery_slow"), START]),
    ("configured",
     "A bridge that is set up is not offered again, and its host is refused.",
     cf.SUBSCRIBE,
     [*ADD, cf.api("not_pressed"), START,
      cf.http("POST", f"{cf.FLOW}/<flow:2>", {"host": HOST})]),
    ("link_errors",
     "The bridge answers the key request with an error status, then stops"
     " answering at all.",
     cf.SUBSCRIBE,
     [START, PICK, cf.api("bridge_broken"), PRESS, cf.api("bridge_down"), PRESS,
      cf.api("bridge_ok"), cf.api("pressed"), PRESS]),
    ("two_flows",
     "Two flows for one bridge: both get to the link step, and the one that"
     " isn't finished goes when the other makes the entry.",
     cf.SUBSCRIBE,
     [START, START, PICK, cf.http("POST", f"{cf.FLOW}/<flow:2>", {"id": BRIDGE}),
      cf.ws({"type": "config_entries/flow/progress"}), cf.api("pressed"),
      cf.http("POST", f"{cf.FLOW}/<flow:2>", {}), cf.http("GET", FLOW1)]),
    ("not_ready",
     "The bridge gives a key and then can't be reached for its resources: the"
     " entry waits for a retry, and is removed.",
     cf.SUBSCRIBE,
     [START, PICK, cf.api("pressed"), cf.api("bridge_resources_down"), PRESS,
      cf.ENTRIES, cf.http("DELETE", f"{cf.ENTRY}/<entry:1>"), cf.ENTRIES]),
    ("remove",
     "A set-up bridge is removed: its entry, devices, entities and states go.",
     cf.SUBSCRIBE,
     [*ADD, cf.http("DELETE", f"{cf.ENTRY}/<entry:1>"), cf.ENTRIES,
      cf.api("not_pressed"), START]),
]


async def main():
    cf.check_versions()
    found = importlib.metadata.version("aiohue")
    if found != AIOHUE:
        sys.exit(f"aiohue is {found}, the vectors are pinned to {AIOHUE}")
    scenarios = []
    for name, doc, subscriptions, steps in SCENARIOS:
        result = await cf.run(subscriptions, json.loads(json.dumps(steps)), Hue())
        result.pop("snapshots")
        rows = {k: result.pop(k) for k in ("devices", "entities", "states", "areas")}
        for state in rows["states"].values():
            # Sets in HA: a group's scenes and members.
            for key in ("hue_scenes", "lights", "entity_id"):
                if isinstance(state["attributes"].get(key), list):
                    state["attributes"][key].sort()
        if name == "add":
            cf.write(HUE / "fresh_setup.json", {"entries": result["entries"], **rows})
            # The same on an install set to US customary units: what its
            # sensors' rows and states are then.
            us = await cf.run([], json.loads(json.dumps(steps)), Hue(),
                              units="us_customary")
            cf.write(HUE / "us_customary.json", {
                "doc": "The sensors after the `add` scenario on an install set to US"
                       " customary units: their rows and states.",
                "entities": [e for e in us["entities"]
                             if e["entity_id"].startswith("sensor.")],
                "states": {k: v for k, v in us["states"].items()
                           if k.startswith("sensor.")},
            })
            # The first scenario keeps its rows and states, for the replay.
            result.update(rows)
        scenarios.append({"name": name, "doc": doc, **result})
    cf.write(OUT, {"scenarios": scenarios})


if __name__ == "__main__":
    asyncio.run(main())
    # As `config_flow.py`: skip CPython's teardown.
    sys.stdout.flush()
    os._exit(0)
