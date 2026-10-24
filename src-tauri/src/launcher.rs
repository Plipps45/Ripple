use serde_json::{json, Value};

#[cfg(windows)]
fn shortcut_path() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("Microsoft").join("Windows").join("Start Menu").join("Programs").join("Ripple.lnk"))
}

pub fn info() -> Value {
    #[cfg(windows)]
    {
        let installed = shortcut_path().is_some_and(|p| p.exists());
        json!({ "platform": "win32", "canInstall": true, "installed": installed })
    }
    #[cfg(not(windows))]
    {
        let platform = if cfg!(target_os = "macos") { "darwin" } else { "linux" };
        json!({ "platform": platform, "canInstall": false, "installed": false, "reason": "Shortcuts are created by the Ripple installer" })
    }
}

pub fn apply(enabled: bool, app_id: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let path = shortcut_path().ok_or("No Start menu folder")?;
        if !enabled {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            };
        }
        if path.exists() {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let app_id = app_id.to_string();
        std::thread::spawn(move || create_shortcut(&path, &exe, &app_id))
            .join()
            .map_err(|_| "Shortcut thread panicked".to_string())?
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = (enabled, app_id);
        Ok(())
    }
}

#[cfg(windows)]
fn create_shortcut(path: &std::path::Path, exe: &std::path::Path, app_id: &str) -> windows::core::Result<()> {
    use windows::core::{Interface, HSTRING};
    use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let result = (|| {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&HSTRING::from(exe.as_os_str()))?;
            if let Some(dir) = exe.parent() {
                link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
            }
            link.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0)?;
            link.SetDescription(&HSTRING::from("Music player"))?;

            let props: IPropertyStore = link.cast()?;
            props.SetValue(&PKEY_AppUserModel_ID, &PROPVARIANT::from(app_id))?;
            props.Commit()?;

            link.cast::<IPersistFile>()?.Save(&HSTRING::from(path.as_os_str()), true)
        })();
        if initialized {
            CoUninitialize();
        }
        result
    }
}
