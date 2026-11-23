use crate::workspace::{Connection, client_for, error, server_for};
use anyhow::{Context, Result, bail, ensure};
use marklogic_core::{client::ServerConfig, config::AppConfig};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        LazyLock,
        atomic::{AtomicU32, Ordering},
    },
};
use tauri::State;
use tempfile::{Builder, TempDir};
use tokio::{process::Command, sync::Mutex};

#[derive(Default)]
pub struct MlcpState {
    busy: Mutex<()>,
    pid: AtomicU32,
}

impl MlcpState {
    pub fn shutdown(&self) {
        let pid = self.pid.swap(0, Ordering::AcqRel);
        if pid == 0 {
            return;
        }
        #[cfg(unix)]
        unsafe {
            // The launcher and its Java child share a dedicated process group.
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .status();
        }
    }
}

#[derive(Deserialize)]
#[serde(
    tag = "direction",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Transfer {
    Pull {
        folder: PathBuf,
        selection: Selection,
        #[serde(default)]
        replace_id: Option<String>,
        #[serde(default)]
        job_id: Option<String>,
        #[serde(default)]
        replace: bool,
    },
    Push {
        folder: PathBuf,
        copy_permissions: bool,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum Selection {
    All,
    Collections(String),
    Directories(String),
}

#[tauri::command]
pub async fn mlcp_preview_uris(
    connection: Connection,
    selection: Selection,
) -> Result<Vec<String>, String> {
    let (kind, values) = match selection {
        Selection::All => ("all", Vec::new()),
        Selection::Collections(value) => ("collections", filter_entries(&value).map_err(error)?),
        Selection::Directories(value) => ("directories", filter_entries(&value).map_err(error)?),
    };
    let input = serde_json::json!({ "kind": kind, "values": values });
    let source = format!(
        r#"const input = {};
const query = input.kind === 'collections' ? cts.collectionQuery(input.values) :
  input.kind === 'directories' ? cts.directoryQuery(input.values, 'infinity') : cts.trueQuery();
JSON.stringify(Array.from(cts.uris('', ['document', 'item-order', 'ascending', 'limit=51'], query), String));"#,
        input
    );
    let query = async {
        let client = client_for(&AppConfig::load()?, &connection)?;
        let result = client.js_query(&source).await?;
        ensure!(result.len() == 1, "Expected one URI preview response");
        Ok::<Vec<String>, anyhow::Error>(serde_json::from_str(&result[0])?)
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), query)
        .await
        .map_err(|_| "URI preview timed out after 30 seconds".to_string())?
        .map_err(error)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferResult {
    archive: PathBuf,
    parts: usize,
    output: String,
}

#[tauri::command]
pub async fn mlcp_transfer(
    state: State<'_, MlcpState>,
    connection: Connection,
    transfer: Transfer,
    events: tauri::ipc::Channel<String>,
) -> Result<TransferResult, String> {
    let _permit = state
        .busy
        .try_lock()
        .map_err(|_| "An MLCP transfer is already running")?;
    let config = AppConfig::load().map_err(error)?;
    let server = server_for(&config, &connection).map_err(error)?;
    let job_root = match &transfer {
        Transfer::Pull {
            folder,
            job_id: Some(id),
            ..
        } => Some(job_directory(folder, id, true).map_err(error)?),
        _ => None,
    };
    let previous_log = job_root.as_ref().and_then(|root| {
        fs::metadata(root.join(".mlcp-last.log"))
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    let result = run_transfer(
        &state,
        &server,
        connection.database.trim(),
        None,
        transfer,
        Some(events),
    )
    .await;
    if let (Some(root), Err(err)) = (&job_root, &result) {
        let path = root.join(".mlcp-last.log");
        let current_log = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok();
        if previous_log == current_log {
            let _ = fs::write(path, format!("{err:#}"));
        }
    }
    result.map_err(error)
}

async fn run_transfer(
    state: &MlcpState,
    server: &ServerConfig,
    database: &str,
    launcher: Option<PathBuf>,
    transfer: Transfer,
    events: Option<tauri::ipc::Channel<String>>,
) -> Result<TransferResult> {
    let process_path = launcher_path()?;
    let launcher = match launcher {
        Some(path) => path,
        None => prepare_gradle().await?,
    };
    let scratch = private_directory("workbench-mlcp-").tempdir()?;
    let mut options = File::create(scratch.path().join("options"))?;
    let operation = match &transfer {
        Transfer::Pull { .. } => "export",
        Transfer::Push { .. } => "import",
    };

    write_connection(&mut options, server, database)?;
    let trust_password = if server.secure && !server.insecure {
        let (store, password) = truststore(&process_path).await?;
        write_option(&mut options, "truststore_path", path_text(&store)?)?;
        write_option(&mut options, "truststore_password", &password)?;
        password
    } else {
        String::new()
    };
    let job_root = match &transfer {
        Transfer::Pull {
            folder,
            job_id: Some(id),
            ..
        } => Some(job_directory(folder, id, true)?),
        _ => None,
    };
    let (mut archive, staging, parts, replacement) = write_transfer(&mut options, transfer)?;
    let log_path = scratch.path().join("output");
    let log = File::create(&log_path)?;
    drop(options);
    let mut command = if launcher
        .extension()
        .is_some_and(|extension| extension == "jar")
    {
        let mut command = Command::new(if cfg!(windows) { "java.exe" } else { "java" });
        command
            .arg("-cp")
            .arg(&launcher)
            .arg("org.gradle.wrapper.GradleWrapperMain")
            .args(["--no-daemon", "--console=plain", "--project-dir"])
            .arg(launcher.parent().unwrap())
            .arg("transfer")
            .env("WORKBENCH_MLCP_COMMAND", operation.to_uppercase())
            .env("WORKBENCH_MLCP_PORT", server.port.to_string())
            .env("WORKBENCH_MLCP_OPTIONS", scratch.path().join("options"));
        command
    } else {
        let mut command = Command::new(launcher);
        command
            .arg("-options_file")
            .arg(scratch.path().join("options"));
        command
    };
    command
        .env("PATH", process_path)
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("Could not launch Gradle. Install Java 17 or newer and retry")?;
    state.pid.store(child.id().unwrap_or(0), Ordering::Release);
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                if let (Some(events), Ok((_, mut output))) = (&events, read_output(&log_path)) {
                    if !output.ends_with('\n') {
                        output.truncate(output.rfind('\n').map_or(0, |index| index + 1));
                    }
                    for secret in [&server.password, &trust_password] {
                        if !secret.is_empty() { output = output.replace(secret, "[redacted]"); }
                    }
                    let _ = events.send(output);
                }
            }
        }
    };
    if status.is_ok() {
        state.pid.store(0, Ordering::Release);
    } else {
        state.shutdown();
    }
    let (failed, mut output) = read_output(&log_path)?;
    for secret in [&server.password, &trust_password] {
        if !secret.is_empty() {
            output = output.replace(secret, "[redacted]");
        }
    }
    if let Some(events) = &events {
        let _ = events.send(output.clone());
    }
    if let Some(root) = &job_root {
        let mut full_output = String::from_utf8_lossy(&fs::read(&log_path)?).into_owned();
        for secret in [&server.password, &trust_password] {
            if !secret.is_empty() {
                full_output = full_output.replace(secret, "[redacted]");
            }
        }
        fs::write(root.join(".mlcp-last.log"), full_output)
            .context("Could not save the job log")?;
    }
    let status = status.context("Could not wait for MLCP to finish")?;
    if !status.success() || failed {
        let reason = match operation {
            "import" => "MLCP push failed. Some documents may already have been written.",
            _ => "MLCP pull failed.",
        };
        if status.success() {
            bail!("{reason}\n{output}");
        }
        bail!("{reason} ({status})\n{output}");
    }

    let parts = if let Some(staging) = staging {
        let count = archive_parts(&archive)
            .map_err(|err| anyhow::anyhow!("{err}\n{output}"))?
            .len();
        if let Some(destination) = replacement {
            let backup = staging.path().join("previous");
            if destination.exists() {
                ensure!(
                    destination.is_dir() && !destination.is_symlink(),
                    "Previous archive is unavailable"
                );
                fs::rename(&destination, &backup)
                    .context("Could not prepare the existing archive for replacement")?;
            }
            if let Err(error) = fs::rename(&archive, &destination) {
                if backup.exists() && fs::rename(&backup, &destination).is_err() {
                    let saved = staging.keep();
                    bail!(
                        "Could not restore the previous archive. It is available at {}",
                        saved.join("previous").display()
                    );
                }
                return Err(error).context("Could not replace the archive");
            }
            archive = destination;
        } else {
            let _ = staging.keep();
        }
        count
    } else {
        parts
    };
    Ok(TransferResult {
        archive,
        parts,
        output,
    })
}

fn write_connection(file: &mut File, server: &ServerConfig, database: &str) -> Result<()> {
    ensure!(
        !server.host.trim().is_empty() && !server.host.contains(',') && server.port != 0,
        "The saved connection needs a single host and a valid port"
    );
    ensure!(
        !server.username.trim().is_empty(),
        "Set a username in the saved server profile"
    );
    for (name, value) in [
        ("mode", "local"),
        ("host", server.host.as_str()),
        ("port", &server.port.to_string()),
        ("username", server.username.as_str()),
        ("password", server.password.as_str()),
        ("database", database),
        ("restrict_hosts", "true"),
        ("ssl", if server.secure { "true" } else { "false" }),
    ] {
        write_option(file, name, value)?;
    }
    Ok(())
}

fn write_option(file: &mut File, name: &str, value: &str) -> Result<()> {
    ensure!(
        !value.contains(['\0', '\n', '\r']),
        "MLCP's {name} option requires a single line"
    );
    // The options reader strips the outer quotes. The CLI parser strips a second
    // pair when a value looks like a switch, so it is never interpreted as one.
    if value.starts_with('-') {
        ensure!(!value.contains('"'), "MLCP cannot read that {name} option");
        writeln!(file, "-{name}\n'\"{value}\"'")?;
    } else {
        ensure!(
            !(value.len() >= 2
                && value.starts_with('"')
                && value.ends_with('"')
                && !value[1..value.len() - 1].contains('"')),
            "MLCP cannot read that {name} option"
        );
        writeln!(file, "-{name}\n\"{value}\"")?;
    }
    Ok(())
}

fn write_transfer(
    file: &mut File,
    transfer: Transfer,
) -> Result<(PathBuf, Option<TempDir>, usize, Option<PathBuf>)> {
    match transfer {
        Transfer::Pull {
            folder,
            selection,
            replace_id,
            job_id,
            replace,
        } => {
            let folder = if let Some(id) = &job_id {
                job_directory(&folder, id, true)?
            } else {
                local_folder(&folder, "Choose an existing folder for the export")?
            };
            let prefix = format!("mlcp-{}-", chrono::Local::now().format("%Y%m%d-%H%M%S"));
            let staging = private_directory(&prefix).tempdir_in(&folder)?;
            let archive = staging.path().join("archive");
            write_option(file, "output_type", "archive")?;
            write_option(file, "output_file_path", &hadoop_path(&archive)?)?;
            write_option(file, "compress", "true")?;
            match selection {
                Selection::All => {}
                Selection::Collections(values) => write_filter(file, "collection_filter", &values)?,
                Selection::Directories(values) => write_filter(file, "directory_filter", &values)?,
            }
            let replacement = if job_id.is_some() && replace {
                Some(folder.join("latest"))
            } else if let Some(id) = replace_id {
                ensure!(
                    id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                    "Invalid pull job identifier"
                );
                Some(folder.join(format!("mlcp-job-{id}")))
            } else {
                None
            };
            Ok((archive, Some(staging), 0, replacement))
        }
        Transfer::Push {
            folder,
            copy_permissions,
        } => {
            let archive = local_folder(&folder, "Choose an existing MLCP archive folder")?;
            let parts = archive_parts(&archive)?;
            let paths = parts
                .iter()
                .map(|part| hadoop_path(part))
                .collect::<Result<Vec<_>>>()?;
            write_option(file, "input_file_type", "archive")?;
            write_option(file, "input_file_path", &paths.join(","))?;
            write_option(
                file,
                "copy_permissions",
                if copy_permissions { "true" } else { "false" },
            )?;
            Ok((archive, None, parts.len(), None))
        }
    }
}

fn job_directory(folder: &Path, id: &str, create: bool) -> Result<PathBuf> {
    ensure!(
        id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
        "Invalid pull job identifier"
    );
    let folder = local_folder(folder, "The job's data folder is unavailable")?;
    let path = folder.join(format!("mlcp-job-{id}"));
    if create && !path.exists() {
        fs::create_dir(&path)?;
    }
    if path.exists() {
        ensure!(
            path.is_dir() && !path.is_symlink(),
            "The job data folder is unavailable"
        );
    }
    Ok(path)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobData {
    archives: Vec<ArchiveInfo>,
    log: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveInfo {
    path: PathBuf,
    size: u64,
    files: usize,
}

#[tauri::command]
pub async fn mlcp_job_data(folder: PathBuf, job_id: String) -> Result<JobData, String> {
    fn load(folder: &Path, id: &str) -> Result<JobData> {
        let root = job_directory(folder, id, false)?;
        let mut archives = Vec::new();
        if root.is_dir() {
            if let Ok(parts) = archive_parts(&root) {
                let size = parts.iter().try_fold(0u64, |total, part| {
                    Ok::<_, std::io::Error>(total.saturating_add(fs::metadata(part)?.len()))
                })?;
                archives.push(ArchiveInfo {
                    path: root.clone(),
                    size,
                    files: count_archive_files(&parts)?,
                });
            }
            for entry in fs::read_dir(&root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let dir = entry.path();
                let path = if dir.join("archive").is_dir() {
                    dir.join("archive")
                } else {
                    dir
                };
                if let Ok(parts) = archive_parts(&path) {
                    let size = parts.iter().try_fold(0u64, |total, part| {
                        Ok::<_, std::io::Error>(total.saturating_add(fs::metadata(part)?.len()))
                    })?;
                    archives.push(ArchiveInfo {
                        path,
                        size,
                        files: count_archive_files(&parts)?,
                    });
                }
            }
        }
        archives.sort_by(|a, b| b.path.cmp(&a.path));
        let log = fs::read_to_string(root.join(".mlcp-last.log")).unwrap_or_default();
        Ok(JobData { archives, log })
    }
    load(&folder, &job_id).map_err(error)
}

#[tauri::command]
pub async fn mlcp_delete_download(
    folder: PathBuf,
    job_id: String,
    archive: PathBuf,
) -> Result<(), String> {
    let root = job_directory(&folder, &job_id, false).map_err(error)?;
    let archive = archive.canonicalize().map_err(|e| e.to_string())?;
    if !archive.starts_with(&root) || archive == root {
        return Err("Download is outside this job's data folder".into());
    }
    if !archive.is_dir() || archive.is_symlink() {
        return Err("Download folder is unavailable".into());
    }
    fs::remove_dir_all(archive).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn mlcp_delete_job_data(folder: PathBuf, job_id: String) -> Result<(), String> {
    let root = job_directory(&folder, &job_id, false).map_err(error)?;
    if root.exists() {
        fs::remove_dir_all(root).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn mlcp_archive_size(folder: PathBuf) -> Result<u64, String> {
    let folder = local_folder(&folder, "Archive folder is unavailable").map_err(error)?;
    let parts = archive_parts(&folder).map_err(error)?;
    parts
        .iter()
        .try_fold(0u64, |total, part| {
            fs::metadata(part).map(|metadata| total.saturating_add(metadata.len()))
        })
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn mlcp_archive_files(folder: PathBuf) -> Result<usize, String> {
    let folder = local_folder(&folder, "Archive folder is unavailable").map_err(error)?;
    let parts = archive_parts(&folder).map_err(error)?;
    count_archive_files(&parts).map_err(error)
}

fn count_archive_files(parts: &[PathBuf]) -> Result<usize> {
    parts.iter().try_fold(0usize, |count, path| {
        Ok(count.saturating_add(zip_file_count(path)?))
    })
}

fn zip_file_count(path: &Path) -> Result<usize> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let tail_length = length.min(65_557) as usize;
    file.seek(SeekFrom::End(-(tail_length as i64)))?;
    let mut tail = vec![0; tail_length];
    file.read_exact(&mut tail)?;
    let end = tail
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .context("Could not read the archive directory")?;
    ensure!(
        end + 22 <= tail.len(),
        "Could not read the archive directory"
    );
    let entries = u16::from_le_bytes([tail[end + 10], tail[end + 11]]) as usize;
    Ok(entries)
}

fn write_filter(file: &mut File, option: &str, value: &str) -> Result<()> {
    let entries = filter_entries(value)?;
    write_option(file, option, &entries.join(","))
}

fn filter_entries(value: &str) -> Result<Vec<String>> {
    let entries: Vec<_> = value.split(',').map(str::trim).collect();
    ensure!(
        entries.iter().all(|entry| !entry.is_empty()),
        "Enter at least one name and remove empty filter entries"
    );
    Ok(entries.into_iter().map(str::to_owned).collect())
}

fn local_folder(path: &Path, message: &str) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "{message}");
    let folder = path.canonicalize().context(message.to_owned())?;
    ensure!(folder.is_dir(), "{message}");
    Ok(folder)
}

fn archive_parts(folder: &Path) -> Result<Vec<PathBuf>> {
    let mut parts = Vec::new();
    if folder.is_dir() {
        for entry in fs::read_dir(folder)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry.path().extension().is_some_and(|ext| ext == "zip")
                && !entry.file_name().to_string_lossy().starts_with(['.', '_'])
            {
                parts.push(entry.path());
            }
        }
    }
    ensure!(
        !parts.is_empty(),
        "No MLCP archive ZIP files found in {}",
        folder.display()
    );
    parts.sort_unstable();
    Ok(parts)
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("MLCP requires UTF-8 local paths")
}

fn hadoop_path(path: &Path) -> Result<String> {
    let path = path_text(path)?;
    #[cfg(windows)]
    let path = {
        let path = path.strip_prefix(r"\\?\").unwrap_or(path);
        ensure!(
            !path.starts_with(r"\\") && !path.starts_with(r"UNC\"),
            "Choose a local drive for MLCP archives"
        );
        format!("/{}", path.replace('\\', "/"))
    };
    ensure!(
        !path.contains([',', '*', '?', '[', ']', '{', '}', '\\']),
        "MLCP treats commas and glob characters as patterns. Choose a simpler archive path."
    );
    Ok(format!("file:{path}"))
}

fn private_directory(prefix: &str) -> Builder<'_, '_> {
    let mut builder = Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    builder
}

fn launcher_path() -> Result<OsString> {
    let mut paths = Vec::new();
    if let Some(home) = env::var_os("JAVA_HOME").filter(|home| !home.is_empty()) {
        paths.push(PathBuf::from(home).join("bin"));
    }
    if let Some(path) = env::var_os("PATH") {
        paths.extend(env::split_paths(&path));
    }
    Ok(env::join_paths(paths)?)
}

async fn prepare_gradle() -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let root = AppConfig::config_path()
        .parent()
        .unwrap()
        .join("mlcp-runtime");
    fs::create_dir_all(&root)?;
    let wrapper = root.join("gradle-wrapper.jar");
    let checksum = "7d3a4ac4de1c32b59bc6a4eb8ecb8e612ccd0cf1ae1e99f66902da64df296172";
    if !fs::read(&wrapper)
        .ok()
        .is_some_and(|bytes| format!("{:x}", Sha256::digest(bytes)) == checksum)
    {
        let bytes = reqwest::Client::new()
            .get("https://raw.githubusercontent.com/gradle/gradle/v8.14.3/gradle/wrapper/gradle-wrapper.jar")
            .send().await.context("Could not download Gradle. Check your network connection and retry.")?
            .error_for_status()?.bytes().await?;
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == checksum,
            "Gradle wrapper verification failed"
        );
        let mut file = tempfile::NamedTempFile::new_in(&root)?;
        file.write_all(&bytes)?;
        file.persist(&wrapper)?;
    }
    fs::write(
        root.join("gradle-wrapper.properties"),
        concat!(
            "distributionUrl=https://services.gradle.org/distributions/gradle-8.14.3-bin.zip\n",
            "distributionSha256Sum=bd71102213493060956ec229d946beee57158dbd89d0e62b91bca0fa2c5f3531\n"
        ),
    )?;
    fs::write(
        root.join("settings.gradle"),
        "rootProject.name = 'workbench-mlcp'\n",
    )?;
    fs::write(root.join("build.gradle"), include_str!("mlcp.gradle"))?;
    fs::write(root.join("logback.xml"), include_str!("mlcp-logback.xml"))?;
    Ok(wrapper)
}

async fn truststore(process_path: &OsStr) -> Result<(PathBuf, String)> {
    if let Some(store) = env::var_os("MLCP_TRUSTSTORE").filter(|path| !path.is_empty()) {
        let path = PathBuf::from(store);
        ensure!(
            path.is_absolute() && path.is_file(),
            "MLCP_TRUSTSTORE must point to a Java truststore file"
        );
        return Ok((
            path,
            env::var("MLCP_TRUSTSTORE_PASSWORD").unwrap_or_else(|_| "changeit".into()),
        ));
    }

    let java = Command::new(if cfg!(windows) { "java.exe" } else { "java" })
        .args(["-XshowSettings:properties", "-version"])
        .env("PATH", process_path)
        .output()
        .await
        .context("Could not find Java. Install the Java version required by your MLCP release.")?;
    let settings = String::from_utf8_lossy(&java.stderr);
    let store = settings
        .lines()
        .find_map(|line| line.trim().strip_prefix("java.home = "))
        .map(|home| Path::new(home).join("lib/security/cacerts"))
        .filter(|path| java.status.success() && path.is_file())
        .context(
            "Could not find the Java truststore. Set MLCP_TRUSTSTORE for this TLS connection.",
        )?;
    Ok((store, "changeit".into()))
}

fn read_output(path: &Path) -> Result<(bool, String)> {
    static FAILURE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)(?:^|\s)(?:ERROR|FATAL)\s+\S+(?::|\s+-\s)|\bOUTPUT[_ ]RECORDS[_ ]FAILED\s*[:=]\s*[1-9]",
        )
        .unwrap()
    });
    let mut log = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    let mut failed = false;
    while log.read_until(b'\n', &mut line)? != 0 {
        if FAILURE.is_match(&String::from_utf8_lossy(&line)) {
            failed = true;
            break;
        }
        line.clear();
    }

    const LIMIT: u64 = 48 * 1024;
    let length = log.get_ref().metadata()?.len();
    log.seek(SeekFrom::Start(length.saturating_sub(LIMIT)))?;
    let mut tail = Vec::new();
    log.read_to_end(&mut tail)?;
    let mut printable = tail.as_slice();
    if length > LIMIT {
        // Skip the first partial line so diagnostics never reveal part of a
        // credential that would otherwise be redacted as a complete string.
        let next_line = tail.iter().position(|byte| *byte == b'\n');
        printable = next_line.map_or(&[], |index| &tail[index + 1..]);
    }
    let output = String::from_utf8_lossy(printable).into_owned();
    Ok((
        failed,
        if length > LIMIT {
            format!("…\n{output}")
        } else {
            output
        },
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use marklogic_core::client::{AppServerEndpoint, AuthType};
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn pulls_a_reusable_archive_and_handles_partial_push_failure() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let launcher = temp.path().join("mlcp.sh");
        fs::write(
            &launcher,
            r#"#!/bin/sh
set -eu
test "$#" -eq 2
test "$1" = -options_file
options="$(cat "$2")"
option() { printf '%s\n' "$options" | awk -v key="-$1" '$0 == key {getline; print substr($0, 2, length($0)-2); exit}'; }
test "$(option restrict_hosts "$2")" = true
if printf '%s\n' "$options" | grep -q -- "-output_file_path"; then
    test "$(option host "$2")" = source.example
    test "$(option port "$2")" = 8000
    test "$(option database "$2")" = SourceDB
    test "$(option password "$2")" = 'source-secret #1'
    test "$(option collection_filter "$2")" = 'orders,customers'
    path="$(option output_file_path "$2")"
    mkdir "${path#file:}"
    touch "${path#file:}/part-0-XML.zip"
else
    test "$(option host "$2")" = destination.example
    test "$(option port "$2")" = 8011
    test "$(option database "$2")" = TargetDB
    test "$(option password "$2")" = 'destination-secret #2'
    path="$(option input_file_path "$2")"
    test -f "${path#file:}"
    if [ "$(option copy_permissions "$2")" = true ]; then
        echo "OUTPUT_RECORDS_FAILED: 1"
        echo "$(option password "$2")"
    fi
fi
"#,
        )?;
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700))?;
        let folder = temp.path().join("shared archives");
        fs::create_dir(&folder)?;
        let folder = folder.canonicalize()?;
        let source = ServerConfig {
            name: "source".into(),
            host: "source.example".into(),
            username: "reader".into(),
            password: "source-secret #1".into(),
            port: 8000,
            secure: false,
            auth_type: AuthType::Digest,
            insecure: false,
            app_servers: Vec::new(),
            color: None,
            stripe_color_secondary: None,
            cluster: Default::default(),
            uri: None,
        };
        let destination = ServerConfig {
            name: "destination".into(),
            host: "destination.example".into(),
            username: "writer".into(),
            password: "destination-secret #2".into(),
            app_servers: vec![AppServerEndpoint {
                name: "XDBC".into(),
                port: 8011,
                ..AppServerEndpoint::app_services_default()
            }],
            ..source.clone()
        };
        let config = AppConfig {
            active_server: Some(source.name.clone()),
            servers: vec![source.clone(), destination],
            ..AppConfig::default()
        };
        let target = Connection {
            server: "destination".into(),
            app_server: Some("XDBC".into()),
            database: "TargetDB".into(),
        };
        let destination = server_for(&config, &target)?;

        let state = MlcpState::default();
        let pull = run_transfer(
            &state,
            &source,
            "SourceDB",
            Some(launcher.clone()),
            Transfer::Pull {
                folder: folder.clone(),
                selection: Selection::Collections("orders, customers".into()),
                replace_id: None,
                job_id: None,
                replace: false,
            },
            None,
        )
        .await?;
        assert_eq!(pull.parts, 1);
        let id = "01234567-89ab-cdef-0123-456789abcdef".to_owned();
        let job_pull = |replace| Transfer::Pull {
            folder: folder.clone(),
            selection: Selection::Collections("orders, customers".into()),
            replace_id: None,
            job_id: Some(id.clone()),
            replace,
        };
        let timestamp = run_transfer(
            &state,
            &source,
            "SourceDB",
            Some(launcher.clone()),
            job_pull(false),
            None,
        )
        .await?;
        assert!(
            timestamp
                .archive
                .starts_with(folder.join(format!("mlcp-job-{id}")))
        );
        let latest = run_transfer(
            &state,
            &source,
            "SourceDB",
            Some(launcher.clone()),
            job_pull(true),
            None,
        )
        .await?;
        assert!(latest.archive.ends_with("latest"));
        let data = mlcp_job_data(folder.clone(), id.clone())
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(data.archives.len(), 2);
        assert!(
            folder
                .join(format!("mlcp-job-{id}/.mlcp-last.log"))
                .exists()
        );
        let replace = || Transfer::Pull {
            folder: folder.clone(),
            selection: Selection::Collections("orders, customers".into()),
            replace_id: Some(id.clone()),
            job_id: None,
            replace: false,
        };
        let first = run_transfer(
            &state,
            &source,
            "SourceDB",
            Some(launcher.clone()),
            replace(),
            None,
        )
        .await?;
        fs::write(first.archive.join("old-marker"), "previous")?;
        let failed_launcher = temp.path().join("failed.sh");
        fs::write(&failed_launcher, "#!/bin/sh\nexit 1\n")?;
        fs::set_permissions(&failed_launcher, fs::Permissions::from_mode(0o700))?;
        assert!(
            run_transfer(
                &state,
                &source,
                "SourceDB",
                Some(failed_launcher),
                replace(),
                None
            )
            .await
            .is_err()
        );
        assert!(first.archive.join("old-marker").exists());
        let second = run_transfer(
            &state,
            &source,
            "SourceDB",
            Some(launcher.clone()),
            replace(),
            None,
        )
        .await?;
        assert_eq!(first.archive, second.archive);
        assert!(!second.archive.join("old-marker").exists());
        let failure = run_transfer(
            &state,
            &destination,
            &target.database,
            Some(launcher.clone()),
            Transfer::Push {
                folder: pull.archive.clone(),
                copy_permissions: true,
            },
            None,
        )
        .await
        .err()
        .context("An MLCP error counter must fail the push")?;
        assert!(failure.to_string().contains("OUTPUT_RECORDS_FAILED: 1"));
        assert!(!failure.to_string().contains(&destination.password));

        let pushed = run_transfer(
            &state,
            &destination,
            &target.database,
            Some(launcher),
            Transfer::Push {
                folder: pull.archive.clone(),
                copy_permissions: false,
            },
            None,
        )
        .await?;
        assert_eq!(pushed.parts, 1);
        assert_eq!(pushed.archive, pull.archive);
        mlcp_delete_job_data(folder.clone(), id.clone())
            .await
            .map_err(anyhow::Error::msg)?;
        assert!(!folder.join(format!("mlcp-job-{id}")).exists());
        assert!(pull.archive.exists());
        Ok(())
    }
}
