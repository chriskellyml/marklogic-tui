//! Legacy TUI terms and structured desktop filter rules.
use regex::{Regex, RegexBuilder};

fn terms(input: &str) -> Vec<Regex> {
    let input = input.trim();
    let terms: Vec<&str> = if let Some(rest) = input.strip_prefix("-F") {
        if let Some(delim) = rest.chars().next() {
            rest[delim.len_utf8()..]
                .split(delim)
                .map(str::trim)
                .collect()
        } else {
            Vec::new()
        }
    } else {
        vec![input]
    };
    terms
        .into_iter()
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            RegexBuilder::new(&regex::escape(s))
                .case_insensitive(!s.chars().any(char::is_uppercase))
                .build()
                .ok()
        })
        .collect()
}

pub struct Terms {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
}
impl Terms {
    pub fn new(include: &str, exclude: &str) -> Self {
        Self {
            include: terms(include),
            exclude: terms(exclude),
        }
    }
    pub fn matches(&self, fields: &[&str]) -> bool {
        let has = |pattern: &Regex| fields.iter().any(|field| pattern.is_match(field));
        (self.include.is_empty() || self.include.iter().any(has)) && !self.exclude.iter().any(has)
    }
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Field {
    Any,
    Text,
    Host,
    File,
    Port,
    Method,
    Level,
    Kind,
}
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    Include,
    Exclude,
}
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Literal,
    Exact,
    Regex,
}
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Case {
    Insensitive,
    Sensitive,
    Smart,
}
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub id: String,
    pub text: String,
    pub enabled: bool,
    pub action: Action,
    pub field: Field,
    pub mode: Mode,
    pub case: Case,
}

pub struct Rules(Vec<(Field, Action, Regex)>);
impl Rules {
    pub fn new(rules: &[Rule]) -> anyhow::Result<Self> {
        use anyhow::Context;
        let mut compiled = Vec::new();
        for rule in rules.iter().filter(|r| r.enabled && !r.text.is_empty()) {
            let pattern = match rule.mode {
                Mode::Literal => regex::escape(&rule.text),
                Mode::Exact => format!(r"\A(?:{})\z", regex::escape(&rule.text)),
                Mode::Regex => rule.text.clone(),
            };
            let pattern = RegexBuilder::new(&pattern)
                .case_insensitive(match rule.case {
                    Case::Insensitive => true,
                    Case::Sensitive => false,
                    Case::Smart => !rule.text.chars().any(char::is_uppercase),
                })
                .build()
                .with_context(|| format!("Invalid filter {:?}", rule.text))?;
            compiled.push((rule.field, rule.action, pattern));
        }
        Ok(Self(compiled))
    }
    pub fn matches(
        &self,
        host: &str,
        file: &str,
        raw: &str,
        port: &str,
        level: &str,
        kind: &str,
    ) -> bool {
        let mut included = false;
        let mut has_include = false;
        // Extract HTTP metadata once per record, only when a method rule needs it.
        let method = self
            .0
            .iter()
            .any(|(f, _, _)| matches!(f, Field::Method))
            .then(|| super::presentation::http_method(raw));
        for (field, action, pattern) in &self.0 {
            let matched = match field {
                Field::Any => [host, file, raw, port, level, kind]
                    .iter()
                    .any(|v| pattern.is_match(v)),
                Field::Text => pattern.is_match(raw),
                Field::Host => pattern.is_match(host),
                Field::File => pattern.is_match(file),
                Field::Port => pattern.is_match(port),
                Field::Method => method
                    .as_ref()
                    .and_then(|v| v.as_deref())
                    .is_some_and(|v| pattern.is_match(v)),
                Field::Level => pattern.is_match(level),
                Field::Kind => pattern.is_match(kind),
            };
            match action {
                Action::Exclude if matched => return false,
                Action::Include => {
                    has_include = true;
                    included |= matched;
                }
                _ => {}
            }
        }
        !has_include || included
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(text: &str, field: Field, action: Action, mode: Mode) -> Rule {
        Rule {
            id: "test".into(),
            text: text.into(),
            enabled: true,
            action,
            field,
            mode,
            case: Case::Sensitive,
        }
    }
    #[test]
    fn structured_rules_preserve_case_literals_disabled_terms_and_exclude_priority() {
        let mut rules = vec![
            rule("Timeout|a,b", Field::Text, Action::Include, Mode::Literal),
            rule("Noise", Field::Text, Action::Exclude, Mode::Literal),
        ];
        let matches = |rules: &[Rule], raw| {
            Rules::new(rules)
                .unwrap()
                .matches("host", "file", raw, "8000", "Info", "ERR")
        };
        assert!(matches(&rules, "Timeout|a,b"));
        assert!(!matches(&rules, "timeout|a,b"));
        assert!(!matches(&rules, "Timeout"));
        assert!(!matches(&rules, "Timeout|a,b Noise"));
        rules[0].case = Case::Insensitive;
        rules[1].enabled = false;
        assert!(matches(&rules, "timeout|a,b Noise"));
        rules[0].enabled = false;
        assert!(matches(&rules, "anything"));
    }
    #[test]
    fn structured_regex_validation_and_scoped_fields() {
        let rules = Rules::new(&[
            rule(r"^(GET|POST)$", Field::Method, Action::Include, Mode::Regex),
            rule("8000", Field::Port, Action::Exclude, Mode::Exact),
        ])
        .unwrap();
        let access = |method: &str| {
            format!(
                r#"127.0.0.1 - admin [14/Oct/2026:10:30:01 +0000] "{method} /8000 HTTP/1.1" 200 89"#
            )
        };
        assert!(rules.matches("host", "file", &access("GET"), "18000", "", "ACC"));
        assert!(!rules.matches("host", "file", &access("GET"), "8000", "", "ACC"));
        assert!(!rules.matches("GET", "file", &access("DELETE"), "18000", "", "ACC"));
        assert!(!rules.matches(
            "host",
            "file",
            "GET in an error message",
            "18000",
            "Error",
            "ERR"
        ));
        assert!(rules.matches("host", "file", r#"{"method":"POST"}"#, "18000", "", "REQ"));
        assert!(!rules.matches("host", "file", r#"{"url":"/GET"}"#, "18000", "", "REQ"));
        assert!(Rules::new(&[rule("[", Field::Text, Action::Include, Mode::Regex)]).is_err());
        // Rust regex syntax (including inline flags) is validated by the actual matcher.
        assert!(
            Rules::new(&[rule(
                "(?i)timeout",
                Field::Text,
                Action::Include,
                Mode::Regex
            )])
            .is_ok()
        );
    }
    #[test]
    fn tui_smart_case_delimiters_and_metadata() {
        let filter = Terms::new("-F, error,Timeout", "-F, debug,NOISE");
        assert!(filter.matches(&["host-a", "ErrorLog.txt", "SERVER ERROR"]));
        assert!(filter.matches(&["host-a", "AccessLog.txt", "Timeout"]));
        assert!(!filter.matches(&["host-a", "AccessLog.txt", "timeout"]));
        assert!(!filter.matches(&["host-a", "ErrorLog.txt", "Debug"]));
        assert!(filter.matches(&["host-a", "ErrorLog.txt", "noise"]));
        assert!(!filter.matches(&["host-a", "ErrorLog.txt", "NOISE"]));
        assert!(Terms::new("ä", "").matches(&["Ä"]));
        assert!(Terms::new(" -F|host-a| beta ", "").matches(&["HOST-A", "message"]));
    }
}
