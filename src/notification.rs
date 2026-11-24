//! The Notification portal, passed on to the notification server
//! (org.freedesktop.Notifications), with its buttons' clicks passed back.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, interface, proxy};

use crate::{Options, desktop, opt};

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Server {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
    fn close_notification(&self, id: u32) -> zbus::Result<()>;
    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;
    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;
    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Icon {
    Themed(Vec<String>),
    Bytes(Vec<u8>),
}

/// A portal icon: `("themed", as)`, `("bytes", ay)` or `("file-descriptor", h)`.
pub fn icon_of(v: &Value<'_>) -> Option<Icon> {
    let Value::Structure(s) = v else { return None };
    let fields = s.fields();
    let Value::Str(tag) = fields.first()? else { return None };
    let inner = match fields.get(1)? {
        Value::Value(b) => &**b,
        other => other,
    };
    match tag.as_str() {
        "themed" => {
            let names: Vec<String> = Vec::try_from(inner.try_clone().ok()?).ok()?;
            Some(Icon::Themed(names))
        }
        "bytes" => Some(Icon::Bytes(Vec::<u8>::try_from(inner.try_clone().ok()?).ok()?)),
        "file-descriptor" => {
            let Value::Fd(fd) = inner else { return None };
            let mut file = std::fs::File::from(fd.as_fd().try_clone_to_owned().ok()?);
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).ok()?;
            Some(Icon::Bytes(bytes))
        }
        _ => None,
    }
}

pub fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn strip_markup(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

struct Sent {
    app_id: String,
    id: String,
    /// action key → (portal action, target)
    actions: HashMap<String, (String, Option<OwnedValue>)>,
    icon: Option<PathBuf>,
}

#[derive(Default)]
pub struct Table {
    by_key: HashMap<(String, String), u32>,
    by_server: HashMap<u32, Sent>,
}

impl Table {
    fn forget(&mut self, server_id: u32) -> Option<Sent> {
        let sent = self.by_server.remove(&server_id)?;
        let key = (sent.app_id.clone(), sent.id.clone());
        if self.by_key.get(&key) == Some(&server_id) {
            self.by_key.remove(&key);
        }
        if let Some(icon) = &sent.icon {
            let _ = std::fs::remove_file(icon);
        }
        Some(sent)
    }
}

pub struct Notifications {
    table: Arc<Mutex<Table>>,
    icons: PathBuf,
}

impl Notifications {
    pub fn new(runtime: &std::path::Path) -> Self {
        let icons = runtime.join("icons");
        let _ = std::fs::create_dir_all(&icons);
        Notifications { table: Arc::default(), icons }
    }

    fn icon_file(&self, bytes: &[u8]) -> Option<PathBuf> {
        let ext = if bytes.starts_with(b"\x89PNG") {
            "png"
        } else if bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") {
            "svg"
        } else if bytes.starts_with(b"\xff\xd8") {
            "jpg"
        } else {
            "img"
        };
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = self.icons.join(format!("{n}.{ext}"));
        std::fs::write(&path, bytes).ok()?;
        Some(path)
    }

    /// Pass the server's clicks and closes back while the connection lasts.
    pub async fn watch(conn: Connection, table: Arc<Mutex<Table>>) -> zbus::Result<()> {
        let server = ServerProxy::new(&conn).await?;
        let mut invoked = server.receive_action_invoked().await?;
        let mut closed = server.receive_notification_closed().await?;
        let emitter = SignalEmitter::new(&conn, crate::PATH)?;
        loop {
            tokio::select! {
                Some(sig) = invoked.next() => {
                    let Ok(args) = sig.args() else { continue };
                    let hit = {
                        let t = table.lock().unwrap();
                        t.by_server.get(&args.id).and_then(|s| {
                            let (action, target) = s.actions.get(&args.action_key)?;
                            let target = target.as_ref().and_then(|t| t.try_clone().ok());
                            Some((s.app_id.clone(), s.id.clone(), action.clone(), target))
                        })
                    };
                    if let Some((app_id, id, action, target)) = hit {
                        let parameter: Vec<Value<'_>> = target.into_iter().map(Value::from).collect();
                        if let Err(e) = Notifications::action_invoked(&emitter, &app_id, &id, &action, parameter).await {
                            eprintln!("xdg-desktop-portal-plain: ActionInvoked: {e}");
                        }
                    }
                }
                Some(sig) = closed.next() => {
                    if let Ok(args) = sig.args() {
                        table.lock().unwrap().forget(args.id);
                    }
                }
                else => return Ok(()),
            }
        }
    }

    pub fn table(&self) -> Arc<Mutex<Table>> {
        self.table.clone()
    }
}

#[interface(name = "org.freedesktop.impl.portal.Notification")]
impl Notifications {
    async fn add_notification(&self, #[zbus(connection)] conn: &Connection, app_id: String, id: String, notification: Options) {
        if let Err(e) = self.add(conn, app_id, id, notification).await {
            eprintln!("xdg-desktop-portal-plain: sending a notification: {e}");
        }
    }

    async fn remove_notification(&self, #[zbus(connection)] conn: &Connection, app_id: String, id: String) {
        let server_id = self.table.lock().unwrap().by_key.get(&(app_id, id)).copied();
        if let Some(server_id) = server_id {
            if let Ok(server) = ServerProxy::new(conn).await {
                let _ = server.close_notification(server_id).await;
            }
            self.table.lock().unwrap().forget(server_id);
        }
    }

    #[zbus(signal)]
    pub async fn action_invoked(emitter: &SignalEmitter<'_>, app_id: &str, id: &str, action: &str, parameter: Vec<Value<'_>>) -> zbus::Result<()>;

    #[zbus(property)]
    fn supported_options(&self) -> Options {
        Options::new()
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

impl Notifications {
    async fn add(&self, conn: &Connection, app_id: String, id: String, n: Options) -> zbus::Result<()> {
        let server = ServerProxy::new(conn).await?;
        let caps = server.get_capabilities().await.unwrap_or_default();
        let markup = caps.iter().any(|c| c == "body-markup");

        let title: String = opt(&n, "title").unwrap_or_default();
        let body = match (opt::<String>(&n, "markup-body"), opt::<String>(&n, "body")) {
            (Some(m), _) if markup => m,
            (Some(m), _) => strip_markup(&m),
            (None, Some(b)) if markup => escape_markup(&b),
            (None, b) => b.unwrap_or_default(),
        };

        let mut icon_file = None;
        let app_icon = match n.get("icon").and_then(|v| icon_of(v)) {
            Some(Icon::Themed(names)) => names.into_iter().next().unwrap_or_default(),
            Some(Icon::Bytes(bytes)) => {
                icon_file = self.icon_file(&bytes);
                icon_file.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
            }
            None => app_id.clone(),
        };

        let mut actions: HashMap<String, (String, Option<OwnedValue>)> = HashMap::new();
        let mut keys: Vec<String> = Vec::new();
        if let Some(a) = opt::<String>(&n, "default-action") {
            let target = n.get("default-action-target").and_then(|t| t.try_clone().ok());
            actions.insert("default".into(), (a, target));
            keys.extend(["default".into(), String::new()]);
        }
        let buttons: Vec<HashMap<String, OwnedValue>> = opt(&n, "buttons").unwrap_or_default();
        for (i, b) in buttons.iter().enumerate() {
            let (Some(label), Some(action)) = (opt::<String>(b, "label"), opt::<String>(b, "action")) else { continue };
            let key = format!("button-{i}");
            actions.insert(key.clone(), (action, b.get("target").and_then(|t| t.try_clone().ok())));
            keys.extend([key, label]);
        }

        let priority: String = opt(&n, "priority").unwrap_or_default();
        let urgency: u8 = match priority.as_str() {
            "low" => 0,
            "urgent" => 2,
            _ => 1,
        };
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("urgency", Value::U8(urgency));
        if !app_id.is_empty() {
            hints.insert("desktop-entry", Value::from(app_id.clone()));
        }
        if let Some(c) = opt::<String>(&n, "category") {
            hints.insert("category", Value::from(c));
        }
        if opt::<String>(&n, "sound").as_deref() == Some("silent") {
            hints.insert("suppress-sound", Value::Bool(true));
        }
        let display: Vec<String> = opt(&n, "display-hint").unwrap_or_default();
        if display.iter().any(|d| d == "transient") {
            hints.insert("transient", Value::Bool(true));
        }
        if display.iter().any(|d| d == "persistent") {
            hints.insert("resident", Value::Bool(true));
        }

        let app_name = if app_id.is_empty() { String::new() } else { desktop::label(&app_id) };
        let key = (app_id.clone(), id.clone());
        let replaces = self.table.lock().unwrap().by_key.get(&key).copied().unwrap_or(0);
        let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let timeout = if urgency == 2 { 0 } else { -1 };
        let server_id = server.notify(&app_name, replaces, &app_icon, &title, &body, &key_refs, hints, timeout).await?;

        let mut t = self.table.lock().unwrap();
        if replaces != 0 && replaces != server_id {
            t.forget(replaces);
        } else if let Some(old) = t.by_server.get(&server_id).and_then(|s| s.icon.clone())
            && Some(&old) != icon_file.as_ref()
        {
            let _ = std::fs::remove_file(old);
        }
        t.by_key.insert(key, server_id);
        t.by_server.insert(server_id, Sent { app_id, id, actions, icon: icon_file });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Structure;

    #[test]
    fn icons() {
        let themed = Value::from(Structure::from(("themed", Value::from(vec!["firefox", "web-browser"]))));
        assert_eq!(icon_of(&themed), Some(Icon::Themed(vec!["firefox".into(), "web-browser".into()])));
        let bytes = Value::from(Structure::from(("bytes", Value::from(vec![1u8, 2, 3]))));
        assert_eq!(icon_of(&bytes), Some(Icon::Bytes(vec![1, 2, 3])));
        assert_eq!(icon_of(&Value::from("x")), None);
    }

    #[test]
    fn markup() {
        assert_eq!(escape_markup("a<b> & c"), "a&lt;b&gt; &amp; c");
        assert_eq!(strip_markup("<b>bold</b> &amp; <i>x</i>"), "bold & x");
    }
}
