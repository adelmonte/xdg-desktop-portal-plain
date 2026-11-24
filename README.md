# xdg-desktop-portal-plain

A toolkit-free [xdg-desktop-portal](https://flatpak.github.io/xdg-desktop-portal/) backend
for niri, in pure Rust: everything xdg-desktop-portal-gtk does on a niri desktop except
file dialogs (pair it with one such as [xdg-desktop-portal-tui](https://github.com/adelmonte/xdg-desktop-portal-tui)) and printing,
without GTK, plus screen sharing.

- **Settings**: dark mode, accent color, contrast and reduced motion, plus any other
  setting (GTK theme, icons, cursor, fonts for sandboxed apps), from a config file that's
  reread as soon as it's saved.
- **AppChooser** (what OpenURI uses to open files and links from Flatpak apps): your
  `mimeapps.list` default, else the app picked last time, else the only one; otherwise
  it asks in a terminal.
- **Notification**: passed on to the notification server, with button clicks passed back.
- **Inhibit**: idle through `org.freedesktop.ScreenSaver` (niri serves it), suspend
  through logind.
- **Access**, **Account**, **DynamicLauncher**: their questions are asked in a terminal
  window (kitty).
- **ScreenCast**: share a screen or a window (including niri's dynamic cast target),
  picked in a terminal; niri makes the PipeWire stream through its
  `org.gnome.Mutter.ScreenCast` service. Apps that ask can be remembered, so they
  don't ask again.
- **Screenshot** and **PickColor**: through niri's `org.gnome.Shell.Screenshot`.
- **Email**: a `mailto:` link opened with `xdg-open`.
- **Lockdown**: from the config file.

## Install

Arch Linux: `cd packaging && makepkg -si`.

From source:

```sh
cargo build --release
sudo install -Dm755 target/release/xdg-desktop-portal-plain /usr/lib/xdg-desktop-portal-plain
sudo install -Dm644 data/plain.portal /usr/share/xdg-desktop-portal/portals/plain.portal
sudo install -Dm644 data/org.freedesktop.impl.portal.desktop.plain.service \
    /usr/share/dbus-1/services/org.freedesktop.impl.portal.desktop.plain.service
install -Dm644 data/config.toml ~/.config/xdg-desktop-portal-plain/config.toml
```

In `~/.config/xdg-desktop-portal/portals.conf`:

```ini
[preferred]
default=tui;plain
```

Then restart the portal: `pkill -f '^/usr/lib/xdg-desktop-portal$'`.

The questions' window has the app id `xdg-desktop-portal-plain`. To float it in niri:

```kdl
window-rule {
    match app-id="xdg-desktop-portal-plain"
    open-floating true
}
```

## Config

`~/.config/xdg-desktop-portal-plain/config.toml`; see `data/config.toml`.
`xdg-desktop-portal-plain --check` checks it and prints the settings it gives.

## License

GPL-3.0-or-later
