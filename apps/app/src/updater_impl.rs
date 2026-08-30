use crate::api::Result;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use tauri::http::HeaderValue;
use tauri::http::header::ACCEPT;
use tauri::{Manager, ResourceId, Runtime, Webview};
use tauri_plugin_http::reqwest;
use tauri_plugin_http::reqwest::ClientBuilder;
use tauri_plugin_updater::{Error, Update, UpdaterExt};
use theseus::{
    LoadingBarType, emit_loading, init_loading, launcher_user_agent, settings,
};
use tokio::time::Instant;
use url::Url;

const UPDATE_SERVER_LATEST_URL: &str = "https://update.axlmc.org/latest";

// Debian and derivatives update via the apt package manager. The whole
// operation (repo setup script plus package install) runs as a single
// `pkexec` invocation so the polkit authorization prompt appears only once.
const AXOLOTL_APT_SETUP_URL: &str = "https://ppa.axlmc.org/setup.sh";
const AXOLOTL_APT_PACKAGE: &str = "axolotl-launcher";

// The updater plugin builds `Update` with no request timeout, so a stalled
// connection would hang the download forever. Bound the whole download.
const UPDATE_DOWNLOAD_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(15 * 60);

// ── Shared types ─────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateMetadata {
    rid: ResourceId,
    current_version: String,
    version: String,
    date: Option<String>,
    body: Option<String>,
    published_at: Option<String>,
    force_update: bool,
    raw_json: serde_json::Value,
}

#[derive(Default)]
pub struct PendingUpdateData(pub Mutex<Option<(Arc<Update>, Vec<u8>)>>);

// ── Updater plugin helpers ───────────────────────────────────────

fn update_channel(channel: &str) -> Result<&str> {
    match channel {
        "release" | "beta" => Ok(channel),
        _ => Err(theseus::Error::from(theseus::ErrorKind::OtherError(
            format!("Unknown update channel: {channel}"),
        ))
        .into()),
    }
}

fn update_platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok("windows-x86_64"),
        ("linux", "x86_64") => Ok("linux-x86_64"),
        ("linux", "aarch64") => Ok("linux-aarch64"),
        ("macos", "x86_64") => Ok("darwin-x86_64"),
        ("macos", "aarch64") => Ok("darwin-aarch64"),
        (os, arch) => {
            Err(theseus::Error::from(theseus::ErrorKind::OtherError(format!(
                "Unsupported updater platform: {os}-{arch}"
            )))
            .into())
        }
    }
}

fn update_endpoint() -> Result<Url> {
    Url::parse(UPDATE_SERVER_LATEST_URL).map_err(|error| {
        theseus::Error::from(theseus::ErrorKind::OtherError(error.to_string()))
            .into()
    })
}

/// Build the platform-updater with the given endpoints and run a check.
async fn check_with_endpoints<R: Runtime>(
    webview: &Webview<R>,
    channel: &str,
) -> Result<Option<Update>> {
    let channel = update_channel(channel)?;
    let platform = update_platform()?;
    let current_version =
        webview.app_handle().package_info().version.to_string();
    let mut updater = webview
        .updater_builder()
        .endpoints(vec![update_endpoint()?])?
        .header("Accept", "application/json")?
        .header("X-Axolotl-Channel", channel)?
        .header("X-Axolotl-Platform", platform)?
        .header("X-Axolotl-Version", current_version)?;

    #[cfg(target_os = "windows")]
    {
        let install_dir = std::env::current_exe()
            .map_err(|error| {
                theseus::Error::from(theseus::ErrorKind::OtherError(format!(
                    "Failed to resolve current executable: {error}"
                )))
            })?
            .parent()
            .ok_or_else(|| {
                theseus::Error::from(theseus::ErrorKind::OtherError(
                    "Current executable has no parent directory".to_string(),
                ))
            })?
            .to_path_buf();

        tracing::debug!(
            install_dir = %install_dir.display(),
            "Using current executable directory for Windows app updates"
        );
        updater = updater.installer_arg(format!(
            "/INSTALL_DIR=\"{}\"",
            install_dir.display()
        ));
    }

    let updater = updater.build()?;
    updater.check().await.map_err(Into::into)
}

/// Check the updater manifest through the configured Update Server endpoint.
async fn check_with_updater<R: Runtime>(
    webview: &Webview<R>,
    channel: &str,
) -> Result<Option<UpdateMetadata>> {
    let Some(mut update) = check_with_endpoints(webview, channel).await? else {
        return Ok(None);
    };
    update.timeout = Some(UPDATE_DOWNLOAD_TIMEOUT);

    let published_at = update
        .raw_json
        .get("published_at")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let force_update = update
        .raw_json
        .get("force_update")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let metadata = UpdateMetadata {
        rid: webview.resources_table().add(update.clone()),
        current_version: update.current_version.clone(),
        version: update.version.clone(),
        date: None,
        body: update.body.clone(),
        published_at,
        force_update,
        raw_json: update.raw_json,
    };

    Ok(Some(metadata))
}

// ── Tauri commands ───────────────────────────────────────────────

#[tauri::command]
pub async fn check_app_update<R: Runtime>(
    webview: Webview<R>,
    channel: String,
) -> Result<Option<UpdateMetadata>> {
    check_with_updater(&webview, &channel).await
}

// Reimplementation of Update::download mostly, minus the actual download part
#[tauri::command]
pub async fn get_update_size<R: Runtime>(
    webview: Webview<R>,
    rid: ResourceId,
) -> Result<Option<u64>> {
    let update = webview.resources_table().get::<Update>(rid)?;

    let mut headers = update.headers.clone();
    if !headers.contains_key(ACCEPT) {
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/octet-stream"),
        );
    }

    let mut request = ClientBuilder::new().user_agent(launcher_user_agent());
    if let Some(timeout) = update.timeout {
        request = request.timeout(timeout);
    }
    if let Some(ref proxy) = update.proxy {
        let proxy = reqwest::Proxy::all(proxy.as_str())?;
        request = request.proxy(proxy);
    }
    let response = request
        .build()?
        .head(update.download_url.clone())
        .headers(headers)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(Error::Network(format!(
            "Download request failed with status: {}",
            response.status()
        ))
        .into());
    }

    let content_length = response
        .headers()
        .get("Content-Length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());

    Ok(content_length)
}

#[tauri::command]
pub async fn enqueue_update_for_installation<R: Runtime>(
    webview: Webview<R>,
    rid: ResourceId,
) -> Result<()> {
    let pending_data = webview.state::<PendingUpdateData>().inner();

    let update = webview.resources_table().get::<Update>(rid)?;

    let progress = init_loading(
        LoadingBarType::LauncherUpdate {
            version: update.version.clone(),
            current_version: update.current_version.clone(),
        },
        1.0,
        "Downloading update...",
    )
    .await?;

    let download_start = Instant::now();
    let update_data = update
        .download(
            |chunk_size, total_size| {
                let Some(total_size) = total_size else {
                    return;
                };
                if let Err(e) = emit_loading(
                    &progress,
                    chunk_size as f64 / total_size as f64,
                    None,
                ) {
                    tracing::error!(
                        "Failed to update download progress bar: {e}"
                    );
                }
            },
            || {},
        )
        .await?;
    let download_duration = download_start.elapsed();
    tracing::info!("Downloaded update in {download_duration:?}");

    pending_data
        .0
        .lock()
        .unwrap()
        .replace((update, update_data));

    Ok(())
}

#[tauri::command]
pub fn remove_enqueued_update<R: Runtime>(webview: Webview<R>) {
    let pending_data = webview.state::<PendingUpdateData>().inner();
    pending_data.0.lock().unwrap().take();
}

// ── Debian / derivatives apt update ─────────────────────────────

/// Whether this Linux system updates Axolotl through apt (Debian and its
/// derivatives) and has `pkexec` available for a single privileged prompt.
#[tauri::command]
pub fn is_apt_linux() -> bool {
    #[cfg(target_os = "linux")]
    {
        let debian_like = std::path::Path::new("/etc/debian_version").exists()
            || std::path::Path::new("/etc/apt").is_dir()
            || std::path::Path::new("/usr/bin/apt-get").exists();
        let has_pkexec = ["/usr/bin/pkexec", "/bin/pkexec"]
            .iter()
            .any(|path| std::path::Path::new(path).exists());
        debian_like && has_pkexec
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Update Axolotl on Debian and its derivatives through apt, prompting for
/// root once via `pkexec`. Runs the repo setup script and the package
/// install in a single privileged shell so only one authorization is asked.
#[tauri::command]
pub async fn install_apt_update(version: String) -> Result<()> {
    if !is_apt_linux() {
        return Err(theseus::Error::from(theseus::ErrorKind::OtherError(
            "apt updates are only supported on Debian-based Linux systems with pkexec"
                .to_string(),
        ))
        .into());
    }

    // Everything runs as root under one pkexec prompt; no `sudo` needed inside.
    let script = format!(
        "curl -fsSL {AXOLOTL_APT_SETUP_URL} | bash && \
         apt-get update && \
         apt-get install -y {AXOLOTL_APT_PACKAGE}"
    );

    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("pkexec")
            .arg("sh")
            .arg("-c")
            .arg(&script)
            .output()
    })
    .await
    .map_err(|join| {
        theseus::Error::from(theseus::ErrorKind::OtherError(format!(
            "Failed to run the apt updater: {join}"
        )))
    })?
    .map_err(|io| {
        theseus::Error::from(theseus::ErrorKind::OtherError(format!(
            "Failed to start pkexec: {io}"
        )))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(theseus::Error::from(theseus::ErrorKind::OtherError(
            format!("apt update failed: {}", stderr.trim()),
        ))
        .into());
    }

    // Persist the post-update announcement trigger so the new release notes
    // show after the app restarts into the freshly installed version.
    let mut current = settings::get().await?;
    current.pending_update_toast_for_version = Some(version);
    settings::set(current).await?;

    Ok(())
}
