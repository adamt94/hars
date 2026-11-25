# Roborock plugin for `hars`

Adds Roborock vacuum and dock support to `hars` as an out-of-process plugin
([ADR 0011](../../docs/adr/0011-plugin-system.md)) powered by the same
`python-roborock` library Home Assistant 2026.6.1 uses.

## What is supported

- **Config flow in the UI**: **Settings → Devices & services → Add integration → Roborock**
  (email + server region → email verification code).
- **Drop-in `hars import` compatibility**: uses Home Assistant 2026.6.1's exact
  `roborock` config entry schema (`username`, `user_data`, `base_url`), so
  existing Roborock entries imported from Home Assistant work without
  re-authenticating.
- **Devices**: Roborock V1 vacuums (S5/S6/S7/S8/Q Revo/Saros with local LAN +
  cloud MQTT fallback), B01 Q7, and B01 Q10.
- **Entities**:
  - `vacuum`: start (with smart zone/segment resume), pause, stop, return to
    base, clean spot, locate, set fan speed, and `vacuum.send_command` (for
    room cleaning `app_segment_clean`, goto, zones, etc.).
  - `sensor`: battery, status, vacuum error, cleaning area/time/progress,
    totals, consumable lifetimes, and dock error.
  - `binary_sensor`: cleaning, charging, mop attached, water box attached,
    water shortage, mop drying.
  - `switch`: child lock, status indicator light, do not disturb.
  - `button`: empty dustbin, wash mop, consumable resets.

## Option 1: Install locally (macOS / Linux, `stdio` transport)

1. Install the plugin into your `hars` config directory:

   ```sh
   HARS_CONFIG=./config cargo run --release -- plugin install ./plugins/roborock
   ```

2. If [`uv`](https://docs.astral.sh/uv/) is installed, no extra Python setup is
   needed—`bin/hars-roborock` provisions `python-roborock==5.12.0`
   automatically. Otherwise, create a virtualenv in the persistent plugin data
   directory (after step 1 so it is not copied over symlinks and survives
   plugin upgrades):

   ```sh
   python3 -m venv ./config/plugins/.data/roborock/.venv
   ./config/plugins/.data/roborock/.venv/bin/pip install -r ./config/plugins/roborock/requirements.txt
   ```

3. Start `hars` and add your account in **Settings → Devices & services → Add
   integration → Roborock**.

## Option 2: Docker sidecar (`unix_socket` transport)

For `hars`'s `FROM scratch` Docker image, run the Roborock plugin as a sidecar
container sharing `<config>/plugins/run/roborock.sock`:

1. Create `<config>/plugins/roborock/` and copy the sidecar manifest and assets
   into it:

   ```sh
   mkdir -p ./config/plugins/roborock/translations
   cp plugins/roborock/plugin.sidecar.json ./config/plugins/roborock/plugin.json
   cp plugins/roborock/translations/en.json ./config/plugins/roborock/translations/en.json
   cp plugins/roborock/icons.json ./config/plugins/roborock/icons.json
   ```

2. Build and run the sidecar beside `hars`:

   ```yaml
   services:
     hars-roborock:
       build: ./plugins/roborock
       volumes:
         - ./config/plugins/run:/config/plugins/run
       restart: unless-stopped
   ```
