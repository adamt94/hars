#!/usr/bin/env python3
"""Vectors for `automation.reload`.

    .venv-template/bin/python -I scripts/vectors/automation_reload.py
    cargo test -p ha-automation reload_tests

Runs Home Assistant 2026.6.1's `automation` (its `reload` service, the
entity component's `async_prepare_reload`, the entity registry and the
restore state behind `RestoreEntity`) from a `configuration.yaml` that
includes `automations.yaml`, in Europe/London, and writes
crates/ha-automation/src/vectors/reload.json: for each scenario, the
automations at start, the steps (the file rewritten, `automation.reload`
and the other `automation.*` services called, states written, time
passing) and what HA did, in order (`step` marks where step `n`
begins): every `state_changed` of an automation, the `entity_registry_updated` and `automation_reloaded`
events, the `automation_triggered` events, the service calls the actions
made, what a service call raised, and the WARNING and ERROR log lines.
After each scenario: the automations' entity registry rows.

What is faked, as only things outside HA may be:

- the clock: the event loop runs on virtual time, which moves only when a
  step says so, and `time.time`, `homeassistant.util.dt.utcnow` and `now`
  follow it;
- randomness: `random` is seeded;
- the services the actions call (`light.turn_on`, `light.turn_off`) only
  record the data they get.

Run it twice: the output must not change.
"""

import asyncio
import datetime as dt
import importlib.metadata
import json
import logging
import os
import pathlib
import random
import sys
import tempfile
import time
import zoneinfo

PINNED = {"homeassistant": "2026.6.1", "jinja2": "3.1.6", "voluptuous": "0.15.2"}
OUT = pathlib.Path(__file__).resolve().parents[2] / "crates/ha-automation/src/vectors"
TZ = "Europe/London"
START = dt.datetime(2026, 11, 10, 16, 0, 0, tzinfo=zoneinfo.ZoneInfo(TZ))
LOGGERS = (
    "homeassistant.components.automation",
    "homeassistant.components.homeassistant.triggers",
    "homeassistant.config",
    "homeassistant.helpers.condition",
    "homeassistant.helpers.entity_platform",
    "homeassistant.helpers.script",
    "homeassistant.helpers.service",
    "homeassistant.helpers.trigger",
)
MOTION = "binary_sensor.hall_motion"
DOOR = "binary_sensor.front_door"


def set_(entity_id, state, attributes=None):
    return {"op": "set", "entity_id": entity_id, "state": state, "attributes": attributes or {}}


def wait(s):
    return {"op": "wait", "s": s}


def write(automations):
    """Rewrite `automations.yaml`."""
    return {"op": "write", "automations": automations}


def write_text(text):
    """Rewrite `automations.yaml` with this text."""
    return {"op": "write", "text": text}


def service(name, **data):
    return {"op": "service", "domain": "automation", "service": name, "data": data}


def reload(**data):
    return service("reload", **data)


def auto(id_, alias, entity=MOTION, light="light.hall", **more):
    """An automation: `entity` turning on turns `light` on."""
    a = {}
    if id_ is not None:
        a["id"] = id_
    if alias is not None:
        a["alias"] = alias
    a["triggers"] = [{"trigger": "state", "entity_id": entity, "to": "on"}]
    a["actions"] = [{"action": "light.turn_on", "target": {"entity_id": light}}]
    a.update(more)
    return a


def pulse(entity=MOTION):
    """`entity` on, then off again."""
    return [set_(entity, "on"), wait(1), set_(entity, "off"), wait(1)]


BASE_STATES = [
    set_(MOTION, "off", {"friendly_name": "Hall Motion"}),
    set_(DOOR, "off", {"friendly_name": "Front Door"}),
    set_("light.hall", "off", {"friendly_name": "Hall"}),
    set_("light.porch", "off", {"friendly_name": "Porch"}),
]

HALL = auto("hall", "Hall motion")
PORCH = auto("porch", "Porch door", entity=DOOR, light="light.porch")


def waiting(id_, alias, entity, light, **more):
    """`entity` on turns `light` on; off for five minutes, off again."""
    return auto(
        id_, alias, entity=entity, light=light, mode="restart",
        actions=[
            {"action": "light.turn_on", "target": {"entity_id": light}},
            {"wait_for_trigger": [{"trigger": "state", "entity_id": entity, "to": "off",
                                   "for": "00:05:00"}]},
            {"action": "light.turn_off", "target": {"entity_id": light}},
        ],
        **more,
    )


WAITING = waiting("hall", "Hall motion", MOTION, "light.hall")
WAITING_PORCH = waiting("porch", "Porch door", DOOR, "light.porch")

# (name, doc, automations at start, steps)
SCENARIOS = [
    (
        "unchanged",
        "A reload of an unchanged file writes no state: only `automation_reloaded`. "
        "The triggers stay attached.",
        [HALL, PORCH],
        [*pulse(), reload(), wait(1), *pulse(), *pulse(DOOR)],
    ),
    (
        "action_changed",
        "One automation's action changes: it is removed and made again (its "
        "`last_triggered` restored), the other is left alone. The new action runs, "
        "the old trigger is gone.",
        [HALL, PORCH],
        [*pulse(), write([auto("hall", "Hall motion", light="light.porch"), PORCH]),
         wait(10), reload(), wait(1), *pulse(), *pulse(DOOR)],
    ),
    (
        "trigger_changed",
        "The trigger moves to another entity: the old entity no longer fires it.",
        [HALL],
        [write([auto("hall", "Hall motion", entity=DOOR)]), reload(), wait(1), *pulse(),
         *pulse(DOOR)],
    ),
    (
        "renamed",
        "A new alias: the entity id stays, the registry's original name follows.",
        [HALL, PORCH],
        [*pulse(), write([auto("hall", "Hallway motion at night"), PORCH]), reload(), wait(1),
         *pulse()],
    ),
    (
        "added",
        "A new automation with an id gets a registry row and an entity id from its alias.",
        [HALL],
        [write([HALL, auto("1794680000000", "Porch: on at 50%", entity=DOOR,
                           light="light.porch")]),
         reload(), wait(1), *pulse(DOOR), *pulse()],
    ),
    (
        "removed",
        "An automation taken out of the file: its entity goes, its triggers with it.",
        [HALL, PORCH],
        [*pulse(DOOR), write([HALL]), reload(), wait(1), *pulse(DOOR), *pulse()],
    ),
    (
        "removed_then_back",
        "Taken out and put back by two reloads: it keeps its entity id and comes "
        "back with what it had.",
        [HALL, PORCH],
        [*pulse(DOOR), service("turn_off", entity_id="automation.porch_door"), wait(1),
         write([HALL]), reload(), wait(60), write([HALL, PORCH]), reload(), wait(1),
         *pulse(DOOR)],
    ),
    (
        "off_then_edited",
        "An automation turned off, then changed: it comes back off, with its "
        "`last_triggered`.",
        [HALL],
        [*pulse(), service("turn_off", entity_id="automation.hall_motion"), wait(1),
         write([auto("hall", "Hall motion", light="light.porch")]), reload(), wait(1),
         *pulse(), service("turn_on", entity_id="automation.hall_motion"), wait(1), *pulse()],
    ),
    (
        "initial_state",
        "`initial_state` in the new config wins over what the old entity had.",
        [HALL, PORCH],
        [service("turn_off", entity_id="automation.hall_motion"), wait(1),
         write([auto("hall", "Hall motion", initial_state=True),
                auto("porch", "Porch door", entity=DOOR, light="light.porch",
                     initial_state=False)]),
         reload(), wait(1), *pulse(), *pulse(DOOR)],
    ),
    (
        "running",
        "A run waiting on a trigger when its automation changes is stopped; the "
        "unchanged automation's run goes on.",
        [WAITING, WAITING_PORCH],
        [set_(MOTION, "on"), wait(1), set_(DOOR, "on"), wait(1),
         write([dict(WAITING, description="changed"), WAITING_PORCH]),
         reload(), wait(1), set_(MOTION, "off"), set_(DOOR, "off"), wait(400)],
    ),
    (
        "running_unchanged_reload",
        "A reload that changes nothing leaves a waiting run alone.",
        [WAITING],
        [set_(MOTION, "on"), wait(1), reload(), wait(1), set_(MOTION, "off"), wait(400)],
    ),
    (
        "single_id",
        "`id` reloads that automation only: the other's change in the file waits "
        "for its own reload.",
        [HALL, PORCH],
        [write([auto("hall", "Hall motion", light="light.porch"),
                auto("porch", "Porch door", entity=DOOR, light="light.hall")]),
         reload(id="hall"), wait(1), *pulse(), *pulse(DOOR), reload(id="porch"), wait(1),
         *pulse(DOOR)],
    ),
    (
        "single_id_unchanged_and_unknown",
        "`id` of an unchanged automation, and of none: nothing but the event.",
        [HALL],
        [reload(id="hall"), wait(1), reload(id="nope"), wait(1), *pulse()],
    ),
    (
        "single_id_added",
        "`id` of an automation new in the file adds it.",
        [HALL],
        [write([HALL, PORCH]), reload(id="porch"), wait(1), *pulse(DOOR)],
    ),
    (
        "single_id_removed",
        "`id` of an automation no longer in the file removes it.",
        [HALL, PORCH],
        [write([HALL]), reload(id="porch"), wait(1), *pulse(DOOR)],
    ),
    (
        "no_id",
        "Automations without an id match by name and config wherever they are in "
        "the list: reordered ones are kept, a changed one is made again.",
        [auto(None, "First"), auto(None, "Second", entity=DOOR, light="light.porch")],
        [write([auto(None, "Second", entity=DOOR, light="light.porch"), auto(None, "First")]),
         reload(), wait(1),
         write([auto(None, "Second", entity=DOOR, light="light.hall"), auto(None, "First")]),
         reload(), wait(1), *pulse(DOOR), *pulse()],
    ),
    (
        "no_alias",
        "Without an alias the name is the place in the list, so moving it is a change.",
        [auto(None, None), auto("porch", "Porch door", entity=DOOR, light="light.porch")],
        [write([auto("porch", "Porch door", entity=DOOR, light="light.porch"),
                auto(None, None)]),
         reload(), wait(1), *pulse()],
    ),
    (
        "becomes_invalid",
        "A change that doesn't validate: the automation is `unavailable`; fixed, it "
        "comes back as it was before.",
        [HALL, PORCH],
        [*pulse(), service("turn_off", entity_id="automation.hall_motion"), wait(1),
         write([{"id": "hall", "alias": "Hall motion",
                 "triggers": [{"trigger": "state", "to": "on"}],
                 "actions": HALL["actions"]}, PORCH]),
         reload(), wait(1), *pulse(),
         write([HALL, PORCH]), reload(), wait(1), *pulse()],
    ),
    (
        "invalid_unchanged",
        "An invalid automation that is unchanged is left alone too.",
        [{"id": "bad", "alias": "Bad", "triggers": [{"trigger": "state", "to": "on"}],
          "actions": HALL["actions"]}, PORCH],
        [reload(), wait(1), *pulse(DOOR)],
    ),
    (
        "duplicate_id",
        "A reload that brings a second automation with an id already in use.",
        [HALL],
        [write([HALL, auto("hall", "Hall again", entity=DOOR, light="light.porch")]),
         reload(), wait(1), *pulse(DOOR), *pulse()],
    ),
    (
        "same_alias",
        "A new automation whose alias is taken gets the next free entity id.",
        [HALL],
        [write([HALL, auto("hall2", "Hall motion", entity=DOOR, light="light.porch")]),
         reload(), wait(1), *pulse(DOOR)],
    ),
    (
        "emptied",
        "An empty file removes every automation; filled again, they return.",
        [HALL, PORCH],
        [write_text(""), reload(), wait(1), *pulse(), write([HALL, PORCH]), reload(), wait(1),
         *pulse()],
    ),
    (
        "broken_yaml",
        "A file that doesn't parse: the reload fails and nothing changes; mended, "
        "the next reload reads it.",
        [HALL],
        [write_text("- id: hall\n  alias: [unclosed\n"), reload(), wait(1), *pulse(),
         write([HALL, PORCH]), reload(), wait(1), *pulse(DOOR)],
    ),
    (
        "bad_data",
        "`reload`'s schema: `id` must be a string, and nothing else is allowed.",
        [HALL],
        [reload(id=5), reload(entity_id="automation.hall_motion"), reload(id=""), wait(1)],
    ),
    (
        "numeric_id",
        "An `id` that isn't a string (YAML reads `id: 5` as a number), or an alias or "
        "description that isn't text (`description:` with nothing after it): HA drops "
        "the automation altogether, and it has no place in the list.",
        [auto(5, "Five"), dict(auto("d", "No description"), description=None),
         dict(auto("a", None), alias=["a", "list"]), dict(auto(None, "Null id"), id=None),
         auto(None, None, entity=DOOR, light="light.porch"), HALL],
        [*pulse(), *pulse(DOOR),
         write([auto(5, "Five"), auto("5", "Five again"),
                auto(None, None, entity=DOOR, light="light.porch"), HALL]),
         reload(), wait(1)],
    ),
    (
        "twice",
        "Two reloads called together, the file changed once.",
        [HALL],
        [write([auto("hall", "Hall motion", light="light.porch")]),
         {"op": "services", "calls": [reload(), reload()]}, wait(1), *pulse()],
    ),
]


def check_versions():
    for pkg, want in PINNED.items():
        have = importlib.metadata.version(pkg)
        if have != want:
            sys.exit(f"{pkg} is {have}; these vectors are made with {want}")


class VirtualLoop(asyncio.SelectorEventLoop):
    """An event loop whose clock moves only when told to."""

    now = 0.0

    def time(self):
        return self.now


LOOP = VirtualLoop()


def wall():
    return START.timestamp() + LOOP.now


def iso(seconds):
    return dt.datetime.fromtimestamp(START.timestamp() + seconds, dt.UTC).isoformat()


async def settle():
    """Let everything runnable run. File reads go to the executor, so the
    loop also has to get its results back."""
    for _ in range(40):
        for _ in range(50):
            await asyncio.sleep(0)
        await LOOP.run_in_executor(None, lambda: None)


async def advance(seconds):
    """Move the clock on, running every timer due on the way in order."""
    target = LOOP.now + seconds
    while True:
        await settle()
        due = [h.when() for h in LOOP._scheduled if not h.cancelled()]
        nxt = min(due, default=None)
        if nxt is None or nxt > target:
            break
        LOOP.now = max(LOOP.now, nxt)
    LOOP.now = target
    await settle()


class Logs(logging.Handler):
    def __init__(self):
        super().__init__(logging.DEBUG)
        self.records = []

    def emit(self, record):
        if record.name.startswith(LOGGERS) and record.levelno >= logging.WARNING:
            self.records.append((record.levelname, record.name, record.getMessage()))


def patch_clock():
    """`time.time`, `dt_util.utcnow` and `dt_util.now` follow the loop, in
    every module that has imported them."""
    from homeassistant.util import dt as dt_util

    orig = {"utcnow": dt_util.utcnow, "now": dt_util.now}

    def utcnow():
        return dt.datetime.fromtimestamp(wall(), dt.UTC)

    def now(time_zone=None):
        return dt.datetime.fromtimestamp(wall(), time_zone or dt_util.get_default_time_zone())

    new = {id(orig["utcnow"]): utcnow, id(orig["now"]): now}
    time.time = wall

    def rebind():
        for module in list(sys.modules.values()):
            names = getattr(module, "__dict__", None)
            if not names or not module.__name__.startswith("homeassistant"):
                continue
            for key, value in list(names.items()):
                if id(value) in new and value in orig.values():
                    setattr(module, key, new[id(value)])

    return rebind


def plain(value):
    """As JSON: tuples and sets as lists, datetimes as ISO text."""
    if isinstance(value, dict):
        return {str(k): plain(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [plain(v) for v in value]
    if isinstance(value, set):
        return sorted(plain(v) for v in value)
    if isinstance(value, dt.datetime):
        return value.isoformat()
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    return f"<{type(value).__name__}> {value}"


def state_json(state):
    if state is None:
        return None
    return {"state": state.state, "attributes": plain(dict(state.attributes))}


async def run(automations, steps):
    from homeassistant import bootstrap, config as conf_util, core, loader
    from homeassistant.config_entries import ConfigEntries
    from homeassistant.helpers import entity_registry as er
    from homeassistant.setup import async_setup_component

    random.seed(1)
    LOOP.now = 0.0
    out = []
    logs = Logs()
    root = logging.getLogger()
    root.addHandler(logs)
    with tempfile.TemporaryDirectory() as config_dir:
        path = pathlib.Path(config_dir)
        (path / "configuration.yaml").write_text("automation: !include automations.yaml\n")
        (path / "automations.yaml").write_text(json.dumps(automations))
        hass = core.HomeAssistant(config_dir)
        hass.config.skip_pip = True
        await hass.config.async_load()
        await hass.config.async_set_time_zone(TZ)
        loader.async_setup(hass)
        hass.config_entries = ConfigEntries(hass, {})
        await hass.config_entries.async_initialize()
        await bootstrap.async_load_base_functionality(hass)
        # The context a step's service call is made with: an event or a
        # state written with it says so.
        call_context = {}

        def record(kind, **data):
            out.append({"t": iso(LOOP.now), "kind": kind, **data})

        def flush_logs():
            for level, logger, msg in logs.records:
                record("log", level=level, logger=logger,
                       msg=msg.replace(config_dir, "<config>"))
            logs.records.clear()

        def by_call(context):
            return context.id == call_context.get("id")

        async def light(call):
            record("call", domain=call.domain, service=call.service, data=plain(dict(call.data)))

        for name in ("turn_on", "turn_off"):
            hass.services.async_register("light", name, light)

        @core.callback
        def state_changed(event):
            entity_id = event.data["entity_id"]
            if not entity_id.startswith("automation."):
                return
            flush_logs()
            new = event.data["new_state"]
            record("state", entity_id=entity_id, old=state_json(event.data["old_state"]),
                   new=state_json(new), by_call=by_call(event.context))

        @core.callback
        def registry_updated(event):
            flush_logs()
            record("registry", data=plain(dict(event.data)))

        @core.callback
        def reloaded(event):
            flush_logs()
            record("reloaded", data=plain(dict(event.data)), by_call=by_call(event.context))

        @core.callback
        def triggered(event):
            flush_logs()
            record("triggered", data=plain(dict(event.data)))

        hass.bus.async_listen("state_changed", state_changed)
        hass.bus.async_listen("entity_registry_updated", registry_updated)
        hass.bus.async_listen("automation_reloaded", reloaded)
        hass.bus.async_listen("automation_triggered", triggered)
        for s in BASE_STATES:
            hass.states.async_set(s["entity_id"], s["state"], s["attributes"])
        conf = await conf_util.async_hass_config_yaml(hass)
        assert await async_setup_component(hass, "homeassistant", conf)
        logs.records.clear()
        await async_setup_component(hass, "automation", conf)
        await settle()
        await hass.async_start()
        await settle()
        flush_logs()
        record("started")

        async def call(step):
            context = core.Context()
            call_context["id"] = context.id
            try:
                await hass.services.async_call(step["domain"], step["service"], step["data"],
                                               blocking=True, context=context)
            except Exception as err:  # noqa: BLE001
                flush_logs()
                record("error", type=type(err).__name__, msg=str(err).replace(config_dir,
                                                                             "<config>"))

        for n, step in enumerate(steps):
            record("step", n=n)
            if step["op"] == "set":
                hass.states.async_set(step["entity_id"], step["state"], step["attributes"])
                await settle()
            elif step["op"] == "wait":
                await advance(step["s"])
            elif step["op"] == "write":
                text = step.get("text")
                if text is None:
                    text = json.dumps(step["automations"])
                (path / "automations.yaml").write_text(text)
            elif step["op"] == "service":
                hass.async_create_task(call(step))
                await settle()
            elif step["op"] == "services":
                for c in step["calls"]:
                    hass.async_create_task(call(c))
                await settle()
            flush_logs()
        rows = [
            {"entity_id": e.entity_id, "unique_id": e.unique_id, "name": e.name,
             "original_name": e.original_name, "object_id_base": e.object_id_base,
             "capabilities": plain(e.capabilities), "options": plain(dict(e.options)),
             "disabled_by": e.disabled_by, "has_entity_name": e.has_entity_name,
             "supported_features": e.supported_features}
            for e in er.async_get(hass).entities.values()
            if e.platform == "automation"
        ]
        states = {s.entity_id: state_json(s) for s in hass.states.async_all("automation")}
        await hass.async_stop(force=True)
    root.removeHandler(logs)
    return out, rows, states


async def scenarios(rebind):
    result = []
    for name, doc, automations, steps in SCENARIOS:
        rebind()
        automations = json.loads(json.dumps(automations))
        steps = json.loads(json.dumps(steps))
        expected, rows, states = await run(automations, steps)
        result.append({"name": name, "doc": doc, "states": BASE_STATES,
                       "automations": automations, "steps": steps, "expected": expected,
                       "registry": rows, "final": states})
    return result


def main():
    check_versions()
    os.environ["TZ"] = TZ
    time.tzset()
    logging.basicConfig(handlers=[logging.NullHandler()])
    logging.getLogger().setLevel(logging.DEBUG)
    rebind = patch_clock()
    asyncio.set_event_loop(LOOP)
    # Import what the scenarios use, so their clock functions are rebound.
    import homeassistant.components.automation  # noqa: F401
    import homeassistant.components.homeassistant.triggers.state  # noqa: F401
    import homeassistant.helpers.entity_registry  # noqa: F401
    import homeassistant.helpers.restore_state  # noqa: F401

    rebind()
    data = {"start": START.isoformat(), "time_zone": TZ,
            "scenarios": LOOP.run_until_complete(scenarios(rebind))}
    (OUT / "reload.json").write_text(json.dumps(data, indent=1, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()
