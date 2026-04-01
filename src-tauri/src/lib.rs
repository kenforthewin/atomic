use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::Manager;
use tauri_plugin_shell::ShellExt;
use tracing;

const SIDECAR_PORT: u16 = 44380;
const HEALTH_POLL_INTERVAL_MS: u64 = 100;
const HEALTH_TIMEOUT_MS: u64 = 10_000;

/// Config returned to the frontend so it can connect to the sidecar
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalServerConfig {
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    #[serde(rename = "authToken")]
    pub auth_token: String,
}

/// Holds the sidecar child process for cleanup on exit
struct SidecarChild(tauri_plugin_shell::process::CommandChild);

struct SidecarState {
    child: Mutex<Option<SidecarChild>>,
}

/// Mutable token state — updated by `save_local_token` after claim.
struct TokenState {
    token: Mutex<String>,
    data_dir: std::path::PathBuf,
}

#[tauri::command]
fn get_local_server_config(
    config: tauri::State<'_, LocalServerConfig>,
    token_state: tauri::State<'_, TokenState>,
) -> LocalServerConfig {
    let mut cfg = config.inner().clone();
    // Use the latest token (may have been updated by save_local_token after claim)
    if let Ok(token) = token_state.token.lock() {
        cfg.auth_token = token.clone();
    }
    cfg
}

/// Save the auth token to disk after a successful claim.
/// Called by the frontend so the token persists across restarts.
#[tauri::command]
fn save_local_token(
    token: String,
    token_state: tauri::State<'_, TokenState>,
) -> Result<(), String> {
    let token_file = token_state.data_dir.join("local_server_token");
    std::fs::write(&token_file, &token)
        .map_err(|e| format!("Failed to write token file: {e}"))?;
    if let Ok(mut t) = token_state.token.lock() {
        *t = token;
    }
    Ok(())
}

/// Read the cached auth token from disk if it exists.
/// Does NOT open the database — safe to call regardless of encryption state.
fn read_cached_token(app_data_dir: &std::path::Path) -> Option<String> {
    let token_file = app_data_dir.join("local_server_token");
    std::fs::read_to_string(token_file)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "atomic_lib=info,atomic_core=info,warn".parse().unwrap()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let app_data_dir = app
                .path()
                .app_data_dir()
                .expect("Failed to get app data directory");

            std::fs::create_dir_all(&app_data_dir)
                .expect("Failed to create app data directory");

            tracing::info!(path = ?app_data_dir, "Data directory");

            // Read cached token (does NOT open the database).
            // On fresh installs or if the file was lost, this returns None.
            // The frontend will handle claiming/setup via the sidecar's HTTP API.
            let auth_token = read_cached_token(&app_data_dir).unwrap_or_default();

            let base_url = format!("http://127.0.0.1:{}", SIDECAR_PORT);
            let config = LocalServerConfig {
                base_url: base_url.clone(),
                auth_token: auth_token.clone(),
            };
            app.manage(config.clone());
            app.manage(TokenState {
                token: Mutex::new(auth_token.clone()),
                data_dir: app_data_dir.clone(),
            });

            // Check if an Atomic server is already running on the port
            let health_url = format!("{}/health", base_url);
            let already_running = reqwest::blocking::Client::new()
                .get(&health_url)
                .timeout(std::time::Duration::from_millis(500))
                .send()
                .is_ok_and(|r| r.status().is_success());

            if already_running {
                tracing::info!(url = %base_url, "Atomic server already running, reusing it");
                app.manage(SidecarState {
                    child: Mutex::new(None),
                });
            } else {
                // Spawn atomic-server as a sidecar
                let shell = app.shell();
                let sidecar_cmd = shell
                    .sidecar("atomic-server")
                    .expect("Failed to create sidecar command")
                    .args([
                        "--data-dir",
                        app_data_dir.to_str().unwrap(),
                        "serve",
                        "--port",
                        &SIDECAR_PORT.to_string(),
                    ]);

                let (mut rx, child) =
                    sidecar_cmd.spawn().expect("Failed to spawn atomic-server sidecar");

                // Log sidecar output
                tauri::async_runtime::spawn(async move {
                    use tauri_plugin_shell::process::CommandEvent;
                    while let Some(event) = rx.recv().await {
                        match event {
                            CommandEvent::Stdout(line) => {
                                tracing::debug!(output = %String::from_utf8_lossy(&line), "sidecar stdout");
                            }
                            CommandEvent::Stderr(line) => {
                                tracing::debug!(output = %String::from_utf8_lossy(&line), "sidecar stderr");
                            }
                            CommandEvent::Terminated(payload) => {
                                tracing::info!(?payload, "sidecar terminated");
                                break;
                            }
                            CommandEvent::Error(err) => {
                                tracing::debug!(error = %err, "sidecar error");
                            }
                            _ => {}
                        }
                    }
                });

                app.manage(SidecarState {
                    child: Mutex::new(Some(SidecarChild(child))),
                });

                // Poll health endpoint until ready
                let start = std::time::Instant::now();
                loop {
                    if start.elapsed().as_millis() as u64 > HEALTH_TIMEOUT_MS {
                        tracing::warn!(timeout_ms = HEALTH_TIMEOUT_MS, "Sidecar health check timed out");
                        break;
                    }
                    match reqwest::blocking::Client::new()
                        .get(&health_url)
                        .timeout(std::time::Duration::from_millis(500))
                        .send()
                    {
                        Ok(resp) if resp.status().is_success() => {
                            tracing::debug!(url = %base_url, elapsed_ms = start.elapsed().as_millis(), "Sidecar ready");
                            break;
                        }
                        _ => {
                            std::thread::sleep(std::time::Duration::from_millis(HEALTH_POLL_INTERVAL_MS));
                        }
                    }
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_local_server_config,
            save_local_token,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // Kill sidecar on app exit
                if let Some(state) = app.try_state::<SidecarState>() {
                    if let Ok(mut child_opt) = state.child.lock() {
                        if let Some(SidecarChild(child)) = child_opt.take() {
                            tracing::info!("Shutting down sidecar");
                            let _ = child.kill();
                        }
                    }
                }
            }
        });
}
