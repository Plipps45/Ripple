use parking_lot::Mutex;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub const IS_WIN: bool = cfg!(target_os = "windows");

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData").join("Local"))
}

const WINDOWS_DATA_FOLDER: &str = "com.weebec.ripple";

pub fn default_data_dir() -> PathBuf {
    if IS_WIN {
        local_app_data().join(WINDOWS_DATA_FOLDER)
    } else if cfg!(target_os = "macos") {
        home().join("Library").join("Application Support").join("Ripple")
    } else {
        home().join(".cache").join("ripple")
    }
}

pub fn config_dir() -> PathBuf {
    if IS_WIN {
        local_app_data().join(WINDOWS_DATA_FOLDER)
    } else if cfg!(target_os = "macos") {
        home().join("Library").join("Application Support").join("Ripple")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"))
            .join("ripple")
    }
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn read_config() -> Map<String, Value> {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn write_config(cfg: &Map<String, Value>) -> std::io::Result<()> {
    fs::create_dir_all(config_dir())?;
    fs::write(config_path(), serde_json::to_string_pretty(cfg).unwrap_or_default())
}

fn absolute(p: &str) -> PathBuf {
    let path = PathBuf::from(p.trim());
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

pub fn resolve_data_dir() -> PathBuf {
    if let Ok(over) = std::env::var("RIPPLE_DATA_DIR") {
        if !over.trim().is_empty() {
            return absolute(&over);
        }
    }
    if let Some(Value::String(dir)) = read_config().get("dataDir") {
        if !dir.trim().is_empty() {
            return absolute(dir);
        }
    }
    default_data_dir()
}

pub fn set_data_dir(dir: Option<&str>) -> std::io::Result<PathBuf> {
    let mut cfg = read_config();
    match dir.filter(|d| !d.trim().is_empty()) {
        Some(d) => {
            cfg.insert("dataDir".into(), Value::String(absolute(d).to_string_lossy().into_owned()));
        }
        None => {
            cfg.remove("dataDir");
        }
    }
    write_config(&cfg)?;
    Ok(cfg
        .get("dataDir")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(default_data_dir))
}

pub fn resolve_download_dir() -> Option<PathBuf> {
    match read_config().get("downloadDir") {
        Some(Value::String(d)) if !d.trim().is_empty() => Some(absolute(d)),
        _ => None,
    }
}

pub fn set_download_dir(dir: Option<&str>) -> std::io::Result<Option<PathBuf>> {
    let mut cfg = read_config();
    match dir.filter(|d| !d.trim().is_empty()) {
        Some(d) => {
            cfg.insert("downloadDir".into(), Value::String(absolute(d).to_string_lossy().into_owned()));
        }
        None => {
            cfg.remove("downloadDir");
        }
    }
    write_config(&cfg)?;
    Ok(cfg.get("downloadDir").and_then(Value::as_str).map(PathBuf::from))
}

pub fn default_downloads_dir() -> PathBuf {
    dirs::download_dir().unwrap_or_else(|| home().join("Downloads"))
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub custom_lyrics_dir: PathBuf,
    pub lyrics_dir: PathBuf,
    pub thumbnails_dir: PathBuf,
    pub metadata_path: PathBuf,
    pub library_dir: PathBuf,
    pub library_path: PathBuf,
    pub bin_dir: PathBuf,
    pub startup_path: PathBuf,
    pub themes_dir: PathBuf,
    pub plugins_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl Paths {
    pub fn new(data_dir: PathBuf) -> Self {
        Paths {
            cache_dir: data_dir.join("tracks"),
            custom_lyrics_dir: data_dir.join("custom-lyrics"),
            lyrics_dir: data_dir.join("lyrics"),
            thumbnails_dir: data_dir.join("thumbnails"),
            metadata_path: data_dir.join("metadata.json"),
            library_dir: data_dir.join("library"),
            library_path: data_dir.join("library.json"),
            bin_dir: data_dir.join("bin"),
            startup_path: data_dir.join("startup.json"),
            themes_dir: data_dir.join("themes"),
            plugins_dir: data_dir.join("plugins"),
            log_dir: data_dir.join("logs"),
            data_dir,
        }
    }

    pub fn ensure_dirs(&self) {
        for d in [
            &self.cache_dir,
            &self.custom_lyrics_dir,
            &self.lyrics_dir,
            &self.thumbnails_dir,
            &self.library_dir,
        ] {
            let _ = fs::create_dir_all(d);
        }
    }

    fn tutorial_markers(&self) -> [PathBuf; 1] {
        [self.data_dir.join(".rippletutorial")]
    }

    pub fn tutorial_done(&self) -> bool {
        self.tutorial_markers().iter().any(|p| p.exists())
    }

    pub fn mark_tutorial_done(&self) -> bool {
        for file in self.tutorial_markers() {
            if let Some(parent) = file.parent() {
                if fs::create_dir_all(parent).is_err() {
                    continue;
                }
            }
            if fs::write(&file, "1").is_ok() {
                return true;
            }
        }
        false
    }

    pub fn read_startup_config(&self) -> Map<String, Value> {
        fs::read_to_string(&self.startup_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    }

    pub fn write_startup_config(&self, patch: Map<String, Value>) -> bool {
        let mut cfg = self.read_startup_config();
        cfg.extend(patch);
        fs::create_dir_all(&self.data_dir).is_ok()
            && fs::write(&self.startup_path, Value::Object(cfg).to_string()).is_ok()
    }
}

pub async fn copy_dir_contents(from: &Path, to: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(to).await?;
    let mut entries = tokio::fs::read_dir(from).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        if name == "config.json" {
            continue;
        }
        let kind = entry.file_type().await?;
        let src = entry.path();
        let dest = to.join(&name);
        if kind.is_dir() {
            Box::pin(copy_dir_contents(&src, &dest)).await?;
        } else if kind.is_file() {
            tokio::fs::copy(&src, &dest).await?;
        }
    }
    Ok(())
}

pub async fn remove_dir_contents(dir: &Path) -> std::io::Result<()> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_name() == "config.json" {
            continue;
        }
        let kind = entry.file_type().await?;
        let target = entry.path();
        if kind.is_dir() {
            let _ = Box::pin(remove_dir_contents(&target)).await;
            let _ = tokio::fs::remove_dir(&target).await;
        } else if kind.is_file() {
            let _ = tokio::fs::remove_file(&target).await;
        }
    }
    Ok(())
}

fn is_executable_file(p: &Path) -> bool {
    match fs::metadata(p) {
        Ok(m) if m.is_file() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        _ => false,
    }
}

pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
    }
    let exts: &[&str] = if IS_WIN { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    for dir in dirs {
        for ext in exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

static BINARY_CACHE: LazyLock<Mutex<HashMap<String, Option<PathBuf>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static EXTRA_BIN_DIRS: LazyLock<Mutex<Vec<PathBuf>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub fn add_bundled_bin_dir(dir: PathBuf) {
    EXTRA_BIN_DIRS.lock().push(dir);
}

pub fn resolve_binary(paths: &Paths, name: &str) -> Option<PathBuf> {
    if let Some(hit) = BINARY_CACHE.lock().get(name) {
        return hit.clone();
    }
    let exe = if IS_WIN { format!("{name}.exe") } else { name.to_string() };
    let env_key = format!(
        "RIPPLE_{}",
        name.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase()
    );
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(v) = std::env::var(&env_key) {
        if !v.is_empty() {
            candidates.push(PathBuf::from(v));
        }
    }
    candidates.push(paths.bin_dir.join(&exe));
    for dir in EXTRA_BIN_DIRS.lock().iter() {
        candidates.push(dir.join(&exe));
    }
    if let Ok(cur) = std::env::current_exe() {
        if let Some(dir) = cur.parent() {
            candidates.push(dir.join("bin").join(&exe));
        }
    }
    let mut found = candidates.into_iter().find(|c| is_executable_file(c)).or_else(|| find_in_path(name));
    if found.is_none() && !IS_WIN {
        let local = home().join(".local").join("bin").join(&exe);
        if is_executable_file(&local) {
            found = Some(local);
        }
    }
    BINARY_CACHE.lock().insert(name.to_string(), found.clone());
    found
}

pub fn clear_binary_cache() {
    BINARY_CACHE.lock().clear();
}

pub fn yt_dlp_command(paths: &Paths, args: &[String]) -> Result<(PathBuf, Vec<String>), String> {
    let bin = resolve_binary(paths, "yt-dlp").ok_or_else(|| {
        "yt-dlp not found. Put yt-dlp and ffmpeg into the bin folder next to the app, or add them to PATH."
            .to_string()
    })?;
    let mut full = Vec::new();
    if let Some(ffmpeg) = resolve_binary(paths, "ffmpeg") {
        if find_in_path("ffmpeg").is_none() {
            if let Some(dir) = ffmpeg.parent() {
                full.push("--ffmpeg-location".to_string());
                full.push(dir.to_string_lossy().into_owned());
            }
        }
    }
    full.extend(args.iter().cloned());
    Ok((bin, full))
}
