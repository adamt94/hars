#!/usr/bin/env python3
"""Vectors for CORS on the auth endpoints and for `POST /api/states/<id>`.

    python3.14 -m venv .venv
    .venv/bin/pip install homeassistant==2026.6.1
    .venv/bin/python -I scripts/vectors/http.py

Boots Home Assistant 2026.6.1's core (aiohttp 3.13.5, aiohttp_cors 0.8.1)
in a temporary config directory with `http`, `auth` and `api`, an admin
and a non-admin user, then sends each request below over real HTTP. It
writes, to crates/ha-api/src/:

- `cors/vectors/cors.json`: preflights and requests, with and without an
  `Origin`, to `/auth/token` and `/auth/revoke` (the views HA marks
  `cors_allowed`), and methods those views lack: the status, the body, and
  every `Access-Control-*`, `Vary` and `Allow` header of the reply;
- `rest/vectors/post_state.json`: `POST /api/states/<entity_id>` with each
  kind of body and `Content-Type` (HA reads the body as JSON whatever the
  type), the status, the body, the `Location` header, and the state
  machine's entry after each step.

Times, context ids and user ids are replaced by `<time>`, `<ulid>` and
`<hex32>`, as the Rust tests do with hars's replies; each `post_state` step
also lists which of `light.a`'s times and context moved (`moved`). aiohttp_cors joins header *sets*, so the
order of `Access-Control-Allow-Headers` and of the "headers are not
allowed" list changes with Python's hash seed: both are sorted here, and
hars sends them sorted (one of the orders HA can send). Nothing reaches the network. Run it twice:
the output must not change.
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

PINNED = {
    "homeassistant": "2026.6.1",
    "aiohttp": "3.13.5",
    "aiohttp_cors": "0.8.1",
}
CRATE = pathlib.Path(__file__).resolve().parents[2] / "crates/ha-api/src"
CLIENT = "http://127.0.0.1:8123/"
ORIGIN = "https://example.org"


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


TIME = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?\+00:00$")
ULID = re.compile(r"^[0-9A-HJKMNP-TV-Z]{26}$")
HEX32 = re.compile(r"^[0-9a-f]{32}$")


def scrub(value):
    if isinstance(value, dict):
        return {k: scrub(v) for k, v in value.items()}
    if isinstance(value, list):
        return [scrub(v) for v in value]
    if isinstance(value, str):
        if TIME.match(value):
            return "<time>"
        if ULID.match(value):
            return "<ulid>"
        if HEX32.match(value):
            return "<hex32>"
    return value


class Run:
    async def start(self, config_dir):
        from homeassistant import bootstrap, core, loader
        from homeassistant.auth import auth_manager_from_config
        from homeassistant.config_entries import ConfigEntries
        from homeassistant.setup import async_setup_component

        hass = self.hass = core.HomeAssistant(config_dir)
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
        for domain in ("auth", "api"):
            assert await async_setup_component(hass, domain, {}), domain
        await hass.async_start()
        await hass.async_block_till_done()
        self.tokens = {}
        for name, group in (("admin", "system-admin"), ("user", "system-users")):
            user = await hass.auth.async_create_user(name, group_ids=[group])
            refresh = await hass.auth.async_create_refresh_token(user, CLIENT)
            self.tokens[name] = hass.auth.async_create_access_token(refresh)
        import aiohttp

        self.session = aiohttp.ClientSession()

    async def stop(self):
        await self.session.close()
        await self.hass.async_stop()

    async def send(self, method, path, headers=None, body=None, user=None):
        """`body`: a str sent as it is, or a dict sent as a form."""
        sent = dict(headers or {})
        if user is not None:
            sent["Authorization"] = f"Bearer {self.tokens[user]}"
        url = f"http://127.0.0.1:{self.port}{path}"
        async with self.session.request(
            method, url, data=body, headers=sent, skip_auto_headers=["Content-Type"]
        ) as r:
            text = await r.text()
            try:
                reply = scrub(json.loads(text))
            except ValueError:
                reply = text
            request = {"method": method, "path": path}
            if headers:
                request["headers"] = headers
            if body is not None:
                request["body"] = body
            if user is not None:
                request["user"] = user
            return request, r.status, reply, r.headers


def sorted_list(text, sep):
    return sep.join(sorted(text.split(sep)))


def cors_headers(headers):
    out = {
        k.lower(): v
        for k, v in sorted(headers.items(), key=lambda kv: kv[0].lower())
        if k.lower().startswith("access-control-") or k.lower() in ("vary", "allow")
    }
    if "access-control-allow-headers" in out:
        out["access-control-allow-headers"] = sorted_list(
            out["access-control-allow-headers"], ","
        )
    return out


NOT_ALLOWED = "CORS preflight request failed: headers are not allowed: "


def cors_step(note, request, status, reply, got):
    if isinstance(reply, str) and reply.startswith(NOT_ALLOWED):
        reply = NOT_ALLOWED + sorted_list(reply[len(NOT_ALLOWED) :], ", ")
    return {
        "note": note,
        "request": request,
        "status": status,
        "content_type": got.get("Content-Type"),
        "body": reply,
        "headers": cors_headers(got),
    }


async def cors(run):
    steps = []
    form = "application/x-www-form-urlencoded"
    preflights = [
        ({"Origin": ORIGIN, "Access-Control-Request-Method": "POST"}, "a preflight"),
        (
            {
                "Origin": ORIGIN,
                "Access-Control-Request-Method": "POST",
                "Access-Control-Request-Headers": "content-type, Authorization",
            },
            "allowed request headers, in any case",
        ),
        (
            {
                "Origin": ORIGIN,
                "Access-Control-Request-Method": "POST",
                "Access-Control-Request-Headers": "X-Foo",
            },
            "a request header not allowed",
        ),
        (
            {
                "Origin": ORIGIN,
                "Access-Control-Request-Method": "POST",
                "Access-Control-Request-Headers": " accept ,, x-b, X-A, origin, x-requested-with",
            },
            "spaces, empty items, several not allowed",
        ),
        (
            {"Origin": ORIGIN, "Access-Control-Request-Method": "DELETE"},
            "any method",
        ),
        ({"Origin": ORIGIN}, "no requested method"),
        ({"Access-Control-Request-Method": "POST"}, "no origin"),
        ({}, "neither"),
    ]
    for path in ("/auth/token", "/auth/revoke"):
        for headers, note in preflights:
            request, status, reply, got = await run.send("OPTIONS", path, headers)
            steps.append(cors_step(note, request, status, reply, got))
    requests = [
        (
            "/auth/token",
            {"Origin": ORIGIN, "Content-Type": form},
            "grant_type=refresh_token&client_id=x&refresh_token=nope",
            "a request from another origin",
        ),
        (
            "/auth/token",
            {"Content-Type": form},
            "grant_type=refresh_token&client_id=x&refresh_token=nope",
            "no origin: no CORS headers",
        ),
        (
            "/auth/revoke",
            {"Origin": ORIGIN, "Content-Type": form},
            "token=nope",
            "revoke from another origin",
        ),
    ]
    for path, headers, body, note in requests:
        request, status, reply, got = await run.send("POST", path, headers, body)
        steps.append(cors_step(note, request, status, reply, got))
    for method in ("GET", "PUT", "DELETE"):
        for headers, note in (
            ({"Origin": ORIGIN}, "a method the view lacks, from another origin"),
            (None, "a method the view lacks"),
        ):
            request, status, reply, got = await run.send(method, "/auth/token", headers)
            steps.append(cors_step(note, request, status, reply, got))
    return steps


async def post_state(run):
    steps = []
    cases = [
        ("light.a", None, '{"state": "on"}', "admin", "no Content-Type, new: 201"),
        (
            "light.a",
            "text/plain",
            '{"state": "off", "attributes": {"x": 1}}',
            "admin",
            "text/plain, existing: 200",
        ),
        (
            "light.a",
            "application/json",
            '{"state": "off", "attributes": {"x": 1}}',
            "admin",
            "same state and attributes",
        ),
        (
            "light.a",
            "application/json",
            '{"state": "off", "attributes": {"x": 1}, "force_update": true}',
            "admin",
            "force_update",
        ),
        ("light.a", "application/json", "", "admin", "empty body"),
        ("light.a", "application/json", "{nope", "admin", "invalid JSON"),
        ("light.a", "application/json", "[1]", "admin", "not an object"),
        ("light.a", "application/json", "null", "admin", "null"),
        ("light.a", "application/json", '{"attributes": {}}', "admin", "no state"),
        ("light.a", "application/json", '{"state": null}', "admin", "null state"),
        ("light.a", "application/json", '{"state": ""}', "admin", "empty state"),
        ("light.a", "application/json", '{"state": 5}', "admin", "a number state"),
        ("light.a", "application/json", '{"state": true}', "admin", "a bool state"),
        ("light.a", "application/json", '{"state": 1.5}', "admin", "a float state"),
        ("light.a", "application/json", '{"state": 1e-7}', "admin", "a small float"),
        (
            "light.a",
            "application/json",
            '{"state": [1, "it\'s", null, false, {"k": 2.0}]}',
            "admin",
            "a list state",
        ),
        (
            "light.a",
            "application/json",
            '{"state": "x", "force_update": 0}',
            "admin",
            "a falsy force_update",
        ),
        (
            "light.a",
            "application/json",
            '{"state": "x", "force_update": "yes"}',
            "admin",
            "a truthy force_update",
        ),
        (
            "light.a",
            "application/json",
            '{"state": "x", "attributes": []}',
            "admin",
            "empty list attributes",
        ),
        (
            "light.a",
            "application/json",
            json.dumps({"state": "é" * 255}),
            "admin",
            "255 characters, more bytes",
        ),
        (
            "light.a",
            "application/json",
            '{"state": "on", "attributes": [1]}',
            "admin",
            "attributes not an object",
        ),
        (
            "light.a",
            "application/json",
            '{"state": "on", "attributes": null}',
            "admin",
            "null attributes",
        ),
        (
            "light.a",
            "application/json",
            json.dumps({"state": "x" * 256}),
            "admin",
            "a state too long",
        ),
        ("light.Bad", "application/json", '{"state": "on"}', "admin", "upper case"),
        ("light.a-b", "application/json", '{"state": "on"}', "admin", "bad entity id"),
        ("light.b", "application/json", '{"state": "on"}', "user", "not an admin"),
    ]
    before = None
    for entity_id, content_type, body, user, note in cases:
        headers = {"Content-Type": content_type} if content_type else None
        request, status, reply, got = await run.send(
            "POST", f"/api/states/{entity_id}", headers, body, user
        )
        state = run.hass.states.get("light.a")
        now = state.as_dict() if state else None
        moved = [
            k
            for k in ("last_changed", "last_updated", "last_reported", "context")
            if before is not None and now is not None and before[k] != now[k]
        ]
        before = now
        steps.append(
            {
                "note": note,
                "request": request,
                "status": status,
                "content_type": got.get("Content-Type"),
                "body": reply,
                "location": got.get("Location"),
                "light.a": scrub(now),
                "moved": moved,
            }
        )
    return steps


def write(rel, data):
    path = CRATE / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=1, ensure_ascii=False) + "\n")


async def main():
    check_versions()
    tmp = pathlib.Path(tempfile.mkdtemp())
    run = Run()
    try:
        await run.start(str(tmp))
        write("cors/vectors/cors.json", await cors(run))
        write("rest/vectors/post_state.json", await post_state(run))
        await run.stop()
    finally:
        shutil.rmtree(tmp)


if __name__ == "__main__":
    asyncio.run(main())
    # CPython 3.14.7 can crash collecting HA's modules at shutdown, after
    # everything is written: skip the teardown.
    sys.stdout.flush()
    os._exit(0)
