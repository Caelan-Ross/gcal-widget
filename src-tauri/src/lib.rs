use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::webview::PageLoadEvent;
use tauri::{Manager, Url};
use tauri_plugin_autostart::MacosLauncher;
#[cfg(not(debug_assertions))]
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout_at, Instant};

// true = glance mode (clicks pass through), false = interactive.
// Starts interactive (clickable) — toggle to glance mode with Ctrl+Alt+C.
static GLANCE: AtomicBool = AtomicBool::new(false);

const REFRESH_PARAMETER: &str = "__gcal_refresh";
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
const COMPOSITOR_SETTLE_TIME: Duration = Duration::from_millis(100);
const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
    Duration::from_secs(300),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowSlot {
    Primary,
    Buffer,
}

impl WindowSlot {
    fn label(self) -> &'static str {
        match self {
            Self::Primary => "gcal",
            Self::Buffer => "gcal-buffer",
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Primary => Self::Buffer,
            Self::Buffer => Self::Primary,
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        match label {
            "gcal" => Some(Self::Primary),
            "gcal-buffer" => Some(Self::Buffer),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RefreshTarget {
    window: WindowSlot,
    token: u64,
    retry_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RefreshEvent {
    window: WindowSlot,
    token: u64,
}

struct RefreshEvents(mpsc::UnboundedSender<RefreshEvent>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptOutcome {
    Success,
    NavigationFailed,
    TimedOut,
    HandoffFailed,
}

#[derive(Debug)]
struct RefreshState {
    active: WindowSlot,
    pending: Option<RefreshTarget>,
    next_token: u64,
    retry_count: usize,
}

impl RefreshState {
    fn new() -> Self {
        Self {
            active: WindowSlot::Primary,
            pending: None,
            next_token: 0,
            retry_count: 0,
        }
    }

    fn begin(&mut self) -> RefreshTarget {
        self.next_token += 1;
        let target = RefreshTarget {
            window: self.active.other(),
            token: self.next_token,
            retry_count: self.retry_count,
        };
        self.pending = Some(target);
        target
    }

    fn matches(&self, event: RefreshEvent) -> bool {
        self.pending
            .is_some_and(|pending| pending.window == event.window && pending.token == event.token)
    }

    fn finish(&mut self, target: RefreshTarget, outcome: AttemptOutcome) -> Duration {
        if self.pending != Some(target) {
            return self.retry_delay();
        }

        self.pending = None;
        if outcome == AttemptOutcome::Success {
            self.active = target.window;
            self.retry_count = 0;
            REFRESH_INTERVAL
        } else {
            self.retry_count = self.retry_count.saturating_add(1);
            self.retry_delay()
        }
    }

    fn retry_delay(&self) -> Duration {
        RETRY_DELAYS[self
            .retry_count
            .saturating_sub(1)
            .min(RETRY_DELAYS.len() - 1)]
    }
}

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

fn refresh_url(base: &Url, token: u64) -> Url {
    let retained_pairs: Vec<(String, String)> = base
        .query_pairs()
        .filter(|(key, _)| key != REFRESH_PARAMETER)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let mut url = base.clone();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        query.extend_pairs(retained_pairs);
        query.append_pair(REFRESH_PARAMETER, &token.to_string());
    }
    url
}

fn refresh_token(url: &Url) -> Option<u64> {
    url.query_pairs()
        .find(|(key, _)| key == REFRESH_PARAMETER)
        .and_then(|(_, value)| value.parse().ok())
}

async fn handoff(app: &tauri::AppHandle, target: RefreshTarget) -> Result<(), String> {
    let old_window = app
        .get_webview_window(target.window.other().label())
        .ok_or_else(|| "visible calendar window is missing".to_owned())?;
    let new_window = app
        .get_webview_window(target.window.label())
        .ok_or_else(|| "refresh buffer window is missing".to_owned())?;

    let result = (|| -> Result<(), String> {
        let position = old_window
            .outer_position()
            .map_err(|error| format!("couldn't read visible window position: {error}"))?;
        new_window
            .set_position(position)
            .map_err(|error| format!("couldn't synchronize buffer position: {error}"))?;

        let size = old_window
            .inner_size()
            .map_err(|error| format!("couldn't read visible window size: {error}"))?;
        new_window
            .set_size(size)
            .map_err(|error| format!("couldn't synchronize buffer size: {error}"))?;
        new_window
            .set_ignore_cursor_events(GLANCE.load(Ordering::Relaxed))
            .map_err(|error| format!("couldn't synchronize glance mode: {error}"))?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = new_window.hide();
        let _ = old_window.show();
        return Err(error);
    }

    // PageLoadEvent::Finished can precede the first WebView compositor frame.
    sleep(COMPOSITOR_SETTLE_TIME).await;
    if let Err(error) = new_window.show() {
        let _ = new_window.hide();
        let _ = old_window.show();
        return Err(format!("couldn't show refreshed window: {error}"));
    }

    // Keep the old fully painted window beneath the new one during handoff.
    sleep(COMPOSITOR_SETTLE_TIME).await;
    if let Err(error) = old_window.hide() {
        let _ = new_window.hide();
        let _ = old_window.show();
        return Err(format!("couldn't hide previous window: {error}"));
    }

    Ok(())
}

async fn perform_refresh(
    app: &tauri::AppHandle,
    state: &RefreshState,
    target: RefreshTarget,
    events: &mut mpsc::UnboundedReceiver<RefreshEvent>,
    calendar_url: &Url,
) -> AttemptOutcome {
    let Some(buffer_window) = app.get_webview_window(target.window.label()) else {
        eprintln!("gcal-widget: refresh buffer window is missing");
        return AttemptOutcome::NavigationFailed;
    };

    if let Err(error) = buffer_window.navigate(refresh_url(calendar_url, target.token)) {
        eprintln!("gcal-widget: couldn't navigate refresh buffer: {error}");
        return AttemptOutcome::NavigationFailed;
    }

    let deadline = Instant::now() + LOAD_TIMEOUT;
    loop {
        match timeout_at(deadline, events.recv()).await {
            Ok(Some(event)) if state.matches(event) => break,
            Ok(Some(_stale_event)) => continue,
            Ok(None) => {
                eprintln!("gcal-widget: refresh event channel closed");
                return AttemptOutcome::NavigationFailed;
            }
            Err(_) => {
                eprintln!(
                    "gcal-widget: background refresh timed out (attempt {})",
                    target.retry_count + 1
                );
                return AttemptOutcome::TimedOut;
            }
        }
    }

    match handoff(app, target).await {
        Ok(()) => AttemptOutcome::Success,
        Err(error) => {
            eprintln!("gcal-widget: refresh handoff failed: {error}");
            AttemptOutcome::HandoffFailed
        }
    }
}

async fn refresh_coordinator(
    app: tauri::AppHandle,
    mut events: mpsc::UnboundedReceiver<RefreshEvent>,
    calendar_url: Url,
) {
    let mut state = RefreshState::new();
    let mut delay = REFRESH_INTERVAL;

    loop {
        sleep(delay).await;
        if app.get_webview_window(state.active.label()).is_none() {
            return;
        }

        let target = state.begin();
        let outcome = perform_refresh(&app, &state, target, &mut events, &calendar_url).await;
        delay = state.finish(target, outcome);
    }
}

fn setup_error(message: &str, error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{message}: {error}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let toggle = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyC);
    let (refresh_tx, refresh_rx) = mpsc::unbounded_channel();

    tauri::Builder::default()
        // This must be the first plugin so a second launch exits before setup.
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {}))
        .manage(RefreshEvents(refresh_tx))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        let now = !GLANCE.load(Ordering::Relaxed);
                        for slot in [WindowSlot::Primary, WindowSlot::Buffer] {
                            if let Some(window) = app.get_webview_window(slot.label()) {
                                let _ = window.set_ignore_cursor_events(now);
                            }
                        }
                        GLANCE.store(now, Ordering::Relaxed);
                    }
                })
                .build(),
        )
        .on_page_load(|webview, payload| {
            if payload.event() != PageLoadEvent::Finished {
                return;
            }

            let _ = webview.eval(DRAG_SCRIPT);
            let Some(window) = WindowSlot::from_label(webview.label()) else {
                return;
            };
            let Some(token) = refresh_token(payload.url()) else {
                return;
            };
            let _ = webview
                .state::<RefreshEvents>()
                .0
                .send(RefreshEvent { window, token });
        })
        .setup(move |app| {
            let primary = app
                .get_webview_window(WindowSlot::Primary.label())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "primary calendar window is missing",
                    )
                })?;
            primary
                .set_ignore_cursor_events(false)
                .map_err(|error| setup_error("couldn't initialize interactive mode", error))?;
            let calendar_url = primary
                .url()
                .map_err(|error| setup_error("couldn't read configured calendar URL", error))?;

            tauri::async_runtime::spawn(refresh_coordinator(
                app.handle().clone(),
                refresh_rx,
                calendar_url,
            ));

            // Non-fatal: another application may already own this shortcut.
            if let Err(error) = app.global_shortcut().register(toggle) {
                eprintln!("gcal-widget: couldn't register Ctrl+Alt+C toggle: {error}");
            }

            // Debug runs must not alter the installed release's login entry.
            #[cfg(not(debug_assertions))]
            app.autolaunch()
                .enable()
                .map_err(|error| setup_error("couldn't enable release autostart", error))?;

            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)
                .map_err(|error| setup_error("couldn't create tray Quit item", error))?;
            let menu = Menu::with_items(app, &[&quit])
                .map_err(|error| setup_error("couldn't create tray menu", error))?;
            let mut tray = TrayIconBuilder::new()
                .tooltip("gcal-widget")
                .menu(&menu)
                .on_menu_event(|app, event| {
                    if event.id.as_ref() == "quit" {
                        app.exit(0);
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            } else {
                eprintln!(
                    "gcal-widget: no default tray icon is configured; continuing without one"
                );
            }
            tray.build(app)
                .map_err(|error| setup_error("couldn't create tray icon", error))?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("gcal-widget failed while running the Tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_refreshes_alternate_active_and_buffer_windows() {
        let mut state = RefreshState::new();
        let first = state.begin();
        assert_eq!(first.window, WindowSlot::Buffer);
        assert_eq!(
            state.finish(first, AttemptOutcome::Success),
            REFRESH_INTERVAL
        );

        let second = state.begin();
        assert_eq!(second.window, WindowSlot::Primary);
        state.finish(second, AttemptOutcome::Success);
        assert_eq!(state.active, WindowSlot::Primary);
    }

    #[test]
    fn only_the_pending_window_and_token_match() {
        let mut state = RefreshState::new();
        let target = state.begin();
        assert!(state.matches(RefreshEvent {
            window: target.window,
            token: target.token,
        }));
        assert!(!state.matches(RefreshEvent {
            window: target.window.other(),
            token: target.token,
        }));
        assert!(!state.matches(RefreshEvent {
            window: target.window,
            token: target.token + 1,
        }));
    }

    #[test]
    fn stale_event_is_rejected_after_a_new_attempt_begins() {
        let mut state = RefreshState::new();
        let stale = state.begin();
        state.finish(stale, AttemptOutcome::TimedOut);
        let current = state.begin();

        assert!(!state.matches(RefreshEvent {
            window: stale.window,
            token: stale.token,
        }));
        assert!(state.matches(RefreshEvent {
            window: current.window,
            token: current.token,
        }));
    }

    #[test]
    fn timeout_keeps_the_active_window_and_schedules_a_retry() {
        let mut state = RefreshState::new();
        let target = state.begin();
        let delay = state.finish(target, AttemptOutcome::TimedOut);

        assert_eq!(state.active, WindowSlot::Primary);
        assert_eq!(state.pending, None);
        assert_eq!(delay, Duration::from_secs(30));
    }

    #[test]
    fn retry_backoff_is_capped_and_success_resets_it() {
        let mut state = RefreshState::new();
        for expected in [30, 60, 120, 240, 300, 300] {
            let target = state.begin();
            assert_eq!(
                state.finish(target, AttemptOutcome::NavigationFailed),
                Duration::from_secs(expected)
            );
        }

        let target = state.begin();
        assert_eq!(
            state.finish(target, AttemptOutcome::Success),
            REFRESH_INTERVAL
        );
        assert_eq!(state.retry_count, 0);
        assert_eq!(state.begin().retry_count, 0);
    }

    #[test]
    fn refresh_url_preserves_path_fragment_and_existing_query() {
        let base = Url::parse("http://example.test/calendar/week?theme=dark&room=a%20b#today")
            .expect("test URL should parse");
        let url = refresh_url(&base, 42);

        assert_eq!(url.path(), "/calendar/week");
        assert_eq!(url.fragment(), Some("today"));
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "theme").unwrap().1,
            "dark"
        );
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "room").unwrap().1,
            "a b"
        );
        assert_eq!(refresh_token(&url), Some(42));
    }

    #[test]
    fn refresh_url_replaces_the_reserved_parameter() {
        let base = Url::parse("http://example.test/?__gcal_refresh=old&keep=yes")
            .expect("test URL should parse");
        let url = refresh_url(&base, 9);

        assert_eq!(
            url.query_pairs()
                .filter(|(key, _)| key == REFRESH_PARAMETER)
                .count(),
            1
        );
        assert_eq!(refresh_token(&url), Some(9));
        assert!(url
            .query_pairs()
            .any(|(key, value)| key == "keep" && value == "yes"));
    }
}
