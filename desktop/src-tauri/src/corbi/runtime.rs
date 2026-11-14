use super::repository::inside;
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

static DOWNLOAD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const LIBRARIES: &[(&str, &str, &str)] = &[
    (
        "marklogic-corb",
        "2.5.5",
        "bbb2a4d1ff3f143eb6f15ec3cbbf67009e785c0fa6f522e8d557999f73578c7d",
    ),
    (
        "marklogic-xcc",
        "11.0.3",
        "96d07b94746b0a8c2abdd5505e3b8a1bffc0342a080ce2658eb89265525fb2e5",
    ),
];

pub fn default_root() -> PathBuf {
    marklogic_core::config::AppConfig::config_path()
        .parent()
        .unwrap()
        .join("corbi")
}

fn project_libraries(root: &Path) -> Result<Option<PathBuf>> {
    let dir = inside(root, Path::new("build/lib"))?;
    if !dir.is_dir() {
        return Ok(None);
    }
    let files = fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
    if ["marklogic-corb-", "marklogic-xcc-"].iter().all(|prefix| {
        files.iter().any(|file| {
            let name = file.file_name();
            let name = name.to_string_lossy();
            name.starts_with(prefix) && name.ends_with(".jar")
        })
    }) {
        Ok(Some(dir))
    } else {
        Ok(None)
    }
}

/// Keep dependencies in the app's workspace, shared by all job folders.
pub async fn libraries(root: &Path) -> Result<PathBuf> {
    if let Some(dir) = project_libraries(root)? {
        return Ok(dir);
    }
    let _guard = DOWNLOAD.lock().await;
    let dir = default_root().join("runtime");
    fs::create_dir_all(&dir)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()?;
    for (artifact, version, checksum) in LIBRARIES {
        let filename = format!("{artifact}-{version}.jar");
        let target = dir.join(&filename);
        if fs::read(&target)
            .ok()
            .is_some_and(|bytes| format!("{:x}", Sha256::digest(bytes)) == *checksum)
        {
            continue;
        }
        let url = format!(
            "https://repo.maven.apache.org/maven2/com/marklogic/{artifact}/{version}/{filename}"
        );
        let bytes = client
            .get(url)
            .send()
            .await
            .context("Could not download CoRB. Check your connection and retry.")?
            .error_for_status()?
            .bytes()
            .await?;
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == *checksum,
            "CoRB download verification failed"
        );
        let temp = target.with_extension("download");
        fs::write(&temp, &bytes)?;
        fs::rename(&temp, &target)?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "downloads pinned dependencies from Maven Central"]
    async fn first_use_downloads_and_reuses_verified_libraries() -> Result<()> {
        let root = marklogic_core::config::test_storage_dir().join("empty-project");
        fs::create_dir_all(&root)?;
        let dir = libraries(&root).await?;
        for (artifact, version, checksum) in LIBRARIES {
            let data = fs::read(dir.join(format!("{artifact}-{version}.jar")))?;
            assert_eq!(format!("{:x}", Sha256::digest(data)), *checksum);
        }
        let modified = fs::metadata(dir.join("marklogic-corb-2.5.5.jar"))?.modified()?;
        assert_eq!(libraries(&root).await?, dir);
        assert_eq!(
            fs::metadata(dir.join("marklogic-corb-2.5.5.jar"))?.modified()?,
            modified
        );
        fs::remove_dir_all(root)?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
