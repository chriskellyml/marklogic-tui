use std::path::PathBuf;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

static INSTALLED_APP: Mutex<Option<PathBuf>> = Mutex::new(None);
static UPDATING: AtomicBool = AtomicBool::new(false);

#[tauri::command]
pub async fn update_app(events: tauri::ipc::Channel<String>) -> Result<(), String> {
    if UPDATING.swap(true, Ordering::SeqCst) {
        return Err("An app update is already running.".into());
    }
    let result = tauri::async_runtime::spawn_blocking(move || build_and_install(events)).await;
    UPDATING.store(false, Ordering::SeqCst);
    let path = result.map_err(|error| format!("Update task failed: {error}"))??;
    *INSTALLED_APP
        .lock()
        .map_err(|_| "Update state is unavailable.")? = Some(path);
    Ok(())
}

#[tauri::command]
pub async fn restart_updated_app(app: tauri::AppHandle) -> Result<(), String> {
    let path = INSTALLED_APP
        .lock()
        .map_err(|_| "Update state is unavailable.")?
        .clone()
        .ok_or("No installed update is ready to restart.")?;
    let output = tauri::async_runtime::spawn_blocking(move || {
        std::process::Command::new("/usr/bin/open")
            .arg("-n")
            .arg(path)
            .output()
    })
    .await
    .map_err(|error| format!("Could not start the updated app: {error}"))?
    .map_err(|error| format!("Could not start the updated app: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Could not start the updated app: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    app.exit(0);
    Ok(())
}

#[cfg(target_os = "macos")]
fn build_and_install(events: tauri::ipc::Channel<String>) -> Result<PathBuf, String> {
    use std::{
        collections::VecDeque,
        io::{BufRead, BufReader},
        process::{Command, Stdio},
        sync::Arc,
    };

    let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .ok_or("Cannot locate the source checkout.")?
        .to_path_buf();
    if !checkout.join("desktop/package.json").is_file() {
        return Err(format!(
            "Source checkout not found at {}.",
            checkout.display()
        ));
    }
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let _running_app = executable.ancestors().find(|path| path.extension().is_some_and(|ext| ext == "app"))
        .ok_or("Updates require the packaged macOS app; this instance appears to be running in development mode.")?;
    // All packaged copies update the same user-facing installation.
    let parent = PathBuf::from(
        std::env::var_os("HOME").ok_or("Cannot locate the user Applications folder")?,
    )
    .join("Applications");
    std::fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    let app = parent.join("MarkLogic Workbench.app");
    let name = "MarkLogic Workbench.app";
    let stage = parent.join(format!(".{name}.update-{}", std::process::id()));
    let backup = parent.join(format!(".{name}.backup-{}", std::process::id()));
    if stage.exists() || backup.exists() {
        return Err(
            "An update staging folder already exists; move it aside before retrying.".into(),
        );
    }

    // A separate target keeps Tauri from replacing the running app while building.
    let target = checkout.join("target/desktop-update");
    let _ = events.send("Starting build…".into());
    let mut child = Command::new("/bin/zsh")
        .args([
            "-lc",
            "npm --prefix desktop run tauri -- build --bundles app",
        ])
        .current_dir(&checkout)
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_INCREMENTAL", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start the desktop build: {e}"))?;
    let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|stream| {
        let events = events.clone();
        let tail = tail.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let line: String = line.chars().take(500).collect();
                if let Ok(mut tail) = tail.lock() {
                    if tail.len() == 20 {
                        tail.pop_front();
                    }
                    tail.push_back(line.clone());
                }
                let _ = events.send(line);
            }
        })
    })
    .collect();
    let status = child
        .wait()
        .map_err(|e| format!("Could not wait for the desktop build: {e}"))?;
    for reader in readers {
        let _ = reader.join();
    }
    if !status.success() {
        let details = tail
            .lock()
            .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        return Err(format!("Desktop build failed. {details}"));
    }
    let _ = events.send("Installing the updated app…".into());
    let built = target.join("release/bundle/macos/MarkLogic Workbench.app");
    if !built.join("Contents/MacOS").is_dir() {
        return Err("The build completed but did not produce the desktop app bundle.".into());
    }
    let copy = Command::new("/usr/bin/ditto")
        .arg(&built)
        .arg(&stage)
        .output()
        .map_err(|e| format!("Could not copy the update: {e}"))?;
    if !copy.status.success() {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(format!(
            "Could not copy the update: {}",
            String::from_utf8_lossy(&copy.stderr)
        ));
    }
    let had_app = app.exists();
    if let Err(error) = if had_app {
        std::fs::rename(&app, &backup)
    } else {
        Ok(())
    } {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(format!(
            "Could not prepare the installed app for replacement: {error}"
        ));
    }
    if let Err(error) = std::fs::rename(&stage, &app) {
        let restore = if had_app {
            std::fs::rename(&backup, &app)
        } else {
            Ok(())
        };
        let _ = std::fs::remove_dir_all(&stage);
        return Err(match restore {
            Ok(()) => {
                format!("Could not install the update; the original app was restored: {error}")
            }
            Err(restore_error) => format!(
                "Could not install the update: {error}. Restore the original app from {}: {restore_error}",
                backup.display()
            ),
        });
    }
    let _ = std::fs::remove_dir_all(backup);
    Ok(app.to_path_buf())
}

#[cfg(not(target_os = "macos"))]
fn build_and_install(_events: tauri::ipc::Channel<String>) -> Result<PathBuf, String> {
    Err("Local app updates are currently available for packaged macOS builds only.".into())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    build: &'static str,
    executable: String,
}
#[tauri::command]
pub fn build_info() -> Result<BuildInfo, String> {
    Ok(BuildInfo {
        build: env!("WORKBENCH_BUILD"),
        executable: std::env::current_exe()
            .map_err(|e| e.to_string())?
            .display()
            .to_string(),
    })
}
