use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

use crate::workspace::{Connection, server_for};
use anyhow::{Context, Result, bail, ensure};
use marklogic_core::{client::ServerConfig, config::AppConfig};
use serde::{Deserialize, Serialize};

use super::repository::{inside, name, permissions, run_path, source};

#[derive(Clone, Serialize, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub project: String,
    pub job: String,
    pub connection: Connection,
    pub modules_database: String,
    pub history_env: String,
    pub id: String,
    pub wet: bool,
    pub status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub project: String,
    pub job: String,
    pub connection: Connection,
    pub modules_database: String,
    pub dry_run_id: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Default)]
pub struct Runner {
    child: Option<Child>,
    last: Option<Run>,
    endpoint: Option<(String, String, bool)>,
}

fn new_run_dir(root: &Path, project: &str, env: &str) -> Result<(String, PathBuf)> {
    let timestamp = chrono::Local::now().format("%Y%m%d%H%M%S").to_string();
    for n in 0..1000 {
        let id = if n == 0 {
            timestamp.clone()
        } else {
            format!("{timestamp}-{n:03}")
        };
        let dir = run_path(root, project, env, &id)?;
        fs::create_dir_all(dir.parent().unwrap())?;
        match fs::create_dir(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("Could not allocate a unique run folder")
}

// Preserve human-readable profile names while making every folder a single safe component.
fn history_folder(profile: &str) -> String {
    profile
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b' ') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn snapshot(source: &Path, dest: &Path) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    fs::create_dir(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "Cannot snapshot linked file {}",
            entry.path().display()
        );
        let target = dest.join(entry.file_name());
        if kind.is_dir() {
            snapshot(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

impl Runner {
    pub fn status(&mut self) -> Result<Option<Run>> {
        if let Some(child) = &mut self.child
            && let Some(exit) = child.try_wait()?
        {
            self.last.as_mut().unwrap().status = if exit.success() {
                "completed"
            } else {
                "failed"
            }
            .into();
            self.child = None;
        }
        Ok(self.last.clone())
    }

    pub fn start(&mut self, root: &Path, req: Request, lib: &Path) -> Result<Run> {
        let server = server_for(&AppConfig::load()?, &req.connection)?;
        self.launch(root, req, server, lib)
    }

    fn launch(
        &mut self,
        root: &Path,
        req: Request,
        server: ServerConfig,
        lib: &Path,
    ) -> Result<Run> {
        let previous = self.status()?;
        ensure!(self.child.is_none(), "A CoRBi run is already active");
        name(&req.job)?;
        let stem = req.job.strip_suffix(".job").context("Select a .job file")?;
        let job = source(root, &req.project, &req.job)?;
        let env = &req.connection.server;
        let history_env = history_folder(env);
        ensure!(
            permissions(root)?
                .get(&req.project)
                .and_then(|jobs| jobs.get(&req.job))
                .and_then(|envs| envs.get(env))
                != Some(&false),
            "Job disabled in {env}"
        );
        ensure!(req.limit != Some(0), "URI limit must be positive");
        let wet = req.dry_run_id.is_some();
        let endpoint = (server.base_url(), server.username.clone(), server.insecure);
        let (id, dir) = if let Some(id) = req.dry_run_id {
            ensure!(
                self.endpoint.as_ref() == Some(&endpoint)
                    && previous.as_ref().is_some_and(|r| r.project == req.project
                        && r.job == req.job
                        && r.connection == req.connection
                        && r.modules_database == req.modules_database
                        && r.id == id
                        && !r.wet
                        && r.status == "completed"),
                "Run a dry run for this job and Workbench connection before executing a wet run"
            );
            let dir = run_path(root, &req.project, &history_env, &id)?;
            ensure!(
                inside(&dir, Path::new("job.options"))?.is_file(),
                "Dry-run options are missing"
            );
            (id, dir)
        } else {
            new_run_dir(root, &req.project, &history_env)?
        };
        let phase = if wet { "wet" } else { "dry" };
        let log_path = inside(&dir, Path::new(&format!("{phase}-output.log")))?;
        let log = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&log_path)?;
        if !wet {
            snapshot(
                &inside(
                    root,
                    &Path::new("src/projects").join(&req.project).join("scripts"),
                )?,
                &dir.join("scripts"),
            )?;
            snapshot(
                &inside(root, Path::new("src/support"))?,
                &dir.join("support"),
            )?;
        }
        let phase_options = if wet { "wet.options" } else { "job.options" };
        ensure!(
            !inside(&dir, Path::new(phase_options))?.exists(),
            "Run options already exist"
        );
        let launcher = inside(&dir, Path::new("WorkbenchCorb.java"))?;
        fs::write(&launcher, include_str!("WorkbenchCorb.java"))?;
        let java = std::env::var_os("JAVA_HOME")
            .map(|home| {
                PathBuf::from(home).join("bin").join(if cfg!(windows) {
                    "java.exe"
                } else {
                    "java"
                })
            })
            .unwrap_or_else(|| "java".into());
        let mut cmd = Command::new(java);
        cmd.current_dir(&dir)
            .arg("-cp")
            .arg(lib.join("*"))
            .arg(&launcher)
            .arg((!wet).to_string())
            .arg(job)
            .arg(&server.host)
            .arg(server.port.to_string())
            .arg(&server.username)
            .arg(&req.connection.database)
            .arg(if server.secure { "xccs" } else { "xcc" })
            .arg(if server.insecure {
                "TrustAnyoneSSLConfig"
            } else {
                "OneWaySSLConfig"
            })
            .arg(&req.modules_database)
            .arg(req.limit.filter(|_| !wet).unwrap_or(1_000_000).to_string())
            .arg(&req.project)
            .arg(stem)
            .arg(&id)
            .stdin(Stdio::piped())
            .stderr(log.try_clone()?)
            .stdout(log);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let child = cmd
            .spawn()
            .context("Could not launch Java. Install Java 11+ or set JAVA_HOME");
        let mut run = Run {
            project: req.project,
            job: req.job,
            connection: req.connection,
            modules_database: req.modules_database,
            history_env: history_env.to_owned(),
            id,
            wet,
            status: "running".into(),
        };
        match child {
            Ok(mut child) => {
                let password = child
                    .stdin
                    .take()
                    .context("Java input unavailable")?
                    .write_all(server.password.as_bytes());
                self.child = Some(child);
                self.last = Some(run.clone());
                if let Err(error) = password {
                    self.stop()?;
                    return Err(error).context("Could not pass Workbench credentials to Java");
                }
            }
            Err(err) => {
                run.status = "failed".into();
                fs::write(log_path, format!("{err:#}\n"))?;
                self.last = Some(run);
                return Err(err);
            }
        }
        self.endpoint = Some(endpoint);
        self.last = Some(run.clone());
        Ok(run)
    }

    pub fn stop(&mut self) -> Result<()> {
        self.status()?;
        if let Some(child) = &mut self.child {
            #[cfg(unix)]
            {
                // Stop CoRB and any subprocesses it started.
                let result = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                if result != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error.into());
                    }
                }
            }
            #[cfg(windows)]
            ensure!(
                Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &child.id().to_string()])
                    .status()?
                    .success(),
                "Could not stop CoRB"
            );
            child.wait()?;
            self.child = None;
            self.last.as_mut().unwrap().status = "stopped".into();
        }
        Ok(())
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marklogic_core::client::AuthType;

    #[test]
    fn launches_and_promotes_only_the_reviewed_target_and_snapshot() -> Result<()> {
        // A local CoRB stand-in verifies the real Java launcher without a database.
        if Command::new("javac").arg("-version").output().is_err() {
            eprintln!("Skipping Java launcher integration check: javac unavailable");
            return Ok(());
        }
        let root = marklogic_core::config::test_storage_dir().join("corbi");
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        let project = root.join("src/projects/demo");
        fs::create_dir_all(project.join("scripts"))?;
        fs::create_dir_all(root.join("build/lib"))?;
        fs::write(project.join("scripts/main.xqy"), "reviewed")?;
        fs::write(
            project.join("work.job"),
            "XCC_CONNECTION_URI=xcc://old-target\nTHREAD-COUNT=3\n",
        )?;
        let fake = root.join("Manager.java");
        fs::write(
            &fake,
            r#"package com.marklogic.developer.corb;
import java.nio.file.*;
import java.util.Properties;
public class Manager {
    public static void main(String[] args) throws Exception {
        Properties opts = new Properties();
        try (var in = Files.newInputStream(Path.of(System.getProperty("OPTIONS-FILE")))) { opts.load(in); }
        boolean dry = "true".equals(opts.getProperty("PROCESS-MODULE.DRY-RUN"));
        if (!"test-secret".equals(System.getProperty("XCC-PASSWORD"))
            || opts.containsKey("XCC_CONNECTION_URI") || opts.containsKey("XCC-PASSWORD")
            || !"Content".equals(opts.getProperty("XCC-DBNAME"))
            || !"Modules".equals(opts.getProperty("MODULES-DATABASE"))
            || !"8123".equals(opts.getProperty("XCC-PORT"))
            || !"selected-host".equals(opts.getProperty("XCC-HOSTNAME"))
            || !"xccs".equals(opts.getProperty("XCC-PROTOCOL"))
            || !"3".equals(opts.getProperty("THREAD-COUNT"))
            || !(dry ? "10" : "1000000").equals(opts.getProperty("URIS-MODULE.LIMIT"))
            || !"reviewed".equals(Files.readString(Path.of("scripts/main.xqy")))) System.exit(7);
        System.out.println("Completed");
    }
}"#,
        )?;
        assert!(
            Command::new("javac")
                .arg("-d")
                .arg(&root)
                .arg(fake)
                .status()?
                .success()
        );
        assert!(
            Command::new("jar")
                .arg("cf")
                .arg(root.join("build/lib/marklogic-corb-test.jar"))
                .arg("-C")
                .arg(&root)
                .arg("com")
                .status()?
                .success()
        );
        let server = ServerConfig {
            name: "dev/eu".into(),
            host: "selected-host".into(),
            username: "tester".into(),
            password: "test-secret".into(),
            port: 8123,
            secure: true,
            insecure: false,
            auth_type: AuthType::Digest,
            app_servers: vec![],
            color: None,
            stripe_color_secondary: None,
            cluster: Default::default(),
            uri: None,
        };
        let request = |database: &str, id: Option<String>| Request {
            project: "demo".into(),
            job: "work.job".into(),
            connection: Connection {
                server: server.name.clone(),
                app_server: Some("xcc".into()),
                database: database.into(),
            },
            modules_database: "Modules".into(),
            dry_run_id: id,
            limit: Some(10),
        };
        let mut runner = Runner::default();
        let dry = runner.launch(
            &root,
            request("Content", None),
            server.clone(),
            &root.join("build/lib"),
        )?;
        assert!(
            runner
                .launch(
                    &root,
                    request("Content", None),
                    server.clone(),
                    &root.join("build/lib")
                )
                .is_err()
        );
        runner.child.as_mut().unwrap().wait()?;
        assert_eq!(runner.status()?.unwrap().status, "completed");
        assert_eq!(dry.history_env, "dev%2Feu");
        let id = Some(dry.id.clone());
        fs::write(project.join("scripts/main.xqy"), "edited after review")?;
        assert!(
            runner
                .launch(
                    &root,
                    request("Other", id.clone()),
                    server.clone(),
                    &root.join("build/lib")
                )
                .is_err()
        );
        fs::write(
            root.join("permissions.json"),
            r#"{"demo":{"work.job":{"dev/eu":false}}}"#,
        )?;
        assert!(
            runner
                .launch(
                    &root,
                    request("Content", id.clone()),
                    server.clone(),
                    &root.join("build/lib")
                )
                .is_err()
        );
        fs::remove_file(root.join("permissions.json"))?;
        runner.launch(
            &root,
            request("Content", id.clone()),
            server.clone(),
            &root.join("build/lib"),
        )?;
        runner.child.as_mut().unwrap().wait()?;
        assert_eq!(runner.status()?.unwrap().status, "completed");
        assert!(
            runner
                .launch(
                    &root,
                    request("Content", id),
                    server.clone(),
                    &root.join("build/lib")
                )
                .is_err()
        );
        runner.launch(
            &root,
            request("Content", None),
            server,
            &root.join("build/lib"),
        )?;
        runner.stop()?;
        assert_eq!(runner.status()?.unwrap().status, "stopped");
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
