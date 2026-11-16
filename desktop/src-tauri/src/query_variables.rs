use marklogic_core::config::AppConfig;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Variable {
    name: String,
    sequence_type: String,
    default_value: Option<String>,
    #[serde(skip)]
    start: usize,
    #[serde(skip)]
    end: usize,
}

// Mask nested XQuery comments and string literals, preserving byte offsets.
fn mask(source: &str) -> String {
    let b = source.as_bytes();
    let mut out = b.to_vec();
    let (mut i, mut depth, mut quote) = (0, 0, None);
    while i < b.len() {
        let pair = b.get(i..i + 2);
        if depth > 0 {
            if pair == Some(b"(:") {
                depth += 1;
                out[i..i + 2].fill(b' ');
                i += 2;
            } else if pair == Some(b":)") {
                depth -= 1;
                out[i..i + 2].fill(b' ');
                i += 2;
            } else {
                out[i] = b' ';
                i += 1;
            }
        } else if let Some(q) = quote {
            out[i] = b' ';
            if b[i] == q {
                if b.get(i + 1) == Some(&q) {
                    out[i + 1] = b' ';
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += 1;
        } else if pair == Some(b"(:") {
            depth = 1;
            out[i..i + 2].fill(b' ');
            i += 2;
        } else if b[i] == b'\'' || b[i] == b'"' {
            quote = Some(b[i]);
            out[i] = b' ';
            i += 1;
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

pub fn declarations(source: &str) -> Vec<Variable> {
    let masked = mask(source);
    let pattern = regex::Regex::new(r"\bdeclare\s+variable\s+\$([\w.:-]+)\s*(?:as\s+([^;]*?))?\s*\b(external)\b\s*(?::=[^;]*)?;").unwrap();
    pattern
        .captures_iter(&masked)
        .map(|c| {
            let whole = c.get(0).unwrap();
            let external = c.get(3).unwrap().start();
            let tail = &source[external + 8..whole.end() - 1];
            let masked_tail = &masked[external + 8..whole.end() - 1];
            Variable {
                name: c[1].to_string(),
                sequence_type: c
                    .get(2)
                    .map_or("item()*", |m| m.as_str().trim())
                    .to_string(),
                default_value: masked_tail
                    .find(":=")
                    .map(|at| tail[at + 2..].trim().to_string()),
                start: external,
                end: whole.end() - 1,
            }
        })
        .collect()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryVariables {
    variables: Vec<Variable>,
    globals: BTreeMap<String, String>,
}

#[tauri::command]
pub fn query_variables(source: String) -> Result<QueryVariables, String> {
    let config = AppConfig::load().map_err(|e| e.to_string())?;
    Ok(QueryVariables {
        variables: declarations(&source),
        globals: config.active_variables,
    })
}

#[derive(Deserialize)]
pub struct Binding {
    value: String,
    #[serde(default)]
    expression: bool,
}

pub fn bind(
    source: &str,
    bindings: &BTreeMap<String, Binding>,
    globals: &BTreeMap<String, String>,
) -> Result<String, String> {
    let variables = declarations(source);
    let mut result = source.to_string();
    for variable in variables.iter().rev() {
        let Some(binding) = bindings.get(&variable.name) else {
            if variable.default_value.is_none() {
                return Err(format!(
                    "${}: provide a value in External variables",
                    variable.name
                ));
            }
            continue;
        };
        let value = marklogic_core::query::substitute_active_variables(&binding.value, globals);
        if value.contains("%%") {
            return Err(format!("${}: unresolved global value", variable.name));
        }
        let expression = if binding.expression {
            if value.trim().is_empty() {
                return Err(format!("${}: enter an XQuery expression", variable.name));
            }
            value
        } else {
            let literal = format!("\"{}\"", value.replace('&', "&amp;").replace('"', "\"\""));
            let ty = variable.sequence_type.trim_end_matches(['?', '*', '+']);
            if ty.starts_with("xs:") && ty != "xs:anyAtomicType" {
                format!("{ty}({literal})")
            } else if ty == "item()" || ty == "xs:anyAtomicType" {
                literal
            } else {
                return Err(format!(
                    "${}: use XQuery expression mode for {}",
                    variable.name, variable.sequence_type
                ));
            }
        };
        result.replace_range(variable.start..variable.end, &format!(":= ({expression})"));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_only_external_declarations_outside_comments_and_strings() {
        let s = "(: outer (: declare variable $bad external; :) :)\ndeclare variable $n as xs:int external := 4; declare variable $label external := 'a;b'; declare variable $local := 'declare variable $fake external;';";
        let vars = declarations(s);
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].sequence_type, "xs:int");
        assert_eq!(vars[1].default_value.as_deref(), Some("'a;b'"));
    }
    #[test]
    fn preserves_defaults_and_accepts_typed_sequence_expressions() {
        let source = "declare variable $values as xs:int* external := (); $values";
        assert_eq!(
            bind(source, &BTreeMap::new(), &BTreeMap::new()).unwrap(),
            source
        );
        let bindings = BTreeMap::from([(
            "values".into(),
            Binding {
                value: "(1, 2)".into(),
                expression: true,
            },
        )]);
        assert!(
            bind(source, &bindings, &BTreeMap::new())
                .unwrap()
                .contains("as xs:int* := ((1, 2))")
        );
        assert!(
            bind(
                "declare variable $required external; $required",
                &BTreeMap::new(),
                &BTreeMap::new()
            )
            .unwrap_err()
            .contains("$required")
        );
        let bindings = BTreeMap::from([(
            "values".into(),
            Binding {
                value: "%%MISSING%%".into(),
                expression: false,
            },
        )]);
        assert!(
            bind(source, &bindings, &BTreeMap::new())
                .unwrap_err()
                .contains("unresolved global")
        );
    }

    #[test]
    fn converts_typed_literals_and_resolves_globals_without_injecting_code() {
        let bindings = BTreeMap::from([
            (
                "n".into(),
                Binding {
                    value: "%%N%%".into(),
                    expression: false,
                },
            ),
            (
                "s".into(),
                Binding {
                    value: "a&\"b".into(),
                    expression: false,
                },
            ),
        ]);
        let s = bind("declare variable $n as xs:int external; declare variable $s as xs:string external; ($n,$s)", &bindings, &BTreeMap::from([("N".into(), "42".into())])).unwrap();
        assert!(s.contains("xs:int(\"42\")"));
        assert!(s.contains("xs:string(\"a&amp;\"\"b\")"));
    }
}
