use crate::paths::Paths;
use crate::server::SharedCtx;
use parking_lot::Mutex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

#[derive(Clone, Copy)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub struct AppState {
    pub ctx: SharedCtx,
    pub paths: Paths,
    pub window_state_path: PathBuf,
    media_scope_path: PathBuf,
    tray_on_close: AtomicBool,
    pub is_quitting: AtomicBool,
    pub has_tray: AtomicBool,
    pub mini: Mutex<Option<Bounds>>,
    pub welcome_result: Mutex<Option<Value>>,
    media_keys_wanted: AtomicBool,
    media_keys_active: AtomicBool,
    pub media_keys_registered: AtomicBool,
    pub visible: AtomicBool,
    log_budget: Mutex<(Instant, u32)>,
    pub last_persist: Mutex<Option<Instant>>,
}

impl AppState {
    pub fn new(ctx: SharedCtx, paths: Paths, app_data: PathBuf) -> Self {
        AppState {
            ctx,
            paths,
            window_state_path: app_data.join("window-state.json"),
            media_scope_path: app_data.join("media-scope.json"),
            tray_on_close: AtomicBool::new(!cfg!(target_os = "linux")),
            is_quitting: AtomicBool::new(false),
            has_tray: AtomicBool::new(false),
            mini: Mutex::new(None),
            welcome_result: Mutex::new(None),
            media_keys_wanted: AtomicBool::new(true),
            media_keys_active: AtomicBool::new(false),
            media_keys_registered: AtomicBool::new(false),
            visible: AtomicBool::new(true),
            log_budget: Mutex::new((Instant::now(), 0)),
            last_persist: Mutex::new(None),
        }
    }

    pub fn is_mini(&self) -> bool {
        self.mini.lock().is_some()
    }

    pub fn tray_on_close(&self) -> bool {
        self.tray_on_close.load(Ordering::SeqCst)
    }

    pub fn set_tray_on_close(&self, v: bool) {
        self.tray_on_close.store(v, Ordering::SeqCst);
    }

    pub fn set_media_keys_wanted(&self, v: bool) {
        self.media_keys_wanted.store(v, Ordering::SeqCst);
    }

    pub fn media_keys_wanted(&self) -> bool {
        self.media_keys_wanted.load(Ordering::SeqCst)
    }

    pub fn activate_media_keys(&self) -> bool {
        !self.media_keys_active.swap(true, Ordering::SeqCst)
    }

    pub fn media_keys_active(&self) -> bool {
        self.media_keys_active.load(Ordering::SeqCst)
    }

    pub fn allow_renderer_log(&self) -> bool {
        let mut b = self.log_budget.lock();
        if b.0.elapsed() > Duration::from_secs(60) {
            *b = (Instant::now(), 0);
        }
        if b.1 >= 20 {
            return false;
        }
        b.1 += 1;
        true
    }

    fn read_media_scope(&self) -> Vec<String> {
        std::fs::read_to_string(&self.media_scope_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default()
    }

    pub fn allow_media(&self, app: &AppHandle, path: &Path) {
        let _ = app.asset_protocol_scope().allow_file(path);
        let mut list = self.read_media_scope();
        let p = path.to_string_lossy().into_owned();
        if !list.contains(&p) {
            list.push(p);
            if let Some(parent) = self.media_scope_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&self.media_scope_path, serde_json::to_string(&list).unwrap_or_default());
        }
    }

    pub fn restore_media_scope(&self, app: &AppHandle) {
        for p in self.read_media_scope() {
            let path = PathBuf::from(&p);
            if path.exists() {
                let _ = app.asset_protocol_scope().allow_file(&path);
            }
        }
    }
}
