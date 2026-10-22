//! Syntax highlighting for query files in the inline editor.
//!
//! edtui does the rendering. This module supplies the grammars: syntect's
//! defaults plus the bundled XQuery grammar, built once and shared. It uses
//! edtui's re-exported syntect so the types always match edtui's.

use edtui::syntect::highlighting::Theme;
use edtui::syntect::parsing::{SyntaxDefinition, SyntaxSet};
use edtui::{SyntaxHighlighter, THEME_SET};
use std::path::Path;
use std::sync::{Arc, OnceLock};

const XQUERY_GRAMMAR: &str = include_str!("../assets/xquery.sublime-syntax");
const THEME_NAME: &str = "dracula";

struct Highlighting {
    syntax_set: Arc<SyntaxSet>,
    theme: Theme,
}

fn highlighting() -> &'static Highlighting {
    static HIGHLIGHTING: OnceLock<Highlighting> = OnceLock::new();
    HIGHLIGHTING.get_or_init(|| {
        let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
        let xquery = SyntaxDefinition::load_from_str(XQUERY_GRAMMAR, false, Some("XQuery"))
            .expect("bundled XQuery grammar is valid");
        builder.add(xquery);
        Highlighting {
            syntax_set: Arc::new(builder.build()),
            theme: THEME_SET
                .themes
                .get(THEME_NAME)
                .cloned()
                .expect("edtui bundles the dracula theme"),
        }
    })
}

/// Highlighter for a query file, chosen by its extension. Returns `None` when
/// there is no path or no grammar for it (for example `.sparql`).
pub(crate) fn highlighter_for(path: Option<&Path>) -> Option<SyntaxHighlighter> {
    let extension = path?.extension()?.to_str()?.to_ascii_lowercase();
    let grammar = match extension.as_str() {
        "xqy" => "xqy",
        "js" | "mjs" | "sjs" => "js",
        "sql" => "sql",
        _ => return None,
    };
    let highlighting = highlighting();
    let syntax = highlighting
        .syntax_set
        .find_syntax_by_extension(grammar)?
        .clone();
    Some(SyntaxHighlighter::with_sets(
        highlighting.theme.clone(),
        THEME_SET.clone(),
        syntax,
        highlighting.syntax_set.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use edtui::syntect::parsing::{ParseState, ScopeStack};

    /// Scope names in effect at the first occurrence of `needle` in `source`.
    fn scopes_at(source: &str, needle: &str) -> Vec<String> {
        let set = &highlighting().syntax_set;
        let syntax = set.find_syntax_by_extension("xqy").expect("xquery grammar");
        // edtui highlights each line without its trailing newline.
        let line = source.to_string();
        let target = line
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in {source:?}"));
        let mut parse = ParseState::new(syntax);
        let mut stack = ScopeStack::new();
        for (index, op) in parse.parse_line(&line, set).expect("line parses") {
            if index > target {
                break;
            }
            stack.apply(&op).expect("scope stack applies");
        }
        stack
            .as_slice()
            .iter()
            .map(|scope| scope.build_string())
            .collect()
    }

    fn has(scopes: &[String], name: &str) -> bool {
        scopes.iter().any(|scope| scope == name)
    }

    #[test]
    fn xquery_grammar_is_registered_for_xqy() {
        let syntax = highlighting()
            .syntax_set
            .find_syntax_by_extension("xqy")
            .expect("xquery grammar");
        assert_eq!(syntax.name, "XQuery");
    }

    #[test]
    fn highlighter_for_maps_query_extensions() {
        assert!(highlighter_for(Some(Path::new("a.xqy"))).is_some());
        assert!(highlighter_for(Some(Path::new("a.XQY"))).is_some());
        assert!(highlighter_for(Some(Path::new("a.sjs"))).is_some());
        assert!(highlighter_for(Some(Path::new("a.mjs"))).is_some());
        assert!(highlighter_for(Some(Path::new("a.sql"))).is_some());
        assert!(highlighter_for(Some(Path::new("a.sparql"))).is_none());
        assert!(highlighter_for(None).is_none());
    }

    #[test]
    fn prolog_keywords_are_storage_scopes() {
        let scopes = scopes_at("declare function local:f($a) { 1 };", "declare");
        assert!(has(&scopes, "storage.type.xquery"), "{scopes:?}");
    }

    #[test]
    fn flwor_keywords_are_control_scopes() {
        let scopes = scopes_at("for $x in 1 to 3 return $x", "return");
        assert!(has(&scopes, "keyword.control.xquery"), "{scopes:?}");
    }

    #[test]
    fn variables_are_variable_scopes() {
        let scopes = scopes_at("declare variable $DRY := true();", "$DRY");
        assert!(has(&scopes, "variable.other.xquery"), "{scopes:?}");
    }

    #[test]
    fn strings_use_string_scopes_and_doubled_quote_escapes() {
        let scopes = scopes_at("let $s := \"a\"\"b\"", "a\"\"b");
        assert!(has(&scopes, "string.quoted.double.xquery"), "{scopes:?}");

        let scopes = scopes_at("let $s := 'it''s'", "''");
        assert!(
            has(&scopes, "constant.character.escape.xquery"),
            "{scopes:?}"
        );
    }

    #[test]
    fn comments_are_comment_scopes_and_nest() {
        let scopes = scopes_at("(: outer (: inner :) still :) declare", "still");
        assert!(has(&scopes, "comment.block.xquery"), "{scopes:?}");

        let scopes = scopes_at("(: outer (: inner :) still :) declare", "declare");
        assert!(!has(&scopes, "comment.block.xquery"), "{scopes:?}");
    }

    #[test]
    fn keyword_inside_hyphenated_name_is_not_a_keyword() {
        let scopes = scopes_at("let $x := order-id", "order-id");
        assert!(!has(&scopes, "keyword.control.xquery"), "{scopes:?}");
    }

    #[test]
    fn numbers_are_numeric_scopes() {
        let scopes = scopes_at("(1 to 10000)[1 to 10]", "10000");
        assert!(has(&scopes, "constant.numeric.xquery"), "{scopes:?}");
    }

    #[test]
    fn prefixed_calls_are_function_names() {
        let scopes = scopes_at("local:operate($uri)", "operate");
        assert!(has(&scopes, "entity.name.function.xquery"), "{scopes:?}");
    }

    #[test]
    fn unprefixed_calls_are_support_functions() {
        let scopes = scopes_at("let $n := count($x)", "count");
        assert!(has(&scopes, "support.function.xquery"), "{scopes:?}");
    }
}
