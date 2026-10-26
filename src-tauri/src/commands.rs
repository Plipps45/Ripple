use crate::server::textutil::now_ms;
use crate::state::AppState;
use crate::{discord, logger, paths};
use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use tauri_plugin_dialog::{DialogExt, FilePath};
use tauri_plugin_opener::OpenerExt;

type Res<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn into_path(fp: FilePath) -> Option<PathBuf> {
    fp.into_path().ok()
}

async fn pick_files(window: &WebviewWindow, title: &str, filter: Option<(&str, &[&str])>, multiple: bool) -> Vec<PathBuf> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut builder = window.dialog().file().set_parent(window).set_title(title);
    if let Some((name, exts)) = filter {
        builder = builder.add_filter(name, exts);
    }
    if multiple {
        builder.pick_files(move |files| {
            let _ = tx.send(files.unwrap_or_default().into_iter().filter_map(into_path).collect::<Vec<_>>());
        });
    } else {
        builder.pick_file(move |file| {
            let _ = tx.send(file.and_then(into_path).into_iter().collect::<Vec<_>>());
        });
    }
    rx.await.unwrap_or_default()
}

async fn pick_folder(window: &WebviewWindow, title: &str, start: Option<PathBuf>) -> Option<PathBuf> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut builder = window.dialog().file().set_parent(window).set_title(title);
    if let Some(dir) = start.filter(|d| d.exists()) {
        builder = builder.set_directory(dir);
    }
    builder.pick_folder(move |f| {
        let _ = tx.send(f.and_then(into_path));
    });
    rx.await.ok().flatten()
}

#[tauri::command]
pub fn window_minimize(window: WebviewWindow) {
    let _ = window.minimize();
}

#[tauri::command]
pub fn window_toggle_maximize(window: WebviewWindow, state: State<'_, AppState>) {
    if state.is_mini() {
        return;
    }
    if window.is_maximized().unwrap_or(false) {
        let _ = window.unmaximize();
    } else {
        let _ = window.maximize();
    }
}

#[tauri::command]
pub fn window_close(window: WebviewWindow) {
    let _ = window.close();
}

#[tauri::command]
pub fn window_start_drag(window: WebviewWindow) {
    let _ = window.start_dragging();
}

#[tauri::command]
pub fn set_zoom(window: WebviewWindow, factor: f64) {
    if factor.is_finite() && factor > 0.0 {
        let _ = window.set_zoom(factor);
    }
}

#[tauri::command]
pub fn toggle_mini_window(window: WebviewWindow, state: State<'_, AppState>) -> bool {
    crate::window::toggle_mini(&window, &state)
}

#[tauri::command]
pub fn get_server_port(state: State<'_, AppState>) -> u16 {
    state.ctx.port()
}

#[tauri::command]
pub fn get_gpu_compositing() -> String {
    "enabled".into()
}

#[tauri::command]
pub fn get_app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
pub fn update_check(app: AppHandle) -> Value {
    crate::updater::check(app);
    json!({ "ok": true })
}

#[tauri::command]
pub fn update_action(app: AppHandle, action: String) {
    crate::updater::action(app, &action);
}

#[tauri::command]
pub fn launcher_info() -> Value {
    crate::launcher::info()
}

#[tauri::command]
pub async fn get_audio_output_info() -> Value {
    crate::audio_output::system_audio_output_info().await
}

#[tauri::command]
pub fn restart_app(app: AppHandle) {
    app.restart();
}

#[tauri::command]
pub fn set_tray_on_close(enabled: bool, state: State<'_, AppState>) {
    state.set_tray_on_close(enabled);
}

#[tauri::command]
pub fn get_multi_instance(state: State<'_, AppState>) -> bool {
    state.paths.read_startup_config().get("multiInstance").map(crate::server::textutil::truthy).unwrap_or(false)
}

#[tauri::command]
pub fn set_multi_instance(value: bool, state: State<'_, AppState>) {
    let mut patch = Map::new();
    patch.insert("multiInstance".into(), json!(value));
    state.paths.write_startup_config(patch);
}

#[tauri::command]
pub fn set_media_keys(enabled: bool, app: AppHandle, state: State<'_, AppState>) {
    state.set_media_keys_wanted(enabled);
    crate::window::register_media_keys(&app, &state);
}

#[tauri::command]
pub fn set_thumbbar_state(playing: bool, shuffle: bool, repeat: bool, accent: String, app: AppHandle) {
    #[cfg(windows)]
    {
        let accent = u32::from_str_radix(accent.trim().trim_start_matches('#'), 16).unwrap_or(0xffffff);
        crate::thumbbar::set_state(&app, crate::thumbbar::View { playing, shuffle, repeat, accent });
    }
    #[cfg(not(windows))]
    let _ = (playing, shuffle, repeat, accent, app);
}

#[tauri::command]
pub fn media_keys_active(app: AppHandle, state: State<'_, AppState>) {
    if state.activate_media_keys() {
        crate::window::register_media_keys(&app, &state);
    }
}

#[tauri::command]
pub fn welcome_finish(detail: Option<Value>, window: WebviewWindow, state: State<'_, AppState>) {
    state.paths.mark_tutorial_done();
    let skipped = detail.as_ref().map(|d| crate::server::textutil::truthy(d.get("skipped").unwrap_or(&Value::Null))).unwrap_or(true);
    if let Some(enabled) = detail.as_ref().filter(|_| !skipped).and_then(|d| d.get("launcher")).and_then(Value::as_bool) {
        if let Err(e) = crate::launcher::apply(enabled, &window.app_handle().config().identifier) {
            logger::error("Start menu shortcut update failed", e);
        }
    }
    if !skipped {
        *state.welcome_result.lock() = detail;
    }
    if let Ok(mut url) = window.url() {
        url.set_path("/index.html");
        url.set_query(None);
        let _ = window.navigate(url);
    }
}

#[tauri::command]
pub fn take_welcome_result(state: State<'_, AppState>) -> Option<Value> {
    state.welcome_result.lock().take()
}

#[tauri::command]
pub fn discord_rpc_update(info: Option<Value>) {
    if let Some(i) = info.filter(|v| v.is_object()) {
        discord::get().update(i);
    }
}

#[tauri::command]
pub fn discord_rpc_clear() {
    discord::get().clear();
}

#[tauri::command]
pub fn discord_rpc_set_enabled(enabled: bool) {
    discord::get().set_enabled(enabled);
}

#[tauri::command]
pub fn discord_rpc_get_enabled() -> bool {
    discord::get().enabled()
}

#[tauri::command]
pub fn discord_rpc_get_connected() -> bool {
    discord::get().connected()
}

#[tauri::command]
pub fn discord_rpc_get_status() -> &'static str {
    discord::get().status().as_str()
}

#[tauri::command]
pub fn discord_rpc_get_client_id(state: State<'_, AppState>) -> String {
    state.paths.read_startup_config().get("discordClientId").and_then(Value::as_str).unwrap_or("").to_string()
}

#[tauri::command]
pub fn discord_rpc_set_client_id(id: String, state: State<'_, AppState>) -> Result<(), String> {
    let id = id.trim().to_string();
    if !id.is_empty() && !discord::valid_client_id(&id) {
        return Err("An application ID is a number of 17 to 20 digits".into());
    }
    let mut patch = Map::new();
    patch.insert("discordClientId".into(), json!(id));
    if !state.paths.write_startup_config(patch) {
        return Err("Could not save the setting".into());
    }
    discord::get().set_client_id(&id);
    Ok(())
}

#[tauri::command]
pub async fn pick_media(window: WebviewWindow, state: State<'_, AppState>) -> Res<Option<String>> {
    let exts = ["png", "jpg", "jpeg", "webp", "gif", "mp4", "webm"];
    let picked = pick_files(&window, "Choose media", Some(("Media", &exts)), false).await;
    let Some(path) = picked.into_iter().next() else { return Ok(None) };
    state.allow_media(window.app_handle(), &path);
    Ok(Some(path.to_string_lossy().into_owned()))
}

fn playlist_cover_prefix(playlist_id: &str) -> String {
    let safe: String = playlist_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("playlist_{safe}_")
}

async fn remove_playlist_covers(dir: &Path, prefix: &str) {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return };
    while let Ok(Some(entry)) = rd.next_entry().await {
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

#[tauri::command]
pub async fn pick_playlist_cover(window: WebviewWindow, state: State<'_, AppState>, playlist_id: String) -> Res<Option<String>> {
    let exts = ["png", "jpg", "jpeg", "webp", "gif"];
    let picked = pick_files(&window, "Choose playlist cover", Some(("Images", &exts)), false).await;
    let Some(src) = picked.into_iter().next() else { return Ok(None) };
    let dir = state.paths.data_dir.join("covers");
    tokio::fs::create_dir_all(&dir).await.map_err(err)?;
    let prefix = playlist_cover_prefix(&playlist_id);
    remove_playlist_covers(&dir, &prefix).await;
    let ext = src.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "png".into());
    let dest = dir.join(format!("{prefix}{}.{ext}", now_ms()));
    tokio::fs::copy(&src, &dest).await.map_err(err)?;
    state.allow_media(window.app_handle(), &dest);
    Ok(Some(dest.to_string_lossy().into_owned()))
}

#[tauri::command]
pub async fn remove_playlist_cover(state: State<'_, AppState>, playlist_id: String) -> Res<()> {
    remove_playlist_covers(&state.paths.data_dir.join("covers"), &playlist_cover_prefix(&playlist_id)).await;
    Ok(())
}

#[tauri::command]
pub async fn import_audio_files(window: WebviewWindow, state: State<'_, AppState>) -> Res<Vec<Value>> {
    let exts = ["mp3", "wav", "flac", "opus", "ogg", "oga", "m4a", "aac", "webm", "weba"];
    let files = pick_files(&window, "Import audio", Some(("Audio", &exts)), true).await;
    if files.is_empty() {
        return Ok(vec![]);
    }
    let lib = state.paths.library_dir.clone();
    tokio::fs::create_dir_all(&lib).await.map_err(err)?;
    let mut out = Vec::new();
    for src in files {
        let Ok(meta) = tokio::fs::metadata(&src).await else { continue };
        let base = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = src.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let mut h = Sha1::new();
        h.update(format!("{base}{}", meta.len()).as_bytes());
        let id = format!("local_{}", &hex::encode(h.finalize())[..12]);
        let file = format!("{id}.{ext}");
        let dest = lib.join(&file);
        if !dest.exists() {
            tokio::fs::copy(&src, &dest).await.map_err(err)?;
        }
        let name = src.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push(json!({ "id": id, "file": file, "ext": ext, "name": name }));
    }
    Ok(out)
}

fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else { return 0 };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

fn webview_cache_dirs(app: &AppHandle) -> Vec<PathBuf> {
    let Ok(base) = app.path().app_local_data_dir() else { return vec![] };
    let profile = base.join("EBWebView").join("Default");
    vec![profile.join("Cache"), profile.join("Code Cache"), profile.join("Service Worker").join("CacheStorage")]
}

fn is_cache_audio(name: &str) -> bool {
    crate::server::media::is_cache_audio(name)
}

#[tauri::command]
pub async fn get_cache_size(app: AppHandle, state: State<'_, AppState>) -> Res<Value> {
    let (tracks, thumbs) = (state.paths.cache_dir.clone(), state.paths.thumbnails_dir.clone());
    let dirs = webview_cache_dirs(&app);
    let (audio, web) = tokio::task::spawn_blocking(move || {
        let mut audio = 0u64;
        if let Ok(rd) = std::fs::read_dir(&tracks) {
            for e in rd.flatten() {
                if is_cache_audio(&e.file_name().to_string_lossy()) {
                    audio += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
        audio += dir_size(&thumbs);
        let web: u64 = dirs.iter().map(|d| dir_size(d)).sum();
        (audio, web)
    })
    .await
    .map_err(err)?;
    Ok(json!({ "audio": audio, "web": web }))
}

#[cfg(windows)]
fn clear_webview_http_cache(window: &WebviewWindow) {
    let _ = window.with_webview(|wv| unsafe {
        use webview2_com::Microsoft::Web::WebView2::Win32::*;
        use windows_core::Interface;
        let Ok(core) = wv.controller().CoreWebView2() else { return };
        let Ok(core13) = core.cast::<ICoreWebView2_13>() else { return };
        let Ok(profile) = core13.Profile() else { return };
        let Ok(profile2) = profile.cast::<ICoreWebView2Profile2>() else { return };
        let kinds = COREWEBVIEW2_BROWSING_DATA_KINDS(
            COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE.0
                | COREWEBVIEW2_BROWSING_DATA_KINDS_CACHE_STORAGE.0
                | COREWEBVIEW2_BROWSING_DATA_KINDS_SERVICE_WORKERS.0,
        );
        let handler = webview2_com::ClearBrowsingDataCompletedHandler::create(Box::new(|_| Ok(())));
        let _ = profile2.ClearBrowsingData(kinds, &handler);
    });
}

#[cfg(not(windows))]
fn clear_webview_http_cache(_window: &WebviewWindow) {}

#[tauri::command]
pub async fn clear_cache(window: WebviewWindow, state: State<'_, AppState>) -> Res<bool> {
    clear_webview_http_cache(&window);
    let (tracks, thumbs) = (state.paths.cache_dir.clone(), state.paths.thumbnails_dir.clone());
    tokio::task::spawn_blocking(move || {
        if let Ok(rd) = std::fs::read_dir(&tracks) {
            for e in rd.flatten() {
                if is_cache_audio(&e.file_name().to_string_lossy()) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(&thumbs) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    })
    .await
    .map_err(err)?;
    Ok(true)
}

#[tauri::command]
pub fn get_data_dir(state: State<'_, AppState>) -> Value {
    let dir = state.paths.data_dir.clone();
    let def = paths::default_data_dir();
    json!({ "dir": dir.to_string_lossy(), "defaultDir": def.to_string_lossy(), "isCustom": dir != def })
}

#[tauri::command]
pub async fn pick_data_dir(window: WebviewWindow, state: State<'_, AppState>) -> Res<Option<String>> {
    Ok(pick_folder(&window, "Choose music folder", Some(state.paths.data_dir.clone())).await.map(|p| p.to_string_lossy().into_owned()))
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn is_within(child: &Path, parent: &Path) -> bool {
    let (c, p) = (normalize(child), normalize(parent));
    if cfg!(windows) {
        let (cs, ps) = (c.to_string_lossy().to_lowercase(), p.to_string_lossy().to_lowercase());
        Path::new(&cs).starts_with(Path::new(&ps))
    } else {
        c.starts_with(&p)
    }
}

fn writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".ripple-write-test-{}", now_ms()));
    let ok = std::fs::write(&probe, b"1").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

#[tauri::command]
pub async fn set_data_dir(dir: Option<String>, move_files: bool, state: State<'_, AppState>) -> Res<Value> {
    let Some(target) = dir.filter(|d| !d.trim().is_empty()).map(|d| normalize(&std::path::absolute(d.trim()).unwrap_or_else(|_| PathBuf::from(d.trim())))) else {
        return Ok(json!({ "ok": false, "error": "No folder selected" }));
    };
    let current = normalize(&state.paths.data_dir);
    if target == current {
        return Ok(json!({ "ok": true, "dir": target.to_string_lossy(), "moved": false }));
    }
    if is_within(&target, &current) || is_within(&current, &target) {
        return Ok(json!({ "ok": false, "error": "Choose a folder outside the current music folder" }));
    }
    if !writable(&target) {
        return Ok(json!({ "ok": false, "error": "That folder is not writable" }));
    }
    if move_files {
        if let Err(e) = paths::copy_dir_contents(&current, &target).await {
            return Ok(json!({ "ok": false, "error": format!("Could not move files: {e}") }));
        }
    }
    paths::set_data_dir(Some(&target.to_string_lossy())).map_err(err)?;
    if move_files {
        let _ = paths::remove_dir_contents(&current).await;
    }
    Ok(json!({ "ok": true, "dir": target.to_string_lossy(), "moved": move_files }))
}

#[tauri::command]
pub fn get_download_dir() -> Value {
    let def = paths::default_downloads_dir();
    let custom = paths::resolve_download_dir();
    json!({
        "dir": custom.clone().unwrap_or_else(|| def.clone()).to_string_lossy(),
        "defaultDir": def.to_string_lossy(),
        "isCustom": custom.is_some()
    })
}

#[tauri::command]
pub async fn pick_download_dir(window: WebviewWindow) -> Res<Option<String>> {
    let start = paths::resolve_download_dir().unwrap_or_else(paths::default_downloads_dir);
    Ok(pick_folder(&window, "Choose download folder", Some(start)).await.map(|p| p.to_string_lossy().into_owned()))
}

#[tauri::command]
pub fn set_download_dir(dir: Option<String>, reset: bool) -> Res<Value> {
    if reset {
        paths::set_download_dir(None).map_err(err)?;
        return Ok(json!({ "ok": true, "dir": paths::default_downloads_dir().to_string_lossy(), "reset": true }));
    }
    let Some(target) = dir.filter(|d| !d.trim().is_empty()).map(|d| std::path::absolute(d.trim()).unwrap_or_else(|_| PathBuf::from(d.trim()))) else {
        return Ok(json!({ "ok": false, "error": "No folder selected" }));
    };
    if !writable(&target) {
        return Ok(json!({ "ok": false, "error": "That folder is not writable" }));
    }
    paths::set_download_dir(Some(&target.to_string_lossy())).map_err(err)?;
    Ok(json!({ "ok": true, "dir": target.to_string_lossy() }))
}

const THEME_MAX_BYTES: u64 = 2 * 1024 * 1024;
const PLUGIN_MAX_BYTES: u64 = 1024 * 1024;

fn safe_theme_id(id: &str) -> bool {
    id.len() >= 5 && id.len() <= 25 && id.starts_with('t') && id[1..].chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn themes_index(state: &AppState) -> PathBuf {
    state.paths.themes_dir.join("index.json")
}

fn read_theme_index(state: &AppState) -> Vec<Value> {
    std::fs::read_to_string(themes_index(state))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("themes").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn write_theme_index(state: &AppState, themes: Vec<Value>) {
    let _ = std::fs::create_dir_all(&state.paths.themes_dir);
    let _ = std::fs::write(themes_index(state), json!({ "themes": themes }).to_string());
}

fn parse_meta(code: &str, keys: &[&str]) -> Map<String, Value> {
    let block = code.find("/*").and_then(|s| code[s..].find("*/").map(|e| &code[s..s + e + 2])).unwrap_or("");
    let mut out = Map::new();
    for key in keys {
        let tag = format!("@{key}");
        let mut value = String::new();
        let mut search = block;
        while let Some(pos) = search.find(&tag) {
            let rest = &search[pos + tag.len()..];
            if rest.starts_with(' ') || rest.starts_with('\t') {
                let v: String = rest.trim_start_matches([' ', '\t']).chars().take_while(|c| !matches!(c, '\r' | '\n' | '*')).collect();
                if !v.trim().is_empty() {
                    value = v.trim().to_string();
                    break;
                }
            }
            search = &rest[..];
        }
        out.insert(key.to_string(), json!(value));
    }
    out
}

#[tauri::command]
pub fn themes_list(state: State<'_, AppState>) -> Vec<Value> {
    read_theme_index(&state)
        .into_iter()
        .filter_map(|mut t| {
            let id = t.get("id").and_then(Value::as_str)?.to_string();
            let css = std::fs::read_to_string(state.paths.themes_dir.join(format!("{id}.css"))).ok()?;
            t["css"] = json!(css);
            Some(t)
        })
        .collect()
}

#[tauri::command]
pub async fn themes_import(window: WebviewWindow, state: State<'_, AppState>) -> Res<Vec<Value>> {
    let files = pick_files(&window, "Add theme", Some(("CSS theme", &["css"])), true).await;
    if files.is_empty() {
        return Ok(vec![]);
    }
    let mut themes = read_theme_index(&state);
    let mut added = Vec::new();
    for file in files {
        if std::fs::metadata(&file).map(|m| m.len() > THEME_MAX_BYTES).unwrap_or(true) {
            continue;
        }
        let Ok(css) = std::fs::read_to_string(&file) else { continue };
        let id = format!("t{}{}", radix36(now_ms()), crate::server::textutil::random_base36(4));
        let _ = std::fs::create_dir_all(&state.paths.themes_dir);
        if std::fs::write(state.paths.themes_dir.join(format!("{id}.css")), &css).is_err() {
            continue;
        }
        let fallback = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let meta = parse_meta(&css, &["name", "author", "description"]);
        let name = meta.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or(fallback);
        let entry = json!({ "id": id, "name": name, "author": meta["author"], "description": meta["description"], "enabled": false });
        themes.push(entry.clone());
        let mut with_css = entry;
        with_css["css"] = json!(css);
        added.push(with_css);
    }
    write_theme_index(&state, themes);
    Ok(added)
}

fn radix36(mut n: u64) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(CHARS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

#[tauri::command]
pub fn themes_set_enabled(id: String, on: bool, state: State<'_, AppState>) {
    if !safe_theme_id(&id) {
        return;
    }
    let themes = read_theme_index(&state)
        .into_iter()
        .map(|mut t| {
            if t.get("id").and_then(Value::as_str) == Some(id.as_str()) {
                t["enabled"] = json!(on);
            }
            t
        })
        .collect();
    write_theme_index(&state, themes);
}

#[tauri::command]
pub fn themes_disable_all(state: State<'_, AppState>) {
    let themes = read_theme_index(&state)
        .into_iter()
        .map(|mut t| {
            t["enabled"] = json!(false);
            t
        })
        .collect();
    write_theme_index(&state, themes);
}

#[tauri::command]
pub fn themes_remove(id: String, state: State<'_, AppState>) {
    if !safe_theme_id(&id) {
        return;
    }
    let _ = std::fs::remove_file(state.paths.themes_dir.join(format!("{id}.css")));
    let themes = read_theme_index(&state).into_iter().filter(|t| t.get("id").and_then(Value::as_str) != Some(id.as_str())).collect();
    write_theme_index(&state, themes);
}

#[tauri::command]
pub fn read_app_css(app: AppHandle) -> String {
    app.asset_resolver().get("styles.css".into()).map(|a| String::from_utf8_lossy(&a.bytes).into_owned()).unwrap_or_default()
}

const EXAMPLE_PLUGIN: &str = "/**\n * @name Hello plugin\n * @author Ripple\n * @version 1.0.0\n * @description Shows a message when the track changes. Edit or delete this file.\n */\nconst off = Ripple.on('trackchange', (track) => Ripple.toast('Now playing: ' + track.title));\nreturn { stop() { off(); } };\n";

fn safe_plugin_name(name: &str) -> bool {
    name.ends_with(".js")
        && !name.contains("..")
        && name.len() > 3
        && name[..name.len() - 3].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | ' '))
}

fn ensure_plugins_dir(state: &AppState) {
    let dir = &state.paths.plugins_dir;
    if dir.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join("example-hello.js"), EXAMPLE_PLUGIN);
}

fn plugins_state_path(state: &AppState) -> PathBuf {
    state.paths.plugins_dir.join("plugins.json")
}

fn read_plugin_state(state: &AppState) -> Map<String, Value> {
    std::fs::read_to_string(plugins_state_path(state))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("enabled").and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

fn write_plugin_state(state: &AppState, enabled: Map<String, Value>) {
    ensure_plugins_dir(state);
    let _ = std::fs::write(plugins_state_path(state), json!({ "enabled": enabled }).to_string());
}

#[tauri::command]
pub fn plugins_list(state: State<'_, AppState>) -> Vec<Value> {
    ensure_plugins_dir(&state);
    let enabled = read_plugin_state(&state);
    let Ok(rd) = std::fs::read_dir(&state.paths.plugins_dir) else { return vec![] };
    let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
        .into_iter()
        .filter(|f| f.to_lowercase().ends_with(".js") && safe_plugin_name(f))
        .filter_map(|f| {
            let full = state.paths.plugins_dir.join(&f);
            if std::fs::metadata(&full).ok()?.len() > PLUGIN_MAX_BYTES {
                return None;
            }
            let code = std::fs::read_to_string(&full).ok()?;
            let meta = parse_meta(&code, &["name", "author", "version", "description"]);
            let fallback = f.trim_end_matches(".js").to_string();
            let name = meta.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or(fallback);
            Some(json!({
                "file": f, "name": name, "author": meta["author"], "version": meta["version"], "description": meta["description"],
                "enabled": enabled.get(&f).map(crate::server::textutil::truthy).unwrap_or(false), "code": code
            }))
        })
        .collect()
}

#[tauri::command]
pub fn plugins_set_enabled(file: String, on: bool, state: State<'_, AppState>) {
    if !safe_plugin_name(&file) {
        return;
    }
    let mut s = read_plugin_state(&state);
    if on {
        s.insert(file, json!(true));
    } else {
        s.shift_remove(&file);
    }
    write_plugin_state(&state, s);
}

#[tauri::command]
pub fn plugins_disable_all(state: State<'_, AppState>) {
    write_plugin_state(&state, Map::new());
}

#[tauri::command]
pub fn plugins_open_folder(app: AppHandle, state: State<'_, AppState>) {
    ensure_plugins_dir(&state);
    let _ = app.opener().open_path(state.paths.plugins_dir.to_string_lossy(), None::<&str>);
}

#[tauri::command]
pub fn plugins_remove(file: String, state: State<'_, AppState>) {
    if !safe_plugin_name(&file) {
        return;
    }
    let _ = std::fs::remove_file(state.paths.plugins_dir.join(&file));
    let mut s = read_plugin_state(&state);
    s.shift_remove(&file);
    write_plugin_state(&state, s);
}

#[tauri::command]
pub async fn plugins_import(window: WebviewWindow, state: State<'_, AppState>) -> Res<u32> {
    ensure_plugins_dir(&state);
    let files = pick_files(&window, "Add plugin", Some(("JavaScript plugin", &["js"])), true).await;
    let mut added = 0;
    for file in files {
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !safe_plugin_name(&name) || std::fs::metadata(&file).map(|m| m.len() > PLUGIN_MAX_BYTES).unwrap_or(true) {
            continue;
        }
        if std::fs::copy(&file, state.paths.plugins_dir.join(&name)).is_ok() {
            added += 1;
        }
    }
    Ok(added)
}

#[tauri::command]
pub fn open_external(url: String, app: AppHandle) {
    if url.starts_with("https://accounts.spotify.com/") {
        let _ = app.opener().open_url(url, None::<&str>);
    }
}

#[tauri::command]
pub fn open_in_browser(url: String, app: AppHandle) {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:") {
        let _ = app.opener().open_url(url, None::<&str>);
    }
}

#[tauri::command]
pub fn log_error(level: String, message: String, stack: Option<String>, state: State<'_, AppState>) {
    if !state.allow_renderer_log() {
        return;
    }
    let text = match stack.filter(|s| !s.is_empty()) {
        Some(s) => format!("{message} | {s}"),
        None => message,
    };
    logger::log(if level.is_empty() { "ERROR" } else { &level }, &text);
}

#[tauri::command]
pub fn open_logs_folder(app: AppHandle) {
    let dir = logger::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = app.opener().open_path(dir.to_string_lossy(), None::<&str>);
}

#[tauri::command]
pub fn get_diagnostics(app: AppHandle, state: State<'_, AppState>) -> String {
    let info = app.package_info();
    let mut lines = vec![
        format!("Ripple {} (Tauri build)", info.version),
        format!("OS {} {} {}", std::env::consts::OS, std::env::consts::ARCH, os_release()),
        format!("Tauri {} WebView {}", tauri::VERSION, tauri::webview_version().unwrap_or_else(|_| "unknown".into())),
        format!("Data {}", state.paths.data_dir.display()),
    ];
    for name in ["yt-dlp", "ffmpeg"] {
        let bin = paths::resolve_binary(&state.paths, name).map(|p| p.display().to_string()).unwrap_or_else(|| "NOT FOUND".into());
        lines.push(format!("{name}: {bin}"));
    }
    lines.push("--- last log lines ---".into());
    lines.extend(logger::tail(60));
    lines.join("\n")
}

fn os_release() -> String {
    #[cfg(windows)]
    {
        std::env::var("OS").unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).unwrap_or_default()
    }
}

pub fn emit_main(app: &AppHandle, event: &str, payload: Value) {
    let _ = app.emit_to("main", event, payload);
}
