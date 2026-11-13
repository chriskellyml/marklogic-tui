//! Persisted preferences shared by frontends.
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    #[default]
    Debug,
    Trace,
}

impl LogLevel {
    pub const ALL: [Self; 5] = [
        Self::Error,
        Self::Warn,
        Self::Info,
        Self::Debug,
        Self::Trace,
    ];
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InlineEditor {
    #[default]
    Edtui,
    Textarea,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub log_level: LogLevel,
    pub inline_editor: InlineEditor,
    #[serde(deserialize_with = "deserialize_autosave")]
    pub autosave_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Debug,
            inline_editor: InlineEditor::Edtui,
            autosave_seconds: 2,
        }
    }
}

fn deserialize_autosave<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let seconds = u64::deserialize(d)?;
    if (1..=60).contains(&seconds) {
        Ok(seconds)
    } else {
        Err(serde::de::Error::custom(
            "autosave_seconds must be between 1 and 60",
        ))
    }
}
