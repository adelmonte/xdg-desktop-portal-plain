//! Settings, Lockdown, AppChooser, Access, Account, DynamicLauncher and Email.

use std::sync::Arc;

use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::interface;

use crate::config::{self, Namespaces};
use crate::prompt::{Choice, Entry, Item, Outcome, Prompt, Prompter};
use crate::{Options, State, desktop, opt, put};

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.portal.Error")]
pub enum PortalError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotFound(String),
}

/// Who is asking, for a prompt's title.
fn asker(app_id: &str) -> String {
    if app_id.is_empty() { "A program".into() } else { desktop::label(app_id) }
}

pub struct Settings {
    pub state: Arc<State>,
}

#[interface(name = "org.freedesktop.impl.portal.Settings")]
impl Settings {
    async fn read_all(&self, namespaces: Vec<String>) -> Namespaces {
        let settings = self.state.settings.read().unwrap();
        settings.iter()
            .filter(|(ns, _)| config::wanted(&namespaces, ns))
            .map(|(ns, keys)| {
                let keys = keys.iter().filter_map(|(k, v)| Some((k.clone(), v.try_clone().ok()?))).collect();
                (ns.clone(), keys)
            })
            .collect()
    }

    async fn read(&self, namespace: String, key: String) -> Result<OwnedValue, PortalError> {
        let settings = self.state.settings.read().unwrap();
        settings.get(&namespace).and_then(|keys| keys.get(&key)).and_then(|v| v.try_clone().ok())
            .ok_or_else(|| PortalError::NotFound(format!("Requested setting {namespace}.{key} not found")))
    }

    #[zbus(signal)]
    pub async fn setting_changed(emitter: &SignalEmitter<'_>, namespace: &str, key: &str, value: Value<'_>) -> zbus::Result<()>;

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

pub struct Lockdown {
    pub state: Arc<State>,
}

impl Lockdown {
    fn get(&self) -> config::Lockdown {
        self.state.config.read().unwrap().lockdown
    }
}

#[interface(name = "org.freedesktop.impl.portal.Lockdown")]
impl Lockdown {
    #[zbus(property, name = "disable-printing")]
    fn disable_printing(&self) -> bool { self.get().disable_printing }
    #[zbus(property, name = "disable-save-to-disk")]
    fn disable_save_to_disk(&self) -> bool { self.get().disable_save_to_disk }
    #[zbus(property, name = "disable-application-handlers")]
    fn disable_application_handlers(&self) -> bool { self.get().disable_application_handlers }
    #[zbus(property, name = "disable-location")]
    fn disable_location(&self) -> bool { self.get().disable_location }
    #[zbus(property, name = "disable-camera")]
    fn disable_camera(&self) -> bool { self.get().disable_camera }
    #[zbus(property, name = "disable-microphone")]
    fn disable_microphone(&self) -> bool { self.get().disable_microphone }
    #[zbus(property, name = "disable-sound-output")]
    fn disable_sound_output(&self) -> bool { self.get().disable_sound_output }
}

/// Tell listeners about the lockdown properties after a reload.
pub async fn lockdown_changed(server: &ObjectServer) -> zbus::Result<()> {
    let iface = server.interface::<_, Lockdown>(crate::PATH).await?;
    let e = iface.signal_emitter();
    let l = iface.get().await;
    l.disable_printing_changed(e).await?;
    l.disable_save_to_disk_changed(e).await?;
    l.disable_application_handlers_changed(e).await?;
    l.disable_location_changed(e).await?;
    l.disable_camera_changed(e).await?;
    l.disable_microphone_changed(e).await?;
    l.disable_sound_output_changed(e).await
}

/// Which app to open something with: the mimeapps.list default, else the one picked
/// last time, else the only one; asks in a terminal when none of those settle it.
pub fn settled_choice(choices: &[String], defaults: &[String], last: Option<&str>) -> Option<String> {
    defaults.iter().find(|d| choices.contains(d)).cloned()
        .or_else(|| last.filter(|l| choices.iter().any(|c| c == l)).map(String::from))
        .or_else(|| (choices.len() == 1).then(|| choices[0].clone()))
}

pub struct AppChooser {
    pub prompter: Arc<Prompter>,
}

#[interface(name = "org.freedesktop.impl.portal.AppChooser")]
impl AppChooser {
    #[zbus(out_args("response", "results"))]
    async fn choose_application(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        choices: Vec<String>,
        options: Options,
    ) -> (u32, Options) {
        if choices.is_empty() {
            return (2, Options::new());
        }
        let content_type: String = opt(&options, "content_type").unwrap_or_default();
        let last: Option<String> = opt(&options, "last_choice");
        let defaults = if content_type.is_empty() { Vec::new() } else { desktop::defaults(&content_type) };
        let mut results = Options::new();
        if let Some(choice) = settled_choice(&choices, &defaults, last.as_deref()) {
            put(&mut results, "choice", choice);
            return (0, results);
        }

        let what = opt::<String>(&options, "filename").filter(|s| !s.is_empty())
            .or_else(|| opt::<String>(&options, "uri").filter(|s| !s.is_empty()))
            .unwrap_or_else(|| content_type.clone());
        let prompt = Prompt {
            title: format!("Open {what} with"),
            subtitle: content_type,
            pick: choices.iter().map(|c| Item { id: c.clone(), label: desktop::label(c) }).collect(),
            pick_initial: last.and_then(|l| choices.iter().position(|c| *c == l)).unwrap_or(0),
            ..Default::default()
        };
        match self.prompter.ask(server, &handle, prompt).await {
            Outcome::Answered(r) => match r.picked {
                Some(choice) => {
                    put(&mut results, "choice", choice);
                    (0, results)
                }
                None => (1, results),
            },
            Outcome::Cancelled => (1, results),
            Outcome::Failed => (2, results),
        }
    }

    async fn update_choices(&self, _handle: OwnedObjectPath, _choices: Vec<String>) {}
}

pub struct Access {
    pub prompter: Arc<Prompter>,
}

#[interface(name = "org.freedesktop.impl.portal.Access")]
impl Access {
    #[zbus(out_args("response", "results"))]
    #[allow(clippy::too_many_arguments)]
    async fn access_dialog(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        title: String,
        subtitle: String,
        body: String,
        options: Options,
    ) -> (u32, Options) {
        type PortalChoice = (String, String, Vec<(String, String)>, String);
        let choices: Vec<PortalChoice> = opt(&options, "choices").unwrap_or_default();
        let title = if app_id.is_empty() { title } else { format!("{title}  ({})", asker(&app_id)) };
        let prompt = Prompt {
            title,
            subtitle,
            body,
            choices: choices.into_iter().map(|(id, label, options, initial)| Choice {
                id,
                label,
                options: options.into_iter().map(|(id, label)| Item { id, label }).collect(),
                initial,
            }).collect(),
            confirm: true,
            grant_label: opt(&options, "grant_label").unwrap_or_default(),
            deny_label: opt(&options, "deny_label").unwrap_or_default(),
            ..Default::default()
        };
        let mut results = Options::new();
        match self.prompter.ask(server, &handle, prompt).await {
            Outcome::Answered(r) => {
                put(&mut results, "choices", r.choices);
                (if r.granted { 0 } else { 1 }, results)
            }
            Outcome::Cancelled => (1, results),
            Outcome::Failed => (2, results),
        }
    }
}

pub struct Account {
    pub prompter: Arc<Prompter>,
}

#[interface(name = "org.freedesktop.impl.portal.Account")]
impl Account {
    #[zbus(out_args("response", "results"))]
    async fn get_user_information(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        options: Options,
    ) -> (u32, Options) {
        let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::getuid()) else {
            return (2, Options::new());
        };
        let gecos = user.gecos.to_string_lossy();
        let real = gecos.split(',').next().unwrap_or("").trim().to_string();
        let face = user.dir.join(".face");
        let image = face.is_file().then(|| crate::file_uri(&face));

        let mut body = format!("Username: {}\nName: {}", user.name, if real.is_empty() { "(none)" } else { &real });
        if image.is_some() {
            body.push_str(&format!("\nPicture: {}", face.display()));
        }
        let prompt = Prompt {
            title: format!("{} wants your name{}", asker(&app_id), if image.is_some() { " and picture" } else { "" }),
            subtitle: opt::<String>(&options, "reason").map(|r| format!("Reason: {r}")).unwrap_or_default(),
            body,
            confirm: true,
            grant_label: "Share".into(),
            deny_label: "Don't share".into(),
            ..Default::default()
        };
        let mut results = Options::new();
        match self.prompter.ask(server, &handle, prompt).await {
            Outcome::Answered(r) if r.granted => {
                put(&mut results, "id", user.name);
                put(&mut results, "name", real);
                put(&mut results, "image", image.unwrap_or_default());
                (0, results)
            }
            Outcome::Answered(_) | Outcome::Cancelled => (1, results),
            Outcome::Failed => (2, results),
        }
    }
}

pub struct DynamicLauncher {
    pub prompter: Arc<Prompter>,
}

#[interface(name = "org.freedesktop.impl.portal.DynamicLauncher")]
impl DynamicLauncher {
    #[zbus(out_args("response", "results"))]
    #[allow(clippy::too_many_arguments)]
    async fn prepare_install(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        name: String,
        icon: OwnedValue,
        options: Options,
    ) -> (u32, Options) {
        let editable = opt::<bool>(&options, "editable_name").unwrap_or(true);
        let kind = if opt::<u32>(&options, "launcher_type") == Some(2) { "web app" } else { "app" };
        let prompt = Prompt {
            title: format!("{} wants to add a launcher for the {kind} {name:?}", asker(&app_id)),
            subtitle: opt::<String>(&options, "target").map(|t| format!("Opens: {t}")).unwrap_or_default(),
            entry: editable.then(|| Entry { label: "Name".into(), initial: name.clone() }),
            confirm: true,
            grant_label: "Add it".into(),
            deny_label: "Don't".into(),
            ..Default::default()
        };
        let mut results = Options::new();
        match self.prompter.ask(server, &handle, prompt).await {
            Outcome::Answered(r) if r.granted => {
                put(&mut results, "name", r.entry.filter(|n| !n.trim().is_empty()).unwrap_or(name));
                results.insert("icon".into(), icon);
                (0, results)
            }
            Outcome::Answered(_) | Outcome::Cancelled => (1, results),
            Outcome::Failed => (2, results),
        }
    }

    /// Installing without asking is never allowed.
    async fn request_install_token(&self, _app_id: String, _options: Options) -> u32 {
        2
    }

    #[zbus(property)]
    fn supported_launcher_types(&self) -> u32 {
        3
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

fn mailto_escape(s: &str, keep: &[u8]) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || keep.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn mailto(options: &Options) -> String {
    let mut to: Vec<String> = opt::<String>(options, "address").into_iter().collect();
    to.extend(opt::<Vec<String>>(options, "addresses").unwrap_or_default());
    let to: Vec<String> = to.iter().filter(|a| !a.is_empty()).map(|a| mailto_escape(a, b"@+")).collect();
    let mut query = Vec::new();
    for key in ["cc", "bcc"] {
        let list = opt::<Vec<String>>(options, key).unwrap_or_default();
        if !list.is_empty() {
            let list: Vec<String> = list.iter().map(|a| mailto_escape(a, b"@+")).collect();
            query.push(format!("{key}={}", list.join(",")));
        }
    }
    for key in ["subject", "body"] {
        if let Some(v) = opt::<String>(options, key).filter(|v| !v.is_empty()) {
            query.push(format!("{key}={}", mailto_escape(&v, b"")));
        }
    }
    for a in opt::<Vec<String>>(options, "attachments").unwrap_or_default() {
        query.push(format!("attach={}", mailto_escape(&a, b"/")));
    }
    let mut uri = format!("mailto:{}", to.join(","));
    if !query.is_empty() {
        uri.push('?');
        uri.push_str(&query.join("&"));
    }
    uri
}

pub struct Email;

#[interface(name = "org.freedesktop.impl.portal.Email")]
impl Email {
    #[zbus(out_args("response", "results"))]
    async fn compose_email(&self, _handle: OwnedObjectPath, _app_id: String, _parent_window: String, options: Options) -> (u32, Options) {
        let uri = mailto(&options);
        let status = tokio::process::Command::new("xdg-open").arg(&uri)
            .stdin(std::process::Stdio::null())
            .status().await;
        match status {
            Ok(s) if s.success() => (0, Options::new()),
            Ok(s) => {
                eprintln!("xdg-desktop-portal-plain: xdg-open {uri}: {s}");
                (2, Options::new())
            }
            Err(e) => {
                eprintln!("xdg-desktop-portal-plain: xdg-open: {e}");
                (2, Options::new())
            }
        }
    }
}

/// Every setting that's new or different in `new`, to signal after a reload.
pub fn changed<'a>(old: &Namespaces, new: &'a Namespaces) -> Vec<(&'a str, &'a str, &'a OwnedValue)> {
    let mut out = Vec::new();
    for (ns, keys) in new {
        for (k, v) in keys {
            let before = old.get(ns).and_then(|o| o.get(k));
            if before.is_none_or(|b| **b != **v) {
                out.push((ns.as_str(), k.as_str(), v));
            }
        }
    }
    out.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choosing() {
        let c = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let choices = c(&["firefox", "zathura", "nvim"]);
        assert_eq!(settled_choice(&choices, &c(&["evince", "zathura"]), Some("nvim")).as_deref(), Some("zathura"));
        assert_eq!(settled_choice(&choices, &c(&["evince"]), Some("nvim")).as_deref(), Some("nvim"));
        assert_eq!(settled_choice(&choices, &[], Some("gone")), None);
        assert_eq!(settled_choice(&c(&["only"]), &[], None).as_deref(), Some("only"));
    }

    #[test]
    fn mailto_uri() {
        let mut o = Options::new();
        put(&mut o, "addresses", vec!["a@b.org".to_string(), "c+d@e.org".to_string()]);
        put(&mut o, "cc", vec!["x@y.org".to_string()]);
        put(&mut o, "subject", "Hi & bye".to_string());
        put(&mut o, "attachments", vec!["/tmp/a b.pdf".to_string()]);
        assert_eq!(mailto(&o), "mailto:a@b.org,c+d@e.org?cc=x@y.org&subject=Hi%20%26%20bye&attach=/tmp/a%20b.pdf");
    }

    #[test]
    fn changes() {
        let a = config::settings(&toml::from_str("[appearance]\ncolor-scheme = \"prefer-dark\"\n").unwrap()).unwrap();
        let b = config::settings(&toml::from_str("[appearance]\ncolor-scheme = \"prefer-light\"\n").unwrap()).unwrap();
        let keys: Vec<(&str, &str)> = changed(&a, &b).iter().map(|(n, k, _)| (*n, *k)).collect();
        assert_eq!(keys, vec![("org.freedesktop.appearance", "color-scheme"), ("org.gnome.desktop.interface", "color-scheme")]);
        assert!(changed(&a, &a).is_empty());
    }
}
