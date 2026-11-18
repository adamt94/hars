# hars

hars is a rebuild of [Home Assistant](https://www.home-assistant.io/)'s core
in Rust, made to run a smart home in a few megabytes of memory. It serves the
unchanged official Home Assistant frontend and companion apps, and keeps its
data in Home Assistant's own file formats. A fresh install idles at about
7 MiB of memory and no CPU ([measurements](docs/perf/RESULTS.md)).

It is early software. **Works today:** setting up in the browser with Home
Assistant's onboarding screens, the web UI and login, dashboards, history and
logbook, the iOS companion app (including push notifications to it),
automations in YAML or made and edited in Settings (state, numeric-state,
time and template triggers, with templates in their conditions, variables
and service calls: "it's 28°C in the lounge" to your phone), the `sun`, `met`, `hue`, `lifx`,
`switchbot_cloud`, `mobile_app`, `person` and `zone` integrations, and
importing a Home Assistant 2026.6 install. **Not yet:** adding
integrations from the UI, automation traces, the `delay` and
`wait_template` actions, templates beyond states, time, maths and type
casts (no areas, devices, labels, regex or JSON functions yet, and no live
re-rendering of `render_template`), scripts, other kinds of notifications
and remote access. If you need those, keep Home Assistant
for now.

## Install

Copy one command, paste it, press Enter. It sets everything up and tells you
what to open when it's done.

**Mac or Linux:** open the Terminal app and paste:

```sh
curl -fsSL https://raw.githubusercontent.com/adamt94/homeassistant-rust/main/install.sh | sh
```

**Windows:** open PowerShell (search for it in the Start menu) and paste:

```powershell
irm https://raw.githubusercontent.com/adamt94/homeassistant-rust/main/install.ps1 | iex
```

When it finishes you see something like:

```
hars is running.

  Open  http://192.168.1.20:8123  (on this computer: http://localhost:8123)
  and create your account.
```

Open that address in a browser, on this computer or any phone or laptop on
the same Wi-Fi. Create your login, set your home's location, and you're on
the dashboard.

hars runs inside [Docker](https://www.docker.com/). If you don't have it, the
command offers to install it first (on Windows and Mac that's Docker Desktop,
which may ask you to accept its terms or restart once; then paste the command
again). Everything hars keeps goes in a `hars` folder in your home folder.
Want to see what the command does before running it? Read
[install.sh](install.sh) or [install.ps1](install.ps1).

**Which computer?** One that stays on: a Raspberry Pi 4 or 5 (64-bit), a mini
PC, a NAS or any Linux server is ideal. A Mac or Windows PC works too, with
one difference: there Docker can't let hars search your network, so if a
LIFX bulb gets a new IP address, hars won't find it again. Give your bulbs a
fixed address in your router's settings (Home Assistant in Docker Desktop
has the same limit).

## Moving from Home Assistant

hars can take over a Home Assistant **2026.6** install: your logins (browsers
and the companion app stay logged in), devices, areas, dashboards,
integrations' settings, location and YAML automations. History starts fresh.

1. In Home Assistant, make a backup and unpack it, or stop Home Assistant and
   copy its config folder (the one with `configuration.yaml` in it). Never
   use the folder Home Assistant is running from.
2. Run the install command with the copy's location in front. Mac or Linux:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/adamt94/homeassistant-rust/main/install.sh | HARS_IMPORT=/path/to/ha-copy sh
   ```

   Windows:

   ```powershell
   $env:HARS_IMPORT = 'C:\path\to\ha-copy'; irm https://raw.githubusercontent.com/adamt94/homeassistant-rust/main/install.ps1 | iex
   ```

3. Log in as you did on Home Assistant. `hars/config/import-report.json`
   lists each integration and automation and whether hars can run it.

If Home Assistant still runs on the same computer, hars picks the next free
port (8124) and tells you. To give hars port 8123, stop Home Assistant and
run the install command again with `HARS_PORT=8123` in front of `sh`.
To try hars beside Home Assistant first, with nothing it decides reaching a
device, see [shadow mode](docs/SHADOW.md).

## Everyday use

| To | Run |
|---|---|
| upgrade | the install command again (your data and settings stay) |
| see what it's doing | `docker logs -f hars` |
| stop / start | `docker stop hars` / `docker start hars` |
| change its port or log detail | edit `hars/hars.env` in your home folder, then run the install command again |
| uninstall | `docker rm -f hars`, then delete the `hars` folder if you don't want your data |

On Linux, if the installer said it uses `sudo`, put `sudo` in front of the
`docker` commands. hars starts again by itself after a restart.

**Back up** by copying the `hars` folder while hars is stopped. On Linux some
files belong to root, so copy with `sudo`. [More on backups and where data
lives](docs/INSTALL.md#your-data-and-backups).

## Troubleshooting

- **The command says Docker isn't running:** open Docker Desktop (Mac,
  Windows) and wait until it says it's running, then paste the command again.
- **The page doesn't load from another device:** use the address with
  numbers the installer printed, not `localhost`, and check both devices are
  on the same network. On Windows, allow Docker Desktop through the firewall
  if it asks.
- **Something went wrong:** `docker logs hars` shows why. For more detail,
  uncomment `RUST_LOG=debug` in `hars/hars.env` and run the install command
  again.
- **An integration or automation is missing after moving from Home
  Assistant:** `hars/config/import-report.json` says why.
- **Limits:** only Home Assistant 2026.6 installs can be imported. The UI is
  in English. The companion app is tested on iOS. Everything in "Not yet"
  above. The [roadmap](docs/ROADMAP.md) lists what is done and planned.

## Other ways to install

[docs/INSTALL.md](docs/INSTALL.md) covers installing with Docker Compose,
setting hars up from a script without a browser, the installer's settings,
running the plain binary and building from source.

## More

- [Documentation index](docs/README.md): how hars is built, performance,
  the roadmap.
- [Contributing](CONTRIBUTING.md), [security policy](SECURITY.md),
  [code of conduct](CODE_OF_CONDUCT.md).

## License

Apache-2.0, as Home Assistant. hars is an independent reimplementation. It is
not affiliated with or endorsed by the Home Assistant project or Nabu Casa.
