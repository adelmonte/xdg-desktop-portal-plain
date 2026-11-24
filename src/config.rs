//! The config file, and the settings it gives the Settings portal.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::Deserialize;
use zbus::zvariant::{OwnedValue, Structure, Value};

pub type Namespaces = HashMap<String, HashMap<String, OwnedValue>>;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    pub terminal: Vec<String>,
    pub appearance: Appearance,
    pub lockdown: Lockdown,
    pub settings: BTreeMap<String, BTreeMap<String, toml::Value>>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            terminal: [
                "kitty", "--class", "xdg-desktop-portal-plain",
                "-o", "remember_window_size=no",
                "-o", "initial_window_width=72c",
                "-o", "initial_window_height=20c",
            ].map(String::from).to_vec(),
            appearance: Appearance::default(),
            lockdown: Lockdown::default(),
            settings: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ColorScheme {
    #[default]
    Default,
    PreferDark,
    PreferLight,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Contrast {
    #[default]
    Normal,
    High,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ReducedMotion {
    #[default]
    NoPreference,
    Reduce,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Appearance {
    pub color_scheme: ColorScheme,
    /// `#rrggbb`
    pub accent_color: Option<String>,
    pub contrast: Contrast,
    pub reduced_motion: ReducedMotion,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Lockdown {
    pub disable_printing: bool,
    pub disable_save_to_disk: bool,
    pub disable_application_handlers: bool,
    pub disable_location: bool,
    pub disable_camera: bool,
    pub disable_microphone: bool,
    pub disable_sound_output: bool,
}

pub fn path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("xdg-desktop-portal-plain")
        .join("config.toml")
}

/// The config and its settings, or why they can't be read. A missing file is the defaults.
pub fn load() -> Result<(Config, Namespaces), String> {
    let path = path();
    let config = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<Config>(&text).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if config.terminal.is_empty() {
        return Err(format!("{}: terminal is empty", path.display()));
    }
    let settings = settings(&config).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((config, settings))
}

fn owned(v: Value<'_>) -> OwnedValue {
    v.try_to_owned().expect("no file descriptors in settings")
}

fn parse_color(s: &str) -> Option<(f64, f64, f64)> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let c = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok().map(|v| f64::from(v) / 255.0);
    Some((c(0)?, c(2)?, c(4)?))
}

/// A config value as a D-Bus value: strings, booleans, numbers (i32, else i64) and
/// lists of strings as themselves, or `{ u = 2 }` and the like for a set type.
pub fn to_value(v: &toml::Value) -> Result<OwnedValue, String> {
    use toml::Value as T;
    Ok(match v {
        T::String(s) => owned(Value::from(s.as_str())),
        T::Boolean(b) => owned(Value::from(*b)),
        T::Float(f) => owned(Value::from(*f)),
        T::Integer(i) => match i32::try_from(*i) {
            Ok(i) => owned(Value::from(i)),
            Err(_) => owned(Value::from(*i)),
        },
        T::Array(items) => {
            let strings: Option<Vec<&str>> = items.iter().map(|i| i.as_str()).collect();
            match strings {
                Some(s) => owned(Value::from(s)),
                None => return Err("lists can only hold strings".into()),
            }
        }
        T::Table(t) if t.len() == 1 => {
            let (ty, v) = t.iter().next().unwrap();
            let int = || v.as_integer().ok_or_else(|| format!("{{ {ty} = ... }} needs a whole number"));
            let range = |_| format!("{v} is out of range for {ty}");
            match ty.as_str() {
                "u" => owned(Value::from(u32::try_from(int()?).map_err(range)?)),
                "i" => owned(Value::from(i32::try_from(int()?).map_err(range)?)),
                "x" => owned(Value::from(int()?)),
                "t" => owned(Value::from(u64::try_from(int()?).map_err(range)?)),
                "y" => owned(Value::from(u8::try_from(int()?).map_err(range)?)),
                "d" => owned(Value::from(v.as_float().or(v.as_integer().map(|i| i as f64)).ok_or("{ d = ... } needs a number")?)),
                "s" => owned(Value::from(v.as_str().ok_or("{ s = ... } needs a string")?)),
                "b" => owned(Value::from(v.as_bool().ok_or("{ b = ... } needs true or false")?)),
                _ => return Err(format!("unknown type {ty} (u, i, x, t, y, d, s or b)")),
            }
        }
        _ => return Err(format!("can't use {v} as a setting")),
    })
}

/// What the Settings portal hands out: `[appearance]` as org.freedesktop.appearance
/// (its color scheme also as org.gnome.desktop.interface's), then `[settings]` as is.
pub fn settings(config: &Config) -> Result<Namespaces, String> {
    let a = &config.appearance;
    let mut out = Namespaces::new();
    let mut appearance = HashMap::new();
    let scheme: u32 = match a.color_scheme {
        ColorScheme::Default => 0,
        ColorScheme::PreferDark => 1,
        ColorScheme::PreferLight => 2,
    };
    appearance.insert("color-scheme".into(), owned(Value::from(scheme)));
    appearance.insert("contrast".into(), owned(Value::from(u32::from(a.contrast == Contrast::High))));
    appearance.insert("reduced-motion".into(), owned(Value::from(u32::from(a.reduced_motion == ReducedMotion::Reduce))));
    if let Some(c) = &a.accent_color {
        let rgb = parse_color(c).ok_or_else(|| format!("accent-color {c:?} isn't #rrggbb"))?;
        appearance.insert("accent-color".into(), owned(Value::from(Structure::from(rgb))));
    }
    out.insert("org.freedesktop.appearance".into(), appearance);

    let gnome_scheme = match a.color_scheme {
        ColorScheme::Default => "default",
        ColorScheme::PreferDark => "prefer-dark",
        ColorScheme::PreferLight => "prefer-light",
    };
    out.entry("org.gnome.desktop.interface".into()).or_default()
        .insert("color-scheme".into(), owned(Value::from(gnome_scheme)));

    for (ns, keys) in &config.settings {
        if ns == "org.freedesktop.appearance" {
            return Err("set org.freedesktop.appearance in [appearance]".into());
        }
        let entry = out.entry(ns.clone()).or_default();
        for (key, v) in keys {
            entry.insert(key.clone(), to_value(v).map_err(|e| format!("{ns}.{key}: {e}"))?);
        }
    }
    Ok(out)
}

/// Whether a namespace is one of those asked for: none asked is all of them, and
/// `org.gnome.*` asks for every namespace starting `org.gnome.`.
pub fn wanted(asked: &[String], ns: &str) -> bool {
    asked.is_empty() || asked.iter().any(|a| match a.strip_suffix('*') {
        _ if a.is_empty() => true,
        Some(prefix) => ns.starts_with(prefix),
        None => a == ns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn appearance() {
        let s = settings(&cfg("[appearance]\ncolor-scheme = \"prefer-dark\"\naccent-color = \"#ff0000\"\n")).unwrap();
        let a = &s["org.freedesktop.appearance"];
        assert_eq!(*a["color-scheme"], Value::U32(1));
        assert_eq!(*a["contrast"], Value::U32(0));
        assert_eq!(*a["accent-color"], Value::from(Structure::from((1.0, 0.0, 0.0))));
        assert_eq!(*s["org.gnome.desktop.interface"]["color-scheme"], Value::from("prefer-dark"));
    }

    #[test]
    fn typed_values() {
        let s = settings(&cfg(r#"
            [settings."org.gnome.desktop.interface"]
            gtk-theme = "Nordic"
            cursor-size = 30
            text-scaling-factor = 1.0
            scaling-factor = { u = 2 }
            enable-animations = false
            color-scheme = "prefer-light"
            [settings."org.gnome.desktop.wm.preferences"]
            workspace-names = ["a", "b"]
        "#)).unwrap();
        let i = &s["org.gnome.desktop.interface"];
        assert_eq!(*i["cursor-size"], Value::I32(30));
        assert_eq!(*i["scaling-factor"], Value::U32(2));
        assert_eq!(*i["text-scaling-factor"], Value::F64(1.0));
        assert_eq!(*i["enable-animations"], Value::Bool(false));
        // an explicit setting wins over the mirrored color scheme
        assert_eq!(*i["color-scheme"], Value::from("prefer-light"));
        assert_eq!(*s["org.gnome.desktop.wm.preferences"]["workspace-names"], Value::from(vec!["a", "b"]));
    }

    #[test]
    fn bad_values() {
        assert!(settings(&cfg("[appearance]\naccent-color = \"blue\"\n")).is_err());
        assert!(settings(&cfg("[settings.a]\nk = { u = -1 }\n")).is_err());
        assert!(settings(&cfg("[settings.a]\nk = [1, 2]\n")).is_err());
        assert!(toml::from_str::<Config>("colour = 1\n").is_err());
    }

    #[test]
    fn namespace_globs() {
        let asked = vec!["org.gnome.*".to_string()];
        assert!(wanted(&asked, "org.gnome.desktop.interface"));
        assert!(!wanted(&asked, "org.freedesktop.appearance"));
        assert!(wanted(&[], "anything"));
        assert!(wanted(&["".into()], "anything"));
        assert!(wanted(&["org.freedesktop.appearance".into()], "org.freedesktop.appearance"));
    }
}
