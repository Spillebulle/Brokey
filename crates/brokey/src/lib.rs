//! The Tauri application. `run` builds the window; `cli` is the text mode.
//!
//! `commands` holds every `#[tauri::command]`, `state` what they share,
//! `settings` the `key = value` file. The core does everything else.

pub mod cli;
pub mod commands;
pub mod settings;
pub mod setup;
pub mod show;
pub mod state;

pub fn run() {
    run_showing(None);
}

/// The window, sent to one installed application's page once the page has
/// loaded when `show` is given (`brokey --show <source>:<id>`, `show.rs`).
pub fn run_showing(show: Option<brokey_core::PackageRef>) {
    // WebKitGTK renders black on some NVIDIA drivers when it uses DMA-BUF
    // buffers; the environment variable is WebKit's own switch for it. Set
    // before the webview exists, only where an NVIDIA device is present, and
    // never overriding a value the user chose.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERING").is_none()
        && brokey_core::system::has_nvidia()
    {
        // SAFETY: called before any other thread exists.
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERING", "1") };
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_log::Builder::new().build())
        // The page opens homepages and release pages in the browser through
        // this; `capabilities/default.json` limits it to http and https.
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_window_state::Builder::new().build())
        // A second `brokey` exits here and the running window comes forward;
        // a second `brokey --show <ref>` also sends it to that page.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            use tauri::Manager;
            show::hand_over(app, &args);
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .manage(show::Pending::new(show))
        // Settings are read here, before the window; the store waits for
        // the first command that needs it so the window opens before the
        // package databases are read.
        .manage(state::AppState::new())
        .invoke_handler(commands::handler())
        .run(tauri::generate_context!())
        .expect("error while running Brokey");
}
