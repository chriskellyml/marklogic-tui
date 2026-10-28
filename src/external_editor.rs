use anyhow::{Context, Result, bail};
use crossterm::{
    event::{DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture},
    execute,
    terminal::{
        Clear as TerminalClear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) fn temp_editor_path(active_path: Option<&Path>) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut file_name = format!("marklogic-tui-edit-{}-{}", std::process::id(), unique);
    if let Some(extension) = active_path
        .and_then(|path| path.extension())
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
    {
        file_name.push('.');
        file_name.push_str(extension);
    } else {
        file_name.push_str(".tmp");
    }
    env::temp_dir().join(file_name)
}

pub(crate) fn edit_text_in_external_editor(
    original_contents: &str,
    active_path: Option<&Path>,
    label: &str,
) -> Result<String> {
    if env::var_os("EDITOR").is_none() {
        bail!("$EDITOR is not set");
    }

    let temp_path = temp_editor_path(active_path);
    fs::write(&temp_path, original_contents).with_context(|| {
        format!(
            "Failed to create temporary {} file: {}",
            label,
            temp_path.display()
        )
    })?;

    let edit_result = suspend_tui_for_external_editor(&temp_path).and_then(|_| {
        fs::read_to_string(&temp_path).with_context(|| {
            format!(
                "Failed to read edited {} from temporary file: {}",
                label,
                temp_path.display()
            )
        })
    });
    let _ = fs::remove_file(&temp_path);
    edit_result
}

pub(crate) fn open_file_in_external_editor(path: &Path) -> Result<()> {
    if env::var_os("EDITOR").is_none() {
        bail!("$EDITOR is not set");
    }
    suspend_tui_for_external_editor(path)
}

fn suspend_tui_for_external_editor(path: &Path) -> Result<()> {
    disable_raw_mode().context("Failed to suspend raw mode")?;
    let mut stdout = io::stdout();
    crate::keyboard::suspend(&mut stdout).context("Failed to suspend enhanced keyboard input")?;
    execute!(
        stdout,
        DisableMouseCapture,
        DisableFocusChange,
        LeaveAlternateScreen
    )
    .context("Failed to leave alternate screen")?;

    let edit_result = run_external_editor(path);
    let resume_result = (|| -> Result<()> {
        enable_raw_mode().context("Failed to restore raw mode")?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableFocusChange,
            EnableMouseCapture,
            TerminalClear(ClearType::All)
        )
        .context("Failed to restore alternate screen")?;
        crate::keyboard::resume(&mut stdout)
            .context("Failed to restore enhanced keyboard input")?;
        Ok(())
    })();

    match (edit_result, resume_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(edit_err), Ok(())) => Err(edit_err),
        (Ok(()), Err(resume_err)) => Err(resume_err),
        (Err(edit_err), Err(resume_err)) => {
            Err(edit_err.context(format!("Also failed to restore terminal: {}", resume_err)))
        }
    }
}

fn run_external_editor(path: &Path) -> Result<()> {
    let status = external_editor_command(path)
        .status()
        .with_context(|| format!("Failed to launch $EDITOR for {}", path.display()))?;
    if !status.success() {
        bail!("$EDITOR exited with status {}", status);
    }
    Ok(())
}

fn external_editor_command(path: &Path) -> Command {
    #[cfg(windows)]
    {
        let mut command = Command::new("cmd");
        command
            .arg("/C")
            .arg(format!(r#"%EDITOR% "{}""#, path.display()));
        command
    }

    #[cfg(not(windows))]
    {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(r#"exec $EDITOR "$1""#)
            .arg("sh")
            .arg(path);
        command
    }
}
