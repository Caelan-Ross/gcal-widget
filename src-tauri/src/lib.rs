use std::sync::atomic::{AtomicBool, Ordering};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::webview::PageLoadEvent;
use tauri::Manager;
use tauri_plugin_autostart::{ManagerExt, MacosLauncher};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

// true = glance mode (clicks pass through), false = interactive.
// Starts interactive (clickable) — toggle to glance mode with Ctrl+Alt+C.
static GLANCE: AtomicBool = AtomicBool::new(false);

// Injected into the remote page on every load so the frameless window can be
// moved. Alt+drag moves it; plain clicks still reach the calendar untouched.
const DRAG_SCRIPT: &str = r#"
(function () {
  if (window.__gcalDragInstalled) return;
  window.__gcalDragInstalled = true;
  document.addEventListener('mousedown', function (e) {
    if (e.button === 0 && e.altKey) {
      e.preventDefault();
      try {
        window.__TAURI__.window.getCurrentWindow().startDragging();
      } catch (err) {
        console.error('gcal-widget drag failed', err);
      }
    }
  }, true);
})();
"#;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Ctrl+Alt+C toggles interaction on/off
    let toggle = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyC);

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None, // no extra launch args
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        if let Some(win) = app.get_webview_window("gcal") {
                            let now = !GLANCE.load(Ordering::Relaxed);
                            let _ = win.set_ignore_cursor_events(now);
                            GLANCE.store(now, Ordering::Relaxed);
                        }
                    }
                })
                .build(),
        )
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let _ = webview.eval(DRAG_SCRIPT);
            }
        })
        .setup(move |app| {
            let win = app.get_webview_window("gcal").unwrap();
            let _ = win.set_ignore_cursor_events(false); // start interactive (clickable)

            // Non-fatal: another instance (e.g. an autostarted copy) may already
            // hold this shortcut. Don't abort startup over it.
            if let Err(e) = app.global_shortcut().register(toggle) {
                eprintln!("gcal-widget: couldn't register Ctrl+Alt+C toggle: {e}");
            }

            // Register the app to launch at login (idempotent — safe to call each start).
            let _ = app.autolaunch().enable();

            // Tray icon with Quit (the clean way out, since skipTaskbar hides it)
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("gcal-widget")
                .menu(&menu)
                .on_menu_event(|app, event| {
                    if event.id.as_ref() == "quit" {
                        app.exit(0);
                    }
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}