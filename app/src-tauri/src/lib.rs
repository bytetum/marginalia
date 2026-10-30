//! Marginalia app shell. The UI is the server's own web page (`../../web`, shared, not copied); this crate adds what a
//! webview cannot do by itself: keep the access token in the OS credential store, hand shared links from the iOS and
//! Android share sheets to the page, and open article links in the system browser.

mod secret;

use secret::Secrets;
use serde::Serialize;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: String,
    /// Where the token is kept, as a phrase ("the system keychain", ...).
    token_storage: &'static str,
    /// True when this build can receive shared links (iOS and Android).
    share: bool,
}

#[tauri::command]
fn app_info(app: AppHandle, secrets: State<'_, Secrets>) -> AppInfo {
    AppInfo {
        version: app.package_info().version.to_string(),
        token_storage: secrets.storage(),
        share: cfg!(mobile),
    }
}

#[tauri::command]
fn get_token(secrets: State<'_, Secrets>) -> Option<String> {
    secrets.get()
}

/// Stores the token; answers where it went (see `AppInfo::token_storage`).
#[tauri::command]
fn set_token(token: String, secrets: State<'_, Secrets>) -> Result<&'static str, String> {
    secrets.set(&token)
}

#[tauri::command]
fn clear_token(secrets: State<'_, Secrets>) -> Result<(), String> {
    secrets.clear()
}

/// True for the app's own pages. Nothing else may be loaded into the webview: the page talks to the server with
/// `fetch`, and links to other sites open in the system browser (tauri-plugin-opener).
fn is_app_url(url: &tauri::Url) -> bool {
    match url.scheme() {
        // tauri://localhost on macOS, iOS and Linux
        "tauri" => url.host_str() == Some("localhost"),
        // http(s)://tauri.localhost on Windows and Android
        "http" | "https" => url.host_str() == Some("tauri.localhost"),
        "about" => url.as_str() == "about:blank",
        _ => false,
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());

    // The share target. The deep-link plugin has to be registered before the share-target plugin.
    #[cfg(target_os = "ios")]
    let builder = builder.plugin(tauri_plugin_deep_link::init());
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_mobile_sharetarget::init());

    builder
        .invoke_handler(tauri::generate_handler![app_info, get_token, set_token, clear_token])
        .setup(|app| {
            let data_dir = app.path().app_local_data_dir()?;
            app.manage(Secrets::new(data_dir));

            // The window is built here (not in tauri.conf.json) to be able to refuse navigation away from the app.
            let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .on_navigation(|url| is_app_url(url))
                .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny);
            #[cfg(desktop)]
            let window = window.title("Marginalia").inner_size(960.0, 800.0).min_inner_size(360.0, 480.0);
            window.build()?;

            #[cfg(target_os = "ios")]
            ios_share_links(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Marginalia app");
}

/// iOS: the share extension opens `marginalia://share?url=<percent-encoded link>`. Queue it for the page (which pops
/// it when it is ready, so a cold start by the share loses nothing) and tell a page that is already running.
#[cfg(target_os = "ios")]
fn ios_share_links(app: &tauri::App) -> tauri::Result<()> {
    use tauri::Emitter as _;
    use tauri_plugin_deep_link::DeepLinkExt as _;

    fn is_share(url: &tauri::Url) -> bool {
        url.scheme() == "marginalia" && url.host_str() == Some("share")
    }

    if let Ok(Some(urls)) = app.deep_link().get_current() {
        for url in urls.iter().filter(|u| is_share(u)) {
            tauri_plugin_mobile_sharetarget::push_new_intent(url.to_string());
        }
    }
    let handle = app.handle().clone();
    app.deep_link().on_open_url(move |event| {
        for url in event.urls().iter().filter(|u| is_share(u)) {
            tauri_plugin_mobile_sharetarget::push_new_intent(url.to_string());
            let _ = handle.emit("new-intent", ());
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_app_url;

    fn url(s: &str) -> tauri::Url {
        s.parse().unwrap()
    }

    #[test]
    fn only_the_apps_own_origins_may_be_loaded() {
        for ok in ["tauri://localhost/", "tauri://localhost/index.html#/a/1", "http://tauri.localhost/", "https://tauri.localhost/#/s", "about:blank"] {
            assert!(is_app_url(&url(ok)), "{ok}");
        }
        for bad in [
            "https://example.com/",
            "http://tauri.localhost.evil.example/",
            "https://evil.example/tauri.localhost",
            "tauri://evil/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "about:srcdoc",
            "http://127.0.0.1:7878/",
        ] {
            assert!(!is_app_url(&url(bad)), "{bad}");
        }
    }
}
