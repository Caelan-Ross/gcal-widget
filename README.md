# gcal-widget

A frameless, always-on-bottom desktop widget for **Windows 11** that displays a
self-hosted web calendar. Built with [Tauri v2](https://tauri.app/).

The widget is a thin native shell: it does **not** bundle a frontend. It points a
WebView2 window directly at the calendar served by its sister project (see below)
and adds desktop-widget behavior on top — no decorations, transparent background,
always-on-bottom, click-through toggle, tray icon, and launch-at-login. Calendar
refreshes are double-buffered: a hidden WebView loads the next generation every
five minutes, then replaces the visible one only after it is ready.

## Sister project: `gcal-embed`

This widget renders nothing of its own. It loads the UI served by **`gcal-embed`**,
a separate project — a vanilla HTML/CSS/JS calendar frontend plus an Express API
proxy that keeps the Google API key server-side. `gcal-embed` runs in Docker on the
LAN at:

```
http://192.168.1.130:8099
```

That URL is configured as **both** `build.frontendDist` and `build.devUrl` in
[`src-tauri/tauri.conf.json`](src-tauri/tauri.conf.json). If you move or re-host
`gcal-embed`, update that URL (and the `remote` scope in
[`src-tauri/capabilities/default.json`](src-tauri/capabilities/default.json), which
allows the page to call the window-drag API). **`gcal-embed` must be running and
reachable from this machine for the widget to show anything.**

## Prerequisites

- **Node.js** (v18+) and npm
- **Rust** toolchain via [rustup](https://rustup.rs/) — the default
  `x86_64-pc-windows-msvc` target
- **Visual Studio Build Tools 2022** with the *Desktop development with C++*
  workload (MSVC compiler + Windows SDK) — required to link the Rust backend
- **WebView2** runtime — ships with Windows 11

Install Rust + Build Tools via winget:

```powershell
winget install Rustlang.Rustup
winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

> **PATH note:** if you installed rustup in the current terminal session, `cargo`
> won't be on `PATH` until you open a **new** terminal. As a one-off you can prepend
> it: `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"`.

## Run (development)

```powershell
npm install
npm run tauri dev
```

The first build compiles the full Rust dependency tree (several minutes); later
runs are incremental. `tauri dev` watches `src-tauri/` and rebuilds on change.

## Build (release)

```powershell
npm run tauri build
```

Outputs under `src-tauri/target/release/`:

- **Standalone exe:** `gcal-widget.exe`
- **Installer:** `bundle/nsis/gcal-widget_<version>_x64-setup.exe`

## Usage

| Action | How |
| --- | --- |
| **Move the window** | Hold **Alt** and drag anywhere on the calendar |
| **Toggle glance / interactive** | **Ctrl+Alt+C** (glance = clicks pass through) |
| **Quit** | Right-click the tray icon → **Quit** |

The window starts **interactive** (clickable). Release builds register themselves
to **launch at login**; development runs leave any installed release entry alone.

> **Autostart points at the release exe that registered it.** After
> `npm run tauri build`, run the installed release exe once to enable the login
> entry (`HKCU\...\CurrentVersion\Run`). Debug runs never overwrite or disable it.

## Layout

| Path | Purpose |
| --- | --- |
| [`src-tauri/tauri.conf.json`](src-tauri/tauri.conf.json) | Window, bundle, and remote-URL config |
| [`src-tauri/src/lib.rs`](src-tauri/src/lib.rs) | App logic: buffered refresh, drag injection, shortcut toggle, tray, autostart |
| [`src-tauri/capabilities/default.json`](src-tauri/capabilities/default.json) | Permissions, incl. remote-origin access for the calendar URL |

The files under `src/` are leftover scaffold and are **not** used — the widget
loads the remote `gcal-embed` URL, not any local frontend.
