//! `hars` — the homeassistant-rust server.

use ha_core::Hass;

mod import;
use std::future::IntoFuture;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// How long in-flight HTTP requests get to finish after a stop signal. Half
/// of `docker stop`'s default 10 s grace period, so the final stages still
/// run before Docker sends SIGKILL.
const HTTP_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> anyhow::Result<()> {
    // Before the runtime and its worker threads allocate anything.
    let thp_disabled = disable_transparent_huge_pages();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        return cli(&args);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(thp_disabled))
}

/// On a host with transparent huge pages set to `always`, the kernel backs
/// mimalloc's arena with 2 MiB pages: 8 MiB of what is otherwise ~4 MiB idle
/// RSS (docs/PERF.md). The first huge pages are faulted before `main` runs,
/// so this sets `PR_SET_THP_DISABLE` (inherited across `exec`) and re-execs
/// the binary once: the new image starts with THP off before it allocates.
/// The pid is unchanged, so this is safe as a container's PID 1. If the
/// re-exec fails, THP is still off for everything allocated from here on.
/// `MIMALLOC_ALLOW_THP=1` keeps THP on. `hars` spawns no children, which would
/// inherit the flag.
fn disable_transparent_huge_pages() -> std::io::Result<bool> {
    #[cfg(target_os = "linux")]
    if std::env::var_os("MIMALLOC_ALLOW_THP").is_none_or(|v| v != "1") {
        use rustix::thread::{disable_transparent_huge_pages, transparent_huge_pages_are_disabled};
        if transparent_huge_pages_are_disabled()? {
            return Ok(true); // already off: inherited, or this is the re-exec
        }
        disable_transparent_huge_pages(true)?;
        use std::os::unix::process::CommandExt;
        // `/proc/self/exe` is the running inode, so an upgrade that replaces
        // or deletes the binary on disk can't swap what gets exec'd.
        let mut args = std::env::args_os();
        let arg0 = args.next().unwrap_or_else(|| "hars".into());
        // Only returns on failure.
        let err = std::process::Command::new("/proc/self/exe")
            .arg0(arg0)
            .args(args)
            .exec();
        return Err(err);
    }
    Ok(false)
}

async fn run(thp_disabled: std::io::Result<bool>) -> anyhow::Result<()> {
    {
        use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    // No colour codes in `docker logs` or journald.
                    .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
                    .with_filter(filter),
            )
            // Warnings and errors for `system_log/list`, whatever the filter.
            .with(ha_api::system_log::layer())
            .init();
    }

    match thp_disabled {
        Ok(true) => tracing::debug!("transparent huge pages disabled"),
        Ok(false) => {}
        Err(e) => tracing::warn!("could not start with transparent huge pages off: {e}"),
    }

    let hass = Hass::new();
    let addr = std::env::var("HARS_BIND").unwrap_or_else(|_| "0.0.0.0:8123".into());
    let mut api_config = api_config()?;
    let config_dir = config_dir()?;
    let (core_config, config_source) = {
        let dir = config_dir.clone();
        tokio::task::spawn_blocking(move || ha_core::config::CoreConfig::load(&dir)).await??
    };
    hass.set_config(core_config);
    api_config.registries = {
        let dir = config_dir.clone();
        std::sync::Arc::new(
            tokio::task::spawn_blocking(move || ha_registry::Registries::load(&dir)).await??,
        )
    };
    let components = {
        let dir = config_dir.clone();
        tokio::task::spawn_blocking(move || ha_components::Components::load(&dir)).await?
    };
    // Zone and person entities, before anything can read the states.
    components.setup(&hass, &api_config.registries);
    api_config.components = std::sync::Arc::new(components);
    let entries = {
        let dir = config_dir.clone();
        tokio::task::spawn_blocking(move || ha_integrations::load_entries(&dir)).await?
    };
    ha_api::system_log::register(&hass);
    // Last states, before any entity that restores itself is made.
    let restore = {
        let dir = config_dir.clone();
        tokio::task::spawn_blocking(move || ha_core::restore::RestoreState::load(&dir)).await?
    };
    hass.set_restore(restore);
    let profiles = {
        let dir = config_dir.clone();
        tokio::task::spawn_blocking(move || ha_integrations::light::Profiles::load(&dir)).await?
    };
    let started = ha_integrations::start(
        &hass,
        api_config.registries.clone(),
        entries.as_deref(),
        profiles,
    );
    api_config.integrations = started.running;
    api_config.brands = match ha_api::brands::Brands::new(&config_dir, ha_api::brands::CDN) {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::error!("brands: {e}");
            None
        }
    };
    api_config.weather = started.weather;
    api_config.lovelace = {
        let dir = config_dir.clone();
        std::sync::Arc::new(
            tokio::task::spawn_blocking(move || ha_api::Lovelace::load(&dir)).await??,
        )
    };
    api_config.frontend_data = {
        let dir = config_dir.clone();
        std::sync::Arc::new(
            tokio::task::spawn_blocking(move || ha_api::FrontendData::load(&dir)).await?,
        )
    };
    api_config.labs = {
        let dir = config_dir.clone();
        std::sync::Arc::new(tokio::task::spawn_blocking(move || ha_api::Labs::load(&dir)).await?)
    };
    api_config.config_dir = std::path::absolute(&config_dir)?.display().to_string();
    api_config.config_source = config_source;
    let auth = {
        let (dir, clock) = (config_dir.clone(), hass.clock().clone());
        tokio::task::spawn_blocking(move || ha_auth::Auth::load(&dir, clock)).await??
    };
    tracing::info!("config directory {}", config_dir.display());
    if !auth.is_onboarded() {
        tracing::warn!(
            "no owner yet: create one with `hars auth add USERNAME PASSWORD` (same HARS_CONFIG)"
        );
    }

    let mut signals = StopSignals::install()?;
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let (stop_http, http_stopped) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(
        axum::serve(
            listener,
            ha_api::router(hass.clone(), auth.clone(), &api_config)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = http_stopped.await;
        })
        .into_future(),
    );
    // HA's bootstrap: every integration's first setup attempt (its first
    // writes, or a failure it retries in the background), at most
    // `STAGE_2_TIMEOUT`; then `homeassistant_start`, where registered
    // entities nobody provides become `unavailable`; then
    // `homeassistant_started`, where automations attach.
    // A stop signal during the wait stops at once.
    let signalled = tokio::select! {
        done = tokio::time::timeout(SETUP_TIMEOUT, hass.setups_done()) => {
            if done.is_err() {
                tracing::warn!(
                    "integrations still setting up after {SETUP_TIMEOUT:?}; starting anyway"
                );
            }
            None
        }
        signal = signals.recv() => Some(signal),
    };
    match signalled {
        Some(signal) => tracing::info!("{signal} received during startup, stopping"),
        None => {
            hass.begin_start();
            api_config.registries.write_unavailable_states(&hass);
            hass.finish_start();
            {
                // The last states are dumped now, then every 15 minutes.
                let hass = hass.clone();
                tokio::spawn(async move {
                    let restore = hass.restore();
                    loop {
                        restore.dump(&hass).await;
                        tokio::time::sleep(ha_core::restore::DUMP_INTERVAL).await;
                    }
                });
            }
            tracing::info!("hars listening on http://{addr}");
            tokio::select! {
                signal = signals.recv() => tracing::info!("{signal} received, stopping"),
                res = &mut server => {
                    res??;
                    anyhow::bail!("HTTP server exited unexpectedly");
                }
            }
        }
    }

    hass.begin_stop();
    let _ = stop_http.send(());
    match tokio::time::timeout(HTTP_DRAIN_TIMEOUT, &mut server).await {
        Ok(res) => res??,
        Err(_) => {
            tracing::warn!(
                "HTTP connections still open after {HTTP_DRAIN_TIMEOUT:?}; closing them"
            );
            server.abort();
        }
    }
    if auth.needs_save() {
        let auth = auth.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || auth.save()).await? {
            tracing::error!("saving auth state: {e}");
        }
    }
    hass.restore().dump(&hass).await;
    hass.finish_stop();
    tracing::info!("hars stopped");
    Ok(())
}

/// SIGTERM (container stop) and SIGINT (Ctrl-C) handlers. They are installed
/// before the port is bound, so a signal that arrives as soon as the server
/// is reachable still stops it cleanly instead of killing it.
struct StopSignals {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
}

impl StopSignals {
    fn install() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                term: signal(SignalKind::terminate())?,
                int: signal(SignalKind::interrupt())?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    async fn recv(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.term.recv() => "SIGTERM",
                _ = self.int.recv() => "SIGINT",
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            "Ctrl-C"
        }
    }
}

/// `HARS_FRONTEND` points at the `hass_frontend` directory of the pinned
/// `home-assistant-frontend` release. Without it only the API is served.
fn api_config() -> anyhow::Result<ha_api::ApiConfig> {
    let Some(dir) = std::env::var_os("HARS_FRONTEND") else {
        tracing::info!("HARS_FRONTEND not set: serving the API without the frontend");
        return Ok(ha_api::ApiConfig::default());
    };
    let frontend = ha_api::Frontend::load(&dir)
        .map_err(|e| anyhow::anyhow!("loading the frontend from {}: {e}", dir.to_string_lossy()))?;
    tracing::info!("serving the frontend from {}", frontend.root().display());
    Ok(ha_api::ApiConfig {
        frontend: Some(std::sync::Arc::new(frontend)),
        ..Default::default()
    })
}

/// `HARS_CONFIG`: HA's config directory (`/config` in the Docker image). Its
/// `.storage` files are read and written in HA's formats.
fn config_dir() -> anyhow::Result<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(
        std::env::var_os("HARS_CONFIG").unwrap_or_else(|| "config".into()),
    );
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("creating the config directory {}: {e}", dir.display()))?;
    Ok(dir)
}

/// HA's bootstrap `STAGE_2_TIMEOUT`.
const SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

const USAGE: &str = "usage: hars                              run the server
       hars auth add USERNAME PASSWORD  add a login (the first one becomes the owner)
       hars import HA_CONFIG_DIR [--dry-run] [--force]
                                        take over a Home Assistant 2026.6 config
                                        (a copy, with HA stopped)";

/// Offline commands, like HA's `hass --script auth`. Run them with the
/// server stopped: the server keeps its own copy of the auth state.
fn cli(args: &[String]) -> anyhow::Result<()> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["auth", "add", username, password] => {
            let dir = config_dir()?;
            let auth = ha_auth::Auth::load(&dir, ha_core::Clock::system())?;
            let added = auth.add_user(username, password)?;
            auth.save()?;
            match added {
                ha_auth::Added::Owner => {
                    println!("created owner {username:?}; onboarding is complete")
                }
                ha_auth::Added::Credential => {
                    println!("added login {username:?}; its user is created at first login")
                }
            }
            Ok(())
        }
        ["import", source, flags @ ..]
            if flags.iter().all(|f| matches!(*f, "--dry-run" | "--force")) =>
        {
            let opts = import::Options {
                source: source.into(),
                dest: config_dir()?,
                dry_run: flags.contains(&"--dry-run"),
                force: flags.contains(&"--force"),
            };
            let report = import::run(&opts)?;
            print!("{}", import::summary(&report, &opts.dest));
            Ok(())
        }
        ["--help" | "-h" | "help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}
