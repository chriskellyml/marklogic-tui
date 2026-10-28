//! Expansion of `import function` / `import variable` directives in XQuery files.
//!
//! Syntax (PoC):
//!
//! ```text
//! import function path/to/lib.xqy:my-function#4;
//! import variable path/to/lib.xqy:my-variable;
//! ```
//!
//! Each directive is replaced, before the query is sent to MarkLogic, by the
//! matching `declare function` / `declare variable` declaration from the
//! referenced file. Paths are resolved relative to the directory of the file
//! that contains the directive. Imported files may contain their own
//! directives; those are expanded first and deduplicated across the whole
//! query. Declarations keep their original prefix, so callers keep using the
//! names exactly as declared in the source file.
//!
//! Expansion never changes the line count of the query: replacement
//! declarations are emitted on the directive's line, and any newlines the
//! directive itself spanned are re-added after it.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

const IMPORT_KEYWORD: &[u8] = b"import";
const DECLARE_KEYWORD: &[u8] = b"declare";

/// Expand all `import` directives in `source`. `file` is the path of the query
/// file the source belongs to; relative import paths resolve against its
/// directory. Returns the source unchanged when it contains no directives.
pub fn expand_xquery_imports(source: &str, file: &Path) -> Result<String> {
    // Fast path: queries without the keyword are sent untouched, so the lexer
    // never runs on them (XQuery element content may contain bare apostrophes
    // that would look like unterminated string literals).
    if !source.contains("import") {
        return Ok(source.to_string());
    }
    let root_source = Source::new(source)?;
    let pieces = split_imports(&root_source)?;
    if !pieces
        .iter()
        .any(|piece| matches!(piece, Piece::Import { .. }))
    {
        return Ok(source.to_string());
    }

    let root = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let base_dir = root
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let mut expander = Expander::new(root.clone());
    for decl in find_declarations(&root_source)? {
        expander.owners.insert(decl.name.clone(), root.clone());
    }

    let mut expanded = String::with_capacity(source.len());
    for piece in pieces {
        match piece {
            Piece::Text(text) => expanded.push_str(text),
            Piece::Import { request, raw, line } => {
                let mut decls = Vec::new();
                expander
                    .resolve(&request, &base_dir, &mut decls)
                    .with_context(|| format!("line {}", line))?;
                expanded.push_str(&decls.join(" "));
                expanded.extend(std::iter::repeat_n('\n', raw.matches('\n').count()));
            }
        }
    }
    Ok(expanded)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DeclKind {
    Function,
    Variable,
}

/// Identity of a declaration within the query: its kind, local name and, for
/// functions, arity. The prefix is ignored, matching how the import syntax
/// names things.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DeclName {
    kind: DeclKind,
    name: String,
    arity: Option<usize>,
}

impl fmt::Display for DeclName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.kind, self.arity) {
            (DeclKind::Function, Some(arity)) => write!(f, "function {}#{}", self.name, arity),
            (DeclKind::Function, None) => write!(f, "function {}", self.name),
            (DeclKind::Variable, _) => write!(f, "variable ${}", self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportRequest {
    path: String,
    name: DeclName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Code,
    Comment,
    Literal,
}

/// Source text paired with a per-byte classification, so that scanning for
/// keywords and delimiters can skip comments and string literals.
struct Source {
    text: String,
    class: Vec<Class>,
}

impl Source {
    fn new(text: &str) -> Result<Self> {
        let bytes = text.as_bytes();
        let n = bytes.len();
        let mut class = vec![Class::Code; n];
        let mut i = 0;
        while i < n {
            if bytes[i] == b'(' && i + 1 < n && bytes[i + 1] == b':' {
                let start = i;
                let mut depth = 0usize;
                let mut j = i;
                while j < n {
                    if bytes[j] == b'(' && j + 1 < n && bytes[j + 1] == b':' {
                        depth += 1;
                        j += 2;
                    } else if bytes[j] == b':' && j + 1 < n && bytes[j + 1] == b')' {
                        depth -= 1;
                        j += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        j += 1;
                    }
                }
                if depth != 0 {
                    bail!("unterminated comment starting at byte {}", start);
                }
                class[start..j].fill(Class::Comment);
                i = j;
            } else if bytes[i] == b'"' || bytes[i] == b'\'' {
                let quote = bytes[i];
                let start = i;
                let mut j = i + 1;
                loop {
                    if j >= n {
                        bail!("unterminated string literal starting at byte {}", start);
                    }
                    if bytes[j] == quote {
                        if j + 1 < n && bytes[j + 1] == quote {
                            j += 2;
                            continue;
                        }
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                class[start..j].fill(Class::Literal);
                i = j;
            } else {
                i += 1;
            }
        }
        Ok(Self {
            text: text.to_string(),
            class,
        })
    }

    fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::new(&text).with_context(|| format!("in {}", path.display()))
    }

    fn bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    fn len(&self) -> usize {
        self.text.len()
    }

    fn is_code(&self, i: usize) -> bool {
        i < self.len() && self.class[i] == Class::Code
    }

    /// Index of the first byte at or after `i` that is neither whitespace nor
    /// part of a comment.
    fn skip_trivia(&self, mut i: usize) -> usize {
        while i < self.len()
            && (self.bytes()[i].is_ascii_whitespace() || self.class[i] == Class::Comment)
        {
            i += 1;
        }
        i
    }

    /// Index just past the run of name characters starting at `i`.
    fn read_name(&self, mut i: usize) -> usize {
        while self.is_code(i) && is_name_byte(self.bytes()[i]) {
            i += 1;
        }
        i
    }

    /// Index of the `;` that ends the statement beginning at `from`, ignoring
    /// semicolons nested inside brackets.
    fn statement_end(&self, from: usize) -> Option<usize> {
        let mut depth: i64 = 0;
        for j in from..self.len() {
            if !self.is_code(j) {
                continue;
            }
            match self.bytes()[j] {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                b';' if depth == 0 => return Some(j),
                _ => {}
            }
        }
        None
    }

    /// Text of `start..end` with newlines outside string literals collapsed to
    /// spaces, so a declaration fits on one line.
    fn single_line(&self, start: usize, end: usize) -> Result<String> {
        let bytes: Vec<u8> = (start..end)
            .map(|k| {
                let b = self.bytes()[k];
                if b == b'\n' && self.class[k] != Class::Literal {
                    b' '
                } else {
                    b
                }
            })
            .collect();
        String::from_utf8(bytes).context("declaration is not valid UTF-8")
    }
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':')
}

/// True when the byte before `i` cannot be part of a longer identifier, so a
/// keyword starting at `i` is a standalone word.
fn is_word_boundary(bytes: &[u8], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = bytes[i - 1];
    !(is_name_byte(prev) || prev == b'$' || prev >= 0x80)
}

enum Piece<'a> {
    Text(&'a str),
    Import {
        request: ImportRequest,
        raw: &'a str,
        line: usize,
    },
}

fn split_imports(source: &Source) -> Result<Vec<Piece<'_>>> {
    let bytes = source.bytes();
    let n = source.len();
    let mut pieces = Vec::new();
    let mut last = 0;
    let mut i = 0;
    while i < n {
        if source.is_code(i)
            && bytes[i..].starts_with(IMPORT_KEYWORD)
            && is_word_boundary(bytes, i)
            && i + IMPORT_KEYWORD.len() < n
            && bytes[i + IMPORT_KEYWORD.len()].is_ascii_whitespace()
        {
            let word_start = source.skip_trivia(i + IMPORT_KEYWORD.len());
            let word_end = source.read_name(word_start);
            let word = &source.text[word_start..word_end];
            if word == "function" || word == "variable" {
                let line = source.text[..i].matches('\n').count() + 1;
                let semi = source
                    .statement_end(i)
                    .ok_or_else(|| anyhow!("line {}: import directive is missing ';'", line))?;
                let body = &source.text[i + IMPORT_KEYWORD.len()..semi];
                let request = parse_import(body.trim())
                    .with_context(|| format!("line {}: invalid import directive", line))?;
                pieces.push(Piece::Text(&source.text[last..i]));
                pieces.push(Piece::Import {
                    request,
                    raw: &source.text[i..=semi],
                    line,
                });
                last = semi + 1;
                i = semi + 1;
                continue;
            }
        }
        i += 1;
    }
    pieces.push(Piece::Text(&source.text[last..]));
    Ok(pieces)
}

/// Parse the text after the `import` keyword, e.g. `function a/b.xqy:f#4`.
fn parse_import(body: &str) -> Result<ImportRequest> {
    let (kind_word, rest) = body
        .split_once(char::is_whitespace)
        .ok_or_else(|| anyhow!("expected 'function' or 'variable' after 'import'"))?;
    let rest = rest.trim();
    match kind_word {
        "function" => {
            let (spec, arity) = rest
                .rsplit_once('#')
                .ok_or_else(|| anyhow!("function import must end with #<arity>"))?;
            let arity: usize = arity
                .trim()
                .parse()
                .map_err(|_| anyhow!("invalid arity '{}'", arity.trim()))?;
            let (path, name) = split_spec(spec)?;
            Ok(ImportRequest {
                path,
                name: DeclName {
                    kind: DeclKind::Function,
                    name,
                    arity: Some(arity),
                },
            })
        }
        "variable" => {
            let (path, name) = split_spec(rest)?;
            Ok(ImportRequest {
                path,
                name: DeclName {
                    kind: DeclKind::Variable,
                    name,
                    arity: None,
                },
            })
        }
        other => bail!("unknown import kind '{}'", other),
    }
}

/// Split `path/to/file.xqy:name` at its last colon.
fn split_spec(spec: &str) -> Result<(String, String)> {
    let (path, name) = spec
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("expected <path>:<name>, got '{}'", spec))?;
    let path = path.trim();
    let name = name.trim().trim_start_matches('$');
    if path.is_empty() {
        bail!("missing file path in '{}'", spec);
    }
    let mut chars = name.chars();
    let valid_start = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let valid_rest = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid_start || !valid_rest {
        bail!("invalid declaration name '{}'", name);
    }
    Ok((path.to_string(), name.to_string()))
}

#[derive(Debug)]
struct Decl {
    name: DeclName,
    start: usize,
    end: usize,
}

/// Find top-level `declare function` and `declare variable` declarations.
fn find_declarations(source: &Source) -> Result<Vec<Decl>> {
    let bytes = source.bytes();
    let n = source.len();
    let mut decls = Vec::new();
    let mut i = 0;
    while i < n {
        if source.is_code(i)
            && bytes[i..].starts_with(DECLARE_KEYWORD)
            && is_word_boundary(bytes, i)
            && let Some(decl) = parse_declaration(source, i)?
        {
            i = decl.end;
            decls.push(decl);
            continue;
        }
        i += 1;
    }
    Ok(decls)
}

fn parse_declaration(source: &Source, start: usize) -> Result<Option<Decl>> {
    let bytes = source.bytes();
    let n = source.len();
    let mut p = source.skip_trivia(start + DECLARE_KEYWORD.len());

    // Skip annotations such as `%private` or `%rest:GET("/x")`.
    while p < n && bytes[p] == b'%' && source.is_code(p) {
        p = source.skip_trivia(source.read_name(p + 1));
        if p < n && bytes[p] == b'(' && source.is_code(p) {
            p = source.skip_trivia(skip_group(source, p));
        }
    }

    let word_end = source.read_name(p);
    let word = &source.text[p..word_end];
    let end = match source.statement_end(start) {
        Some(semi) => semi + 1,
        None if word == "function" || word == "variable" => {
            bail!(
                "declaration at byte {} is missing its terminating ';'",
                start
            )
        }
        None => return Ok(None),
    };

    match word {
        "function" => {
            let name_start = source.skip_trivia(word_end);
            let name_end = source.read_name(name_start);
            let qname = &source.text[name_start..name_end];
            let local = local_part(qname);
            let paren = source.skip_trivia(name_end);
            if paren >= n || bytes[paren] != b'(' || !source.is_code(paren) {
                bail!("expected '(' after function name '{}'", qname);
            }
            let arity = count_params(source, paren);
            Ok(Some(Decl {
                name: DeclName {
                    kind: DeclKind::Function,
                    name: local.to_string(),
                    arity: Some(arity),
                },
                start,
                end,
            }))
        }
        "variable" => {
            let dollar = source.skip_trivia(word_end);
            if dollar >= n || bytes[dollar] != b'$' || !source.is_code(dollar) {
                bail!("expected '$' after 'declare variable'");
            }
            let name_start = dollar + 1;
            let name_end = source.read_name(name_start);
            let qname = &source.text[name_start..name_end];
            Ok(Some(Decl {
                name: DeclName {
                    kind: DeclKind::Variable,
                    name: local_part(qname).to_string(),
                    arity: None,
                },
                start,
                end,
            }))
        }
        // `declare namespace`, `declare option`, etc. are not importable.
        _ => Ok(None),
    }
}

fn local_part(qname: &str) -> &str {
    qname.rsplit(':').next().unwrap_or(qname)
}

/// Index just past the bracket group opening at `open`.
fn skip_group(source: &Source, open: usize) -> usize {
    let mut depth: i64 = 0;
    for j in open..source.len() {
        if !source.is_code(j) {
            continue;
        }
        match source.bytes()[j] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return j + 1;
                }
            }
            _ => {}
        }
    }
    source.len()
}

/// Number of `$` parameter markers directly inside the parameter list opening
/// at `open`.
fn count_params(source: &Source, open: usize) -> usize {
    let end = skip_group(source, open);
    let mut depth: i64 = 0;
    let mut count = 0;
    for j in open..end {
        if !source.is_code(j) {
            continue;
        }
        match source.bytes()[j] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'$' if depth == 1 => count += 1,
            _ => {}
        }
    }
    count
}

struct Expander {
    stack: Vec<PathBuf>,
    emitted: HashSet<(PathBuf, DeclName)>,
    /// Which file a given declaration name was claimed from. Used to reject
    /// two different sources (or the query itself) defining the same name.
    owners: HashMap<DeclName, PathBuf>,
}

impl Expander {
    fn new(root: PathBuf) -> Self {
        Self {
            stack: vec![root],
            emitted: HashSet::new(),
            owners: HashMap::new(),
        }
    }

    /// Append to `out` the declarations needed for `request`: first any
    /// declarations its own file imports, then the requested declaration.
    fn resolve(
        &mut self,
        request: &ImportRequest,
        base_dir: &Path,
        out: &mut Vec<String>,
    ) -> Result<()> {
        let file = base_dir.join(&request.path);
        let canon = file.canonicalize().with_context(|| {
            format!(
                "cannot import {}: file not found: {}",
                request.name,
                file.display()
            )
        })?;

        if let Some(pos) = self.stack.iter().position(|p| p == &canon) {
            let chain = self.stack[pos..]
                .iter()
                .chain(std::iter::once(&canon))
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" -> ");
            bail!("circular import: {}", chain);
        }

        let key = (canon.clone(), request.name.clone());
        if self.emitted.contains(&key) {
            return Ok(());
        }

        let source = Source::load(&canon)?;
        let matches = find_declarations(&source)?
            .into_iter()
            .filter(|decl| decl.name == request.name)
            .collect::<Vec<_>>();
        let decl = match matches.as_slice() {
            [] => bail!("{} not found in {}", request.name, canon.display()),
            [decl] => decl,
            _ => bail!(
                "{} is declared more than once in {}",
                request.name,
                canon.display()
            ),
        };

        if let Some(owner) = self.owners.get(&request.name)
            && owner != &canon
        {
            bail!(
                "{} conflicts with a declaration of the same name in {}",
                request.name,
                owner.display()
            );
        }
        self.owners.insert(request.name.clone(), canon.clone());
        self.emitted.insert(key);

        self.stack.push(canon.clone());
        let dir = canon.parent().unwrap_or_else(|| Path::new("."));
        for piece in split_imports(&source).with_context(|| format!("in {}", canon.display()))? {
            if let Piece::Import { request, .. } = piece {
                self.resolve(&request, dir, out)
                    .with_context(|| format!("in {}", canon.display()))?;
            }
        }
        self.stack.pop();

        out.push(source.single_line(decl.start, decl.end)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = format!(
                "marklogic-tui-imports-{}-{}-{}",
                name,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn expand(dir: &TempDir, query_rel: &str, query: &str) -> Result<String> {
        let path = dir.write(query_rel, query);
        expand_xquery_imports(query, &path)
    }

    #[test]
    fn query_without_directives_is_returned_unchanged() {
        let dir = TempDir::new("no-directives");
        let query = "xquery version \"1.0-ml\";\n(: import function a.xqy:f#1; :)\n'x'";
        assert_eq!(expand(&dir, "q.xqy", query).unwrap(), query);
    }

    #[test]
    fn import_function_inlines_declaration_with_original_prefix() {
        let dir = TempDir::new("function");
        dir.write(
            "path/to/clean-process.xqy",
            "xquery version \"1.0-ml\";\nmodule namespace local = \"urn:x\";\n\ndeclare variable $GLOB_VAR := 'g';\n\ndeclare function local:my-function($a, $b, $c, $d) {\n  $GLOB_VAR\n};\n",
        );
        let query = "import variable path/to/clean-process.xqy:GLOB_VAR;\nimport function path/to/clean-process.xqy:my-function#4;\nlocal:my-function(1, 2, 3, 4)";

        let expanded = expand(&dir, "q.xqy", query).unwrap();

        // Newlines inside the declaration collapse to spaces, so the indentation
        // inside the function body shows up as extra spaces.
        assert_eq!(
            expanded,
            "declare variable $GLOB_VAR := 'g';\ndeclare function local:my-function($a, $b, $c, $d) {   $GLOB_VAR };\nlocal:my-function(1, 2, 3, 4)"
        );
    }

    #[test]
    fn expansion_preserves_line_numbers() {
        let dir = TempDir::new("lines");
        dir.write("lib.xqy", "declare function local:f($a) {\n  $a + 1\n};\n");
        let query = "import function lib.xqy:f#1;\n\nlocal:f(1)";

        let expanded = expand(&dir, "q.xqy", query).unwrap();

        assert_eq!(expanded.lines().count(), query.lines().count());
        assert!(expanded.ends_with("\n\nlocal:f(1)"));
        assert!(expanded.contains("declare function local:f($a) {   $a + 1 };"));
    }

    #[test]
    fn multi_line_directive_keeps_line_count() {
        let dir = TempDir::new("multiline-directive");
        dir.write("lib.xqy", "declare function local:f($a) { $a };");
        let query = "import function lib.xqy:f\n#1;\nlocal:f(1)";

        let expanded = expand(&dir, "q.xqy", query).unwrap();

        assert_eq!(
            expanded,
            "declare function local:f($a) { $a };\n\nlocal:f(1)"
        );
    }

    #[test]
    fn arity_must_match_declaration() {
        let dir = TempDir::new("arity");
        dir.write("lib.xqy", "declare function local:f($a, $b) { $a };");
        let err = expand(&dir, "q.xqy", "import function lib.xqy:f#1;\n1").unwrap_err();

        let message = format!("{:#}", err);
        assert!(message.contains("not found"), "{message}");
        assert!(message.contains("function f#1"), "{message}");
    }

    #[test]
    fn missing_file_is_reported() {
        let dir = TempDir::new("missing-file");
        let err = expand(&dir, "q.xqy", "import function nope.xqy:f#1;\n1").unwrap_err();

        assert!(format!("{:#}", err).contains("not found"));
    }

    #[test]
    fn missing_declaration_is_reported() {
        let dir = TempDir::new("missing-decl");
        dir.write("lib.xqy", "declare function local:other($a) { $a };");
        let err = expand(&dir, "q.xqy", "import variable lib.xqy:GLOB;\n1").unwrap_err();

        assert!(format!("{:#}", err).contains("not found"));
    }

    #[test]
    fn semicolons_and_braces_inside_comments_and_strings_are_ignored() {
        let dir = TempDir::new("lexical");
        dir.write(
            "lib.xqy",
            "declare function local:f($a) {\n  (: a } ; comment :)\n  let $s := \"x; } ;\"\n  let $t := 'it''s ; }'\n  return $a\n};\ndeclare function local:after() { 1 };\n",
        );
        let expanded = expand(&dir, "q.xqy", "import function lib.xqy:f#1;\n1").unwrap();

        assert!(expanded.starts_with("declare function local:f($a) {"));
        assert!(expanded.contains("(: a } ; comment :)"));
        assert!(expanded.contains("\"x; } ;\""));
        assert!(expanded.contains("'it''s ; }'"));
        assert!(!expanded.contains("local:after"));
    }

    #[test]
    fn nested_imports_are_pulled_in_before_their_user() {
        let dir = TempDir::new("nested");
        dir.write("lib/base.xqy", "declare variable $GLOB_VAR := 'g';");
        dir.write(
            "lib/mid.xqy",
            "import variable base.xqy:GLOB_VAR;\ndeclare function local:f() { $GLOB_VAR };",
        );
        let query = "import function lib/mid.xqy:f#0;\nlocal:f()";

        let expanded = expand(&dir, "q.xqy", query).unwrap();

        assert_eq!(
            expanded,
            "declare variable $GLOB_VAR := 'g'; declare function local:f() { $GLOB_VAR };\nlocal:f()"
        );
    }

    #[test]
    fn shared_dependencies_are_emitted_once() {
        let dir = TempDir::new("dedupe");
        dir.write("base.xqy", "declare variable $V := 1;");
        dir.write(
            "a.xqy",
            "import variable base.xqy:V;\ndeclare function local:a() { $V };",
        );
        dir.write(
            "b.xqy",
            "import variable base.xqy:V;\ndeclare function local:b() { $V };",
        );
        let query = "import function a.xqy:a#0;\nimport function b.xqy:b#0;\nimport variable base.xqy:V;\n1";

        let expanded = expand(&dir, "q.xqy", query).unwrap();

        assert_eq!(expanded.matches("declare variable $V").count(), 1);
        assert_eq!(expanded.matches("declare function local:a").count(), 1);
        assert_eq!(expanded.matches("declare function local:b").count(), 1);
    }

    #[test]
    fn circular_imports_are_rejected() {
        let dir = TempDir::new("cycle");
        dir.write(
            "a.xqy",
            "import function b.xqy:g#0;\ndeclare function local:f() { 1 };",
        );
        dir.write(
            "b.xqy",
            "import function a.xqy:f#0;\ndeclare function local:g() { 2 };",
        );

        let err = expand(&dir, "q.xqy", "import function a.xqy:f#0;\n1").unwrap_err();

        assert!(
            format!("{:#}", err).contains("circular import"),
            "{:#}",
            err
        );
    }

    #[test]
    fn import_conflicting_with_query_declaration_is_rejected() {
        let dir = TempDir::new("conflict");
        dir.write("lib.xqy", "declare variable $X := 1;");
        let query = "import variable lib.xqy:X;\ndeclare variable $X := 2;\n$X";

        let err = expand(&dir, "q.xqy", query).unwrap_err();

        assert!(format!("{:#}", err).contains("conflicts"), "{:#}", err);
    }

    #[test]
    fn import_directive_in_comment_or_string_is_left_alone() {
        let dir = TempDir::new("literal");
        let query = "(: import function x.xqy:f#1; :)\n'import variable y.xqy:v;'\n1";
        assert_eq!(expand(&dir, "q.xqy", query).unwrap(), query);
    }

    #[test]
    fn module_and_other_imports_are_not_touched() {
        let dir = TempDir::new("other-imports");
        let query = "import module namespace m = \"urn:m\" at \"/m.xqy\";\nimport schema namespace s = \"urn:s\";\n1";
        assert_eq!(expand(&dir, "q.xqy", query).unwrap(), query);
    }

    #[test]
    fn parse_import_accepts_nested_path_and_variable_dollar() {
        let req = parse_import("function path/to/clean-process.xqy:my-function#4").unwrap();
        assert_eq!(req.path, "path/to/clean-process.xqy");
        assert_eq!(req.name.name, "my-function");
        assert_eq!(req.name.arity, Some(4));

        let req = parse_import("variable path/to/clean-process.xqy:$my-var").unwrap();
        assert_eq!(req.name.kind, DeclKind::Variable);
        assert_eq!(req.name.name, "my-var");
    }

    #[test]
    fn parse_import_rejects_malformed_directives() {
        assert!(parse_import("function a.xqy:f").is_err());
        assert!(parse_import("function a.xqy:f#x").is_err());
        assert!(parse_import("function :f#1").is_err());
        assert!(parse_import("variable f").is_err());
        assert!(parse_import("variable a.xqy:1bad").is_err());
    }

    #[test]
    fn missing_semicolon_is_reported() {
        let dir = TempDir::new("no-semi");
        let err = expand(&dir, "q.xqy", "import function a.xqy:f#1\n1").unwrap_err();

        assert!(format!("{:#}", err).contains("missing ';'"));
    }
}
