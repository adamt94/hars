# Installing hars: the details

The [README](../README.md#get-started) has the one command most people need.
This page is for when you want to know what it does, change how it runs, or
install another way.

## What the install command does

[install.sh](../install.sh) (Linux, macOS) and [install.ps1](../install.ps1)
(Windows) do the same steps:

1. Check for Docker. If it's missing, ask, then install it: Docker's
   official script on Linux, Docker Desktop through Homebrew (macOS) or
   winget (Windows). If Docker Desktop is installed but stopped, start it.
   On Linux, if your user can't use Docker, use `sudo`.
2. Download the hars image, `ghcr.io/adamt94/homeassistant-rust:latest`.
   [A workflow](../.github/workflows/publish.yml) publishes it from every
   commit to `main`, for x86-64 and ARM64.
3. Make the `hars` folder (`hars/config` holds the data) and `hars/hars.env`
   (settings). If a container named `hars` already runs from that folder,
   stop it (giving hars its minute to save) and remove it; the data stays.
   It checks the port first, and refuses to touch a `hars` container that
   keeps its data anywhere else.
4. With `HARS_IMPORT`, run `hars import` on the Home Assistant copy.
5. Start the container: named `hars`, restarted by Docker after a crash or a
   reboot, given a minute to save when stopped. On Linux it shares the
   host's network (`--network host`), so hars can find devices on your LAN.
   Docker Desktop can't, so on macOS and Windows the port is published
   instead (`-p 8123:8123`). hars still reaches devices at the addresses
   they were set up with, but network broadcasts don't get out, so it can't
   find a LIFX bulb that moved to a new IP address.
6. Wait until hars answers, print the address, and open it in your browser.

Running it again does the same: that is the upgrade.

### Its settings

Set these before the command: `curl -fsSL … | HARS_PORT=8124 sh` on Linux and
macOS, `$env:HARS_PORT = '8124'; irm … | iex` in PowerShell.

| Setting | Does |
|---|---|
| `HARS_DIR` | where hars keeps its folder (default: `hars` in your home folder) |
| `HARS_PORT` | the web port (default: 8123, or the next free one up to 8133) |
| `HARS_IMPORT` | a copy of a Home Assistant 2026.6 to 2026.11 config folder ([which releases](#which-home-assistant-releases)) to bring over before hars first starts (a new install only; otherwise [import by hand](#importing-by-hand)) |
| `HARS_YES=1` | answer yes to the questions (installing Docker), for unattended installs |
| `HARS_IMAGE` | another image, such as one you built (`docker build -t hars .`) |

`hars/hars.env` is passed to the container. It holds `HARS_BIND` (the port;
the installer keeps it across upgrades) and can hold `RUST_LOG=debug` for
more detail in the log. Edit it, then run the install command again.

## Your data and backups

Everything is in `hars/config`:

- `.storage/`: logins, registries, dashboards, integrations' settings, the
  location, in Home Assistant's own formats.
- `.hars/recorder/`: history, 10 days or 64 MiB by default.
- `configuration.yaml` and the files it includes, and `import-report.json`
  if you imported.

Upgrades keep it. To back up, stop hars and copy the folder. On Linux some
files belong to root:

```sh
docker stop hars
sudo tar czf hars-backup-$(date +%F).tar.gz -C ~/hars config
docker start hars
```

To restore, stop hars, move `config` aside, `sudo tar xzf` the backup in its
place (`-C ~/hars`) and start hars.

On stop, hars finishes open requests (up to 5 s), saves, and flushes
history.

## Docker Compose

If you'd rather manage hars with Compose, on a Linux machine with Docker and
Docker Compose:

```sh
mkdir hars && cd hars
curl -fsSLO https://raw.githubusercontent.com/adamt94/homeassistant-rust/main/compose.yaml
docker compose up -d
```

Then open `http://<your-machine>:8123` and follow the onboarding screens.
The data is in `./config` next to `compose.yaml`. The container is also
named `hars`, so `docker logs -f hars` works; run the other commands in the
folder with `compose.yaml`:

| To | Run |
|---|---|
| stop / start | `docker compose stop` / `docker compose start` |
| upgrade | `docker compose pull && docker compose up -d` |
| remove (data stays) | `docker compose down` |

In a clone of this repository Compose builds the image from the
[Dockerfile](../Dockerfile) when it can't pull it (`git pull && docker
compose up -d --build` to upgrade). To change the port, add
`environment: [HARS_BIND=0.0.0.0:8124]` under the service. With Docker
Desktop (macOS, Windows) host networking is off by default: turn it on in
Docker Desktop's settings, or replace `network_mode: host` with
`ports: ["8123:8123"]` (then hars can't find a LIFX bulb that changes IP
address).

To move from Home Assistant with Compose, before the first
`docker compose up`:

```sh
docker compose run --rm -v /path/to/ha-copy:/ha:ro hars import /ha --dry-run
docker compose run --rm -v /path/to/ha-copy:/ha:ro hars import /ha
```

## Without a browser (scripted setup)

To set hars up from a script, create the owner before the first start. This
completes onboarding, so the browser goes straight to the login page. With
the installer, set `HARS_YES=1`, run it, then:

```sh
docker stop hars
docker run --rm -v ~/hars/config:/config ghcr.io/adamt94/homeassistant-rust auth add YOUR_NAME YOUR_PASSWORD
docker start hars
```

(With Compose: `docker compose run --rm hars auth add YOUR_NAME
YOUR_PASSWORD` before `docker compose up -d`.) It prints `created owner
"YOUR_NAME"; onboarding is complete`. To keep the password out of your
shell history, run `read -rs PW`, type it, and write `"$PW"` in its place.
The same command adds more logins later.

The location then stays at Home Assistant's defaults (0,0, time zone UTC,
metric units, named "Home"), which sunrise and sunset, time triggers and the
days in history follow. Write your own to `config/.storage/core.config`
(Home Assistant's own file) and restart:

```sh
sudo tee ~/hars/config/.storage/core.config >/dev/null <<'END'
{"version": 1, "minor_version": 4, "key": "core.config", "data": {
  "latitude": 51.5072, "longitude": -0.1276, "elevation": 11,
  "unit_system_v2": "metric", "location_name": "Home",
  "time_zone": "Europe/London", "currency": "GBP", "country": "GB",
  "language": "en", "radius": 100, "external_url": null, "internal_url": null}}
END
docker restart hars
```

Put in your own values. `unit_system_v2` is `metric` or `us_customary`.
`time_zone` is a name like `America/New_York`; hars reads it only from this
file, not from `TZ`, and an unknown name means UTC. `elevation` and `radius`
(the size of the home zone) are in meters. Keep every key: if the file
doesn't load, hars won't start and `docker logs hars` says why.

## Importing by hand

The installer's `HARS_IMPORT` runs this. To check first what would come
across, or to import into an install that is already set up:

```sh
docker stop hars
docker run --rm -v ~/hars/config:/config -v /path/to/ha-copy:/ha:ro ghcr.io/adamt94/homeassistant-rust import /ha --dry-run
docker run --rm -v ~/hars/config:/config -v /path/to/ha-copy:/ha:ro ghcr.io/adamt94/homeassistant-rust import /ha
docker start hars
```

If hars already has a login, or has run once, the import refuses to replace
what's there; add `--force` to replace it. Stop Home Assistant before
starting hars if both use port 8123.

An import that was interrupted leaves `hars/config/.import-staging` behind,
and the next one stops rather than clear a folder it didn't make. If no
other import is running, delete that folder and import again.

### Which Home Assistant releases

The file `.HA_VERSION` in the config folder says which release wrote it.

| Your Home Assistant | The import |
|---|---|
| 2026.6.x or 2026.7.x | works. hars copies the files as they are |
| 2026.8.x to 2026.11.x | works. hars first puts some files back into the 2026.6 form ([below](#coming-from-20268-to-202611)) |
| older than 2026.6 | refused. Update Home Assistant to a release from 2026.6 to 2026.11 (not later: `ha core update --version 2026.11.2`, or the `2026.11.2` image tag), let it start once, then copy the folder again |
| newer than 2026.11 | not tested. hars imports the folder if every file is in a format it reads or converts, and says the release is untested. Otherwise it refuses, names the file, and changes none of your files |

hars behaves like Home Assistant 2026.6.1 and shows the 2026.6 app,
whichever release you come from. The import says so, and
`import-report.json` records the release (`ha_version`), whether hars is
tested with it (`ha_version_tested`) and the release hars behaves like
(`behaves_like`).

The import decides file by file: it checks each file's own storage version,
not only `.HA_VERSION`. It never writes to the folder you import from. It
converts and checks a copy first, and it changes your hars folder only when
every file has passed.

#### Coming from 2026.7

- The UI is 2026.6's: pages and cards the frontend gained since aren't
  there, and a dashboard that uses a newer card shows an error in that
  card's place.
- An automation that uses a trigger or condition 2026.7 made generally
  available (`light.turned_on`, `climate.started_heating`, ...; a Labs
  preview in 2026.6) is `unavailable`. `import-report.json` lists each one
  with the reason, and the automation stays in your YAML untouched.
- Behaviour follows 2026.6. Two rules 2026.7 changed: which zone wins when
  zones overlap (2026.6: the nearest centre; 2026.7: the smallest zone) and
  how a person's state follows a router or Bluetooth tracker.
- The companion app's Live Activity push tokens (`mobile_app`, new in
  2026.7) are kept in the imported file until hars next changes it, then
  dropped, as Home Assistant 2026.6 would drop them. `newer_stores` in the
  report names such files.

#### Coming from 2026.8 to 2026.11

Everything above applies, and these releases store some things in a newer
format. hars reads and writes the 2026.6 formats only, so the import
converts these files. The terminal and `import-report.json` (`converted`,
`converted_entries`, `dropped`, `split_devices`) say what was converted in
your folder.

| Since | What changed in Home Assistant | What the import does |
|---|---|---|
| 2026.8 | The device list (`core.device_registry`, storage version 3): every device belongs to one integration. A device that two integrations shared became two devices with new ids | Writes the list in the 2026.6 format. The two devices stay two, with the new ids, because your entities already point at them |
| 2026.8 | The entity list (`core.entity_registry` 1.23) has a setting for how new entity ids are made | Leaves the setting out. If you changed it, the report lists it under `dropped`. hars makes entity ids as 2026.6 does |
| 2026.9 | Child devices (device list 3.4): a part of a device, such as one socket of a power strip, kept in its own list without a maker or model | Lists each as an ordinary device again. The maker and model are not in your files to bring back. The link to its parent is kept in the file but not shown |
| 2026.10 | LIFX settings (entry version 2): the bulb's serial is written without colons, in the settings, the device and the entity ids | Writes the serial with colons again, as hars's LIFX code expects it |
| 2026.10 | Hue settings (entry 1.2): Zigbee addresses moved from one kind of device connection to another | Nothing. hars and Home Assistant 2026.6 both run a 1.2 entry, and hars does not use those connections |
| 2026.11 | Entity names (`core.entity_registry` 1.24): a name you gave is stored without the device's name, which Home Assistant adds when it shows the name | Stores each name in full again. Every entity keeps the friendly name 2026.11 gave it |
| 2026.11 | Weather settings (`met`, entry version 2): the place is stored another way, and the forecast's name moved from the entity to the device | Stores the place the 2026.6 way and names the forecast after the entry's title |
| 2026.11 | Last states (`core.restore_state`) are kept by an entity's registry id | Keeps them by entity id again, under the id each entity has now |

What you will notice:

- **A device that two integrations shared is two devices.** Home Assistant
  2026.8 did this, and hars keeps it. Home Assistant still accepts the old
  device id in an action and takes it to mean both devices. hars does not:
  the old id reaches no device. The import names each YAML file and
  dashboard that still uses an old id, with the two new ids to use
  instead. hars does not edit your YAML.
- **A weather forecast is named as in 2026.6.** In 2026.11 the forecast for
  a place you named "Cabin" shows as "Cabin". In hars it shows as
  "Forecast Cabin", as it did before 2026.11. Its entity id is unchanged.
- **A name can differ in one capital letter.** 2026.11 capitalises what is
  left of a name after it takes the device's name off the front. "Desk lamp
  reading light" on the device "Desk lamp" became "Desk lamp Reading
  light" in 2026.11, and stays so.
- **The `sun` trigger's `offset_type` (2026.8) and `period` (2026.9), and
  dashboard cards added since 2026.6, are not in hars.** hars has no `sun`
  trigger yet, so an automation that uses one is `unavailable` and listed
  in the report whichever release wrote it. A newer card shows an error in
  its place.
- **The LIFX entry from before one entry per bulb is gone.** 2026.10
  removed it from your settings. It ran nothing.

What stops an import from these releases, with the reason on the terminal:

- A file in a format newer than 2026.11's. For example:

  ```text
  Error: core.device_registry is storage version 3.5, in a config directory of Home Assistant 2026.12.0; hars reads version 1.12, as Home Assistant 2026.6.1 does, and converts versions 3.2 and 3.4 to it (Home Assistant 2026.8 to 2026.11). ...
  ```

- A child device that is disabled only because its parent device is
  disabled. 2026.6 has no such state. Enable the parent in Home Assistant
  and copy the folder again.
- Settings of the LIFX or weather integration that don't have the shape
  2026.10 and 2026.11 write.

The conversion follows Home Assistant's own code: each rule is the reverse
of a migration in the release that made the change, or what that release
answers to code written for the old format. It is tested by a round trip:
one 2026.6.1 install, updated by each release's own code, must come back
through the import as it was, apart from the differences listed above. The
install in that test is small and made up. No real 2026.8 to 2026.11
install has been imported yet, so read the report and look your devices
over after an import. If something is wrong, your Home Assistant folder is
as you left it.

#### Going back to Home Assistant

Start Home Assistant on its own folder again. The import did not change
it. Starting a newer Home Assistant on the folder hars has been running on
is not tested when the import converted files.

#### Older files

The import also refuses a folder whose device, entity or area list or
integration settings are at an *older* storage version than 2026.6 writes:
start Home Assistant 2026.6 to 2026.11 on the folder once, which brings
them up to date, and copy it again.

## Troubleshooting

- **The browser shows Home Assistant's onboarding screen** and the log says
  `not set up yet`: there is no login yet. Follow the screens, or set up
  [without a browser](#without-a-browser-scripted-setup).
- **`Error: Address already in use (os error 98)` in the log:** something
  else, often Home Assistant, already uses the port. Stop it, or pick
  another port (`HARS_PORT`, or `HARS_BIND` in `hars.env`).

## Just the binary, or from source

- **The binary:** the image's `/hars` is a static Linux binary:
  `docker cp "$(docker create ghcr.io/adamt94/homeassistant-rust)":/hars .`
  It reads `HARS_CONFIG` (default `./config`), `HARS_BIND` and
  `HARS_FRONTEND` (an unpacked `hass_frontend` folder; without it only the
  API is served).
- **From source:** `cargo build --release`; see
  [CONTRIBUTING.md](../CONTRIBUTING.md).
