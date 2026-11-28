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
| [Philips Hue](#philips-hue) | `hue` | Local, pushed by the bridge | The UI, or import |
| [LIFX](#lifx) | `lifx` | Local, polled every 10 s | The UI, or import |
| [SwitchBot Cloud](#switchbot-cloud) | `switchbot_cloud` | Cloud, polled every 600 s | The UI, or import |
| [MQTT](#mqtt) | `mqtt` | Local, pushed by your broker | Import |
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
  integration**, and pick the integration. Today the list holds Philips
  Hue, LIFX and SwitchBot Cloud. hars checks what you type before it saves
  anything, starts the integration at once, and shows the devices that it
  found. In that last step you can give each device an area, or make a new
  area there, and give it a name. A device that you
  name gets entity ids to match, as in Home Assistant: a Bot called
  "Coffee machine" in the Kitchen has `button.kitchen_coffee_machine`, and
  a Hue lamp called "Reading lamp" in the Study has
  `light.study_reading_lamp`.
  History that was recorded under the old id stays under it. To undo the
  integration, open it and delete its entry.

  A rename does not rewrite entity ids that you typed into a dashboard or
  an automation, as in Home Assistant: change those by hand.

  The same name, area and labels can be changed later on the device's page.
  Two limits apply there today. hars refuses a new name for a device that
  has an entity of another integration than SwitchBot Cloud, Philips Hue or
  LIFX, because that entity would keep the old name until the next start.
  It also refuses one for a device with an entity whose own name starts
  with the device's and that you haven't named, because Home Assistant
  would rewrite that entity's name. (A device with no entities, or with
  entities of those three only and none like that, takes a new name.) And a
  device can't be disabled or enabled. Moving a device to another area and
  labelling it work for every device.

  Adding and deleting are off when hars can't save the change, and so is
  changing a device, an entity or the areas whose file it can't save. That
  happens when the config folder came from a Home Assistant release older
  than 2026.6, so the file that lists the integrations, the devices, the
  entities or the areas is in a format that hars reads but doesn't write.
  hars then says so when you pick the integration or save the change, and
  for integrations in its log at start. To fix it,
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

**Adding a bridge.** In **Settings > Devices & services > Add integration**,
pick **Philips Hue**. hars asks Philips' discovery service
(discovery.meethue.com) which bridges are on your network, as Home
Assistant does, and lists them by address. Pick yours, or pick **Manually
add a Hue Bridge** and type its address if the list is empty or the
service can't be reached. Then press the round button on the bridge and
choose **Submit** within about 30 seconds. If you submit before pressing,
the dialog just stays open: press the button and submit again. For
everything listed above, hars then makes the same devices, areas,
entities and entity ids as a fresh Home Assistant 2026.6 would, and the
lights work at once. (Home Assistant also lists buttons, dials and
doorbells; hars has no entities for those yet.) In the dialog's last
step you can put each device in an area and give it a name. To remove the bridge, open the
integration and delete its entry: no restart is needed.

Two limits of adding from the UI:

- hars does not listen for bridges that announce themselves on the network
  (zeroconf, SSDP, HomeKit), because listening would wake it while idle.
  So a bridge never shows up as "Discovered": you start the dialog.
- A first-generation (round) bridge, which only has the old v1 API, can't
  be added. hars says so and makes nothing, where Home Assistant would set
  it up over v1.

The key that the bridge hands out when you press its button is saved in
`.storage/core.config_entries`, as in Home Assistant. hars does not write
it to the log or send it to the browser. hars does not
check the bridge's TLS certificate (bridges sign their own), again as
Home Assistant, so only add a bridge on a network you trust.

- **Lights:** each light, and each room and zone as a grouped light.
  Brightness, colour, colour temperature, and transitions work as in Home
  Assistant.
- **Scenes:** scenes and smart scenes, with `scene.turn_on` and
  `hue.activate_scene`. `hue.hue_activate_scene` works for scenes, but not
  for smart scenes.
- **Sensors:** temperature, light level, battery, and Zigbee connectivity.
  A temperature is in °C or °F to match the unit system of the install
  when the sensor is first added. As in Home Assistant, a sensor keeps its
  unit if you change the unit system later, and a unit that you chose for
  it in Home Assistant before an import is kept. You can't choose a unit
  for a sensor in hars yet.
- **Binary sensors:** motion, grouped motion, camera motion, contact,
  tamper, and entertainment.
- **Switches:** the enable switch of each motion and light sensor, and the
  bridge's automations.

Not yet:

- Button, dial, and doorbell events (`hue_event`), and the motion-aware
  sensor.
- A new search for the bridge when its IP address changes. Give the bridge a
  fixed address in your router's settings.
- Tidying up after the bridge: a light that you delete in the Hue app
  keeps its device in hars until you remove the bridge, where Home
  Assistant removes it at the next start. A light or scene that you add in
  the Hue app works at once and is listed with its device after the next
  start.
- Two entries for one bridge. If an imported install has the same bridge
  twice, hars starts the first one that is not disabled or ignored and
  marks any other as failed; delete that one. A bridge that you ignored in
  Home Assistant stays ignored: hars does not start it. When you add that bridge from the UI, the new entry
  replaces the ignored one, as in Home Assistant.
- Pairing again when the bridge rejects its key (after a factory reset,
  say). Home Assistant opens the link dialog by itself; in hars, delete the
  entry and add the bridge again.
- Options (Home Assistant's **Configure** button for a bridge).

### LIFX

hars talks to each bulb on your network over UDP and asks for its state
every 10 seconds, as Home Assistant does. hars supports single-zone bulbs,
colour and white.

To add a bulb, open **Settings > Devices & services > Add integration** and
pick **LIFX**. Type the bulb's IP address, or leave the field empty: hars then
sends one search across your network, listens for 8.5 seconds, and lists the
bulbs that answered. Nothing searches while the dialog is closed. With
the installer's set-up for Docker Desktop on a Mac or on Windows, the search
finds nothing, as Home Assistant's does there: type the bulb's address.

Which products hars adds:

- Colour and white single-zone bulbs (the A19, BR30, Mini, GU10, Candle,
  Filament and Downlight families, and the rest that have neither zones nor
  extra features). A white bulb gets a white light, as in Home Assistant.
- Not light strips (Z, Beam, Neon, String), tiles, ceilings, the Clean
  (HEV light) and the Night Vision bulbs (infrared). Home Assistant adds
  them with entities that hars doesn't run yet, so hars says that it can't
  run that product and adds nothing. An imported one is still run, as a
  single-zone bulb.
- A LIFX Switch is refused, as in Home Assistant: the dialog says it can't
  connect.

Home Assistant also finds new bulbs by itself (through DHCP, HomeKit and a
search every 15 minutes) and offers them in **Discovered**. hars doesn't:
start the dialog yourself.

- **Lights:** power, brightness, colour, colour temperature, and
  transitions.
- **Buttons:** identify and restart.
- **Signal strength:** the sensor is listed, disabled, as in Home
  Assistant, but hars doesn't read it: enabled, it stays unavailable.

Not yet:

- The `lifx.*` services: effects, `lifx.set_state`, and themes.
- The signal-strength sensor's readings.
- Finding a bulb that moved where hars can't search. hars searches for
  each of its bulbs 5 seconds after it starts and every 15 minutes, and
  saves the new address of one that moved, as Home Assistant does. The
  installer's set-up for Docker Desktop on a Mac or on Windows publishes one
  port, so hars can't search there. Give each bulb a fixed address.

hars has been checked against one colour bulb.

### SwitchBot Cloud

hars uses SwitchBot's cloud API. Add it in **Settings > Devices & services >
Add integration** with the token and the secret key that the SwitchBot app
shows under its developer options, or import an install that has it. hars
asks for the state of the Bot and of the Hub 2 every 600 seconds, which is
Home Assistant's interval.

- **Bot:** a button that presses the Bot, and a battery sensor.
- **Hub 2:** temperature and humidity. The temperature follows the same
  rule for its unit as [Hue's](#philips-hue).
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

### MQTT

hars connects to your MQTT broker as Home Assistant does: the same client
settings, the same birth and will messages on `homeassistant/status`, and
reconnects 10 s after the broker goes away. Entities are unavailable while
the broker is gone.

- **Discovery:** `sensor` and `binary_sensor` entities that devices announce
  on the discovery topic (`homeassistant/...` by default), one entity per
  message or several in one device message. Zigbee2MQTT and Tasmota (with
  `SetOption19 1`) announce their sensors this way. Changed and removed
  announcements update and remove the entity, its registry entry and its
  device.
- **What a sensor does:** value templates, units, device and state classes,
  availability topics, JSON attributes, `expire_after`, `force_update`,
  `off_delay`, icons, entity categories, and entities that start disabled.
- **The `mqtt.publish` action**, with QoS 0, 1 and 2, retain, and a
  message expiry on MQTT 5.

hars speaks MQTT 3.1, 3.1.1 and 5 over plain TCP. Like Home Assistant, it
moves an entry to MQTT 5 if your broker takes it.

Not yet: adding MQTT from the UI (it comes across in an import), TLS,
WebSockets, client certificates, the other discovered entity types
(switches, lights, covers, climate and the rest: hars logs that it skipped
them), MQTT triggers in automations, entities from `mqtt:` in
`configuration.yaml`, the `mqtt.dump` and `mqtt.reload` actions, the MQTT
pages of the UI, and Tasmota's and other integrations that are set up
through MQTT. An `expire_after` sensor starts unavailable after a restart
until a new value comes (Home Assistant restores its last value).

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
another type, such as `camera`, `alarm_control_panel`, or `humidifier`, needs that
type added first. [Add an entity component](guides/add-an-entity-component.md)
shows how.

| Entity type | Services |
|---|---|
| `light` | `light.turn_on`, `light.turn_off`, `light.toggle` |
| `switch` | `switch.turn_on`, `switch.turn_off`, `switch.toggle` |
| `scene` | `scene.turn_on` |
| `button` | `button.press` |
| `climate` | Home Assistant's ten `climate` services |
| `vacuum` | `start`, `pause`, `stop`, `return_to_base`, `clean_spot`, `locate`, `set_fan_speed`, `send_command` |
| `select` | `select_option`, `select_first`, `select_last`, `select_next`, `select_previous` |
| `number` | `set_value` |
| `cover` | `open_cover`, `close_cover`, `stop_cover`, `set_cover_position`, `toggle`, and the five `*_tilt` services |
| `lock` | `lock`, `unlock`, `open` |
| `fan` | `turn_on`, `turn_off`, `toggle`, `set_percentage`, `increase_speed`, `decrease_speed`, `set_preset_mode`, `oscillate`, `set_direction` |
| `media_player` | `turn_on`, `turn_off`, `toggle`, `volume_up`, `volume_down`, `volume_set`, `volume_mute`, `media_play_pause`, `media_play`, `media_pause`, `media_stop`, `media_next_track`, `media_previous_track`, `media_seek`, `play_media`, `select_source`, `select_sound_mode`, `shuffle_set`, `repeat_set`, `join`, `unjoin`, `clear_playlist` |
| `remote` | `turn_on`, `turn_off`, `toggle`, `send_command`, `learn_command`, `delete_command` |
| `weather` | `weather.get_forecasts` |
| `notify` | `notify.send_message` (shared across `mobile_app` and plugins) |
| `sensor`, `binary_sensor`, `device_tracker` | None. These types are states only |

## Planned

The roadmap sets no dates and no order for the items below. To show that you
need one, [open an integration request](https://github.com/adamt94/hars/issues/new?template=integration_request.yml),
or add a 👍 to a request that exists.

Work left on the integrations above:

- Hue button, dial, and doorbell events.
- The `lifx.*` services, the signal-strength sensor's readings, and LIFX
  light strips, tiles, the Clean and Night Vision bulbs.
- More SwitchBot device types.

New integrations and features:

| Planned | What it brings |
|---|---|
| MQTT, what remains | Switches, lights, covers, climate and the other MQTT discovery types, so all of Zigbee2MQTT's devices; MQTT triggers, TLS, adding MQTT from the UI |
| ESPHome (`esphome`) | ESPHome devices, over their native API |
| Helpers (`input_*`, `template`) | Toggles, numbers, and template entities |
| To-do lists (`todo`) | Shopping and to-do lists |
| Text-to-speech | Spoken announcements |
| SwitchBot webhooks, remote access | Features that need a public URL |

A large integration that not everyone uses, such as a Zigbee stack, goes
behind a Cargo feature. People who don't use the integration
don't carry its code in memory
([performance rule 10](PERF.md#rules)).

## Ask for an integration

[Open an integration request](https://github.com/adamt94/hars/issues/new?template=integration_request.yml).
Say which devices you have and which entities and services you use. That
detail decides how much of the integration must be ported first.

## Add an integration yourself

[Add an integration](guides/add-an-integration.md) walks through a complete
port, with `met` as the example. Agree the integration with a maintainer in
an issue before you start, because each one adds to the binary and to the
memory that every user pays for.
