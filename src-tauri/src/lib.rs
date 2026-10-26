mod audio_output;
mod commands;
mod discord;
mod launcher;
mod logger;
mod paths;
pub mod server;
mod state;
#[cfg(windows)]
mod thumbbar;
mod updater;
mod window;

use paths::Paths;
use state::AppState;
use std::io::Write;
use tauri::{Manager, RunEvent};
use tauri_plugin_global_shortcut::ShortcutState;

fn run_server_only() {
    let args: Vec<String> = std::env::args().collect();
    let port = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(0);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(async move {
        let p = Paths::new(paths::resolve_data_dir());
        logger::init(p.log_dir.clone());
        let ctx = server::Ctx::new(p);
        let bound = server::start(ctx.clone(), port).await.expect("bind");
        println!("RIPPLE_SERVER {bound} {}", ctx.token);
        let _ = std::io::stdout().flush();
        let _ = tokio::task::spawn_blocking(|| {
            let mut sink = String::new();
            while std::io::stdin().read_line(&mut sink).map(|n| n > 0).unwrap_or(false) {
                sink.clear();
            }
        })
        .await;
    });
}

pub fn run() {
    if std::env::args().any(|a| a == "--server-only") {
        return run_server_only();
    }

    let p = Paths::new(paths::resolve_data_dir());
    logger::init(p.log_dir.clone());
    let multi_instance = p.read_startup_config().get("multiInstance").map(server::textutil::truthy).unwrap_or(false);

    let mut builder = tauri::Builder::default();
    if !multi_instance {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if args.iter().any(|a| a == "--quit") {
                window::quit_app(app);
            } else {
                window::show_main(app);
            }
        }));
    }
    builder = builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        if let Some(cmd) = window::media_command_for(shortcut) {
                            commands::emit_main(app, "media-command", serde_json::json!(cmd));
                        }
                    }
                })
                .build(),
        );

    let app = builder
        .setup(move |app| {
            if std::env::args().any(|a| a == "--quit") {
                std::process::exit(0);
            }
            if let Ok(res) = app.path().resource_dir() {
                paths::add_bundled_bin_dir(res.join("bin"));
            }
            let ctx = server::Ctx::new(p.clone());
            tauri::async_runtime::block_on(server::start(ctx.clone(), server::DEFAULT_PORT))?;
            let app_data = app.path().app_data_dir().unwrap_or_else(|_| p.data_dir.join("tauri"));
            app.manage(AppState::new(ctx, p.clone(), app_data));
            let handle = app.handle().clone();
            app.state::<AppState>().restore_media_scope(&handle);
            if let Err(e) = window::setup_tray(&handle) {
                logger::error("Tray creation failed", e);
            }
            window::create_main_window(&handle)?;
            let saved_id = p.read_startup_config().get("discordClientId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            discord::get().set_client_id(&saved_id);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::window_minimize,
            commands::window_toggle_maximize,
            commands::window_close,
            commands::window_start_drag,
            commands::set_zoom,
            commands::toggle_mini_window,
            commands::get_server_port,
            commands::get_gpu_compositing,
            commands::get_app_version,
            commands::update_check,
            commands::update_action,
            commands::launcher_info,
            commands::get_audio_output_info,
            commands::restart_app,
            commands::set_tray_on_close,
            commands::get_multi_instance,
            commands::set_multi_instance,
            commands::set_media_keys,
            commands::set_thumbbar_state,
            commands::media_keys_active,
            commands::welcome_finish,
            commands::take_welcome_result,
            commands::discord_rpc_update,
            commands::discord_rpc_clear,
            commands::discord_rpc_set_enabled,
            commands::discord_rpc_get_enabled,
            commands::discord_rpc_get_connected,
            commands::discord_rpc_get_status,
            commands::discord_rpc_get_client_id,
            commands::discord_rpc_set_client_id,
            commands::pick_media,
            commands::pick_playlist_cover,
            commands::remove_playlist_cover,
            commands::import_audio_files,
            commands::get_cache_size,
            commands::clear_cache,
            commands::get_data_dir,
            commands::pick_data_dir,
            commands::set_data_dir,
            commands::get_download_dir,
            commands::pick_download_dir,
            commands::set_download_dir,
            commands::themes_list,
            commands::themes_import,
            commands::themes_set_enabled,
            commands::themes_disable_all,
            commands::themes_remove,
            commands::read_app_css,
            commands::plugins_list,
            commands::plugins_set_enabled,
            commands::plugins_disable_all,
            commands::plugins_open_folder,
            commands::plugins_remove,
            commands::plugins_import,
            commands::open_external,
            commands::open_in_browser,
            commands::log_error,
            commands::open_logs_folder,
            commands::get_diagnostics,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Ripple");

    app.run(|_app, event| {
        if let RunEvent::Exit = event {
            discord::get().shutdown();
        }
    });
}
