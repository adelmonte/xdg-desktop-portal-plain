//! A toolkit-free xdg-desktop-portal backend for niri: settings from a config file,
//! notifications and idle inhibit passed on to the desktop's own services, and the
//! few questions that need a person asked in a terminal window.

mod config;
mod desktop;
mod inhibit;
mod notification;
mod portals;
mod prompt;

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use futures_util::StreamExt;
use inotify::{Inotify, WatchMask};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, connection};

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.plain";
pub const PATH: &str = "/org/freedesktop/portal/desktop";

pub type Options = HashMap<String, OwnedValue>;

pub struct State {
    pub config: RwLock<config::Config>,
    pub settings: RwLock<config::Namespaces>,
}

pub fn opt<'a, T>(options: &'a Options, key: &str) -> Option<T>
where
    T: TryFrom<Value<'a>>,
{
    let v = options.get(key)?;
    T::try_from(Value::from(v.try_clone().ok()?)).ok()
}

pub fn put<'a>(options: &mut Options, key: &str, v: impl Into<Value<'a>>) {
    if let Ok(v) = v.into().try_to_owned() {
        options.insert(key.into(), v);
    }
}

pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

async fn reload(conn: &Connection, state: &State) {
    let (config, settings) = match config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xdg-desktop-portal-plain: {e} (keeping the settings from before)");
            return;
        }
    };
    let lockdown_changed = config.lockdown != state.config.read().unwrap().lockdown;
    let changes: Vec<(String, String, OwnedValue)> = {
        let old = state.settings.read().unwrap();
        portals::changed(&old, &settings).into_iter()
            .filter_map(|(n, k, v)| Some((n.to_string(), k.to_string(), v.try_clone().ok()?)))
            .collect()
    };
    *state.config.write().unwrap() = config;
    *state.settings.write().unwrap() = settings;
    eprintln!("xdg-desktop-portal-plain: reloaded {} ({} settings changed)", config::path().display(), changes.len());

    if let Ok(emitter) = SignalEmitter::new(conn, PATH) {
        for (ns, key, value) in &changes {
            if let Err(e) = portals::Settings::setting_changed(&emitter, ns, key, Value::from(value.try_clone().unwrap())).await {
                eprintln!("xdg-desktop-portal-plain: SettingChanged: {e}");
            }
        }
    }
    if lockdown_changed && let Err(e) = portals::lockdown_changed(conn.object_server()).await {
        eprintln!("xdg-desktop-portal-plain: lockdown: {e}");
    }
}

/// Reload whenever the config file is written, replaced or removed.
async fn watch_config(conn: Connection, state: Arc<State>) -> std::io::Result<()> {
    let path = config::path();
    let dir = path.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir)?;
    let name = path.file_name().unwrap().to_os_string();
    let inotify = Inotify::init()?;
    inotify.watches().add(&dir, WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::MOVED_FROM | WatchMask::DELETE)?;
    let mut events = inotify.into_event_stream([0u8; 4096])?;
    while let Some(ev) = events.next().await {
        let Ok(ev) = ev else { continue };
        if ev.name.as_deref() != Some(name.as_os_str()) {
            continue;
        }
        // An editor's save can be several events in a row
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        while let Ok(Some(_)) = tokio::time::timeout(std::time::Duration::from_millis(10), events.next()).await {}
        reload(&conn, &state).await;
    }
    Ok(())
}

const HELP: &str = "\
xdg-desktop-portal-plain: a toolkit-free xdg-desktop-portal backend

Usage: xdg-desktop-portal-plain
       xdg-desktop-portal-plain --check     check the config file and print the settings

Started by D-Bus when the portal needs it. Settings come from
$XDG_CONFIG_HOME/xdg-desktop-portal-plain/config.toml, reread when it changes.";

fn check() -> i32 {
    match config::load() {
        Ok((_, settings)) => {
            let mut namespaces: Vec<_> = settings.iter().collect();
            namespaces.sort_by_key(|(n, _)| *n);
            for (ns, keys) in namespaces {
                println!("{ns}");
                let mut keys: Vec<_> = keys.iter().collect();
                keys.sort_by_key(|(k, _)| *k);
                for (k, v) in keys {
                    println!("  {k} = {}", **v);
                }
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> zbus::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.first().and_then(|a| a.to_str()) {
        None => {}
        Some("--prompt") if args.len() == 2 => {
            if let Err(e) = prompt::run(Path::new(&args[1])) {
                eprintln!("xdg-desktop-portal-plain: {e}");
                std::process::exit(1);
            }
            return Ok(());
        }
        Some("--check") => std::process::exit(check()),
        Some("--help" | "-h") => {
            println!("{HELP}");
            return Ok(());
        }
        Some("--version" | "-V") => {
            println!("xdg-desktop-portal-plain {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {
            eprintln!("xdg-desktop-portal-plain: unknown arguments {:?} (see --help)", args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>());
            std::process::exit(2);
        }
    }

    let (config, settings) = config::load().unwrap_or_else(|e| {
        eprintln!("xdg-desktop-portal-plain: {e} (using the defaults)");
        let c = config::Config::default();
        let s = config::settings(&c).unwrap_or_default();
        (c, s)
    });
    let state = Arc::new(State { config: RwLock::new(config), settings: RwLock::new(settings) });
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("xdg-desktop-portal-plain");
    std::fs::create_dir_all(&runtime)?;

    let prompter = Arc::new(prompt::Prompter::new(state.clone(), runtime.clone()));
    let notifications = notification::Notifications::new(&runtime);
    let table = notifications.table();
    let conn = connection::Builder::session()?
        .serve_at(PATH, portals::Settings { state: state.clone() })?
        .serve_at(PATH, portals::Lockdown { state: state.clone() })?
        .serve_at(PATH, portals::AppChooser { prompter: prompter.clone() })?
        .serve_at(PATH, portals::Access { prompter: prompter.clone() })?
        .serve_at(PATH, portals::Account { prompter: prompter.clone() })?
        .serve_at(PATH, portals::DynamicLauncher { prompter })?
        .serve_at(PATH, portals::Email)?
        .serve_at(PATH, inhibit::Inhibit::default())?
        .serve_at(PATH, notifications)?
        .name(BUS_NAME)?
        .build()
        .await?;

    {
        let conn = conn.clone();
        tokio::spawn(async move {
            if let Err(e) = notification::Notifications::watch(conn, table).await {
                eprintln!("xdg-desktop-portal-plain: watching the notification server: {e}");
            }
        });
    }
    {
        let conn = conn.clone();
        tokio::spawn(async move {
            if let Err(e) = watch_config(conn, state).await {
                eprintln!("xdg-desktop-portal-plain: watching {}: {e}", config::path().display());
            }
        });
    }

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    let _ = std::fs::remove_dir_all(runtime.join("icons"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_are_escaped() {
        assert_eq!(file_uri(Path::new("/home/a b/ü#.png")), "file:///home/a%20b/%C3%BC%23.png");
    }
}
