//! ScreenCast and Screenshot, through the GNOME-style D-Bus services niri serves
//! (org.gnome.Mutter.ScreenCast, org.gnome.Shell.Screenshot): the backend only picks
//! what to share; niri makes the PipeWire stream.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Structure, Value};
use zbus::{Connection, interface, proxy};

use crate::prompt::{Choice, Item, Outcome, Prompt, Prompter};
use crate::{Options, desktop, opt, put};

const MONITOR: u32 = 1;
const WINDOW: u32 = 2;
const CURSOR_HIDDEN: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;
const CURSOR_METADATA: u32 = 4;
const RESTORE_VENDOR: &str = "plain";
const RESTORE_VERSION: u32 = 1;

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast",
    default_service = "org.gnome.Mutter.ScreenCast",
    default_path = "/org/gnome/Mutter/ScreenCast"
)]
trait MutterScreenCast {
    fn create_session(&self, properties: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(interface = "org.gnome.Mutter.ScreenCast.Session", default_service = "org.gnome.Mutter.ScreenCast")]
trait MutterSession {
    fn record_monitor(&self, connector: &str, properties: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;
    fn record_window(&self, properties: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;
    fn start(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn closed(&self) -> zbus::Result<()>;
}

#[proxy(interface = "org.gnome.Mutter.ScreenCast.Stream", default_service = "org.gnome.Mutter.ScreenCast")]
trait MutterStream {
    #[zbus(signal, name = "PipeWireStreamAdded")]
    fn pipewire_stream_added(&self, node_id: u32) -> zbus::Result<()>;
    #[zbus(property)]
    fn parameters(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}

#[proxy(
    interface = "org.gnome.Shell.Introspect",
    default_service = "org.gnome.Shell.Introspect",
    default_path = "/org/gnome/Shell/Introspect"
)]
trait ShellIntrospect {
    fn get_windows(&self) -> zbus::Result<HashMap<u64, HashMap<String, OwnedValue>>>;
}

type MonitorId = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
type Monitor = (MonitorId, Vec<Mode>, HashMap<String, OwnedValue>);
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<MonitorId>, HashMap<String, OwnedValue>);

#[proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
trait DisplayConfig {
    #[allow(clippy::type_complexity)]
    fn get_current_state(&self) -> zbus::Result<(u32, Vec<Monitor>, Vec<LogicalMonitor>, HashMap<String, OwnedValue>)>;
}

#[proxy(
    interface = "org.gnome.Shell.Screenshot",
    default_service = "org.gnome.Shell.Screenshot",
    default_path = "/org/gnome/Shell/Screenshot"
)]
trait ShellScreenshot {
    fn screenshot(&self, include_cursor: bool, flash: bool, filename: &str) -> zbus::Result<(bool, String)>;
    fn pick_color(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}

/// Something that can be shared.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Monitor { connector: String, name: String },
    Window { id: u64, app_id: String, title: String },
}

impl Source {
    fn key(&self) -> String {
        match self {
            Source::Monitor { connector, .. } => format!("monitor:{connector}"),
            Source::Window { id, .. } => format!("window:{id}"),
        }
    }

    fn label(&self) -> String {
        match self {
            Source::Monitor { connector, name } if name.is_empty() || name == connector => format!("Screen  {connector}"),
            Source::Monitor { connector, name } => format!("Screen  {name} ({connector})"),
            Source::Window { app_id, title, .. } => {
                let app = desktop::label(app_id);
                if title.is_empty() { format!("Window  {app}") } else { format!("Window  {title} — {app}") }
            }
        }
    }

    fn kind(&self) -> u32 {
        match self {
            Source::Monitor { .. } => MONITOR,
            Source::Window { .. } => WINDOW,
        }
    }

    /// What a restore token keeps: window ids don't survive restarts, app ids do.
    fn restore_entry(&self) -> HashMap<String, String> {
        let mut m = HashMap::new();
        match self {
            Source::Monitor { connector, .. } => {
                m.insert("kind".into(), "monitor".into());
                m.insert("connector".into(), connector.clone());
            }
            Source::Window { app_id, title, .. } => {
                m.insert("kind".into(), "window".into());
                m.insert("app-id".into(), app_id.clone());
                m.insert("title".into(), title.clone());
            }
        }
        m
    }
}

/// The saved source that matches what's there now: the same output, or a window of
/// the same app (the one with the same title if there are several).
pub fn restore_match(saved: &HashMap<String, String>, sources: &[Source]) -> Option<Source> {
    match saved.get("kind")?.as_str() {
        "monitor" => {
            let c = saved.get("connector")?;
            sources.iter().find(|s| matches!(s, Source::Monitor { connector, .. } if connector == c)).cloned()
        }
        "window" => {
            let a = saved.get("app-id")?;
            let t = saved.get("title");
            let same_app: Vec<&Source> = sources.iter()
                .filter(|s| matches!(s, Source::Window { app_id, .. } if app_id == a))
                .collect();
            same_app.iter().find(|s| matches!(s, Source::Window { title, .. } if Some(title) == t))
                .or(same_app.first()).map(|s| (*s).clone())
        }
        _ => None,
    }
}

async fn sources(conn: &Connection, types: u32) -> Vec<Source> {
    let mut out = Vec::new();
    if types & MONITOR != 0
        && let Ok(dc) = DisplayConfigProxy::new(conn).await
    {
        match dc.get_current_state().await {
            Ok((_, monitors, _, _)) => {
                for ((connector, ..), _, props) in monitors {
                    let name = props.get("display-name").and_then(|v| String::try_from(v.try_clone().ok()?).ok()).unwrap_or_default();
                    out.push(Source::Monitor { connector, name });
                }
            }
            Err(e) => eprintln!("xdg-desktop-portal-plain: listing outputs: {e}"),
        }
    }
    if types & WINDOW != 0
        && let Ok(si) = ShellIntrospectProxy::new(conn).await
    {
        match si.get_windows().await {
            Ok(windows) => {
                let mut windows: Vec<_> = windows.into_iter().collect();
                windows.sort_by_key(|(id, _)| *id);
                for (id, props) in windows {
                    let get = |k: &str| props.get(k).and_then(|v| String::try_from(v.try_clone().ok()?).ok()).unwrap_or_default();
                    let app_id = get("app-id");
                    let app_id = app_id.strip_suffix(".desktop").unwrap_or(&app_id).to_string();
                    out.push(Source::Window { id, app_id, title: get("title") });
                }
            }
            Err(e) => eprintln!("xdg-desktop-portal-plain: listing windows: {e}"),
        }
    }
    out
}

#[derive(Default)]
struct Cast {
    types: u32,
    cursor_mode: u32,
    persist_mode: u32,
    restore: Vec<HashMap<String, String>>,
    mutter: Option<OwnedObjectPath>,
}

type Casts = Arc<Mutex<HashMap<OwnedObjectPath, Cast>>>;

async fn stop_mutter(conn: &Connection, path: OwnedObjectPath) {
    if let Ok(b) = MutterSessionProxy::builder(conn).path(path) && let Ok(s) = b.build().await {
        let _ = s.stop().await;
    }
}

/// The portal's session object for one screen cast.
struct CastSession {
    path: OwnedObjectPath,
    casts: Casts,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl CastSession {
    async fn close(&self, #[zbus(connection)] conn: &Connection) {
        let cast = self.casts.lock().unwrap().remove(&self.path);
        if let Some(path) = cast.and_then(|c| c.mutter) {
            stop_mutter(conn, path).await;
        }
        let conn = conn.clone();
        let path = self.path.clone();
        tokio::spawn(async move {
            let _ = conn.object_server().remove::<CastSession, _>(&path).await;
        });
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

pub struct ScreenCast {
    pub prompter: Arc<Prompter>,
    casts: Casts,
}

impl ScreenCast {
    pub fn new(prompter: Arc<Prompter>) -> Self {
        ScreenCast { prompter, casts: Arc::default() }
    }

    /// Ask niri to record a source, and wait for its PipeWire node.
    async fn record(conn: &Connection, source: &Source, cursor_mode: u32) -> zbus::Result<(OwnedObjectPath, u32, HashMap<String, OwnedValue>)> {
        let mutter_cursor: u32 = match cursor_mode {
            CURSOR_HIDDEN => 0,
            CURSOR_METADATA => 2,
            _ => 1,
        };
        let session_path = MutterScreenCastProxy::new(conn).await?.create_session(HashMap::new()).await?;
        let session = MutterSessionProxy::builder(conn).path(session_path.clone())?.build().await?;
        let mut props: HashMap<&str, Value<'_>> = HashMap::new();
        props.insert("cursor-mode", Value::U32(mutter_cursor));
        let stream_path = match source {
            Source::Monitor { connector, .. } => session.record_monitor(connector, props).await?,
            Source::Window { id, .. } => {
                props.insert("window-id", Value::U64(*id));
                session.record_window(props).await?
            }
        };
        let stream = MutterStreamProxy::builder(conn).path(stream_path)?.build().await?;
        let mut added = stream.receive_pipewire_stream_added().await?;
        session.start().await?;
        let node = tokio::time::timeout(Duration::from_secs(5), added.next()).await;
        let node = match node {
            Ok(Some(sig)) => sig.args()?.node_id,
            _ => {
                let _ = session.stop().await;
                return Err(zbus::Error::Failure("niri didn't start the stream".into()));
            }
        };
        let params = stream.parameters().await.unwrap_or_default();
        Ok((session_path, node, params))
    }

    /// End the portal session when niri ends the cast (its window closed, say).
    fn follow(conn: Connection, mutter: OwnedObjectPath, portal: OwnedObjectPath, casts: Casts) {
        tokio::spawn(async move {
            let Ok(b) = MutterSessionProxy::builder(&conn).path(mutter) else { return };
            let Ok(session) = b.build().await else { return };
            let Ok(mut closed) = session.receive_closed().await else { return };
            if closed.next().await.is_some() && casts.lock().unwrap().remove(&portal).is_some() {
                if let Ok(e) = SignalEmitter::new(&conn, portal.clone()) {
                    let _ = CastSession::closed(&e).await;
                }
                let _ = conn.object_server().remove::<CastSession, _>(&portal).await;
            }
        });
    }
}

fn restore_value(entries: Vec<HashMap<String, String>>) -> OwnedValue {
    let data = Value::from(entries);
    Value::from(Structure::from((RESTORE_VENDOR, RESTORE_VERSION, data))).try_to_owned().expect("no fds")
}

fn parse_restore(options: &Options) -> Vec<HashMap<String, String>> {
    let Some(Value::Structure(s)) = options.get("restore_data").map(|v| &**v) else { return Vec::new() };
    let [Value::Str(vendor), Value::U32(version), data] = s.fields() else { return Vec::new() };
    if vendor.as_str() != RESTORE_VENDOR || *version != RESTORE_VERSION {
        return Vec::new();
    }
    let data = match data {
        Value::Value(inner) => &**inner,
        other => other,
    };
    data.try_clone().ok().and_then(|d| Vec::<HashMap<String, String>>::try_from(d).ok()).unwrap_or_default()
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    #[zbus(out_args("response", "results"))]
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: Options,
    ) -> (u32, Options) {
        let obj = CastSession { path: session_handle.clone(), casts: self.casts.clone() };
        if server.at(&session_handle, obj).await.is_err() {
            return (2, Options::new());
        }
        self.casts.lock().unwrap().insert(session_handle, Cast { types: MONITOR, ..Default::default() });
        (0, Options::new())
    }

    #[zbus(out_args("response", "results"))]
    async fn select_sources(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        options: Options,
    ) -> (u32, Options) {
        let mut casts = self.casts.lock().unwrap();
        let Some(cast) = casts.get_mut(&session_handle) else { return (2, Options::new()) };
        cast.types = opt::<u32>(&options, "types").filter(|t| t & (MONITOR | WINDOW) != 0).unwrap_or(MONITOR);
        cast.cursor_mode = opt::<u32>(&options, "cursor_mode").unwrap_or(CURSOR_HIDDEN);
        cast.persist_mode = opt::<u32>(&options, "persist_mode").unwrap_or(0).min(2);
        cast.restore = parse_restore(&options);
        (0, Options::new())
    }

    #[zbus(out_args("response", "results"))]
    #[allow(clippy::too_many_arguments)]
    async fn start(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        _options: Options,
    ) -> (u32, Options) {
        let (types, cursor_mode, persist_mode, restore) = {
            let casts = self.casts.lock().unwrap();
            let Some(c) = casts.get(&session_handle) else { return (2, Options::new()) };
            (c.types, c.cursor_mode, c.persist_mode, c.restore.clone())
        };
        let available = sources(conn, types).await;
        if available.is_empty() {
            return (2, Options::new());
        }

        let restored: Option<Source> = restore.first().and_then(|r| restore_match(r, &available));
        let (source, remember) = match restored {
            Some(s) => (s, true),
            None => {
                let prompt = Prompt {
                    title: format!("{} wants to share your screen", if app_id.is_empty() { "A program".into() } else { desktop::label(&app_id) }),
                    subtitle: "Pick what it sees".into(),
                    pick: available.iter().map(|s| Item { id: s.key(), label: s.label() }).collect(),
                    choices: if persist_mode > 0 {
                        vec![Choice { id: "remember".into(), label: "Remember this choice".into(), options: vec![], initial: "false".into() }]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                };
                match self.prompter.ask(server, &handle, prompt).await {
                    Outcome::Answered(r) => {
                        let Some(s) = r.picked.and_then(|k| available.iter().find(|s| s.key() == k).cloned()) else {
                            return (1, Options::new());
                        };
                        (s, r.choices.iter().any(|(id, v)| id == "remember" && v == "true"))
                    }
                    Outcome::Cancelled => return (1, Options::new()),
                    Outcome::Failed => return (2, Options::new()),
                }
            }
        };

        let (mutter, node, params) = match Self::record(conn, &source, cursor_mode).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("xdg-desktop-portal-plain: screen cast: {e}");
                return (2, Options::new());
            }
        };
        let still_open = {
            let mut casts = self.casts.lock().unwrap();
            match casts.get_mut(&session_handle) {
                Some(c) => {
                    c.mutter = Some(mutter.clone());
                    true
                }
                None => false,
            }
        };
        if !still_open {
            stop_mutter(conn, mutter).await;
            return (1, Options::new());
        }
        Self::follow(conn.clone(), mutter, session_handle, self.casts.clone());

        let mut stream: HashMap<String, OwnedValue> = HashMap::new();
        put(&mut stream, "source_type", source.kind());
        put(&mut stream, "id", source.key());
        for key in ["size", "position"] {
            if let Some(v) = params.get(key).and_then(|v| v.try_clone().ok()) {
                stream.insert(key.into(), v);
            }
        }
        let mut results = Options::new();
        put(&mut results, "streams", vec![(node, stream)]);
        if persist_mode > 0 && remember {
            put(&mut results, "persist_mode", persist_mode);
            results.insert("restore_data".into(), restore_value(vec![source.restore_entry()]));
        }
        (0, results)
    }

    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        MONITOR | WINDOW
    }

    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        CURSOR_HIDDEN | CURSOR_EMBEDDED
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        5
    }
}

pub struct Screenshot {
    runtime: PathBuf,
    seq: AtomicU64,
}

impl Screenshot {
    pub fn new(runtime: &std::path::Path) -> Self {
        Screenshot { runtime: runtime.join("screenshots"), seq: AtomicU64::new(0) }
    }
}

/// Screenshots are allowed per app by the portal itself (its permission store, and an
/// Access question for apps that haven't been allowed yet) before this is reached.
#[interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl Screenshot {
    #[zbus(out_args("response", "results"))]
    async fn screenshot(
        &self,
        #[zbus(connection)] conn: &Connection,
        _handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _options: Options,
    ) -> (u32, Options) {
        if let Err(e) = std::fs::create_dir_all(&self.runtime) {
            eprintln!("xdg-desktop-portal-plain: {}: {e}", self.runtime.display());
            return (2, Options::new());
        }
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let path = self.runtime.join(format!("screenshot-{}-{n}.png", std::process::id()));
        let taken = async {
            let shot = ShellScreenshotProxy::new(conn).await?;
            shot.screenshot(false, false, &path.to_string_lossy()).await
        };
        match taken.await {
            Ok((true, used)) => {
                let mut results = Options::new();
                put(&mut results, "uri", crate::file_uri(std::path::Path::new(&used)));
                (0, results)
            }
            Ok((false, _)) => (2, Options::new()),
            Err(e) => {
                eprintln!("xdg-desktop-portal-plain: screenshot: {e}");
                (2, Options::new())
            }
        }
    }

    #[zbus(out_args("response", "results"))]
    async fn pick_color(
        &self,
        #[zbus(connection)] conn: &Connection,
        _handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _options: Options,
    ) -> (u32, Options) {
        let picked = async { ShellScreenshotProxy::new(conn).await?.pick_color().await };
        match picked.await {
            Ok(r) => match r.get("color").and_then(|c| c.try_clone().ok()) {
                Some(c) => {
                    let mut results = Options::new();
                    results.insert("color".into(), c);
                    (0, results)
                }
                None => (1, Options::new()),
            },
            Err(e) => {
                eprintln!("xdg-desktop-portal-plain: pick color: {e}");
                (2, Options::new())
            }
        }
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u64, app: &str, title: &str) -> Source {
        Source::Window { id, app_id: app.into(), title: title.into() }
    }

    #[test]
    fn restoring() {
        let sources = vec![
            Source::Monitor { connector: "eDP-1".into(), name: "Built-in display".into() },
            win(3, "kitty", "one"),
            win(7, "kitty", "two"),
        ];
        assert_eq!(restore_match(&sources[0].restore_entry(), &sources), Some(sources[0].clone()));
        // a window comes back by app and title, though its id changed
        assert_eq!(restore_match(&win(99, "kitty", "two").restore_entry(), &sources), Some(sources[2].clone()));
        assert_eq!(restore_match(&win(99, "kitty", "gone").restore_entry(), &sources), Some(sources[1].clone()));
        assert_eq!(restore_match(&win(1, "firefox", "x").restore_entry(), &sources), None);
    }

    #[test]
    fn restore_round_trip() {
        let entry = Source::Monitor { connector: "DP-2".into(), name: String::new() }.restore_entry();
        let mut o = Options::new();
        o.insert("restore_data".into(), restore_value(vec![entry.clone()]));
        assert_eq!(parse_restore(&o), vec![entry]);
        let mut other = Options::new();
        other.insert("restore_data".into(), Value::from(Structure::from(("gnome", 1u32, Value::from(1u32)))).try_to_owned().unwrap());
        assert!(parse_restore(&other).is_empty());
    }

    #[test]
    fn labels() {
        assert_eq!(Source::Monitor { connector: "eDP-1".into(), name: "Built-in display".into() }.label(), "Screen  Built-in display (eDP-1)");
        assert_eq!(win(1, "no-such-app", "Title").label(), "Window  Title — no-such-app");
    }
}
