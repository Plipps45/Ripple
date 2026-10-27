use crate::commands::emit_main;
use crate::state::{AppState, Bounds};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut};

const BRIDGE_JS: &str = include_str!("bridge.js");
const MIN_W: f64 = 360.0;
const MIN_H: f64 = 500.0;

const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion \
--enable-features=HardwareMediaKeyHandling --autoplay-policy=no-user-gesture-required \
--disable-background-networking --disable-component-update --disable-sync --disable-domain-reliability";

struct WorkArea {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn work_areas(app: &AppHandle) -> (Option<WorkArea>, Vec<WorkArea>) {
    let to_logical = |m: &tauri::Monitor| {
        let s = m.scale_factor();
        let wa = m.work_area();
        WorkArea { x: wa.position.x as f64 / s, y: wa.position.y as f64 / s, w: wa.size.width as f64 / s, h: wa.size.height as f64 / s }
    };
    let primary = app.primary_monitor().ok().flatten().map(|m| to_logical(&m));
    let all = app.available_monitors().map(|ms| ms.iter().map(to_logical).collect()).unwrap_or_default();
    (primary, all)
}

fn read_window_state(state: &AppState) -> Option<Value> {
    std::fs::read_to_string(&state.window_state_path).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).filter(Value::is_object)
}

fn clamp_bounds(app: &AppHandle, b: Bounds) -> Bounds {
    let (primary, all) = work_areas(app);
    let primary = primary.unwrap_or(WorkArea { x: 0.0, y: 0.0, w: 1280.0, h: 800.0 });
    let width = b.width.min(primary.w).max(MIN_W);
    let height = b.height.min(primary.h).max(MIN_H);
    let wa = all
        .into_iter()
        .find(|w| b.x >= w.x && b.x < w.x + w.w && b.y >= w.y && b.y < w.y + w.h)
        .unwrap_or(primary);
    let x = b.x.max(wa.x).min(wa.x + (wa.w - width).max(0.0));
    let y = b.y.max(wa.y).min(wa.y + (wa.h - height).max(0.0));
    Bounds { x, y, width, height }
}

fn initial_bounds(app: &AppHandle, saved: Option<&Value>) -> Bounds {
    let (primary, _) = work_areas(app);
    let wa = primary.unwrap_or(WorkArea { x: 0.0, y: 0.0, w: 1280.0, h: 800.0 });
    let mut b = Bounds {
        width: (wa.w * 0.82).round(),
        height: (wa.h * 0.86).round(),
        x: wa.x + (wa.w * 0.09).round(),
        y: wa.y + (wa.h * 0.07).round(),
    };
    if let Some(sb) = saved.and_then(|s| s.get("bounds")) {
        let get = |k: &str| sb.get(k).and_then(Value::as_f64);
        if let (Some(w), Some(h)) = (get("width"), get("height")) {
            b.width = w;
            b.height = h;
        }
        if let Some(x) = get("x") {
            b.x = x;
        }
        if let Some(y) = get("y") {
            b.y = y;
        }
    }
    clamp_bounds(app, b)
}

fn logical_bounds(window: &WebviewWindow) -> Option<Bounds> {
    let s = window.scale_factor().ok()?;
    let pos = window.outer_position().ok()?.to_logical::<f64>(s);
    let size = window.inner_size().ok()?.to_logical::<f64>(s);
    Some(Bounds { x: pos.x, y: pos.y, width: size.width, height: size.height })
}

pub fn persist_window_state(window: &WebviewWindow, state: &AppState) {
    if state.is_mini() || window.is_minimized().unwrap_or(false) {
        return;
    }
    let maximized = window.is_maximized().unwrap_or(false);
    let fullscreen = window.is_fullscreen().unwrap_or(false);
    let mut bounds = logical_bounds(window);
    if maximized || fullscreen {
        bounds = read_window_state(state).and_then(|s| {
            let b = s.get("bounds")?;
            Some(Bounds {
                x: b.get("x")?.as_f64()?,
                y: b.get("y")?.as_f64()?,
                width: b.get("width")?.as_f64()?,
                height: b.get("height")?.as_f64()?,
            })
        });
    }
    let mut out = json!({ "isMaximized": maximized, "isFullScreen": fullscreen });
    if let Some(b) = bounds {
        out["bounds"] = json!({ "x": b.x.round(), "y": b.y.round(), "width": b.width.round(), "height": b.height.round() });
    }
    if let Some(parent) = state.window_state_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&state.window_state_path, serde_json::to_string_pretty(&out).unwrap_or_default());
}

fn schedule_persist(window: &WebviewWindow) {
    let state = window.state::<AppState>();
    {
        let mut last = state.last_persist.lock();
        if last.map(|t| t.elapsed() < Duration::from_millis(400)).unwrap_or(false) {
            return;
        }
        *last = Some(Instant::now());
    }
    let w = window.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(450)).await;
        let st = w.state::<AppState>();
        persist_window_state(&w, &st);
    });
}

fn set_visibility(app: &AppHandle, visible: bool) {
    let state = app.state::<AppState>();
    if state.visible.swap(visible, std::sync::atomic::Ordering::SeqCst) != visible {
        emit_main(app, "window-visibility", json!(visible));
    }
}

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_minimized().unwrap_or(false) {
            let _ = w.unminimize();
        }
        let _ = w.show();
        let _ = w.set_focus();
        set_visibility(app, true);
    }
}

pub fn toggle_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false) {
            let _ = w.hide();
            set_visibility(app, false);
        } else {
            show_main(app);
        }
    }
}

pub fn create_main_window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let state = app.state::<AppState>();
    let first_run = !state.paths.tutorial_done();
    let saved = read_window_state(&state);
    let b = initial_bounds(app, saved.as_ref());
    let boot = json!({ "token": state.ctx.token, "isWindows": cfg!(windows) });
    let script = format!("window.__RIPPLE_BOOT__ = {boot};\n{BRIDGE_JS}");
    let page = if first_run { "welcome.html" } else { "index.html" };

    let mut args = BROWSER_ARGS.to_string();
    if let Ok(extra) = std::env::var("RIPPLE_FLAGS") {
        for flag in extra.split(';').map(str::trim).filter(|f| !f.is_empty()) {
            args.push_str(&format!(" --{}", flag.trim_start_matches("--")));
        }
    }
    if let Ok(port) = std::env::var("RIPPLE_REMOTE_DEBUG_PORT") {
        if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() {
            args.push_str(&format!(" --remote-debugging-port={port}"));
        }
    }

    let bg = if first_run { tauri::window::Color(0xca, 0xec, 0xe2, 0xff) } else { tauri::window::Color(0, 0, 0, 0xff) };
    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::App(page.into()))
        .title("Ripple")
        .inner_size(b.width, b.height)
        .position(b.x, b.y)
        .min_inner_size(MIN_W, MIN_H)
        .decorations(false)
        .visible(false)
        .background_color(bg)
        .initialization_script(&script)
        .additional_browser_args(&args)
        .on_page_load(|w, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let _ = w.show();
            }
        })
        .build()?;

    if saved.as_ref().and_then(|s| s.get("isMaximized")).and_then(Value::as_bool).unwrap_or(false) {
        let _ = window.maximize();
    }
    #[cfg(windows)]
    {
        set_taskbar_icon(&window);
        crate::thumbbar::install(app, &window);
        allow_own_media_permission(&window);
    }

    let w = window.clone();
    window.on_window_event(move |event| match event {
        WindowEvent::Resized(_) => {
            let minimized = w.is_minimized().unwrap_or(false);
            set_visibility(w.app_handle(), !minimized && w.is_visible().unwrap_or(true));
            schedule_persist(&w);
        }
        WindowEvent::Moved(_) => schedule_persist(&w),
        WindowEvent::CloseRequested { api, .. } => {
            let st = w.state::<AppState>();
            persist_window_state(&w, &st);
            let quitting = st.is_quitting.load(std::sync::atomic::Ordering::SeqCst);
            if st.tray_on_close() && st.has_tray.load(std::sync::atomic::Ordering::SeqCst) && !quitting {
                api.prevent_close();
                let _ = w.hide();
                set_visibility(w.app_handle(), false);
            }
        }
        _ => {}
    });
    Ok(window)
}

// Chromium only names audio outputs once the page holds media permission; grant it to Ripple's own pages
// so the output picker can list devices without a prompt. Remote content keeps the default behaviour.
#[cfg(windows)]
fn allow_own_media_permission(window: &WebviewWindow) {
    let _ = window.with_webview(|wv| unsafe {
        use webview2_com::Microsoft::Web::WebView2::Win32::*;
        let Ok(core) = wv.controller().CoreWebView2() else { return };
        let handler = webview2_com::PermissionRequestedEventHandler::create(Box::new(|_, args| {
            let Some(args) = args else { return Ok(()) };
            let mut kind = COREWEBVIEW2_PERMISSION_KIND(0);
            args.PermissionKind(&mut kind)?;
            let mut uri = windows_core::PWSTR::null();
            args.Uri(&mut uri)?;
            let uri = webview2_com::take_pwstr(uri);
            let own_page = uri.starts_with("http://tauri.localhost/") || uri.starts_with("https://tauri.localhost/");
            if kind == COREWEBVIEW2_PERMISSION_KIND_MICROPHONE && own_page {
                args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)?;
            }
            Ok(())
        }));
        let mut token = 0i64;
        let _ = core.add_PermissionRequested(&handler, &mut token);
    });
}

#[cfg(windows)]
fn set_taskbar_icon(window: &WebviewWindow) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, IMAGE_ICON, LR_DEFAULTCOLOR, SM_CXICON, SM_CYICON, WM_SETICON,
    };
    const APP_ICON_ID: usize = 32512;
    let Ok(hwnd) = window.hwnd() else { return };
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let icon = LoadImageW(module, APP_ICON_ID as *const u16, IMAGE_ICON, GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON), LR_DEFAULTCOLOR);
        if !icon.is_null() {
            SendMessageW(hwnd.0 as _, WM_SETICON, ICON_BIG as usize, icon as isize);
        }
    }
}

pub fn toggle_mini(window: &WebviewWindow, state: &AppState) -> bool {
    let mut mini = state.mini.lock();
    if mini.is_none() {
        let maximized = window.is_maximized().unwrap_or(false);
        if maximized {
            let _ = window.unmaximize();
        }
        let before = logical_bounds(window).unwrap_or(Bounds { x: 100.0, y: 100.0, width: 1000.0, height: 700.0 });
        *mini = Some(before);
        let _ = window.set_min_size(Some(LogicalSize::new(320.0, 120.0)));
        let _ = window.set_size(LogicalSize::new(380.0, 160.0));
        let _ = window.set_always_on_top(true);
        true
    } else {
        let before = mini.take();
        let _ = window.set_always_on_top(false);
        let _ = window.set_min_size(Some(LogicalSize::new(MIN_W, MIN_H)));
        match before {
            Some(b) => {
                let c = clamp_bounds(window.app_handle(), b);
                let _ = window.set_size(LogicalSize::new(c.width, c.height));
                let _ = window.set_position(LogicalPosition::new(c.x, c.y));
            }
            None => {
                let _ = window.set_size(LogicalSize::new(1000.0, 700.0));
            }
        }
        false
    }
}

pub fn quit_app(app: &AppHandle) {
    app.state::<AppState>().is_quitting.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(w) = app.get_webview_window("main") {
        let st = app.state::<AppState>();
        persist_window_state(&w, &st);
    }
    app.exit(0);
}

pub fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show/Hide", true, None::<&str>)?;
    let play = MenuItem::with_id(app, "play", "Play/Pause", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &play, &sep, &quit])?;
    let mut builder = TrayIconBuilder::with_id("ripple-tray")
        .tooltip("Ripple")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => toggle_main(app),
            "play" => emit_main(app, "media-command", json!("toggle")),
            "quit" => quit_app(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                toggle_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    app.state::<AppState>().has_tray.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

const MEDIA_KEYS: [(Code, &str); 4] = [
    (Code::MediaPlayPause, "toggle"),
    (Code::MediaTrackNext, "next"),
    (Code::MediaTrackPrevious, "prev"),
    (Code::MediaStop, "stop"),
];

pub fn media_command_for(shortcut: &Shortcut) -> Option<&'static str> {
    MEDIA_KEYS.iter().find(|(code, _)| Shortcut::new(None, *code).id() == shortcut.id()).map(|(_, cmd)| *cmd)
}

pub fn register_media_keys(app: &AppHandle, state: &AppState) {
    if !cfg!(windows) {
        return;
    }
    let gs = app.global_shortcut();
    let want = state.media_keys_wanted() && state.media_keys_active();
    let registered = state.media_keys_registered.load(std::sync::atomic::Ordering::SeqCst);
    if want == registered {
        return;
    }
    for (code, _) in MEDIA_KEYS {
        let sc = Shortcut::new(None, code);
        if want {
            if let Err(e) = gs.register(sc) {
                crate::logger::log("warn", &format!("[media] could not register {code:?}: {e}"));
            }
        } else {
            let _ = gs.unregister(sc);
        }
    }
    state.media_keys_registered.store(want, std::sync::atomic::Ordering::SeqCst);
}
