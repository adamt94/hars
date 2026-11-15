# hars

hars is a rebuild of [Home Assistant](https://www.home-assistant.io/)'s core
in Rust, made to run a smart home in a few megabytes of memory. It serves the
unchanged official Home Assistant frontend and companion apps, and keeps its
data in Home Assistant's own file formats. A fresh install idles at about
7 MiB of memory and no CPU ([measurements](docs/perf/RESULTS.md)).

It is early software. **Works today:** the web UI and login, dashboards,
history and logbook, the iOS companion app, YAML automations (state,
numeric-state and time triggers), the `sun`, `met`, `hue`, `lifx`,
`switchbot_cloud`, `mobile_app`, `person` and `zone` integrations, and
importing a Home Assistant 2026.6 install. **Not yet:** the onboarding screen,
setting your location, adding integrations or editing automations from the
UI, templates beyond plain text and variables (no `states()` or `now()`
yet), scripts, notifications and remote access. If you need those,
keep Home Assistant for now.

## Install

You need a Linux machine (x86-64 or ARM64) with Docker and Docker Compose.

1. Make a folder for hars and download the Compose file into it:

   ```sh
   mkdir hars && cd hars
   curl -fsSLO https://raw.githubusercontent.com/adamt94/homeassistant-rust/main/compose.yaml
   ```

2. Create your login. hars has no onboarding screen yet, so the first login
   is made here; it becomes the owner (administrator).

   ```sh
   docker compose run --rm hars auth add YOUR_NAME YOUR_PASSWORD
   ```

   It prints `created owner "YOUR_NAME"; onboarding is complete`.
   To keep the password out of your shell history, run `read -rs PW`, type
   it, and write `"$PW"` in its place.
   Moving from Home Assistant? Skip this step and
   [import](#moving-from-home-assistant) instead: your logins come with it.

3. Start hars:

   ```sh
   docker compose up -d
   ```

4. Open `http://<your-machine>:8123` and log in with that name and password.

Next, [set your location](#set-your-location). Docker restarts hars if it
stops and after a reboot. The image is pulled from
`ghcr.io/adamt94/homeassistant-rust`, which
[a workflow](.github/workflows/publish.yml) publishes from every commit to
`main`. If you run these steps in a clone of this repository (in place of
step 1's download) and the image can't be pulled, Compose builds it from the
[Dockerfile](Dockerfile) instead, which takes a few minutes.

## Set your location

A fresh install starts with Home Assistant's defaults: location 0,0, time
zone UTC, metric units, named "Home". Sunrise and sunset, time triggers and
the days in history all follow them. There is no settings screen for this
yet, so write them to `config/.storage/core.config` (Home Assistant's own
file) and restart. Skip this if you imported: your settings came with it.

```sh
sudo tee config/.storage/core.config >/dev/null <<'EOF'
{"version": 1, "minor_version": 4, "key": "core.config", "data": {
  "latitude": 51.5072, "longitude": -0.1276, "elevation": 11,
  "unit_system_v2": "metric", "location_name": "Home",
  "time_zone": "Europe/London", "currency": "GBP", "country": "GB",
  "language": "en", "radius": 100, "external_url": null, "internal_url": null}}
EOF
docker compose restart
```

Put in your own values. `unit_system_v2` is `metric` or `us_customary`.
`time_zone` is a name like `America/New_York`; hars reads it only from this
file, not from `TZ`, and an unknown name means UTC. `elevation` and `radius`
(the size of the home zone) are in meters. Keep every key: if the file
doesn't load, hars won't start and `docker compose logs` says why.

## Moving from Home Assistant

hars can take over a Home Assistant **2026.6** install: users, passwords and
logged-in sessions (browsers and the companion app), registries, areas,
dashboards, integrations' settings and the YAML automations. Do this instead
of step 2, before the first start.

1. Make a copy of Home Assistant's config folder with Home Assistant stopped,
   or unpack a backup. Never point hars at the folder Home Assistant is using.
2. Check what will come across, then import:

   ```sh
   docker compose run --rm -v /path/to/ha-copy:/ha:ro hars import /ha --dry-run
   docker compose run --rm -v /path/to/ha-copy:/ha:ro hars import /ha
   ```

3. Read `config/import-report.json`: it lists each integration and automation
   and whether hars can run it.
4. Stop Home Assistant (both can't use port 8123) and run
   `docker compose up -d`.

History is not imported: it starts fresh. If you already ran step 2 of the
install, the import refuses to replace that login; add `--force` to replace
it. To try hars beside Home Assistant first, with nothing it decides reaching
a device, see [shadow mode](docs/SHADOW.md).

## Running it

Run these in the folder with `compose.yaml`.

| To | Run |
|---|---|
| see the logs | `docker compose logs -f` |
| stop / start | `docker compose stop` / `docker compose start` |
| add another login | `docker compose stop`, then `docker compose run --rm hars auth add NAME PASSWORD`, then `docker compose start` |
| upgrade | `docker compose pull && docker compose up -d` (in a clone that builds: `git pull && docker compose up -d --build`) |
| remove (data stays) | `docker compose down` |

On stop, hars finishes open requests (up to 5 s), saves, and flushes history.
Compose gives it a minute.

**Where your data lives:** in `./config`, next to `compose.yaml`. It holds
`.storage/` (logins, registries, dashboards, integrations' settings, in Home
Assistant's formats), `.hars/recorder/` (history, 10 days or 64 MiB by
default), `configuration.yaml` and what it includes, and
`import-report.json` if you imported. Upgrades keep it.

**Back up:** stop hars and copy the folder. Some files are readable by root
only, so use `sudo`:

```sh
docker compose stop
sudo tar czf hars-backup-$(date +%F).tar.gz config
docker compose start
```

To restore, stop hars, move `config` aside, `sudo tar xzf` the backup in its
place and start hars.

**Networking:** `compose.yaml` uses the host's network (`network_mode: host`)
so hars can reach devices on your LAN (LIFX discovery and other broadcasts
need it). hars listens on port 8123. To use another port, add
`environment: [HARS_BIND=0.0.0.0:8124]` under the service.

## Troubleshooting and limits

- **The browser shows Home Assistant's onboarding screen**, and the log says
  `no owner yet`: there is no login. Stop hars, run install step 2, start it.
- **Nothing answers on port 8123 with Docker Desktop (macOS, Windows):** host
  networking is off there by default. Turn it on in Docker Desktop's settings,
  or replace `network_mode: host` with `ports: ["8123:8123"]` (LAN discovery
  then won't work).
- **`Error: Address already in use (os error 98)` in the log:** something
  else, often Home Assistant, already uses port 8123. Stop it or change
  `HARS_BIND` as above.
- **An integration or automation is missing after an import:** check
  `config/import-report.json`; it says why.
- **More detail in the log:** add `environment: [RUST_LOG=debug]`.
- **Limits:** only Home Assistant 2026.6 installs can be imported. The UI is
  in English. The companion app is tested on iOS. Everything in "Not yet"
  above. The full list of what is done and planned is in the
  [roadmap](docs/ROADMAP.md).

## Other ways to run it

- **Just the binary:** the image's `/hars` is a static Linux binary:
  `docker cp "$(docker create ghcr.io/adamt94/homeassistant-rust)":/hars .`
  It reads `HARS_CONFIG` (default `./config`), `HARS_BIND` and
  `HARS_FRONTEND` (an unpacked `hass_frontend` folder; without it only the
  API is served).
- **From source:** `cargo build --release`; see
  [CONTRIBUTING.md](CONTRIBUTING.md).

## More

- [Documentation index](docs/README.md): how hars is built, performance,
  the roadmap.
- [Contributing](CONTRIBUTING.md), [security policy](SECURITY.md),
  [code of conduct](CODE_OF_CONDUCT.md).

## License

Apache-2.0, as Home Assistant. hars is an independent reimplementation. It is
not affiliated with or endorsed by the Home Assistant project or Nabu Casa.
