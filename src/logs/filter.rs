use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::{Regex, RegexBuilder};

pub(super) const PRESETS_FILE: &str = "filters.cfg";
const DELAY: Duration = Duration::from_millis(300);
const PREFIX_DELAY: Duration = Duration::from_millis(700);

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Group {
    include: String,
    exclude: String,
    // A search saved with a filter remains an additional condition when layered.
    search: String,
    pattern_search: bool,
}
fn yes() -> bool {
    true
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Preset {
    groups: Vec<Group>,
    search: String,
    search_mode: String,
    #[serde(default)]
    search_pattern: bool,
    #[serde(default = "yes")]
    search_filters: bool,
    context_before: usize,
    context_after: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum Field {
    Include,
    Exclude,
    Name,
}
pub(crate) enum Dialog {
    Edit {
        field: Field,
        input: String,
        cursor: usize,
        original: Vec<Group>,
        group: usize,
        changed: Option<Instant>,
    },
    Pick {
        names: Vec<String>,
        selected: usize,
        add: bool,
    },
}

pub(super) struct State {
    groups: Vec<Group>,
    pub(super) dialog: Option<Dialog>,
    pending: Option<Instant>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            groups: vec![Group::default()],
            dialog: None,
            pending: None,
        }
    }
}

struct CompiledGroup {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
    search: String,
    pattern: Option<Patterns>,
}
pub(super) struct Matcher(Vec<CompiledGroup>);
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
impl Matcher {
    pub(super) fn new(state: &State) -> Self {
        Self(
            state
                .groups
                .iter()
                .map(|g| CompiledGroup {
                    include: terms(&g.include),
                    exclude: terms(&g.exclude),
                    search: g.search.to_lowercase(),
                    pattern: g.pattern_search.then(|| Patterns::new(&g.search)),
                })
                .collect(),
        )
    }
    pub(super) fn matches(&self, fields: &[&str]) -> bool {
        self.0.iter().all(|g| {
            let has = |r: &Regex| fields.iter().any(|s| r.is_match(s));
            (g.include.is_empty() || g.include.iter().any(has))
                && !g.exclude.iter().any(has)
                && (g.search.is_empty()
                    || if let Some(p) = &g.pattern {
                        fields.iter().any(|s| p.matches(s))
                    } else {
                        fields.iter().any(|s| s.to_lowercase().contains(&g.search))
                    })
        })
    }
}
impl State {
    pub(super) fn active(&self) -> bool {
        self.groups
            .iter()
            .any(|g| !g.include.is_empty() || !g.exclude.is_empty() || !g.search.is_empty())
    }
    pub(super) fn description(&self) -> String {
        let g = &self.groups[0];
        format!(
            "in: {} · out: {}{}",
            if g.include.is_empty() {
                "—"
            } else {
                &g.include
            },
            if g.exclude.is_empty() {
                "—"
            } else {
                &g.exclude
            },
            if self.groups.len() > 1 {
                format!(" · {} added", self.groups.len() - 1)
            } else {
                String::new()
            }
        )
    }
}
impl LogViewer {
    pub(crate) fn content_filter_active(&self) -> bool {
        self.content_filters.active()
    }
    pub(crate) fn filter_description(&self) -> String {
        self.content_filters.description()
    }
    pub(crate) fn filter_dialog(&self) -> Option<&Dialog> {
        self.content_filters.dialog.as_ref()
    }
    fn apply_filter_input(&mut self) {
        let Some(Dialog::Edit {
            field,
            input,
            group,
            ..
        }) = self.content_filters.dialog.as_ref()
        else {
            return;
        };
        match field {
            Field::Include => self.content_filters.groups[*group].include = input.clone(),
            Field::Exclude => self.content_filters.groups[*group].exclude = input.clone(),
            Field::Name => return,
        }
        self.filter();
    }
    fn read_presets(&self) -> Result<BTreeMap<String, Preset>> {
        let path = self.cache_root.join(PRESETS_FILE);
        match fs::read(&path) {
            Ok(data) => {
                serde_json::from_slice(&data).with_context(|| format!("Reading {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e).with_context(|| format!("Reading {}", path.display())),
        }
    }
    fn save_filter(&mut self, name: &str) -> Result<()> {
        let mut presets = self.read_presets()?;
        presets.insert(
            name.to_owned(),
            Preset {
                groups: self.content_filters.groups.clone(),
                search: self.search.clone(),
                search_mode: self.options.search_mode.label().to_owned(),
                search_pattern: self.combined,
                search_filters: !self.combined || self.options.search_mode == SearchMode::Filter,
                context_before: self.context_before,
                context_after: self.context_after,
            },
        );
        fs::create_dir_all(&self.cache_root)?;
        atomic_write(
            &self.cache_root.join(PRESETS_FILE),
            &serde_json::to_vec_pretty(&presets)?,
        )
    }
    fn pick_filter(&mut self, add: bool) {
        match self.read_presets() {
            Ok(presets) if !presets.is_empty() => {
                self.content_filters.dialog = Some(Dialog::Pick {
                    names: presets.into_keys().collect(),
                    selected: 0,
                    add,
                })
            }
            Ok(_) => self.message = "No saved filters. Use fs to save one.".into(),
            Err(e) => self.message = format!("Could not read filters: {e:#}"),
        }
    }
    fn load_filter(&mut self, name: &str, add: bool) {
        let result = self
            .read_presets()
            .and_then(|mut presets| presets.remove(name).context("Filter no longer exists"));
        match result {
            Ok(mut p) => {
                if p.groups.is_empty() {
                    p.groups.push(Group::default());
                }
                if add {
                    if !p.search.is_empty() && p.search_filters {
                        p.groups.push(Group {
                            search: p.search,
                            pattern_search: p.search_pattern,
                            ..Group::default()
                        });
                    }
                    self.content_filters.groups.extend(p.groups);
                } else {
                    self.content_filters.groups = p.groups;
                    self.search = p.search;
                    self.options.search_mode = match p.search_mode.as_str() {
                        "off" => SearchMode::Off,
                        "highlight" => SearchMode::Highlight,
                        _ => SearchMode::Filter,
                    };
                    self.context_before = p.context_before;
                    self.context_after = p.context_after;
                }
                self.filter();
                self.message = format!("{} filter: {name}", if add { "Added" } else { "Recalled" });
            }
            Err(e) => self.message = format!("Could not load filter: {e:#}"),
        }
    }
    fn toggle_follow_after_prefix(&mut self) {
        self.following = !self.following;
        if self.following {
            self.scroll_to_bottom();
        }
        self.message = format!("Following {}.", if self.following { "on" } else { "off" });
    }
    pub(crate) fn filter_prefix_pending(&self) -> bool {
        self.content_filters.pending.is_some()
    }
    pub(crate) fn handle_filter_prefix(&mut self, key: KeyEvent) -> bool {
        self.expire_filter_prefix();
        if self.content_filters.pending.take().is_some() {
            if key.modifiers.is_empty() {
                match key.code {
                    KeyCode::Char('i' | 'o') => {
                        let field = if key.code == KeyCode::Char('i') {
                            Field::Include
                        } else {
                            Field::Exclude
                        };
                        let input = match field {
                            Field::Include => self.content_filters.groups[0].include.clone(),
                            _ => self.content_filters.groups[0].exclude.clone(),
                        };
                        self.content_filters.dialog = Some(Dialog::Edit {
                            field,
                            cursor: input.len(),
                            original: self.content_filters.groups.clone(),
                            group: 0,
                            input,
                            changed: None,
                        });
                        return true;
                    }
                    KeyCode::Char('c') => {
                        self.content_filters.groups = vec![Group::default()];
                        self.search.clear();
                        self.filter();
                        self.message = "Content filters cleared.".into();
                        return true;
                    }
                    KeyCode::Char('s') => {
                        self.content_filters.dialog = Some(Dialog::Edit {
                            field: Field::Name,
                            input: String::new(),
                            cursor: 0,
                            original: Vec::new(),
                            group: 0,
                            changed: None,
                        });
                        return true;
                    }
                    KeyCode::Char('r') => {
                        self.pick_filter(false);
                        return true;
                    }
                    KeyCode::Char('a') => {
                        self.pick_filter(true);
                        return true;
                    }
                    _ => {}
                }
            }
            self.toggle_follow_after_prefix();
        }
        if !self.files_focus && key.code == KeyCode::Char('f') && key.modifiers.is_empty() {
            self.content_filters.pending = Some(Instant::now());
            self.message =
                "f: follow · fi/fo: filter · fc: clear · fs: save · fr: recall · fa: add".into();
            return true;
        }
        false
    }
    fn expire_filter_prefix(&mut self) {
        if self
            .content_filters
            .pending
            .is_some_and(|t| t.elapsed() >= PREFIX_DELAY)
        {
            self.content_filters.pending = None;
            self.toggle_follow_after_prefix();
        }
    }
    pub(super) fn tick_filter(&mut self) {
        self.expire_filter_prefix();
        let due = matches!(self.content_filters.dialog.as_ref(), Some(Dialog::Edit { changed: Some(t), .. }) if t.elapsed() >= DELAY);
        if due {
            if let Some(Dialog::Edit { changed, .. }) = &mut self.content_filters.dialog {
                *changed = None;
            }
            self.apply_filter_input();
        }
    }
    pub(crate) fn handle_filter_dialog(&mut self, key: KeyEvent) {
        let Some(mut dialog) = self.content_filters.dialog.take() else {
            return;
        };
        match &mut dialog {
            Dialog::Pick {
                names,
                selected,
                add,
            } => match key.code {
                KeyCode::Esc => return,
                KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(names.len() - 1)
                }
                KeyCode::Enter => {
                    let name = names[*selected].clone();
                    let add = *add;
                    self.load_filter(&name, add);
                    return;
                }
                _ => {}
            },
            Dialog::Edit {
                field,
                input,
                cursor,
                original,
                group,
                changed,
            } => match key.code {
                KeyCode::Esc => {
                    if matches!(field, Field::Name) {
                        return;
                    }
                    self.content_filters.groups = original.clone();
                    self.filter();
                    return;
                }
                KeyCode::Enter => {
                    if matches!(field, Field::Name) {
                        let name = input.trim();
                        if name.is_empty() {
                            self.message = "Enter a name for the filter.".into();
                            self.content_filters.dialog = Some(dialog);
                            return;
                        }
                        self.message = match self.save_filter(name) {
                            Ok(()) => format!("Saved filter: {name}"),
                            Err(e) => format!("Could not save filter: {e:#}"),
                        };
                    } else {
                        self.content_filters.dialog = Some(dialog);
                        self.apply_filter_input();
                        self.content_filters.dialog = None;
                    }
                    return;
                }
                KeyCode::Tab if !matches!(field, Field::Name) => {
                    match field {
                        Field::Include => {
                            self.content_filters.groups[*group].include = input.clone()
                        }
                        Field::Exclude => {
                            self.content_filters.groups[*group].exclude = input.clone()
                        }
                        Field::Name => {}
                    }
                    *group = (*group + 1) % self.content_filters.groups.len();
                    *input = match field {
                        Field::Include => self.content_filters.groups[*group].include.clone(),
                        Field::Exclude => self.content_filters.groups[*group].exclude.clone(),
                        Field::Name => String::new(),
                    };
                    *cursor = input.len();
                    *changed = None;
                    self.filter();
                }
                KeyCode::Left => {
                    *cursor = input[..*cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(i, _)| i);
                }
                KeyCode::Right => {
                    if let Some(c) = input[*cursor..].chars().next() {
                        *cursor += c.len_utf8();
                    }
                }
                KeyCode::Home => *cursor = 0,
                KeyCode::End => *cursor = input.len(),
                KeyCode::Backspace if *cursor > 0 => {
                    let prev = input[..*cursor].char_indices().next_back().unwrap().0;
                    input.drain(prev..*cursor);
                    *cursor = prev;
                    *changed = Some(Instant::now());
                }
                KeyCode::Delete if *cursor < input.len() => {
                    let end = *cursor + input[*cursor..].chars().next().unwrap().len_utf8();
                    input.drain(*cursor..end);
                    *changed = Some(Instant::now());
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.clear();
                    *cursor = 0;
                    *changed = Some(Instant::now());
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    input.insert(*cursor, c);
                    *cursor += c.len_utf8();
                    *changed = Some(Instant::now());
                }
                _ => {}
            },
        }
        self.content_filters.dialog = Some(dialog);
    }
}
impl Dialog {
    pub(crate) fn edit_prompt(&self) -> Option<(String, usize)> {
        match self {
            Dialog::Edit {
                field,
                input,
                cursor,
                group,
                ..
            } => {
                let label = match field {
                    Field::Include => "Filter in: ",
                    Field::Exclude => "Filter out: ",
                    Field::Name => "Save filter as: ",
                };
                let label = if matches!(field, Field::Name) {
                    label.to_owned()
                } else {
                    format!("{label}[{}] ", group + 1)
                };
                Some((
                    format!("{label}{input}"),
                    label.chars().count() + input[..*cursor].chars().count(),
                ))
            }
            _ => None,
        }
    }
    pub(crate) fn choices(&self) -> Option<(&[String], usize, bool)> {
        match self {
            Dialog::Pick {
                names,
                selected,
                add,
            } => Some((names, *selected, *add)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn command(viewer: &mut LogViewer, c: char) {
        assert!(viewer.handle_filter_prefix(key('f')));
        assert!(viewer.handle_filter_prefix(key(c)));
    }
    fn type_text(viewer: &mut LogViewer, text: &str) {
        for c in text.chars() {
            viewer.handle_filter_dialog(key(c));
        }
    }
    fn enter(viewer: &mut LogViewer) {
        viewer.handle_filter_dialog(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }
    #[test]
    fn smart_case_delimiters_and_layers() {
        let state = State {
            groups: vec![
                Group {
                    include: "-F, error,Timeout".into(),
                    exclude: "-F, debug,NOISE".into(),
                    ..Group::default()
                },
                Group {
                    include: "server".into(),
                    ..Group::default()
                },
            ],
            ..State::default()
        };
        let m = Matcher::new(&state);
        assert!(m.matches(&["SERVER ERROR"]));
        assert!(m.matches(&["server Timeout"]));
        assert!(!m.matches(&["server timeout"]));
        assert!(!m.matches(&["SERVER ERROR Debug"]));
        assert!(m.matches(&["server error noise"]));
        assert!(!m.matches(&["error"]));
    }
    #[test]
    fn filter_debounces_and_escape_restores() {
        let mut v = LogViewer::default();
        v.files_focus = false;
        v.lines = vec!["alpha".into(), "beta".into()];
        v.filter();
        command(&mut v, 'i');
        type_text(&mut v, "alpha");
        assert_eq!(v.matches, vec![0, 1]);
        if let Some(Dialog::Edit { changed, .. }) = &mut v.content_filters.dialog {
            *changed = Some(Instant::now() - DELAY);
        }
        v.tick_filter();
        assert_eq!(v.matches, vec![0]);
        v.handle_filter_dialog(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(v.matches, vec![0, 1]);
    }
    #[test]
    fn presets_recall_and_add_are_persistent_and_conjunctive() {
        let path = std::env::temp_dir().join(format!(
            "log-filter-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut v = LogViewer {
            cache_root: path.clone(),
            files_focus: false,
            ..LogViewer::default()
        };
        v.lines = vec!["alpha beta".into(), "alpha".into(), "beta".into()];
        command(&mut v, 'i');
        type_text(&mut v, "beta");
        enter(&mut v);
        command(&mut v, 's');
        type_text(&mut v, "saved");
        enter(&mut v);
        command(&mut v, 'c');
        command(&mut v, 'i');
        type_text(&mut v, "alpha");
        enter(&mut v);
        command(&mut v, 'a');
        enter(&mut v);
        assert_eq!(v.matches, vec![0]);
        command(&mut v, 'i');
        v.handle_filter_dialog(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        v.handle_filter_dialog(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        enter(&mut v);
        assert_eq!(v.matches, vec![0, 1]);
        let mut restored = LogViewer {
            cache_root: path.clone(),
            files_focus: false,
            ..LogViewer::default()
        };
        restored.lines = v.lines.clone();
        command(&mut restored, 'r');
        enter(&mut restored);
        assert_eq!(restored.matches, vec![0, 2]);
        let _ = fs::remove_dir_all(path);
    }
}
