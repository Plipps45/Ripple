(() => {
  if (window.__rippleBridgeInstalled) return;
  window.__rippleBridgeInstalled = true;

  const BOOT = window.__RIPPLE_BOOT__ || {};
  const pending = [];

  function internals() {
    return window.__TAURI_INTERNALS__;
  }

  function invoke(cmd, args) {
    const core = internals();
    if (core && typeof core.invoke === 'function') return core.invoke(cmd, args || {});
    return new Promise((resolve, reject) => {
      pending.push({ cmd, args, resolve, reject });
      flushSoon();
    });
  }

  let flushTimer = null;
  function flushSoon() {
    if (flushTimer) return;
    flushTimer = setTimeout(function flush() {
      flushTimer = null;
      const core = internals();
      if (!core) return flushSoon();
      while (pending.length) {
        const p = pending.shift();
        core.invoke(p.cmd, p.args || {}).then(p.resolve, p.reject);
      }
    }, 16);
  }

  const send = (cmd, args) => { invoke(cmd, args).catch(() => {}); };

  function listen(event, cb) {
    const attach = () => {
      const tauri = window.__TAURI__;
      if (!tauri || !tauri.event) return setTimeout(attach, 25);
      tauri.event.listen(event, (e) => {
        try { cb(e.payload); } catch (err) { console.error('[ripple-bridge]', err); }
      });
    };
    attach();
  }

  function fileUrl(path) {
    if (!path) return null;
    const tauri = window.__TAURI__;
    if (tauri && tauri.core && typeof tauri.core.convertFileSrc === 'function') return tauri.core.convertFileSrc(path);
    const enc = encodeURIComponent(path);
    return BOOT.isWindows ? `http://asset.localhost/${enc}` : `asset://localhost/${enc}`;
  }

  let zoomFactor = 1.0;

  window.electronAPI = {
    serverToken: BOOT.token || '',
    isWindows: !!BOOT.isWindows,
    minimizeWindow: () => send('window_minimize'),
    maximizeWindow: () => send('window_toggle_maximize'),
    closeWindow: () => send('window_close'),
    getServerPort: () => invoke('get_server_port'),
    getGpuCompositing: () => invoke('get_gpu_compositing'),
    pickMedia: () => invoke('pick_media').then(fileUrl),
    pickPlaylistCover: (playlistId) => invoke('pick_playlist_cover', { playlistId: String(playlistId) }).then(fileUrl),
    removePlaylistCover: (playlistId) => invoke('remove_playlist_cover', { playlistId: String(playlistId) }),
    importAudioFiles: () => invoke('import_audio_files'),
    onWindowVisibility: (cb) => listen('window-visibility', cb),
    setZoomFactor: (factor) => {
      const f = Number(factor);
      if (!Number.isFinite(f) || f <= 0) return;
      zoomFactor = f;
      send('set_zoom', { factor: f });
    },
    getZoomFactor: () => zoomFactor,

    discordRpcUpdate: (trackInfo) => send('discord_rpc_update', { info: trackInfo || null }),
    discordRpcClear: () => send('discord_rpc_clear'),
    discordRpcSetEnabled: (enabled) => send('discord_rpc_set_enabled', { enabled: !!enabled }),
    discordRpcGetEnabled: () => invoke('discord_rpc_get_enabled'),
    discordRpcGetConnected: () => invoke('discord_rpc_get_connected'),
    discordRpcGetStatus: () => invoke('discord_rpc_get_status'),
    discordRpcGetClientId: () => invoke('discord_rpc_get_client_id'),
    discordRpcSetClientId: (id) => invoke('discord_rpc_set_client_id', { id: String(id || '') }),
    getCacheSize: () => invoke('get_cache_size'),
    clearCache: () => invoke('clear_cache'),
    getDataDir: () => invoke('get_data_dir'),
    pickDataDir: () => invoke('pick_data_dir'),
    setDataDir: (dir, move) => invoke('set_data_dir', { dir: dir == null ? null : String(dir), moveFiles: !!move }),
    getDownloadDir: () => invoke('get_download_dir'),
    pickDownloadDir: () => invoke('pick_download_dir'),
    setDownloadDir: (dir) => invoke('set_download_dir', { dir: dir == null ? null : String(dir), reset: false }),
    resetDownloadDir: () => invoke('set_download_dir', { dir: null, reset: true }),
    restartApp: () => send('restart_app'),
    getAppVersion: () => invoke('get_app_version'),
    checkForUpdates: () => invoke('update_check'),
    updateAction: (action) => send('update_action', { action: String(action || '') }),
    onUpdateAvailable: (cb) => listen('update-available', cb),
    onUpdateProgress: (cb) => listen('update-progress', cb),
    onUpdateReady: (cb) => listen('update-ready', cb),
    onUpdateError: (cb) => listen('update-error', cb),
    onUpdateNotAvailable: (cb) => listen('update-not-available', cb),
    setTrayOnClose: (enabled) => send('set_tray_on_close', { enabled: !!enabled }),
    getMultiInstance: () => invoke('get_multi_instance'),
    setMultiInstance: (v) => send('set_multi_instance', { value: !!v }),
    onMediaCommand: (cb) => listen('media-command', cb),
    toggleMiniWindow: () => invoke('toggle_mini_window'),
    setMediaKeys: (v) => send('set_media_keys', { enabled: !!v }),
    mediaKeysActive: () => send('media_keys_active'),
    setThumbbarState: (s) => send('set_thumbbar_state', {
      playing: !!s.playing,
      shuffle: !!s.shuffle,
      repeat: !!s.repeat,
      accent: String(s.accent || '#ffffff')
    }),
    welcomeFinish: (detail) => send('welcome_finish', { detail: detail == null ? null : detail }),
    onWelcomeResult: (cb) => {
      invoke('take_welcome_result').then((result) => {
        if (result) {
          try { cb(result); } catch (err) { console.error('[ripple-bridge]', err); }
        }
      }).catch(() => {});
    },
    getLauncherInfo: () => invoke('launcher_info'),
    listThemes: () => invoke('themes_list'),
    importThemes: () => invoke('themes_import'),
    setThemeEnabled: (id, on) => send('themes_set_enabled', { id: String(id), on: !!on }),
    disableAllThemes: () => send('themes_disable_all'),
    removeTheme: (id) => send('themes_remove', { id: String(id) }),
    listPlugins: () => invoke('plugins_list'),
    setPluginEnabled: (file, on) => send('plugins_set_enabled', { file: String(file), on: !!on }),
    disableAllPlugins: () => send('plugins_disable_all'),
    openPluginsFolder: () => send('plugins_open_folder'),
    removePlugin: (file) => send('plugins_remove', { file: String(file) }),
    importPlugins: () => invoke('plugins_import'),
    getAppCss: () => invoke('read_app_css'),
    openExternal: (u) => send('open_external', { url: String(u || '') }),
    logError: (level, message, stack) => send('log_error', {
      level: String(level || 'ERROR'),
      message: String(message || ''),
      stack: stack == null ? null : String(stack),
    }),
    openLogsFolder: () => send('open_logs_folder'),
    getDiagnostics: () => invoke('get_diagnostics'),
    getAudioOutputInfo: () => invoke('get_audio_output_info'),
  };

  const openInBrowser = (url) => {
    if (typeof url === 'string' && /^(https?:|mailto:)/i.test(url)) send('open_in_browser', { url });
  };
  window.open = function (url) {
    try { openInBrowser(new URL(String(url), location.href).href); } catch (e) {}
    return null;
  };
  document.addEventListener('click', (e) => {
    const a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
    if (!a) return;
    const href = a.href || '';
    if (a.target === '_blank' || (/^https?:/i.test(href) && !href.startsWith(location.origin))) {
      if (/^(https?:|mailto:)/i.test(href)) {
        e.preventDefault();
        openInBrowser(href);
      }
    }
  }, true);

  const NO_DRAG_TAGS = new Set(['BUTTON', 'INPUT', 'SELECT', 'TEXTAREA', 'A', 'LABEL', 'VIDEO']);
  function appRegion(el) {
    for (let node = el; node && node.nodeType === 1; node = node.parentElement) {
      if (NO_DRAG_TAGS.has(node.tagName) || node.isContentEditable) return 'no-drag';
      const cs = getComputedStyle(node);
      const v = cs.getPropertyValue('-webkit-app-region') || cs.getPropertyValue('app-region') || cs.webkitAppRegion || '';
      if (v === 'drag' || v === 'no-drag') return v;
    }
    return 'none';
  }
  document.addEventListener('mousedown', (e) => {
    if (e.button !== 0 || appRegion(e.target) !== 'drag') return;
    if (e.detail === 2) {
      send('window_toggle_maximize');
    } else {
      send('window_start_drag');
    }
  }, true);
})();
