//! The Inhibit portal: idle through org.freedesktop.ScreenSaver (niri serves it),
//! suspend through logind. Nothing manages logout or user switching on niri.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::OnceCell;
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::zvariant::{ObjectPath, OwnedFd, OwnedObjectPath, Value};
use zbus::{Connection, interface, proxy};

use crate::{Options, opt};

const SUSPEND: u32 = 4;
const IDLE: u32 = 8;

#[proxy(
    interface = "org.freedesktop.ScreenSaver",
    default_service = "org.freedesktop.ScreenSaver",
    default_path = "/org/freedesktop/ScreenSaver"
)]
trait ScreenSaver {
    fn inhibit(&self, application_name: &str, reason_for_inhibit: &str) -> zbus::Result<u32>;
    #[zbus(name = "UnInhibit")]
    fn un_inhibit(&self, cookie: u32) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Login {
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;
}

#[derive(Default)]
struct Held {
    cookie: Option<u32>,
    /// logind's inhibitor lasts until this closes
    _sleep: Option<OwnedFd>,
}

/// One inhibition, ended by the portal closing its Request.
struct InhibitRequest {
    held: Mutex<Option<Held>>,
    path: OwnedObjectPath,
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl InhibitRequest {
    async fn close(&self, #[zbus(connection)] conn: &Connection) {
        let held = self.held.lock().unwrap().take();
        if let Some(Held { cookie: Some(cookie), .. }) = held
            && let Ok(ss) = ScreenSaverProxy::new(conn).await
            && let Err(e) = ss.un_inhibit(cookie).await
        {
            eprintln!("xdg-desktop-portal-plain: ending an idle inhibit: {e}");
        }
        let conn = conn.clone();
        let path = self.path.clone();
        tokio::spawn(async move {
            let _ = conn.object_server().remove::<InhibitRequest, _>(&path).await;
        });
    }
}

/// A monitor's session, ended by the portal closing it.
struct Session {
    path: OwnedObjectPath,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl Session {
    async fn close(&self, #[zbus(connection)] conn: &Connection) {
        let conn = conn.clone();
        let path = self.path.clone();
        tokio::spawn(async move {
            let _ = conn.object_server().remove::<Session, _>(&path).await;
        });
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

#[derive(Default)]
pub struct Inhibit {
    system: OnceCell<Connection>,
}

impl Inhibit {
    async fn system(&self) -> zbus::Result<&Connection> {
        self.system.get_or_try_init(Connection::system).await
    }

    async fn hold(&self, conn: &Connection, app: &str, reason: &str, flags: u32) -> Held {
        let mut held = Held::default();
        if flags & IDLE != 0 {
            match ScreenSaverProxy::new(conn).await {
                Ok(ss) => match ss.inhibit(app, reason).await {
                    Ok(c) => held.cookie = Some(c),
                    Err(e) => eprintln!("xdg-desktop-portal-plain: idle inhibit: {e}"),
                },
                Err(e) => eprintln!("xdg-desktop-portal-plain: idle inhibit: {e}"),
            }
        }
        if flags & SUSPEND != 0 {
            let fd = async {
                let login = LoginProxy::new(self.system().await?).await?;
                login.inhibit("sleep", app, reason, "block").await
            };
            match fd.await {
                Ok(fd) => held._sleep = Some(fd),
                Err(e) => eprintln!("xdg-desktop-portal-plain: suspend inhibit: {e}"),
            }
        }
        held
    }
}

#[interface(name = "org.freedesktop.impl.portal.Inhibit")]
impl Inhibit {
    #[allow(clippy::too_many_arguments)]
    async fn inhibit(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        _window: String,
        flags: u32,
        options: Options,
    ) {
        let reason: String = opt(&options, "reason").unwrap_or_default();
        let app = if app_id.is_empty() { "unknown" } else { &app_id };
        let held = self.hold(conn, app, &reason, flags).await;
        let req = InhibitRequest { held: Mutex::new(Some(held)), path: handle.clone() };
        if let Err(e) = server.at(&handle, req).await {
            eprintln!("xdg-desktop-portal-plain: inhibit request: {e}");
        }
    }

    async fn create_monitor(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _window: String,
    ) -> u32 {
        if server.at(&session_handle, Session { path: session_handle.clone() }).await.is_err() {
            return 2;
        }
        // The first state goes out once the portal has the reply
        let conn = conn.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let Ok(emitter) = SignalEmitter::new(&conn, crate::PATH) else { return };
            let mut state = HashMap::new();
            state.insert("screensaver-active", Value::Bool(false));
            state.insert("session-state", Value::U32(1));
            let _ = Inhibit::state_changed(&emitter, session_handle.as_ref(), state).await;
        });
        0
    }

    async fn query_end_response(&self, _session_handle: OwnedObjectPath) {}

    #[zbus(signal)]
    async fn state_changed(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, state: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}
