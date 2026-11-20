# Supported and planned integrations

This page lists the integrations that hars runs today, the limits of each
one, and the integrations that are planned. The [roadmap](ROADMAP.md) is the
record of what is done. This page is the summary for users.

hars does not run Python. Home Assistant's own integrations, custom
components, and HACS integrations don't load in hars. Each integration is
ported to Rust and compiled into hars
([ADR 0005](adr/0005-integrations-compiled-in.md)). A port must give the same
states, attributes, and device requests as Home Assistant 2026.6.1, and its
memory and wakeup cost is measured before it merges.

## Supported today

| Integration | Domain | Connection | Add it by |
|---|---|---|---|
| [Philips Hue](#philips-hue) | `hue` | Local, pushed by the bridge | Import |
| [LIFX](#lifx) | `lifx` | Local, polled every 10 s | Import |
| [SwitchBot Cloud](#switchbot-cloud) | `switchbot_cloud` | Cloud, polled every 600 s | The UI, or import |
| [Weather](#weather) | `met` | Cloud, fetched about once an hour | Onboarding |
| [Sun](#sun) | `sun` | None, calculated | On by default |
| [Companion app](#companion-app) | `mobile_app` | The app calls hars | Log in from the app |
| [People and zones](#people-and-zones) | `person`, `zone` | None | Always on |

The domains that hars starts from config entries are the `SUPPORTED` list in
[`crates/ha-integrations/src/lib.rs`](../crates/ha-integrations/src/lib.rs).
`mobile_app`, `person`, and `zone` are built in.

### How you add an integration

An integration reaches hars in one of four ways:

- **The UI.** Open **Settings > Devices & services**, choose **Add
  integration**, and pick the integration. Today the list holds SwitchBot
  Cloud only. hars checks what you type before it saves anything, starts the
  integration at once, and shows the devices that it found. Choose **Skip
  and finish** in the last step: hars can't yet save a name or an area from
  that step. To undo it, open the integration and delete its entry.

  Adding and deleting are off when hars can't save the change. That
  happens when the config folder came from a Home Assistant release older
  than 2026.6, so the file that lists the integrations, the devices or the
  entities is in a format that hars reads but doesn't write. hars then says
  so when you pick the integration, and in its log at start. To fix it,
  start Home Assistant 2026.6 or 2026.7 once on that folder, so that it
  updates its files, and
  [import the folder again](INSTALL.md#which-home-assistant-releases).
- **Import.** Set the integration up in Home Assistant 2026.6 to 2026.11
  ([which releases](INSTALL.md#which-home-assistant-releases)), then
  [move the install to hars](../README.md#move-from-home-assistant). The
  import brings the integration's settings and credentials, its devices, and
  its entity ids. `hars/config/import-report.json` says whether hars has
  each integration.
- **Onboarding.** The setup screens add the weather forecast for the
  location that you choose.
- **The app.** The companion app registers itself when you log in.

### Philips Hue

hars talks to the Hue bridge on your network over the bridge's v2 API. The
bridge pushes each change to hars, so hars does not poll.

- **Lights:** each light, and each room and zone as a grouped light.
  Brightness, colour, colour temperature, and transitions work as in Home
  Assistant.
- **Scenes:** scenes and smart scenes, with `scene.turn_on` and
  `hue.activate_scene`. `hue.hue_activate_scene` works for scenes, but not
  for smart scenes.
- **Sensors:** temperature, light level, battery, and Zigbee connectivity.
- **Binary sensors:** motion, grouped motion, camera motion, contact,
  tamper, and entertainment.
- **Switches:** the enable switch of each motion and light sensor, and the
  bridge's automations.

Not yet:

- Button, dial, and doorbell events (`hue_event`), and the motion-aware
  sensor.
- A new search for the bridge when its IP address changes. Give the bridge a
  fixed address in your router's settings.
- Pairing again when the bridge rejects the imported key.

### LIFX

hars talks to each bulb on your network over UDP and asks for its state
every 10 seconds, as Home Assistant does. hars supports single-zone colour
bulbs. Light strips, tiles, and other multi-zone products are not supported.

- **Lights:** power, brightness, colour, colour temperature, and
  transitions.
- **Buttons:** identify and restart.

Not yet:

- The `lifx.*` services: effects, `lifx.set_state`, and themes.
- The signal-strength sensor.
- Saving a bulb's new IP address. hars finds a bulb that moved, but it
  searches again after a restart. The installer's set-up for Docker Desktop
  on a Mac or on Windows publishes one port, so hars can't search there.
  Give each bulb a fixed address.

hars has been checked against one colour bulb.

### SwitchBot Cloud

hars uses SwitchBot's cloud API. Add it in **Settings > Devices & services >
Add integration** with the token and the secret key that the SwitchBot app
shows under its developer options, or import an install that has it. hars
asks for the state of the Bot and of the Hub 2 every 600 seconds, which is
Home Assistant's interval.

- **Bot:** a button that presses the Bot, and a battery sensor.
- **Hub 2:** temperature and humidity.
- **Infrared air conditioner:** a `climate` entity and a power switch. An
  infrared device can't report its state, so hars shows the last command.

Not yet:

- A Bot in switch mode.
- Every other SwitchBot device type.
- SwitchBot webhooks. They need a public URL, and hars has no remote access
  yet. Home Assistant stops its polls of a Bot once a webhook is set. hars
  keeps the polls.

### Weather

The `met` integration fetches the forecast for your home from
[api.met.no](https://api.met.no/) about once an hour. It provides one
weather entity (`weather.forecast_home` on a new install), the hourly and
daily forecasts that the weather card
shows, and the `weather.get_forecasts` service.

Not yet: a change of the forecast's settings from the UI.

### Sun

`sun.sun` and the six `sensor.sun_next_*` sensors are calculated from your
home's location. The times match Home Assistant's to within a millisecond.
A new install has the sun integration. An import keeps it off if you had
turned it off in Home Assistant.

### Companion app

The Home Assistant iOS app works with hars. The app sends your phone's data
to hars, encrypted when the app's registration supports encryption.

- **Location:** a `device_tracker`, which moves the matching `person`
  between zones.
- **Sensors:** the sensors and binary sensors that the app reports, such as
  battery level.
- **Notifications:** `notify.mobile_app_<device>`, `notify.notify`, and
  `notify.send_message`. hars sends a notification over the app's local
  push connection while the app holds one open, and through the app's push
  service otherwise.

Not yet: `notify.persistent_notification`, Assist timer notifications, and
templates that the app asks hars to render.
hars is tested with version 2026.9.0 of the iOS app. The Android app is
untested.

### People and zones

`person` entities and `zone.home` are built in. hars reads them from Home
Assistant's own storage files, so an import keeps them.

## Entity types

An integration creates entities of the types below. An integration that needs
another type, such as `cover`, `fan`, `lock`, or `media_player`, needs that
type added first. [Add an entity component](guides/add-an-entity-component.md)
shows how.

| Entity type | Services |
|---|---|
| `light` | `light.turn_on`, `light.turn_off`, `light.toggle` |
| `switch` | `switch.turn_on`, `switch.turn_off`, `switch.toggle` |
| `scene` | `scene.turn_on` |
| `button` | `button.press` |
| `climate` | Home Assistant's ten `climate` services |
| `weather` | `weather.get_forecasts` |
| `notify` | `notify.send_message`, for the companion app |
| `sensor`, `binary_sensor`, `device_tracker` | None. These types are states only |

## Planned

The roadmap sets no dates and no order for the items below. To show that you
need one, [open an integration request](https://github.com/adamt94/homeassistant-rust/issues/new?template=integration_request.yml),
or add a 👍 to a request that exists.

Work left on the integrations above:

- Hue button, dial, and doorbell events.
- The `lifx.*` services and the signal-strength sensor.
- More SwitchBot device types.

New integrations and features:

| Planned | What it brings |
|---|---|
| Adding Hue and LIFX from the UI | Set up a bridge or a bulb in **Settings** with no import, as SwitchBot Cloud is today |
| MQTT (`mqtt`) | Devices that publish to an MQTT broker |
| zigbee2mqtt | Zigbee devices, through MQTT |
| ESPHome (`esphome`) | ESPHome devices, over their native API |
| Helpers (`input_*`, `template`) | Toggles, numbers, and template entities |
| To-do lists (`todo`) | Shopping and to-do lists |
| Text-to-speech | Spoken announcements |
| SwitchBot webhooks, remote access | Features that need a public URL |

A large integration that not everyone uses, such as an MQTT or a Zigbee
stack, goes behind a Cargo feature. People who don't use the integration
don't carry its code in memory
([performance rule 10](PERF.md#rules)).

## Ask for an integration

[Open an integration request](https://github.com/adamt94/homeassistant-rust/issues/new?template=integration_request.yml).
Say which devices you have and which entities and services you use. That
detail decides how much of the integration must be ported first.

## Add an integration yourself

[Add an integration](guides/add-an-integration.md) walks through a complete
port, with `met` as the example. Agree the integration with a maintainer in
an issue before you start, because each one adds to the binary and to the
memory that every user pays for.
