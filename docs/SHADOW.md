# Shadow mode

Shadow mode runs `hars` beside the Home Assistant it will replace, with HA
still in charge of the house. For a week it does what it would do (the same
triggers, the same automations) while nothing it decides reaches a device,
and a diff tool compares its decisions with HA's. It is the
[roadmap](ROADMAP.md)'s gate before cutover (M7): **one week with no
differences.**

## What shadow mode does

`HARS_SHADOW=1` changes one thing: a service call is validated and its
`call_service` event fired as usual, so automations, the logbook and the
recorder see it, but a handler that acts on the outside world doesn't run.
No command goes to a Hue light, a LIFX bulb, a SwitchBot, a phone. Only
`automation`, `homeassistant` (which forwards to the target's domain, checked
in its turn), `system_log`, `persistent_notification`, `frontend`, `weather`
and `labs` run (`ha_core::shadow`); an unknown domain is held back.

The states `hars` shows still follow the real devices: Hue reports over its
event stream, LIFX and SwitchBot are polled, and they report what HA's
commands did. So `hars`'s automations see what HA's saw, and a
`wait_for_trigger` that waits for the light it "turned on" completes when HA's
command arrives.

Every held-back call and every `automation_triggered` goes to a JSON Lines
file (`HARS_SHADOW_LOG`, default `<config>/.hars/shadow.jsonl`), appended to,
with a `shadow_started` line each time `hars` starts. If the disk stops
accepting writes, `hars` holds the batch that failed and up to 4 MiB of
decisions behind it, retries every second, and when it recovers writes a
`shadow_gap` line saying how many it had to drop. Shutdown waits for the queue
to be written (the final-write stage), with one last try if the disk is
failing.

Entities whose state only changes when *commanded* are the exception: a
SwitchBot IR air conditioner (`assumed_state`) and its switch, and a SwitchBot
Bot's button (its state is when it was last pressed). Nothing reports their
change, so in shadow mode `hars` never updates them. An automation that reads one will
see a stale value; read the diff with that in mind.

## What it does not check

Shadow mode compares **decisions**. It does not show that the dashboards look
right, that the phone keeps its session and reports, that history graphs and
the Map trail render, or that device commands are accepted (the held-back
handlers are the code that talks to the device). Those are the other M5, M6
and M7 acceptance items. A passing week says the automations match; it does
not say `hars` can replace HA.

## Setting it up

`hars` must have **its own config directory**. Never point `HARS_CONFIG` at
HA's: both would write `.storage`.

1. Take a copy of HA's config directory (a backup, or a copy with HA
   stopped) and import it:

   ```sh
   IMAGE=ghcr.io/adamt94/homeassistant-rust
   docker run --rm -v hars-shadow:/config -v /path/to/ha-config-copy:/ha:ro $IMAGE import /ha --dry-run
   docker run --rm -v hars-shadow:/config -v /path/to/ha-config-copy:/ha:ro $IMAGE import /ha
   ```

   Read `import-report.json`: an automation reported `unavailable` will show
   up as a *trigger missing* in the diff.
2. Run it on another port, on the LAN so it can reach the devices:

   ```sh
   docker run -d --name hars-shadow --network host --restart unless-stopped \
     -e HARS_SHADOW=1 -e HARS_BIND=0.0.0.0:8124 -v hars-shadow:/config $IMAGE
   docker logs hars-shadow | grep "SHADOW MODE"
   ```

   Look at it at `http://host:8124`. Services called from its UI go through the
   same hold-back, so it cannot control a device.
3. Leave the phone on HA. Do not log the companion app in to `hars`: its
   webhooks would feed `hars` state that HA doesn't have.

Side effects on the house are reads only, but they are more reads: a SwitchBot
Cloud account is polled by both (about 290 more requests a day for a Hub and
a Bot, against its 10,000 daily limit), a Hue bridge gets a second
event-stream client, and LIFX bulbs get a second poller.

## Reading the result

From a clone of this repository (it needs only Python 3), with the log
copied out of the container
(`docker cp hars-shadow:/config/.hars/shadow.jsonl .`):

```sh
scripts/shadow-diff.py --ha-db /path/to/home-assistant_v2.db \
                       --hars-log /path/to/shadow.jsonl
```

(Copy the database first if HA is running; the tool opens it read-only.) It
compares, over the window from `hars`'s first start to now:

- **calls**: the same service, targets and data within 1 s. Different data on
  the same service and targets is a *mismatch*; HA's call with no match is
  *missing*; `hars`'s with no match is *extra*. Only calls made on behalf of an
  automation are compared: those in a context an `automation_triggered` of
  the same side started (`--all-contexts` adds the UI's and the apps'). Calls
  are matched in two passes (identical calls first, then the same service and
  targets with other data), each taking the earliest unmatched one within the
  tolerance; any call left over is reported, so a difference is never hidden,
  though a pair of near-simultaneous differing calls can be described as
  missing and extra rather than as mismatches.
- **triggers**: HA's `automation_triggered` against `hars`'s, by automation and
  time, with a per-automation count.

It exits 0 when nothing differs and the log has no gaps, 1 otherwise, and 2
when it can't read its input (a log line that isn't JSON, other than one a
crash cut off; a reversed window), so it never certifies input it didn't
read. When `hars` was down (it prints each start it saw), leave the gap out
with `--exclude START,END`; otherwise HA's calls in it count as missing. If you
change an automation in HA, import again (`--force`) and start the week over.

`python3 -m unittest discover -s scripts` runs its tests.
