//! Versioned report definitions and local report execution, independent of the TUI.
use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, NaiveDateTime, Timelike};
use regex::Regex;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub const MEMORY_HISTORY_YAML: &str = include_str!("../reports/memory-history.yaml");

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub inputs: Vec<Input>,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub default: String,
    #[serde(default)]
    pub required: bool,
}

// Each step consumes the preceding step's data. Add new typed steps here as the DSL grows.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    CachedErrorLogs,
    MemoryHistory {
        day_input: String,
        host_input: String,
    },
}

impl Definition {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("Reading {}", path.display()))?;
        let definition: Self = serde_yaml::from_str(&text).context("Invalid report YAML")?;
        definition.validate()?;
        Ok(definition)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("Unsupported report version {}; expected 1", self.version);
        }
        if self.name.trim().is_empty() {
            bail!("Report name is required");
        }
        let mut ids = BTreeSet::new();
        for input in &self.inputs {
            if input.id.trim().is_empty() || !ids.insert(&input.id) {
                bail!("Input IDs must be nonempty and unique: {}", input.id);
            }
        }
        // Version 1 has one source and one analysis. Reject other orders instead of silently
        // ignoring steps; this boundary can expand when another data/analysis type is added.
        let [
            Step::CachedErrorLogs,
            Step::MemoryHistory {
                day_input,
                host_input,
            },
        ] = self.steps.as_slice()
        else {
            bail!("Expected cached_error_logs followed by memory_history");
        };
        for id in [day_input, host_input] {
            if !ids.contains(id) {
                bail!("Unknown input referenced by memory_history: {id}");
            }
        }
        Ok(())
    }

    pub fn validate_values(&self, values: &BTreeMap<String, String>) -> Result<()> {
        for input in &self.inputs {
            if input.required && values.get(&input.id).is_none_or(|v| v.trim().is_empty()) {
                bail!("{} is required", input.label);
            }
        }
        let Some(Step::MemoryHistory { day_input, .. }) = self.steps.last() else {
            return Ok(());
        };
        parse_day(
            values
                .get(day_input)
                .map(String::as_str)
                .unwrap_or("latest"),
        )?;
        Ok(())
    }
}

fn parse_day(value: &str) -> Result<Option<NaiveDate>> {
    if value.trim().is_empty() || value.trim() == "latest" {
        return Ok(None);
    }
    Ok(Some(
        NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
            .context("Day must be YYYY-MM-DD or latest")?,
    ))
}

pub struct Entry {
    pub path: PathBuf,
    pub definition: Result<Definition, String>,
}

pub fn discover(root: &Path) -> Result<Vec<Entry>> {
    fs::create_dir_all(root)?;
    // Seed once; editing or removing the built-in definition is respected thereafter.
    let marker = root.join(".initialized-v1");
    if !marker.exists() {
        let path = root.join("memory-history.yaml");
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(MEMORY_HISTORY_YAML.as_bytes())?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        fs::write(marker, "1\n")?;
    }
    let mut paths = fs::read_dir(root)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    Ok(paths
        .into_iter()
        .filter(|p| {
            p.is_file() && matches!(p.extension().and_then(|v| v.to_str()), Some("yaml" | "yml"))
        })
        .map(|path| Entry {
            definition: Definition::load(&path).map_err(|e| format!("{e:#}")),
            path,
        })
        .collect())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub host: String,
    pub time: NaiveDateTime,
    pub percentages: BTreeMap<String, f64>,
}

impl Sample {
    pub fn hour(&self) -> f64 {
        self.time.time().num_seconds_from_midnight() as f64 / 3600.0
            + self.time.time().nanosecond() as f64 / 3_600_000_000_000.0
    }
}

#[derive(Debug)]
pub struct MemoryHistory {
    pub day: NaiveDate,
    pub samples: Vec<Sample>,
    pub files: usize,
    pub malformed: usize,
    pub warnings: Vec<String>,
}

pub fn execute(
    definition: &Definition,
    values: &BTreeMap<String, String>,
    cache: &Path,
) -> Result<MemoryHistory> {
    definition.validate()?;
    definition.validate_values(values)?;
    let mut samples = Vec::new();
    let mut files = 0;
    let mut malformed = 0;
    let mut warnings = Vec::new();
    let parser = MemoryParser::new()?;
    let mut loaded = false;
    for step in &definition.steps {
        match step {
            Step::CachedErrorLogs => {
                files = crate::logs::visit_cached_error_logs(
                    cache,
                    |host, line| {
                        if !line.contains("Memory ") || !line.contains("phys=") {
                            return;
                        }
                        match parser.parse(host, line) {
                            Some(sample) => samples.push(sample),
                            None => malformed += 1,
                        }
                    },
                    &mut warnings,
                )?;
                loaded = true;
            }
            Step::MemoryHistory {
                day_input,
                host_input,
            } => {
                if !loaded {
                    bail!("memory_history needs cached_error_logs first");
                }
                let host = values.get(host_input).map(|v| v.trim()).unwrap_or("");
                if !host.is_empty() {
                    samples.retain(|s| s.host == host);
                }
                let day = parse_day(values.get(day_input).map(String::as_str).unwrap_or("latest"))?
                    .or_else(|| samples.iter().map(|s| s.time.date()).max())
                    .context("No memory records found. Download ErrorLog.txt in :logs first; check the host input.")?;
                samples.retain(|s| s.time.date() == day);
                if samples.is_empty() {
                    bail!(
                        "No memory records for {day}. Download the corresponding error logs in :logs."
                    );
                }
                samples.sort_by(|a, b| a.time.cmp(&b.time).then(a.host.cmp(&b.host)));
                // Rotated files and repeated downloads can overlap. Keep distinct samples at
                // the same timestamp while removing exact duplicates, regardless of file order.
                let mut seen = BTreeSet::new();
                samples.retain(|s| {
                    seen.insert(format!("{}\0{}\0{:?}", s.host, s.time, s.percentages))
                });
                return Ok(MemoryHistory {
                    day,
                    samples,
                    files,
                    malformed,
                    warnings,
                });
            }
        }
    }
    bail!("Report has no output step")
}

struct MemoryParser {
    record: Regex,
    percentage: Regex,
}
impl MemoryParser {
    fn new() -> Result<Self> {
        Ok(Self {
            record: Regex::new(
                r"^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(?:\.\d+)?)\s+.*?\bMemory (\d+(?:\.\d+)?)%\s+phys=\d+\s+(size=.*)$",
            )?,
            percentage: Regex::new(r"\b([a-z]+)=\d+\((\d+(?:\.\d+)?)%\)")?,
        })
    }
    fn parse(&self, host: &str, line: &str) -> Option<Sample> {
        let captures = self.record.captures(line.trim_end())?;
        let time = NaiveDateTime::parse_from_str(&captures[1], "%Y-%m-%d %H:%M:%S%.f").ok()?;
        let mut percentages = BTreeMap::from([("used".into(), captures[2].parse().ok()?)]);
        for capture in self.percentage.captures_iter(&captures[3]) {
            percentages.insert(capture[1].into(), capture[2].parse().ok()?);
        }
        if !percentages.contains_key("size") {
            return None;
        }
        Some(Sample {
            host: host.into(),
            time,
            percentages,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ml-reports-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn definition() -> Definition {
        serde_yaml::from_str(MEMORY_HISTORY_YAML).unwrap()
    }

    fn line(time: &str, used: u32) -> String {
        format!(
            "{time} Info: Memory {used}% phys=32000 size=42000(131%) rss=16000(50%) anon=8000(25%) swap=0(0%) forest=9000(28%) cache=4000(12%)\n"
        )
    }

    fn snapshot(root: &Path, id: &str, host: &str, name: &str, text: &str, inline: bool) {
        fs::write(root.join(format!("{id}.json")), serde_json::to_vec(&json!({
            "file": {"host": host, "name": name, "path": format!("/Logs/{name}"), "size": text.len(), "modified": ""},
            "start": 0, "end": text.len(), "length": text.len(), "anchor_start": 0,
            "anchor": "", "generation": 0, "text_bytes": text.len(), "text": if inline { text } else { "" }
        })).unwrap()).unwrap();
        if !inline {
            fs::write(root.join(format!("{id}.log")), text).unwrap();
        }
    }

    #[test]
    fn parses_memory_variants_without_inventing_missing_metrics() {
        let parser = MemoryParser::new().unwrap();
        let sample = parser
            .parse("host-a", &line("2026-10-26 12:30:00.419", 64))
            .unwrap();
        assert_eq!(sample.percentages["used"], 64.0);
        assert_eq!(sample.percentages["size"], 131.0);
        assert_eq!(sample.percentages["rss"], 50.0);
        assert_eq!(sample.percentages["swap"], 0.0);
        assert!((sample.hour() - (12.5 + 0.419 / 3600.0)).abs() < 1e-9);
        let older = parser
            .parse(
                "old",
                "2026-10-26 00:00:00 Warning: Memory 92% phys=100 size=110(110%)",
            )
            .unwrap();
        assert!(!older.percentages.contains_key("rss"));
        assert!(
            parser
                .parse(
                    "old",
                    "2026-10-26 12:00:00 Info: Memory 50% phys=10 size=bad"
                )
                .is_none()
        );
        assert!(
            parser
                .parse(
                    "old",
                    "2026-99-26 12:00:00 Info: Memory 50% phys=10 size=10(100%)"
                )
                .is_none()
        );
    }

    #[test]
    fn executes_across_hosts_rotations_and_cache_formats_with_committed_prefix_only() {
        let temp = Temp::new();
        let first = line("2026-10-26 08:00:00.000", 60);
        let last = line("2026-10-26 23:00:00.000", 80);
        snapshot(
            &temp.0,
            "one",
            "host-a",
            "ErrorLog.txt",
            &format!("{last}{first}"),
            false,
        );
        // A download in flight may have appended content without committing metadata yet.
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(temp.0.join("one.log"))
            .unwrap()
            .write_all(line("2026-10-27 01:00:00.000", 99).as_bytes())
            .unwrap();
        snapshot(&temp.0, "two", "host-a", "ErrorLog_1.txt", &first, true);
        snapshot(
            &temp.0,
            "three",
            "host-b",
            "ErrorLog.txt",
            &line("2026-10-26 12:00:00.000", 70),
            false,
        );
        snapshot(
            &temp.0,
            "ignore",
            "host-c",
            "8000_ErrorLog.txt",
            &line("2026-10-28 01:00:00.000", 90),
            true,
        );
        let report = execute(&definition(), &BTreeMap::new(), &temp.0).unwrap();
        assert_eq!(report.day.to_string(), "2026-10-26");
        assert_eq!(report.samples.len(), 3);
        assert_eq!(report.files, 3);
        assert_eq!(report.samples[0].percentages["used"], 60.0);
        assert_eq!(report.samples[1].host, "host-b");
        assert_eq!(report.samples[2].percentages["used"], 80.0);
    }

    #[test]
    fn filters_day_and_host_and_chooses_latest_for_that_host() {
        let temp = Temp::new();
        snapshot(
            &temp.0,
            "a",
            "a",
            "ErrorLog.txt",
            &format!(
                "{}{}",
                line("2026-10-25 20:00:00", 30),
                line("2026-10-26 20:00:00", 40)
            ),
            true,
        );
        snapshot(
            &temp.0,
            "b",
            "b",
            "ErrorLog.txt",
            &line("2026-10-27 20:00:00", 60),
            true,
        );
        let mut values =
            BTreeMap::from([("host".into(), "a".into()), ("day".into(), "latest".into())]);
        assert_eq!(
            execute(&definition(), &values, &temp.0)
                .unwrap()
                .day
                .to_string(),
            "2026-10-26"
        );
        values.insert("day".into(), "2026-10-25".into());
        let report = execute(&definition(), &values, &temp.0).unwrap();
        assert_eq!(report.samples.len(), 1);
        assert_eq!(report.samples[0].percentages["used"], 30.0);
        values.insert("day".into(), "nonsense".into());
        assert!(
            execute(&definition(), &values, &temp.0)
                .unwrap_err()
                .to_string()
                .contains("Day must")
        );
    }

    #[test]
    fn bad_cache_and_malformed_records_are_reported_while_other_hosts_work() {
        let temp = Temp::new();
        snapshot(
            &temp.0,
            "bad",
            "broken",
            "ErrorLog.txt",
            &line("2026-10-26 12:00:00", 20),
            false,
        );
        fs::write(temp.0.join("bad.log"), "truncated").unwrap();
        snapshot(
            &temp.0,
            "good",
            "working",
            "ErrorLog.txt",
            &format!(
                "{}2026-10-26 14:00:00 Info: Memory 40% phys=10 size=broken\n",
                line("2026-10-26 12:00:00", 30)
            ),
            true,
        );
        let report = execute(&definition(), &BTreeMap::new(), &temp.0).unwrap();
        assert_eq!(report.samples.len(), 1);
        assert_eq!(report.malformed, 1);
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn strict_yaml_validation_and_discovery_preserve_user_edits() {
        let temp = Temp::new();
        let entries = discover(&temp.0).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].definition.is_ok());
        let path = &entries[0].path;
        let edited = MEMORY_HISTORY_YAML.replace("name: Memory history", "name: Custom history");
        fs::write(path, &edited).unwrap();
        assert_eq!(
            discover(&temp.0).unwrap()[0]
                .definition
                .as_ref()
                .unwrap()
                .name,
            "Custom history"
        );
        fs::write(path, edited.replace("version: 1", "version: 99")).unwrap();
        assert!(
            discover(&temp.0).unwrap()[0]
                .definition
                .as_ref()
                .unwrap_err()
                .contains("Unsupported report version")
        );
        fs::write(
            path,
            MEMORY_HISTORY_YAML.replace("type: cached_error_logs", "type: shell"),
        )
        .unwrap();
        assert!(Definition::load(path).is_err());
        fs::write(
            path,
            MEMORY_HISTORY_YAML.replace("day_input: day", "day_input: typo"),
        )
        .unwrap();
        assert!(
            Definition::load(path)
                .unwrap_err()
                .to_string()
                .contains("Unknown input")
        );
        fs::write(path, format!("{MEMORY_HISTORY_YAML}\nunknown: true\n")).unwrap();
        assert!(Definition::load(path).is_err());
        fs::remove_file(path).unwrap();
        assert!(discover(&temp.0).unwrap().is_empty());
    }
}
