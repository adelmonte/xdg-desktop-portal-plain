//! Questions put to the user in a terminal window: the backend writes a prompt
//! file, runs itself with `--prompt FILE` in the terminal, and reads the reply.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use zbus::interface;
use zbus::object_server::ObjectServer;
use zbus::zvariant::OwnedObjectPath;

use crate::State;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub id: String,
    pub label: String,
}

/// A question asked alongside the main one: yes or no when it has no options.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Choice {
    pub id: String,
    pub label: String,
    pub options: Vec<Item>,
    pub initial: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub label: String,
    pub initial: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Prompt {
    pub title: String,
    pub subtitle: String,
    pub body: String,
    /// Pick one of these
    pub pick: Vec<Item>,
    pub pick_initial: usize,
    pub entry: Option<Entry>,
    pub choices: Vec<Choice>,
    /// End with grant or deny
    pub confirm: bool,
    pub grant_label: String,
    pub deny_label: String,
    pub answer: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Reply {
    pub granted: bool,
    pub picked: Option<String>,
    pub entry: Option<String>,
    pub choices: Vec<(String, String)>,
}

pub enum Outcome {
    Answered(Reply),
    Cancelled,
    Failed,
}

pub struct Prompter {
    pub state: Arc<State>,
    pub runtime: PathBuf,
    seq: AtomicU64,
}

/// The impl side of a portal Request: Close shuts the prompt's window.
struct Request {
    pid: Arc<Mutex<Option<u32>>>,
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl Request {
    async fn close(&self) {
        if let Some(pid) = *self.pid.lock().unwrap() {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGTERM);
        }
    }
}

impl Prompter {
    pub fn new(state: Arc<State>, runtime: PathBuf) -> Self {
        Prompter { state, runtime, seq: AtomicU64::new(0) }
    }

    pub async fn ask(&self, server: &ObjectServer, handle: &OwnedObjectPath, mut prompt: Prompt) -> Outcome {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let base = self.runtime.join(format!("{}-{n}", std::process::id()));
        let path = base.with_extension("prompt.toml");
        prompt.answer = base.with_extension("reply.toml");
        let _ = std::fs::remove_file(&prompt.answer);
        let written = toml::to_string(&prompt).map_err(|e| e.to_string())
            .and_then(|t| std::fs::write(&path, t).map_err(|e| format!("writing {}: {e}", path.display())));
        if let Err(e) = written {
            eprintln!("xdg-desktop-portal-plain: {e}");
            return Outcome::Failed;
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("xdg-desktop-portal-plain"));
        let terminal = self.state.config.read().unwrap().terminal.clone();

        let pid = Arc::new(Mutex::new(None));
        let _ = server.at(handle, Request { pid: pid.clone() }).await;
        let child = tokio::process::Command::new(&terminal[0])
            .args(&terminal[1..])
            .arg(&exe).arg("--prompt").arg(&path)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let ran = match child {
            Ok(mut child) => {
                *pid.lock().unwrap() = child.id();
                child.wait().await.is_ok()
            }
            Err(e) => {
                eprintln!("xdg-desktop-portal-plain: starting {}: {e}", terminal[0]);
                false
            }
        };
        let _ = server.remove::<Request, _>(handle).await;
        let _ = std::fs::remove_file(&path);
        let reply = std::fs::read_to_string(&prompt.answer).ok().and_then(|s| toml::from_str::<Reply>(&s).ok());
        let _ = std::fs::remove_file(&prompt.answer);
        match reply {
            Some(r) => Outcome::Answered(r),
            None if ran => Outcome::Cancelled,
            None => Outcome::Failed,
        }
    }
}

// The terminal side

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

struct Term<R, W> {
    input: R,
    out: W,
}

impl<R: BufRead, W: Write> Term<R, W> {
    /// A line of input, or None at end of input or on q (cancel).
    fn line(&mut self, ask: &str) -> Option<String> {
        write!(self.out, "{ask}").ok()?;
        self.out.flush().ok()?;
        let mut s = String::new();
        if self.input.read_line(&mut s).ok()? == 0 {
            return None;
        }
        let s = s.trim().to_string();
        (s != "q").then_some(s)
    }

    fn yes_no(&mut self, ask: &str, initial: bool) -> Option<bool> {
        let hint = if initial { "Y/n" } else { "y/N" };
        loop {
            match self.line(&format!("{ask} [{hint}] "))?.to_lowercase().as_str() {
                "" => return Some(initial),
                "y" | "yes" => return Some(true),
                "n" | "no" => return Some(false),
                _ => {}
            }
        }
    }

    fn pick(&mut self, items: &[Item], initial: usize) -> Option<usize> {
        let initial = initial.min(items.len().saturating_sub(1));
        for (i, item) in items.iter().enumerate() {
            let mark = if i == initial { "*" } else { " " };
            writeln!(self.out, "  {mark}{:>2}) {}", i + 1, item.label).ok()?;
        }
        loop {
            let s = self.line(&format!("Choose [{}] ", initial + 1))?;
            if s.is_empty() {
                return Some(initial);
            }
            if let Ok(n) = s.parse::<usize>()
                && (1..=items.len()).contains(&n)
            {
                return Some(n - 1);
            }
        }
    }

    fn run(&mut self, p: &Prompt) -> Option<Reply> {
        let mut reply = Reply::default();
        writeln!(self.out, "\n{BOLD}{}{RESET}", p.title).ok()?;
        if !p.subtitle.is_empty() {
            writeln!(self.out, "{}", p.subtitle).ok()?;
        }
        if !p.body.is_empty() {
            writeln!(self.out, "{DIM}{}{RESET}", p.body).ok()?;
        }
        writeln!(self.out, "{DIM}(q, or closing the window, cancels){RESET}\n").ok()?;
        if !p.pick.is_empty() {
            let i = self.pick(&p.pick, p.pick_initial)?;
            reply.picked = Some(p.pick[i].id.clone());
        }
        if let Some(e) = &p.entry {
            let s = self.line(&format!("{} [{}] ", e.label, e.initial))?;
            reply.entry = Some(if s.is_empty() { e.initial.clone() } else { s });
        }
        for c in &p.choices {
            if c.options.is_empty() {
                let yes = self.yes_no(&c.label, c.initial == "true")?;
                reply.choices.push((c.id.clone(), yes.to_string()));
            } else {
                writeln!(self.out, "{}", c.label).ok()?;
                let initial = c.options.iter().position(|o| o.id == c.initial).unwrap_or(0);
                let i = self.pick(&c.options, initial)?;
                reply.choices.push((c.id.clone(), c.options[i].id.clone()));
            }
        }
        reply.granted = if p.confirm {
            let grant = if p.grant_label.is_empty() { "Allow" } else { &p.grant_label };
            let deny = if p.deny_label.is_empty() { "Deny" } else { &p.deny_label };
            self.yes_no(&format!("{grant}? (no: {deny})"), false)?
        } else {
            true
        };
        Some(reply)
    }
}

/// `--prompt FILE`: ask in this terminal, and write the reply where the prompt says.
pub fn run(path: &Path) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let prompt: Prompt = toml::from_str(&text).map_err(std::io::Error::other)?;
    let stdin = std::io::stdin();
    let mut term = Term { input: stdin.lock(), out: std::io::stdout() };
    if let Some(reply) = term.run(&prompt) {
        let text = toml::to_string(&reply).map_err(std::io::Error::other)?;
        let tmp = prompt.answer.with_extension("partial");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, &prompt.answer)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_with(p: &Prompt, input: &str) -> Option<Reply> {
        let mut out = Vec::new();
        Term { input: input.as_bytes(), out: &mut out }.run(p)
    }

    #[test]
    fn confirm_defaults_to_deny() {
        let p = Prompt { title: "t".into(), confirm: true, ..Default::default() };
        assert!(!run_with(&p, "\n").unwrap().granted);
        assert!(run_with(&p, "y\n").unwrap().granted);
        assert!(run_with(&p, "").is_none());
        assert!(run_with(&p, "q\n").is_none());
    }

    #[test]
    fn pick_entry_and_choices() {
        let items = |ids: &[&str]| ids.iter().map(|i| Item { id: i.to_string(), label: i.to_uppercase() }).collect::<Vec<_>>();
        let p = Prompt {
            pick: items(&["a", "b", "c"]),
            pick_initial: 1,
            entry: Some(Entry { label: "Name".into(), initial: "x".into() }),
            choices: vec![
                Choice { id: "remember".into(), label: "Remember".into(), options: vec![], initial: "true".into() },
                Choice { id: "q".into(), label: "Quality".into(), options: items(&["lo", "hi"]), initial: "hi".into() },
            ],
            ..Default::default()
        };
        let r = run_with(&p, "\n\nn\n1\n").unwrap();
        assert_eq!(r.picked.as_deref(), Some("b"));
        assert_eq!(r.entry.as_deref(), Some("x"));
        assert_eq!(r.choices, vec![("remember".into(), "false".into()), ("q".into(), "lo".into())]);
        assert!(r.granted);
        let r = run_with(&p, "9\n3\nmine\n\n\n").unwrap();
        assert_eq!(r.picked.as_deref(), Some("c"));
        assert_eq!(r.entry.as_deref(), Some("mine"));
        assert_eq!(r.choices, vec![("remember".into(), "true".into()), ("q".into(), "hi".into())]);
    }
}
