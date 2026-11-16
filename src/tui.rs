//! Pure terminal drawing for the persistent interactive UI.

use crate::UsageTotal;
use crate::agents::AgentInfo;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Page {
    Workspace,
    Setup,
    #[default]
    Chat,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Overlay {
    #[default]
    None,
    Agents,
    Status,
    Approval,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct View {
    pub(crate) page: Page,
    pub(crate) overlay: Overlay,
    pub(crate) cwd: String,
    pub(crate) model: String,
    pub(crate) reasoning: Option<String>,
    pub(crate) connection: String,
    pub(crate) transcript: Vec<String>,
    pub(crate) model_in_flight: bool,
    pub(crate) activity_tick: u64,
    pub(crate) turn_started: Option<Instant>,
    pub(crate) turn_usage_before: UsageTotal,
    pub(crate) last_turn_elapsed: Option<Duration>,
    pub(crate) last_turn_rate: Option<f64>,
    pub(crate) input: String,
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    pub(crate) status: String,
    pub(crate) workspace_selected: usize,
    pub(crate) setup_provider: usize,
    pub(crate) setup_provider_committed: Option<usize>,
    pub(crate) setup_tab: usize,
    pub(crate) setup_tab_focus: bool,
    pub(crate) setup_editing: bool,
    pub(crate) setup_tab_fields: [usize; 3],
    pub(crate) setup_field: usize,
    pub(crate) setup_url: String,
    pub(crate) setup_key: String,
    pub(crate) setup_clear_key: bool,
    pub(crate) setup_model: String,
    pub(crate) setup_models: Vec<String>,
    pub(crate) setup_model_selected: usize,
    pub(crate) setup_manual: bool,
    pub(crate) setup_reasoning: String,
    pub(crate) setup_reasoning_selected: usize,
    pub(crate) setup_reasoning_manual: bool,
    pub(crate) setup_decision_model_selected: usize,
    pub(crate) setup_response_language_selected: usize,
    pub(crate) setup_helpers: [bool; 3],
    pub(crate) setup_helper_selected: usize,
    pub(crate) setup_helper_available: [bool; 3],
    pub(crate) setup_status: String,
    pub(crate) approval_what: String,
    pub(crate) approval_preview: String,
    pub(crate) approval_scroll: usize,
    pub(crate) approval_allow: bool,
}

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const SECONDARY: Color = Color::Gray;
const USER_BG: Color = Color::Rgb(25, 62, 105);
const EDITOR_BG: Color = Color::Rgb(27, 45, 75);
const ACTIVE_BG: Color = Color::Rgb(22, 51, 63);
const CODE_BG: Color = Color::Rgb(39, 45, 52);
const MAX_TRANSCRIPT_LINE_CHARS: usize = 2_048;
const MAX_CODE_TOKENS_PER_ROW: usize = 256;
const ASCII_BORDER: border::Set = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

/// Remove terminal control sequences before text reaches the backend.
fn safe(_view: &View, value: &str) -> String {
    let mut chars = value.chars().peekable();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for part in chars.by_ref() {
                        if ('@'..='~').contains(&part) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '_' | '^') => {
                    while let Some(part) = chars.next() {
                        if part == '\u{7}' {
                            break;
                        }
                        if part == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c == '\n' || c == '\r' {
            out.push('\n');
        } else if c == '\t' {
            out.push_str("    ");
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

fn single(view: &View, value: &str) -> String {
    safe(view, value).replace('\n', " ")
}

fn panel<'a>(title: &'a str) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_set(ASCII_BORDER)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            title,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

fn rect(area: Rect, y: u16, height: u16) -> Rect {
    Rect::new(
        area.x,
        area.y.saturating_add(y),
        area.width,
        height.min(area.height.saturating_sub(y)),
    )
}

fn focused(label: String, active: bool) -> Line<'static> {
    if active {
        Line::from(Span::styled(
            format!("> {label}"),
            Style::default()
                .fg(Color::White)
                .bg(ACTIVE_BG)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::raw(format!("  {label}"))
    }
}

/// Clip a single-line value around its Unicode-scalar insertion point, reserving one cell for
/// the hardware cursor. The returned cursor offset is measured in terminal cells, not chars.
fn field_window(value: &str, cursor: usize, cells: usize) -> (String, usize) {
    if cells == 0 {
        return (String::new(), 0);
    }
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());
    let width =
        |start: usize, end: usize| Line::raw(chars[start..end].iter().collect::<String>()).width();
    let mut start = 0;
    while start < cursor && width(start, cursor) >= cells {
        start += 1;
    }
    let mut end = start;
    while end < chars.len() && width(start, end + 1) <= cells {
        end += 1;
    }
    (chars[start..end].iter().collect(), width(start, cursor))
}

fn draw_header(frame: &mut Frame, view: &View, area: Rect) {
    if area.height == 0 {
        return;
    }
    if area.height < 3 || area.width < 20 {
        frame.render_widget(
            Paragraph::new("Revex").style(Style::default().fg(ACCENT)),
            area,
        );
        return;
    }
    let model = if view.model.is_empty() {
        "No model"
    } else {
        &view.model
    };
    let connection = if view.connection.is_empty() {
        "Not connected"
    } else {
        &view.connection
    };
    let line = format!(
        "{}  |  {}  |  {}",
        single(view, model),
        single(view, connection),
        single(view, &view.cwd)
    );
    let title = format!(" Revex v{} ", env!("CARGO_PKG_VERSION"));
    frame.render_widget(Paragraph::new(line).block(panel(&title)), area);
}

fn draw_footer(frame: &mut Frame, view: &View, area: Rect) {
    if area.height == 0 {
        return;
    }
    let hint = match view.page {
        Page::Workspace => "Up/Down choose  |  Enter confirm",
        Page::Setup if view.setup_tab_focus => {
            "Tab/Shift+Tab or Left/Right screens  |  Enter fields  |  Ctrl+S save"
        }
        Page::Setup if view.setup_editing && view.setup_tab == 2 && area.width < 60 => {
            "Up/Down Space Enter Ctrl+S"
        }
        Page::Setup if view.setup_editing && view.setup_tab == 2 => {
            "Up/Down choice  |  Space toggle  |  Enter done  |  Ctrl+S save"
        }
        Page::Setup if view.setup_editing => {
            "Up/Down choice  |  Enter done  |  Esc fields  |  Ctrl+S save"
        }
        Page::Setup => "Up/Down items  |  Enter edit/select  |  Ctrl+S save  |  Esc tabs",
        Page::Chat => "Enter send  |  /agents  /status  /config  /exit",
    };
    let status = if view.status.is_empty() {
        "Ready"
    } else {
        &view.status
    };
    let lines = [
        Line::from(Span::styled(
            single(view, status),
            Style::default().fg(ACCENT),
        )),
        Line::from(Span::styled(hint, Style::default().fg(MUTED))),
    ];
    frame.render_widget(Paragraph::new(lines.to_vec()), area);
}

fn draw_workspace(frame: &mut Frame, view: &View, area: Rect) {
    if area.height == 0 {
        return;
    }
    let lines = vec![
        Line::raw("Use this workspace?"),
        Line::raw(single(view, &view.cwd)),
        Line::raw(""),
        focused("Yes / Continue".into(), view.workspace_selected == 0),
        focused("No / Exit".into(), view.workspace_selected != 0),
        Line::raw(""),
        Line::from(Span::styled(
            "To use another directory, cd there and restart Revex.",
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" Workspace "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_setup(frame: &mut Frame, view: &View, area: Rect) {
    if area.height == 0 {
        return;
    }
    let block = panel(" Setup ");
    let inner = block.inner(area);
    let mut tabs = vec![Span::raw(if view.setup_tab_focus { "> " } else { "  " })];
    for (index, name) in ["Server", "Model", "Helpers"].iter().enumerate() {
        if index > 0 {
            tabs.push(Span::raw("  "));
        }
        let style = if index == view.setup_tab {
            Style::default()
                .fg(Color::White)
                .bg(ACTIVE_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(SECONDARY)
        };
        tabs.push(Span::styled(*name, style));
    }
    let mut lines = vec![Line::from(tabs), Line::raw("")];
    let mut active_row = 0;
    let mut editable = None;
    match view.setup_tab {
        0 => {
            lines.push(Line::from(Span::styled(
                "MODEL SERVER  |  Connect an OpenAI-compatible model server",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            let providers = ["Ollama", "LM Studio", "oMLX", "Custom API"];
            let provider = providers
                .get(view.setup_provider)
                .copied()
                .unwrap_or("Custom API");
            let suffix = match view.setup_provider_committed {
                None => " (suggested)",
                Some(selected) if selected != view.setup_provider => " (press Enter to choose)",
                _ => "",
            };
            active_row = 4;
            lines.push(focused(
                format!("Provider: {provider}{suffix}"),
                !view.setup_tab_focus && view.setup_field == 0,
            ));
            let choices = providers
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    if view.setup_provider_committed == Some(index) {
                        format!("[{name}]")
                    } else {
                        name.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("  ");
            lines.push(Line::from(Span::styled(
                format!("    {choices}"),
                Style::default().fg(SECONDARY),
            )));
            let (url, url_cursor) = field_window(
                &single(view, &view.setup_url),
                if !view.setup_tab_focus && view.setup_field == 1 {
                    view.cursor
                } else {
                    0
                },
                inner.width.saturating_sub(7) as usize,
            );
            if view.setup_field == 1 {
                active_row = lines.len();
                editable = view.setup_editing.then_some((7, url_cursor));
            }
            lines.push(focused(
                format!("URL: {url}"),
                !view.setup_tab_focus && view.setup_field == 1,
            ));
            let key = if view.setup_clear_key {
                "(clear on Save)".to_owned()
            } else if view.setup_key.is_empty() {
                "(blank keeps saved key)".to_owned()
            } else {
                "*".repeat(view.setup_key.chars().count())
            };
            let (key, key_cursor) = field_window(
                &key,
                if !view.setup_tab_focus && view.setup_field == 2 {
                    view.cursor
                } else {
                    0
                },
                inner.width.saturating_sub(11) as usize,
            );
            if view.setup_field == 2 {
                active_row = lines.len();
                editable = view.setup_editing.then_some((11, key_cursor));
            }
            lines.push(focused(
                format!("API key: {key}"),
                !view.setup_tab_focus && view.setup_field == 2,
            ));
            lines.push(Line::from(Span::styled(
                "    Blank keeps saved key; Ctrl+K clears it",
                Style::default().fg(MUTED),
            )));
        }
        1 => {
            lines.push(Line::from(Span::styled(
                "MODEL  |  Choose an ID and reasoning effort",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            let model = if view.setup_manual {
                single(view, &view.setup_model)
            } else {
                view.setup_models
                    .get(view.setup_model_selected)
                    .map(|value| single(view, value))
                    .unwrap_or_else(|| single(view, &view.setup_model))
            };
            let model = if model.is_empty() {
                "(choose or enter a model)".into()
            } else {
                model
            };
            let (model, model_cursor) = field_window(
                &model,
                if view.setup_editing && view.setup_field == 3 && view.setup_manual {
                    view.cursor
                } else {
                    0
                },
                inner.width.saturating_sub(9) as usize,
            );
            active_row = lines.len();
            if view.setup_manual && view.setup_editing {
                editable = Some((9, model_cursor));
            }
            lines.push(focused(
                format!("Model: {model}"),
                !view.setup_tab_focus && view.setup_field == 3,
            ));
            if view.setup_manual {
                lines.push(Line::from(Span::styled(
                    "    Manual ID; Up/Down returns to server list",
                    Style::default().fg(MUTED),
                )));
            } else if view.setup_models.is_empty() {
                lines.push(Line::from(Span::styled(
                    "    Model list unavailable; type an ID manually",
                    Style::default().fg(MUTED),
                )));
            } else {
                let selected = view.setup_model_selected.min(view.setup_models.len() - 1);
                let start = selected.saturating_sub(1);
                for (index, name) in view.setup_models.iter().enumerate().skip(start).take(3) {
                    lines.push(focused(
                        format!("  {}", single(view, name)),
                        !view.setup_tab_focus && view.setup_field == 3 && index == selected,
                    ));
                }
                lines.push(Line::from(Span::styled(
                    "    Type to use a manual model ID",
                    Style::default().fg(MUTED),
                )));
            }
            let choices = ["Default", "low", "medium", "high", "xhigh", "max"];
            let reasoning = if view.setup_reasoning_manual {
                let value = single(view, &view.setup_reasoning);
                if value.is_empty() {
                    "(enter effort)".into()
                } else {
                    value
                }
            } else {
                choices
                    .get(view.setup_reasoning_selected)
                    .copied()
                    .unwrap_or("Default")
                    .to_owned()
            };
            let (reasoning, cursor) = field_window(
                &reasoning,
                if view.setup_editing && view.setup_field == 4 && view.setup_reasoning_manual {
                    view.cursor
                } else {
                    0
                },
                inner.width.saturating_sub(13) as usize,
            );
            if view.setup_field == 4 {
                active_row = lines.len();
                editable =
                    (view.setup_editing && view.setup_reasoning_manual).then_some((13, cursor));
            }
            lines.push(focused(
                format!("Reasoning: {reasoning}"),
                !view.setup_tab_focus && view.setup_field == 4,
            ));
            lines.push(Line::from(Span::styled(
                "    Up/Down choose; type for manual effort",
                Style::default().fg(MUTED),
            )));
            let decision_models = ["self", "clef", "clef-flash"];
            let selected = view
                .setup_decision_model_selected
                .min(decision_models.len() - 1);
            if view.setup_field == 7 {
                active_row = lines.len();
                editable = None;
            }
            lines.push(focused(
                format!("Decision model: {}", decision_models[selected]),
                !view.setup_tab_focus && view.setup_field == 7,
            ));
            lines.push(Line::from(Span::styled(
                "    self uses the current chat model; Up/Down choose",
                Style::default().fg(MUTED),
            )));
            let languages = ["English", "Japanese"];
            let selected = view
                .setup_response_language_selected
                .min(languages.len() - 1);
            if view.setup_field == 8 {
                active_row = lines.len();
                editable = None;
            }
            lines.push(focused(
                format!("Response language: {}", languages[selected]),
                !view.setup_tab_focus && view.setup_field == 8,
            ));
            lines.push(Line::from(Span::styled(
                "    Up/Down choose response language",
                Style::default().fg(MUTED),
            )));
        }
        _ => {
            lines.push(Line::from(Span::styled(
                "HELPER PROVIDERS  |  Select any",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            for (index, name) in ["Local", "Codex", "Claude"].iter().enumerate() {
                let mark = if view.setup_helpers[index] { 'x' } else { ' ' };
                let availability = if index == 0 {
                    "built in"
                } else if view.setup_helper_available[index] {
                    "CLI available"
                } else {
                    "CLI unavailable"
                };
                if view.setup_field == 5 && view.setup_helper_selected == index {
                    active_row = lines.len();
                }
                lines.push(focused(
                    format!("[{mark}] {name} ({availability})"),
                    !view.setup_tab_focus
                        && view.setup_field == 5
                        && view.setup_helper_selected == index,
                ));
            }
            if view.setup_field == 6 {
                active_row = lines.len();
            }
            lines.push(focused(
                "Confirm / Continue".into(),
                !view.setup_tab_focus && view.setup_field == 6,
            ));
        }
    }
    if !view.setup_status.is_empty() {
        lines.push(Line::from(Span::styled(
            single(view, &view.setup_status),
            Style::default().fg(ACCENT),
        )));
    }
    if view.setup_tab_focus {
        active_row = 0;
        editable = None;
    }
    let scroll = active_row.saturating_sub(inner.height.saturating_sub(1) as usize);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        area,
    );
    if view.overlay == Overlay::None
        && inner.width > 0
        && inner.height > 0
        && let Some((prefix, offset)) = editable
        && active_row >= scroll
        && active_row - scroll < inner.height as usize
    {
        let x = inner
            .x
            .saturating_add((prefix + offset).min(inner.width.saturating_sub(1) as usize) as u16);
        frame.set_cursor_position((x, inner.y + (active_row - scroll) as u16));
    }
}

fn wrap_visual(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for ch in text.chars() {
        let char_width = Line::raw(ch.to_string()).width();
        if !current.is_empty() && current_width + char_width > width {
            rows.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += char_width;
    }
    rows.push(current);
    rows
}

#[derive(Clone, Copy)]
enum CodeLanguage {
    Rust,
    Python,
    JavaScript,
    Other,
}

fn fence_language(content: &str) -> CodeLanguage {
    let info = content
        .strip_prefix("```")
        .or_else(|| content.strip_prefix("~~~"))
        .unwrap_or("");
    match info
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" | "rust" => CodeLanguage::Rust,
        "py" | "python" => CodeLanguage::Python,
        "js" | "javascript" | "jsx" | "ts" | "typescript" | "tsx" => CodeLanguage::JavaScript,
        _ => CodeLanguage::Other,
    }
}

fn code_keyword(word: &str, language: CodeLanguage) -> bool {
    match language {
        CodeLanguage::Rust => matches!(
            word,
            "fn" | "let"
                | "mut"
                | "pub"
                | "struct"
                | "impl"
                | "enum"
                | "trait"
                | "match"
                | "use"
                | "mod"
                | "const"
                | "static"
                | "if"
                | "else"
                | "for"
                | "while"
                | "return"
                | "async"
                | "await"
        ),
        CodeLanguage::Python => matches!(
            word,
            "def"
                | "class"
                | "import"
                | "from"
                | "as"
                | "if"
                | "elif"
                | "else"
                | "for"
                | "while"
                | "return"
                | "async"
                | "await"
                | "try"
                | "except"
                | "with"
                | "yield"
                | "lambda"
                | "pass"
                | "None"
                | "True"
                | "False"
        ),
        CodeLanguage::JavaScript => matches!(
            word,
            "const"
                | "let"
                | "var"
                | "function"
                | "class"
                | "import"
                | "export"
                | "from"
                | "if"
                | "else"
                | "for"
                | "while"
                | "return"
                | "async"
                | "await"
                | "new"
                | "this"
                | "try"
                | "catch"
        ),
        CodeLanguage::Other => false,
    }
}

fn code_row(text: &str, width: usize, fence: bool, language: CodeLanguage) -> Line<'static> {
    if fence {
        let padding = width.saturating_sub(Line::raw(text).width());
        return Line::styled(
            format!("{text}{}", " ".repeat(padding)),
            Style::default().fg(ACCENT).bg(CODE_BG),
        );
    }
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if spans.len() >= MAX_CODE_TOKENS_PER_ROW {
            spans.push(Span::styled(
                chars[i..].iter().collect::<String>(),
                Style::default().fg(Color::White).bg(CODE_BG),
            ));
            break;
        }
        let start = i;
        let comment = (chars[i] == '#'
            && matches!(language, CodeLanguage::Python | CodeLanguage::Other))
            || (chars[i] == '/'
                && chars.get(i + 1) == Some(&'/')
                && !matches!(language, CodeLanguage::Python));
        let color = if comment {
            i = chars.len();
            MUTED
        } else if chars[i] == '"' || chars[i] == '\'' {
            let quote = chars[i];
            i += 1;
            let mut escaped = false;
            while i < chars.len() {
                let ch = chars[i];
                i += 1;
                if ch == quote && !escaped {
                    break;
                }
                escaped = ch == '\\' && !escaped;
            }
            Color::Green
        } else if chars[i].is_ascii_alphanumeric() || chars[i] == '_' {
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if code_keyword(&word, language) {
                ACCENT
            } else if word.chars().all(|ch| ch.is_ascii_digit()) {
                Color::Yellow
            } else {
                Color::White
            }
        } else {
            i += 1;
            Color::White
        };
        spans.push(Span::styled(
            chars[start..i].iter().collect::<String>(),
            Style::default().fg(color).bg(CODE_BG),
        ));
    }
    let padding = width.saturating_sub(Line::raw(text).width());
    spans.push(Span::styled(
        " ".repeat(padding),
        Style::default().bg(CODE_BG),
    ));
    Line::from(spans)
}

fn user_padding(width: usize) -> Line<'static> {
    Line::styled(" ".repeat(width), Style::default().bg(USER_BG))
}

fn is_blank_row(line: &Line<'_>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

fn transcript_rows(view: &View, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for entry in &view.transcript {
        let entry = safe(view, entry);
        let user = entry.starts_with("You  ");
        let assistant = entry.starts_with("Revex  ");
        let error = !user && !assistant && entry.starts_with("error:");
        let tool = !user && !assistant && (entry.starts_with("tool ") || entry.contains("] tool "));
        let source: Vec<&str> = entry.lines().collect();
        let start = source
            .iter()
            .take_while(|line| line.trim().is_empty())
            .count();
        let end = source
            .iter()
            .rposition(|line| !line.trim().is_empty())
            .map_or(start, |index| index + 1);
        if user {
            let outer_gap = rows
                .last()
                .is_some_and(|line| is_blank_row(line) && line.style.bg != Some(USER_BG));
            if !outer_gap {
                rows.push(Line::raw(""));
            }
            rows.push(user_padding(width));
        } else if assistant && !rows.last().is_some_and(is_blank_row) {
            rows.push(Line::raw(""));
        }
        let mut in_code = None;
        for (line_index, &line) in source[start..end].iter().enumerate() {
            let content = line.strip_prefix("Revex  ").unwrap_or(line).trim_start();
            let fence = assistant && (content.starts_with("```") || content.starts_with("~~~"));
            let language = in_code.unwrap_or_else(|| fence_language(content));
            let display = if fence {
                String::new()
            } else if user && line_index == 0 {
                format!("> {}", line.strip_prefix("You  ").unwrap_or(line))
            } else if assistant && line_index == 0 {
                line.strip_prefix("Revex  ").unwrap_or(line).to_owned()
            } else if !user && !assistant && line_index == 0 {
                format!("{} {line}", if error { '!' } else { '*' })
            } else {
                line.to_owned()
            };
            let mut chars = display.chars();
            let bounded: String = chars.by_ref().take(MAX_TRANSCRIPT_LINE_CHARS).collect();
            let truncated = chars.next().is_some();
            for (visual_index, visual) in wrap_visual(&bounded, width).into_iter().enumerate() {
                if user {
                    let padding = width.saturating_sub(Line::raw(&visual).width());
                    let (label, rest) = if line_index == 0 && visual_index == 0 {
                        (
                            ">",
                            Some(visual.strip_prefix('>').unwrap_or(&visual).to_owned()),
                        )
                    } else {
                        ("", None)
                    };
                    rows.push(Line::from(vec![
                        Span::styled(
                            label,
                            Style::default()
                                .fg(ACCENT)
                                .bg(USER_BG)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{}{}", rest.unwrap_or(visual), " ".repeat(padding)),
                            Style::default().fg(Color::White).bg(USER_BG),
                        ),
                    ]));
                } else if assistant && (in_code.is_some() || fence) {
                    rows.push(code_row(&visual, width, fence, language));
                } else if assistant {
                    rows.push(Line::raw(visual));
                } else {
                    let color = if error {
                        Color::Red
                    } else if tool {
                        Color::Yellow
                    } else {
                        SECONDARY
                    };
                    rows.push(Line::styled(visual, Style::default().fg(color)));
                }
            }
            if truncated {
                let marker = "... (line truncated)";
                if user || (assistant && (in_code.is_some() || fence)) {
                    let background = if user { USER_BG } else { CODE_BG };
                    let padding = width.saturating_sub(Line::raw(marker).width());
                    rows.push(Line::styled(
                        format!("{marker}{}", " ".repeat(padding)),
                        Style::default().fg(MUTED).bg(background),
                    ));
                } else {
                    rows.push(Line::raw(marker));
                }
            }
            if fence {
                in_code = if in_code.is_some() {
                    None
                } else {
                    Some(language)
                };
            }
        }
        if user {
            rows.push(user_padding(width));
            rows.push(Line::raw(""));
        } else if assistant {
            rows.push(Line::raw(""));
        }
    }
    rows
}

fn editor_rows(input: &str, cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let chars: Vec<char> = input.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut rows = vec![String::new()];
    let mut row = 0;
    let mut column = 0;
    let mut insertion = (0, 0);
    for (index, ch) in chars.iter().copied().enumerate() {
        let char_width = if ch == '\n' {
            0
        } else {
            Line::raw(ch.to_string()).width()
        };
        if ch != '\n' && column + char_width > width && !rows[row].is_empty() {
            rows.push(String::new());
            row += 1;
            column = 0;
        }
        if index == cursor {
            if column >= width && width > 0 {
                rows.push(String::new());
                row += 1;
                column = 0;
            }
            insertion = (row, column);
        }
        if ch == '\n' {
            rows.push(String::new());
            row += 1;
            column = 0;
            continue;
        }
        rows[row].push(ch);
        column += char_width;
    }
    if cursor == chars.len() {
        if column >= width && width > 0 {
            rows.push(String::new());
            row += 1;
            column = 0;
        }
        insertion = (row, column);
    }
    (rows, insertion)
}

fn clip_cells(value: &str, cells: usize) -> String {
    if Line::raw(value).width() <= cells {
        return value.to_owned();
    }
    if cells <= 3 {
        return ".".repeat(cells);
    }
    let mut clipped = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let width = Line::raw(ch.to_string()).width();
        if used + width > cells - 3 {
            break;
        }
        clipped.push(ch);
        used += width;
    }
    format!("{clipped}...")
}

fn middle_clip_cells(value: &str, cells: usize) -> String {
    if Line::raw(value).width() <= cells {
        return value.to_owned();
    }
    if cells <= 3 {
        return ".".repeat(cells);
    }
    let prefix_limit = (cells - 3) / 3;
    let mut prefix = String::new();
    let mut prefix_width = 0;
    for ch in value.chars() {
        let width = Line::raw(ch.to_string()).width();
        if prefix_width + width > prefix_limit {
            break;
        }
        prefix.push(ch);
        prefix_width += width;
    }
    let suffix_limit = cells - 3 - prefix_width;
    let mut suffix = String::new();
    let mut suffix_width = 0;
    for ch in value.chars().rev() {
        let width = Line::raw(ch.to_string()).width();
        if suffix_width + width > suffix_limit {
            break;
        }
        suffix.insert(0, ch);
        suffix_width += width;
    }
    format!("{prefix}...{suffix}")
}

fn compact_workspace(view: &View) -> String {
    let cwd = single(view, &view.cwd);
    if cwd.is_empty() {
        return "workspace?".into();
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        if cwd == home {
            return "~".into();
        }
        if let Some(tail) = cwd.strip_prefix(&home)
            && tail.starts_with('/')
        {
            return format!("~{tail}");
        }
    }
    cwd
}

fn helper_cost(agents: &[AgentInfo]) -> String {
    let mut total = 0.0;
    let mut known = false;
    for agent in agents.iter().filter(|agent| agent.id != "main") {
        if agent.status != crate::agents::Status::Done {
            return "-".into();
        }
        let Some(cost) = agent.cost_usd else {
            return "-".into();
        };
        if !cost.is_finite() || cost < 0.0 {
            return "-".into();
        }
        known = true;
        total += cost;
    }
    if known {
        format!("~${total:.3}")
    } else {
        "-".into()
    }
}

fn chat_activity(view: &View) -> Option<String> {
    if !view.model_in_flight {
        return None;
    }
    let status = single(view, &view.status);
    let status = status.trim();
    let frames = ["|", "/", "-", "\\"];
    let mut active = format!(
        "{} Thinking",
        frames[view.activity_tick as usize % frames.len()]
    );
    let queued = status.rsplit('|').next().unwrap_or("").trim();
    if queued != "0 queued" && queued.ends_with(" queued") {
        active.push_str(&format!(" {queued}"));
    }
    Some(active)
}

fn chat_footer(view: &View, agents: &[AgentInfo], width: usize) -> String {
    let model = view.model.trim();
    let known_model = !model.is_empty() && model != "server default";
    let model = if known_model {
        single(view, model)
    } else if model == "server default" {
        "server model".into()
    } else {
        "no model".into()
    };
    let effort = known_model
        .then_some(view.reasoning.as_deref())
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .map(|s| single(view, s));
    let cwd = compact_workspace(view);
    if width < 60 {
        let effort = effort.filter(|value| width >= 35 && Line::raw(value).width() <= 8);
        let effort_width = effort
            .as_ref()
            .map_or(0, |value| Line::raw(value).width() + 3);
        let available = width.saturating_sub(3 + effort_width);
        let path_width = Line::raw(&cwd)
            .width()
            .min(available.saturating_sub(Line::raw(&model).width().min(16)));
        let model_width = available.saturating_sub(path_width);
        let parts = [
            clip_cells(&model, model_width),
            effort.unwrap_or_default(),
            middle_clip_cells(&cwd, path_width),
        ];
        return parts
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" . ");
    }
    let rate = view
        .last_turn_rate
        .map(|value| format!("{value:.1} tok/s"))
        .unwrap_or_else(|| "-".into());
    let elapsed = view
        .turn_started
        .map(|start| start.elapsed())
        .or(view.last_turn_elapsed)
        .map(|duration| format!("{:.1}s", duration.as_secs_f64()));
    let time = elapsed.unwrap_or_else(|| "-".into());
    let cost = helper_cost(agents);
    let effort_width = effort
        .as_ref()
        .map_or(0, |value| Line::raw(value).width() + 3);
    let fixed_width = effort_width
        + 4 * 3
        + Line::raw(&rate).width()
        + Line::raw(&time).width()
        + Line::raw(&cost).width();
    let available = width.saturating_sub(fixed_width);
    let model_reserve = Line::raw(&model).width().min(24);
    let path_width = Line::raw(&cwd)
        .width()
        .min(available.saturating_sub(model_reserve));
    let model_width = available.saturating_sub(path_width);
    let mut parts = vec![clip_cells(&model, model_width)];
    if let Some(effort) = effort {
        parts.push(effort);
    }
    parts.extend([middle_clip_cells(&cwd, path_width), rate, time, cost]);
    parts.join(" . ")
}

fn draw_chat(frame: &mut Frame, view: &View, agents: &[AgentInfo], area: Rect) {
    let input = safe(view, &view.input);
    let (editor_lines, insertion) =
        editor_rows(&input, view.cursor, area.width.saturating_sub(2) as usize);
    let padding = u16::from(area.height >= 9);
    let editor_cap = area.height.saturating_sub(6 + padding * 2).clamp(1, 6) as usize;
    let editor_height = editor_lines.len().min(editor_cap) as u16;
    let editor_y = area.height.saturating_sub(1 + padding + editor_height);
    let composer_y = editor_y.saturating_sub(padding);
    let divider_y = composer_y.saturating_sub(1);
    let transcript_area = rect(area, 0, divider_y);
    if transcript_area.height > 0 {
        let mut lines = transcript_rows(view, transcript_area.width as usize);
        if let Some(activity) = chat_activity(view) {
            lines.push(Line::styled(
                format!("* {activity}"),
                Style::default().fg(SECONDARY),
            ));
        }
        let top_padding = (transcript_area.height as usize)
            .saturating_sub(lines.len())
            .min(u16::MAX as usize) as u16;
        let top = lines
            .len()
            .saturating_sub(transcript_area.height as usize)
            .saturating_sub(view.scroll)
            .min(u16::MAX as usize) as u16;
        frame.render_widget(
            Paragraph::new(Text::from(lines)).scroll((top, 0)),
            rect(
                transcript_area,
                top_padding,
                transcript_area.height.saturating_sub(top_padding),
            ),
        );
    }

    frame.render_widget(
        Paragraph::new(format!("-- INPUT {}", "-".repeat(area.width as usize)))
            .style(Style::default().fg(ACCENT)),
        rect(area, divider_y, 1),
    );
    frame.render_widget(
        Block::default().style(Style::default().bg(EDITOR_BG)),
        rect(area, composer_y, editor_height + padding * 2),
    );

    let editor_scroll = insertion
        .0
        .saturating_sub(editor_height.saturating_sub(1) as usize);
    for (visible_row, line) in editor_lines
        .iter()
        .skip(editor_scroll)
        .take(editor_height as usize)
        .enumerate()
    {
        let original_row = editor_scroll + visible_row;
        let prefix = if original_row == 0 { "> " } else { "  " };
        let placeholder = input.is_empty() && original_row == 0;
        let editor = Line::from(vec![
            Span::styled(
                prefix,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if placeholder {
                    "Ask Revex to do anything".to_owned()
                } else {
                    line.clone()
                },
                Style::default().fg(if placeholder { MUTED } else { Color::White }),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(editor),
            rect(area, editor_y + visible_row as u16, 1),
        );
    }
    if view.overlay == Overlay::None && area.width > 2 {
        let x = area
            .x
            .saturating_add((insertion.1 + 2).min(area.width.saturating_sub(1) as usize) as u16);
        let y = area
            .y
            .saturating_add(editor_y + (insertion.0 - editor_scroll) as u16);
        frame.set_cursor_position((x, y));
    }

    let footer = rect(area, area.height.saturating_sub(1), 1);
    let footer_text = Rect::new(
        footer.x.saturating_add(2),
        footer.y,
        footer.width.saturating_sub(2),
        footer.height,
    );
    frame.render_widget(
        Paragraph::new(chat_footer(view, agents, footer_text.width as usize))
            .style(Style::default().fg(Color::LightBlue)),
        footer_text,
    );
}

fn modal_area(area: Rect, wanted_width: u16, wanted_height: u16) -> Rect {
    let width = wanted_width.min(area.width);
    let height = wanted_height.min(area.height);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

fn draw_approval(frame: &mut Frame, view: &View, area: Rect) {
    let modal = modal_area(area, 72, 17);
    if modal.width == 0 || modal.height == 0 {
        return;
    }
    let block = panel(" Approval ");
    let inner = block.inner(modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(block, modal);
    if inner.height == 0 {
        return;
    }
    let row = |y: u16| Rect::new(inner.x, inner.y.saturating_add(y), inner.width, 1);
    frame.render_widget(
        Paragraph::new("Approval required").style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        row(0),
    );
    let width = inner.width as usize;
    let wrap = |line: &str, color: Color| {
        wrap_visual(line, width)
            .into_iter()
            .map(|visual| (visual, color))
            .collect::<Vec<_>>()
    };
    let what = safe(view, &view.approval_what);
    let what_rows: Vec<_> = what
        .lines()
        .flat_map(|line| wrap(line, Color::White))
        .collect();
    let preview = safe(view, &view.approval_preview);
    let preview_shortened = preview.lines().any(|line| {
        ((line.starts_with("- ") || line.starts_with("+ ")) && line.ends_with("..."))
            || line.contains("(more lines omitted)")
            || line.contains("(showing first 4096 bytes)")
    });
    let mut all = Vec::new();
    let mut old = Vec::new();
    let mut new = Vec::new();
    let mut notes = Vec::new();
    for line in preview.lines() {
        let color = if line.starts_with("- ") {
            Color::Red
        } else if line.starts_with("+ ") {
            Color::Green
        } else {
            Color::White
        };
        let visual = wrap(line, color);
        all.extend(visual.iter().cloned());
        if color == Color::Red {
            old.extend(visual);
        } else if color == Color::Green {
            new.extend(visual);
        } else {
            notes.extend(visual);
        }
    }
    let long_request = what_rows.len() > 1;
    if !long_request && inner.height > 1 {
        let request = what_rows.first().map_or("", |line| line.0.as_str());
        frame.render_widget(Paragraph::new(request), row(1));
    }
    let body_start = if long_request { 1 } else { 3 };
    let body_rows = inner
        .height
        .saturating_sub(if long_request { 4 } else { 6 }) as usize;
    let split = !long_request
        && !old.is_empty()
        && !new.is_empty()
        && notes.len() <= 1
        && body_rows >= notes.len() + 2;
    let mut shown = Vec::new();
    let max_scroll;
    if split {
        shown.extend(notes.iter().cloned());
        let available = body_rows.saturating_sub(shown.len());
        let old_slots = available / 2;
        let new_slots = available.saturating_sub(old_slots);
        max_scroll = old
            .len()
            .saturating_sub(old_slots)
            .max(new.len().saturating_sub(new_slots));
        let offset = view.approval_scroll.min(max_scroll);
        let old_start = offset.min(old.len().saturating_sub(old_slots));
        let new_start = offset.min(new.len().saturating_sub(new_slots));
        shown.extend(old.iter().skip(old_start).take(old_slots).cloned());
        shown.extend(new.iter().skip(new_start).take(new_slots).cloned());
    } else {
        let content = if long_request {
            let mut content = what_rows;
            if !all.is_empty() {
                content.push((String::new(), Color::White));
            }
            content.extend(all);
            content
        } else {
            all
        };
        max_scroll = content.len().saturating_sub(body_rows);
        shown.extend(
            content
                .iter()
                .skip(view.approval_scroll.min(max_scroll))
                .take(body_rows)
                .cloned(),
        );
    }
    for (index, (line, color)) in shown.into_iter().enumerate() {
        frame.render_widget(
            Paragraph::new(line).style(Style::default().fg(color)),
            row(body_start + index as u16),
        );
    }
    if inner.height >= 3 {
        let scroll = view.approval_scroll.min(max_scroll);
        let message = if max_scroll == 0 {
            "Preview".to_owned()
        } else if scroll < max_scroll {
            format!(
                "Down more  |  Up/Down scroll {}/{}",
                scroll + 1,
                max_scroll + 1
            )
        } else {
            format!(
                "Up earlier  |  Up/Down scroll {}/{}",
                scroll + 1,
                max_scroll + 1
            )
        };
        let message = if preview_shortened {
            if max_scroll == 0 {
                "Preview shortened".to_owned()
            } else {
                format!("Shortened | {message}")
            }
        } else {
            message
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(MUTED)),
            row(inner.height - 3),
        );
    }
    if inner.height >= 2 {
        let choices = if view.approval_allow {
            "  Deny     > Approve <"
        } else {
            "> Deny <     Approve"
        };
        frame.render_widget(
            Paragraph::new(choices).style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            row(inner.height - 2),
        );
    }
    frame.render_widget(
        Paragraph::new("Enter confirm  |  Esc deny").style(Style::default().fg(MUTED)),
        row(inner.height - 1),
    );
}

fn draw_overlay(frame: &mut Frame, view: &View, agents: &[AgentInfo], area: Rect) {
    if view.overlay == Overlay::Approval {
        draw_approval(frame, view, area);
        return;
    }
    let (title, width, height, lines): (&str, u16, u16, Vec<Line>) = match view.overlay {
        Overlay::None => return,
        Overlay::Agents => {
            let mut lines = vec![Line::from(Span::styled(
                "ID   STATUS     PROVIDER MODEL        INPUT OUTPUT USAGE       TASK",
                Style::default().fg(ACCENT),
            ))];
            if agents.is_empty() {
                lines.push(Line::raw("No agents yet."));
            }
            for agent in agents {
                let input = if agent.usage.input_missing == 0 {
                    agent.usage.input.to_string()
                } else {
                    format!("{}+?", agent.usage.input)
                };
                let output = if agent.usage.output_missing == 0 {
                    agent.usage.output.to_string()
                } else {
                    format!("{}+?", agent.usage.output)
                };
                let completeness = if agent.usage.requests == 0
                    || (agent.usage.input_missing == agent.usage.requests
                        && agent.usage.output_missing == agent.usage.requests)
                {
                    "unavailable"
                } else if agent.usage.input_missing == 0 && agent.usage.output_missing == 0 {
                    "complete"
                } else {
                    "partial"
                };
                let model = agent.model.as_deref().unwrap_or("-");
                let model = single(view, model).chars().take(12).collect::<String>();
                lines.push(Line::raw(format!(
                    "{:<4} {:<10} {:<8} {:<12} {:>5} {:>5} {:<11} {}",
                    single(view, &agent.id),
                    agent.status.name(),
                    agent.provider.name(),
                    model,
                    input,
                    output,
                    completeness,
                    single(view, &agent.task)
                )));
            }
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Esc close",
                Style::default().fg(MUTED),
            )));
            (
                " Agents ",
                96,
                (agents.len() as u16).saturating_add(5).max(7),
                lines,
            )
        }
        Overlay::Status => {
            let lines = vec![
                Line::raw(format!("Workspace: {}", single(view, &view.cwd))),
                Line::raw(format!("Model: {}", single(view, &view.model))),
                Line::raw(format!("Connection: {}", single(view, &view.connection))),
                Line::raw(format!("Session: {}", single(view, &view.status))),
                Line::raw(""),
                Line::from(Span::styled("Esc close", Style::default().fg(MUTED))),
            ];
            (" Status ", 70, 9, lines)
        }
        Overlay::Approval => unreachable!(),
    };
    let modal = modal_area(area, width, height);
    if modal.width == 0 || modal.height == 0 {
        return;
    }
    frame.render_widget(Clear, modal);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(title))
            .wrap(Wrap { trim: false }),
        modal,
    );
}

pub(crate) fn render(frame: &mut Frame, view: &View, agents: &[AgentInfo]) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    if area.height < 7 || area.width < 16 {
        frame.render_widget(
            Paragraph::new("Revex").style(Style::default().fg(ACCENT)),
            area,
        );
        return;
    }
    if view.page == Page::Chat {
        draw_chat(frame, view, agents, area);
        draw_overlay(frame, view, agents, area);
        return;
    }
    let header = rect(area, 0, 3);
    let footer_height = 2;
    let footer_y = area.height.saturating_sub(footer_height);
    let footer = rect(area, footer_y, footer_height);
    draw_header(frame, view, header);
    draw_footer(frame, view, footer);
    let body_y = 3;
    let body_height = footer_y.saturating_sub(body_y);
    match view.page {
        Page::Workspace => draw_workspace(frame, view, rect(area, body_y, body_height)),
        Page::Setup => draw_setup(frame, view, rect(area, body_y, body_height)),
        Page::Chat => unreachable!(),
    }
    draw_overlay(frame, view, agents, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Status;
    use ratatui::{
        Terminal,
        backend::{Backend, TestBackend},
    };

    fn screen(view: &View, agents: &[AgentInfo], width: u16, height: u16) -> String {
        screen_with_cursor(view, agents, width, height).0
    }

    fn screen_with_cursor(
        view: &View,
        agents: &[AgentInfo],
        width: u16,
        height: u16,
    ) -> (String, (u16, u16)) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, view, agents)).unwrap();
        if width == 0 {
            return (String::new(), (0, 0));
        }
        let output = terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        let cursor = terminal.backend_mut().get_cursor_position().unwrap();
        (output, (cursor.x, cursor.y))
    }

    fn print_snapshot_if_requested(name: &str, output: &str) {
        if std::env::var("REVEX_TUI_SNAPSHOT").as_deref() == Ok("1") {
            println!("--- {name} ---\n{output}\n--- end {name} ---");
        }
    }

    #[test]
    fn first_run_setup_shows_provider_and_masks_key() {
        let view = View {
            page: Page::Setup,
            cwd: "/tmp/project".into(),
            setup_provider: 1,
            setup_field: 2,
            setup_url: "http://localhost:1234/v1".into(),
            setup_key: "secret-token".into(),
            setup_model: "local-model".into(),
            setup_status: "Connected".into(),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        print_snapshot_if_requested("first-run setup 80x24", &output);
        assert!(output.contains(concat!("Revex v", env!("CARGO_PKG_VERSION"))));
        assert!(output.contains("LM Studio"));
        assert!(output.contains("http://localhost:1234/v1"));
        assert!(output.contains("API key"));
        assert!(output.contains("Connected"));
        assert!(!output.contains("secret-token"));
        assert!(output.contains("****"));
    }

    #[test]
    fn setup_defaults_to_ollama_and_offers_four_providers() {
        let view = View {
            page: Page::Setup,
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("Server  Model  Helpers"));
        assert!(output.contains("Provider: Ollama (suggested)"));
        assert!(!output.contains("[Ollama]"));
    }

    #[test]
    fn setup_can_select_omlx() {
        let view = View {
            page: Page::Setup,
            setup_provider: 2,
            setup_provider_committed: Some(2),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("Provider: oMLX"));
        assert!(output.contains("Ollama  LM Studio  [oMLX]  Custom API"));
    }

    #[test]
    fn setup_reasoning_picker_has_default_and_known_efforts() {
        let view = View {
            page: Page::Setup,
            setup_tab: 1,
            setup_field: 4,
            setup_reasoning_selected: 3,
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("> Reasoning: high"));
        assert!(output.contains("Up/Down choose; type for manual effort"));
        assert!(!output.contains("Confirm / Continue"));
    }

    #[test]
    fn model_tab_shows_selected_decision_model() {
        let view = View {
            page: Page::Setup,
            setup_tab: 1,
            setup_field: 7,
            setup_decision_model_selected: 2,
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("> Decision model: clef-flash"));
        assert!(!output.contains("HELPER PROVIDERS"));
    }

    #[test]
    fn model_tab_shows_selected_response_language() {
        let view = View {
            page: Page::Setup,
            setup_tab: 1,
            setup_field: 8,
            setup_response_language_selected: 1,
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("> Response language: Japanese"));
        assert!(!output.contains("HELPER PROVIDERS"));
    }

    #[test]
    fn manual_reasoning_field_shows_insertion_cursor() {
        let view = View {
            page: Page::Setup,
            setup_editing: true,
            setup_tab: 1,
            setup_field: 4,
            setup_reasoning_manual: true,
            setup_reasoning: "ultra".into(),
            cursor: 2,
            ..View::default()
        };
        let (output, cursor) = screen_with_cursor(&view, &[], 80, 24);
        assert!(output.contains("Reasoning: ultra"));
        assert_eq!(cursor, (16, 10));
    }

    #[test]
    fn setup_confirm_is_field_six() {
        let view = View {
            page: Page::Setup,
            setup_tab: 2,
            setup_field: 6,
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("> Confirm / Continue"));
    }

    #[test]
    fn setup_helper_list_shows_selection_and_cli_availability() {
        let view = View {
            page: Page::Setup,
            setup_editing: true,
            setup_tab: 2,
            setup_field: 5,
            setup_helpers: [true, true, false],
            setup_helper_selected: 1,
            setup_helper_available: [true, false, true],
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("[x] Local"));
        assert!(output.contains("> [x] Codex (CLI unavailable)"));
        assert!(output.contains("[ ] Claude (CLI available)"));
        assert!(output.contains("Space toggle"));
    }

    #[test]
    fn helpers_tab_shows_saved_helpers_even_with_cached_models() {
        let view = View {
            page: Page::Setup,
            setup_tab: 2,
            setup_field: 0,
            setup_models: vec!["m1".into(), "m2".into(), "m3".into()],
            setup_helpers: [true, false, true],
            setup_helper_available: [true, true, false],
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("[x] Local"));
        assert!(output.contains("[ ] Codex (CLI available)"));
        assert!(output.contains("[x] Claude (CLI unavailable)"));
    }

    #[test]
    fn narrow_helper_list_keeps_selected_choice_and_hint_visible() {
        let view = View {
            page: Page::Setup,
            setup_editing: true,
            setup_tab: 2,
            setup_field: 5,
            setup_helper_selected: 2,
            setup_helpers: [false, false, true],
            setup_helper_available: [true, true, false],
            ..View::default()
        };
        let output = screen(&view, &[], 28, 12);
        assert!(output.contains("> [x] Claude"));
        assert!(output.contains("Space"));
    }

    #[test]
    fn configured_chat_keeps_transcript_input_and_footer_visible() {
        let view = View {
            page: Page::Chat,
            cwd: "/tmp/project".into(),
            model: "my-model".into(),
            connection: "connected".into(),
            transcript: vec!["hello".into(), "answer".into()],
            input: "next question".into(),
            cursor: 4,
            status: "Ready".into(),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 20);
        print_snapshot_if_requested("configured chat 80x20", &output);
        for expected in ["/tmp/project", "hello", "answer", "> next question"] {
            assert!(output.contains(expected), "missing {expected}:\n{output}");
        }
        let footer = output.lines().last().unwrap().trim_end();
        assert!(footer.starts_with("  my-model . /tmp/project"));
        assert!(footer.ends_with(". - . - . -"));
        assert!(!footer.contains("connected"));
        assert!(!footer.contains("Revex"));
        assert!(!output.contains("+---"));
        assert!(!output.contains(">_ Revex"));
    }

    #[test]
    fn empty_chat_shows_prominent_prompt_placeholder() {
        let view = View {
            cwd: "/tmp/project".into(),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 20);
        assert!(output.contains("> Ask Revex to do anything"));
        assert!(!output.contains("Start a conversation."));
    }

    #[test]
    fn chat_has_ascii_divider_and_padded_composer() {
        let view = View {
            input: "next task".into(),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 20);
        let rows: Vec<_> = output.lines().collect();
        assert!(rows[15].starts_with("-- INPUT"));
        assert!(rows[15].chars().skip(9).all(|ch| ch == '-'));
        assert!(rows[16].trim().is_empty());
        assert!(rows[17].starts_with("> next task"));
        assert!(rows[18].trim().is_empty());
        assert!(rows[19].contains("no model"));
    }

    #[test]
    fn composer_has_full_width_padding_above_and_below_text() {
        let view = View {
            input: "hello".into(),
            cursor: 5,
            ..View::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &view, &[])).unwrap();
        let buffer = terminal.backend().buffer();
        for y in [20, 22] {
            for x in 0..80 {
                assert_eq!(buffer[(x, y)].bg, EDITOR_BG, "padding at {x},{y}");
                assert_eq!(buffer[(x, y)].symbol(), " ", "padding text at {x},{y}");
            }
        }
        assert_eq!(buffer[(0, 21)].symbol(), ">");
        assert_eq!(buffer[(79, 21)].bg, EDITOR_BG);
        let cursor = terminal.backend_mut().get_cursor_position().unwrap();
        assert_eq!((cursor.x, cursor.y), (7, 21));

        let multiline = View {
            input: "first\nsecond".into(),
            cursor: 12,
            ..View::default()
        };
        terminal
            .draw(|frame| render(frame, &multiline, &[]))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in [19, 22] {
            for x in 0..80 {
                assert_eq!(buffer[(x, y)].bg, EDITOR_BG, "multiline padding at {x},{y}");
            }
        }
        assert_eq!(buffer[(0, 20)].symbol(), ">");
        assert_eq!(buffer[(2, 21)].symbol(), "s");
    }

    #[test]
    fn footer_uses_blue_text_on_default_background() {
        let view = View {
            model: "my-model".into(),
            cwd: "/tmp/project".into(),
            transcript: vec!["You  restored prompt".into()],
            input: "next task".into(),
            ..View::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &view, &[])).unwrap();
        let buffer = terminal.backend().buffer();
        for x in 0..80 {
            assert_eq!(buffer[(x, 23)].bg, Color::Reset, "footer background at {x}");
        }
        assert_eq!(buffer[(0, 23)].symbol(), " ");
        assert_eq!(buffer[(1, 23)].symbol(), " ");
        assert_eq!(buffer[(2, 23)].symbol(), "m");
        assert_eq!(buffer[(2, 23)].fg, Color::LightBlue);
        assert_eq!(buffer[(13, 23)].fg, Color::LightBlue);
        assert_eq!(buffer[(79, 16)].bg, USER_BG);
        assert_eq!(buffer[(79, 21)].bg, EDITOR_BG);
        assert_eq!(buffer[(79, 19)].bg, Color::Reset);
    }

    #[test]
    fn indented_footer_keeps_cost_visible_with_long_model_and_workspace() {
        let view = View {
            model: "deepseek-v4.1-flash-uncensored-extra-long".into(),
            reasoning: Some("high".into()),
            cwd: "/Volumes/Work/VeryLongDirectoryName/Clients/revex/target/release".into(),
            last_turn_rate: Some(12.7),
            last_turn_elapsed: Some(Duration::from_secs_f64(22.9)),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        let footer = output.lines().last().unwrap().trim_end();
        assert!(footer.starts_with("  deepseek"), "{footer}");
        assert!(footer.ends_with(" . 12.7 tok/s . 22.9s . -"), "{footer}");
        assert_eq!(Line::raw(footer).width(), 80);
    }

    #[test]
    fn restored_user_entry_displays_ascii_prompt_without_you_label() {
        let view = View {
            transcript: vec!["You  saved question".into(), "Revex  saved answer".into()],
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("> saved question"));
        assert!(!output.contains("You  saved question"));
        assert!(output.contains("saved answer"));
        assert!(!output.contains("Revex  saved answer"));
    }

    #[test]
    fn activity_rows_use_ascii_bullets_and_errors_keep_separate_marker() {
        let view = View {
            transcript: vec![
                "tool read_file".into(),
                "Task queued".into(),
                "error: failed".into(),
            ],
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("* tool read_file"));
        assert!(output.contains("* Task queued"));
        assert!(output.contains("! error: failed"));
    }

    #[test]
    fn fixed_ui_chrome_is_ascii_across_chat_setup_and_approval() {
        let chat = View {
            model: "local-model".into(),
            cwd: "/tmp/project".into(),
            status: "running  |  1 queued".into(),
            model_in_flight: true,
            ..View::default()
        };
        assert!(screen(&chat, &[], 80, 24).is_ascii());
        let setup = View {
            page: Page::Setup,
            setup_key: "secret".into(),
            ..View::default()
        };
        assert!(screen(&setup, &[], 80, 24).is_ascii());
        let approval = View {
            overlay: Overlay::Approval,
            approval_what: "write file".into(),
            approval_preview: "old\nnew".into(),
            ..View::default()
        };
        assert!(screen(&approval, &[], 80, 24).is_ascii());
    }

    #[test]
    fn narrow_chat_preserves_unicode_input_cursor_and_ascii_chrome() {
        let view = View {
            transcript: vec!["You  first".into()],
            input: "a界b".into(),
            cursor: 3,
            ..View::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(28, 12)).unwrap();
        terminal.draw(|frame| render(frame, &view, &[])).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 8)].bg, EDITOR_BG);
        assert_eq!(buffer[(27, 10)].bg, EDITOR_BG);
        let (output, cursor) = screen_with_cursor(&view, &[], 28, 12);
        assert!(output.contains("> first"));
        assert!(output.contains("> a界 b"), "{output}");
        assert!(output.lines().nth(7).unwrap().starts_with("-- INPUT"));
        assert!(output.lines().last().unwrap().starts_with("  no model"));
        assert_eq!(cursor, (6, 9));
    }

    #[test]
    fn short_transcript_sits_near_input_while_overflow_still_scrolls() {
        let mut view = View {
            transcript: vec!["recent result".into()],
            ..View::default()
        };
        let short = screen(&view, &[], 50, 20);
        let rows: Vec<_> = short.lines().map(str::trim_end).collect();
        assert_eq!(rows[14], "* recent result");
        assert!(rows[15].starts_with("-- INPUT"));

        view.transcript = (0..20).map(|i| format!("message {i:02}")).collect();
        let latest = screen(&view, &[], 50, 20);
        let rows: Vec<_> = latest.lines().map(str::trim_end).collect();
        assert_eq!(rows[14], "* message 19");
        assert_eq!(rows[0], "* message 05");
        view.scroll = 5;
        let earlier = screen(&view, &[], 50, 20);
        let rows: Vec<_> = earlier.lines().map(str::trim_end).collect();
        assert_eq!(rows[0], "* message 00");
        assert_eq!(rows[14], "* message 14");
    }

    #[test]
    fn transcript_roles_and_events_have_distinct_styles() {
        let view = View {
            transcript: vec![
                "You  question".into(),
                "Revex  answer".into(),
                "tool read_file".into(),
                "Task queued".into(),
                "error: unavailable".into(),
            ],
            ..View::default()
        };
        let rows = transcript_rows(&view, 40);
        assert_eq!(rows[2].spans[0].style.fg, Some(Color::Cyan));
        assert_eq!(rows[5].to_string(), "answer");
        assert_eq!(rows[5].spans[0].style.fg, None);
        assert_eq!(rows[7].style.fg, Some(Color::Yellow));
        assert_eq!(rows[8].style.fg, Some(Color::Gray));
        assert_eq!(rows[9].style.fg, Some(Color::Red));
    }

    #[test]
    fn setup_focused_choice_is_shaded_beneath_section_heading() {
        let view = View {
            page: Page::Setup,
            setup_field: 0,
            ..View::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &view, &[])).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(buffer[(3, 6)].modifier.contains(Modifier::BOLD));
        assert_ne!(buffer[(3, 8)].bg, Color::Reset);
    }

    #[test]
    fn idle_chat_footer_stays_sparse_with_long_cwd_and_model() {
        let view = View {
            cwd: "/Users/someone/work/revex/target/release".into(),
            model: "server default".into(),
            status: "idle  |  0 queued".into(),
            ..View::default()
        };
        for width in [24, 80] {
            let output = screen(&view, &[], width, 20);
            let footer = output.lines().last().unwrap().trim_end();
            assert!(footer.starts_with("  server model"));
            assert!(!footer.contains("Ready"));
            assert!(!footer.contains("0 agents"));
            assert!(!footer.contains("server default"));
        }
    }

    #[test]
    fn active_chat_shows_status_in_divider_without_crowding_footer() {
        let view = View {
            status: "running  |  2 queued".into(),
            model_in_flight: true,
            ..View::default()
        };
        let wide = screen(&view, &[], 80, 20);
        assert!(wide.contains("* | Thinking 2 queued"));
        assert!(
            wide.lines()
                .find(|line| line.starts_with("-- INPUT"))
                .unwrap()
                .chars()
                .skip(9)
                .all(|ch| ch == '-')
        );
        assert!(!wide.lines().last().unwrap().contains("running"));
        let narrow = screen(&view, &[], 24, 20);
        assert!(!narrow.lines().last().unwrap().contains("running"));
    }

    #[test]
    fn chat_footer_shows_model_reasoning_and_workspace() {
        let view = View {
            model: "local-model".into(),
            reasoning: Some("high".into()),
            cwd: "/tmp/project".into(),
            connection: "connected".into(),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 20);
        let lines: Vec<_> = output.lines().collect();
        assert!(lines[0].trim().is_empty());
        assert!(lines[19].starts_with("  local-model . high . /tmp/project"));
        assert!(!lines[19].contains("connected"));
    }

    #[test]
    fn chat_footer_never_fabricates_server_model_or_reasoning() {
        let view = View {
            model: "server default".into(),
            reasoning: Some("high".into()),
            cwd: "/tmp/project".into(),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 20);
        let lines: Vec<_> = output.lines().collect();
        assert!(lines[19].contains("server model"));
        assert!(!output.contains("server default"));
        assert!(!output.contains("(high)"));
    }

    #[test]
    fn chat_footer_abbreviates_home_and_preserves_workspace_tail() {
        let home = std::env::var("HOME").unwrap();
        let view = View {
            model: "deepseek-v4.1-flash-updated".into(),
            reasoning: Some("high".into()),
            cwd: format!("{home}/Developer/revex/target/release"),
            connection: "connected".into(),
            status: "idle  |  0 queued".into(),
            ..View::default()
        };
        let footer = chat_footer(&view, &[], 80);
        assert!(footer.starts_with("deepseek-v4.1"), "{footer}");
        assert!(
            footer.contains(" . high . ~/Developer/revex/target/release"),
            "{footer}"
        );
        assert!(footer.ends_with(" . - . - . -"), "{footer}");
        assert!(!footer.contains("Revex"));
        assert!(!footer.contains("connected"));
        assert!(!footer.contains("0 agents"));
        assert!(Line::raw(&footer).width() <= 80);
    }

    #[test]
    fn chat_footer_middle_clips_nonhome_path_without_changing_tail() {
        let view = View {
            model: "long-provider-model-name".into(),
            cwd: "/Volumes/Work/Clients/SomeVeryLongProjectName/revex/target/release".into(),
            ..View::default()
        };
        let footer = chat_footer(&view, &[], 80);
        assert!(footer.contains("..."), "{footer}");
        assert!(footer.contains("revex/target/release"), "{footer}");
        assert!(footer.contains("/Volumes"), "{footer}");
        assert!(Line::raw(&footer).width() <= 80);
    }

    #[test]
    fn footer_cost_requires_reports_from_every_helper() {
        let mut claude = AgentInfo {
            id: "a1".into(),
            provider: crate::agents::Provider::Claude,
            task: "inspect".into(),
            status: Status::Done,
            model: None,
            cwd: "/tmp/project".into(),
            usage: Default::default(),
            cost_usd: Some(0.125),
        };
        let view = View {
            model: "local-model".into(),
            cwd: "/tmp/project".into(),
            ..View::default()
        };
        let main = AgentInfo {
            id: "main".into(),
            provider: crate::agents::Provider::Local,
            cost_usd: Some(999.0),
            ..claude.clone()
        };
        let footer = chat_footer(&view, &[main.clone(), claude.clone()], 80);
        assert!(footer.ends_with(" . ~$0.125"), "{footer}");
        let known = claude.clone();
        claude.provider = crate::agents::Provider::Codex;
        claude.id = "a2".into();
        claude.cost_usd = None;
        let mixed = chat_footer(&view, &[main.clone(), claude, known.clone()], 80);
        assert!(mixed.ends_with(" . -"), "{mixed}");
        let local = AgentInfo {
            id: "a3".into(),
            provider: crate::agents::Provider::Local,
            cost_usd: None,
            ..known.clone()
        };
        let mixed_local = chat_footer(&view, &[main, known, local], 80);
        assert!(mixed_local.ends_with(" . -"), "{mixed_local}");
    }

    #[test]
    fn appending_transcript_with_zero_scroll_keeps_latest_visible() {
        let mut view = View {
            transcript: (0..30).map(|i| format!("message {i:02}")).collect(),
            scroll: 0,
            ..View::default()
        };
        view.transcript.push("latest appended answer".into());
        let output = screen(&view, &[], 50, 12);
        assert!(output.contains("latest appended answer"));
        assert!(!output.contains("message 00"));
    }

    #[test]
    fn user_prompt_row_has_subtle_background_across_terminal_width() {
        let view = View {
            transcript: vec!["You  restored prompt".into(), "progress event".into()],
            ..View::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        terminal.draw(|frame| render(frame, &view, &[])).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 6)].bg, USER_BG);
        assert_eq!(buffer[(0, 7)].bg, USER_BG);
        assert_eq!(buffer[(39, 7)].bg, USER_BG);
        assert_eq!(buffer[(0, 8)].bg, USER_BG);
        for x in 0..40 {
            assert_eq!(buffer[(x, 9)].bg, Color::Reset, "outer gap at {x},9");
        }
        assert_eq!(buffer[(0, 10)].bg, Color::Reset);
    }

    #[test]
    fn user_block_shades_full_width_padding_above_and_below() {
        let view = View {
            transcript: vec!["You  restored prompt".into()],
            ..View::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        terminal.draw(|frame| render(frame, &view, &[])).unwrap();
        let buffer = terminal.backend().buffer();
        for y in [7, 8, 9] {
            for x in 0..40 {
                assert_eq!(buffer[(x, y)].bg, USER_BG, "user block at {x},{y}");
            }
        }
        for y in [7, 9] {
            assert!((0..40).all(|x| buffer[(x, y)].symbol() == " "));
        }
        for y in [6, 10] {
            for x in 0..40 {
                assert_eq!(buffer[(x, y)].bg, Color::Reset, "outer gap at {x},{y}");
                assert_eq!(buffer[(x, y)].symbol(), " ");
            }
        }
    }

    #[test]
    fn user_block_has_unshaded_outer_gap_then_visible_blue_rows_at_eighty_columns() {
        let view = View {
            transcript: vec!["You  saved question".into()],
            ..View::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, &view, &[])).unwrap();
        let buffer = terminal.backend().buffer();
        for x in 0..80 {
            assert_eq!(buffer[(x, 14)].bg, Color::Reset, "top outer gap at {x}");
            for y in [15, 16, 17] {
                assert_eq!(
                    buffer[(x, y)].bg,
                    Color::Rgb(25, 62, 105),
                    "blue block at {x},{y}"
                );
            }
            assert_eq!(buffer[(x, 18)].bg, Color::Reset, "bottom outer gap at {x}");
        }
        assert_eq!(buffer[(0, 16)].symbol(), ">");
        assert_eq!(buffer[(2, 16)].fg, Color::White);
        assert_eq!(buffer[(0, 19)].bg, Color::Reset);
        assert_eq!(buffer[(0, 20)].bg, Color::Rgb(27, 45, 75));
    }

    #[test]
    fn adjacent_messages_reuse_assistant_outer_gap_and_separate_users() {
        let view = View {
            transcript: vec![
                "Revex  before".into(),
                "You  restored".into(),
                "You  next".into(),
                "Revex  after".into(),
            ],
            ..View::default()
        };
        let rows = transcript_rows(&view, 20);
        let content: Vec<_> = rows
            .iter()
            .map(|row| row.to_string().trim_end().to_owned())
            .collect();
        assert_eq!(
            content,
            [
                "",
                "before",
                "",
                "",
                "> restored",
                "",
                "",
                "",
                "> next",
                "",
                "",
                "after",
                ""
            ]
        );
        for index in [3, 5, 7, 9] {
            assert_eq!(rows[index].style.bg, Some(USER_BG));
            assert_eq!(rows[index].width(), 20);
        }
        for index in [0, 2, 6, 10, 12] {
            assert_eq!(rows[index].style.bg, None);
        }
    }

    #[test]
    fn multiline_wide_user_block_keeps_shading_and_unicode_text() {
        let view = View {
            transcript: vec!["You  界界界界界\nmore".into()],
            ..View::default()
        };
        let rows = transcript_rows(&view, 10);
        assert_eq!(rows.len(), 7);
        assert_eq!(rows[0].style.bg, None);
        assert_eq!(rows[1].style.bg, Some(USER_BG));
        assert_eq!(rows[5].style.bg, Some(USER_BG));
        assert_eq!(rows[6].style.bg, None);
        for row in &rows[2..5] {
            assert_eq!(row.width(), 10);
            assert!(row.spans.iter().all(|span| span.style.bg == Some(USER_BG)));
        }
        let displayed = rows.iter().map(ToString::to_string).collect::<String>();
        assert!(displayed.contains("> 界界界界"));
        assert!(displayed.contains("界"));
        assert!(displayed.contains("more"));
    }

    #[test]
    fn assistant_replies_hide_internal_label_in_plain_and_fenced_content() {
        let view = View {
            transcript: vec![
                "Revex  saved reply\nsecond line".into(),
                "Revex  ```rust\nlet value = 1;\n```".into(),
            ],
            ..View::default()
        };
        let output = screen(&view, &[], 60, 20);
        assert!(output.contains("saved reply"));
        assert!(output.contains("second line"));
        assert!(!output.contains("```rust"));
        assert!(output.contains("let value = 1;"));
        assert!(!output.contains("Revex  saved reply"));
        assert!(!output.contains("Revex  ```rust"));
        assert!(output.lines().last().unwrap().contains("no model"));
    }

    #[test]
    fn wrapped_user_prompt_keeps_background_on_each_visible_row() {
        let view = View {
            transcript: vec!["You  abcdefghijklmnopqrstuvwxyz".into()],
            ..View::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(20, 12)).unwrap();
        terminal.draw(|frame| render(frame, &view, &[])).unwrap();
        let buffer = terminal.backend().buffer();
        for y in [2, 3, 4, 5] {
            assert_eq!(buffer[(0, y)].bg, USER_BG);
            assert_eq!(buffer[(19, y)].bg, USER_BG);
        }
        for y in [1, 6] {
            for x in 0..20 {
                assert_eq!(buffer[(x, y)].bg, Color::Reset, "outer gap at {x},{y}");
            }
        }
    }

    #[test]
    fn assistant_multiline_reply_has_one_blank_row_on_each_side() {
        let view = View {
            transcript: vec![
                "You  question".into(),
                "Revex  first line\nsecond line".into(),
                "tool finished".into(),
                "Revex  next answer".into(),
            ],
            ..View::default()
        };
        let rows = transcript_rows(&view, 40);
        let rows: Vec<_> = rows
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect();
        assert_eq!(
            rows[..],
            [
                "",
                "",
                "> question",
                "",
                "",
                "first line",
                "second line",
                "",
                "* tool finished",
                "",
                "next answer",
                "",
            ]
        );
    }

    #[test]
    fn consecutive_user_and_assistant_blocks_share_one_separator() {
        let view = View {
            transcript: vec![
                "You  live question".into(),
                "You  restored question".into(),
                "Revex  answer".into(),
            ],
            ..View::default()
        };
        let rows = transcript_rows(&view, 40);
        let rows: Vec<_> = rows
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect();
        assert_eq!(
            rows[..],
            [
                "",
                "",
                "> live question",
                "",
                "",
                "",
                "> restored question",
                "",
                "",
                "answer",
                "",
            ]
        );
    }

    #[test]
    fn event_trailing_blank_lines_do_not_multiply_message_gap() {
        let view = View {
            transcript: vec!["tool finished\n\n\n".into(), "You  next task".into()],
            ..View::default()
        };
        let rows = transcript_rows(&view, 40);
        let rows: Vec<_> = rows
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect();
        assert_eq!(rows[..], ["* tool finished", "", "", "> next task", "", ""]);
    }

    #[test]
    fn consecutive_assistant_blocks_share_one_blank_row() {
        let view = View {
            transcript: vec!["Revex  answer one".into(), "Revex  answer two".into()],
            ..View::default()
        };
        let rows = transcript_rows(&view, 40);
        let rows: Vec<_> = rows
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect();
        assert_eq!(rows[..], ["", "answer one", "", "answer two", ""]);
    }

    #[test]
    fn fenced_code_reply_highlights_keyword_string_and_comment() {
        let view = View {
            transcript: vec![
                "Revex  Example:\n```rust\nlet name = \"hi\"; // note\n```\nDone".into(),
            ],
            ..View::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(frame, &view, &[])).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 11)].fg, Color::Cyan);
        assert_eq!(buffer[(11, 11)].fg, Color::Green);
        assert_eq!(buffer[(17, 11)].fg, Color::DarkGray);
        assert_eq!(buffer[(59, 11)].bg, CODE_BG);
    }

    #[test]
    fn fenced_code_shades_full_width_and_preserves_blank_lines_and_indentation() {
        let view = View {
            transcript: vec![
                "Revex  Before\n```rust\n  let x = 1;\n\n    println!(\"x\");\n```\nAfter".into(),
            ],
            ..View::default()
        };
        let rows = transcript_rows(&view, 20);
        assert_eq!(rows.len(), 9);
        assert_eq!(rows[1].to_string(), "Before");
        assert!(rows[3].to_string().starts_with("  let x = 1;"));
        assert!(rows[5].to_string().starts_with("    println!"));
        assert_eq!(rows[7].to_string(), "After");
        assert!(!rows.iter().any(|row| row.to_string().contains("```")));
        let mut terminal = Terminal::new(TestBackend::new(20, 16)).unwrap();
        terminal.draw(|frame| render(frame, &view, &[])).unwrap();
        let buffer = terminal.backend().buffer();
        for y in 4..=8 {
            for x in 0..20 {
                assert_eq!(buffer[(x, y)].bg, CODE_BG, "code block at {x},{y}");
            }
        }
        assert_eq!(buffer[(0, 3)].bg, Color::Reset);
        assert_eq!(buffer[(0, 9)].bg, Color::Reset);
        assert_ne!(CODE_BG, USER_BG);

        let tilde = View {
            transcript: vec!["Revex  ~~~python\n  print(1)\n~~~".into()],
            ..View::default()
        };
        let tilde_rows = transcript_rows(&tilde, 20);
        assert!(!tilde_rows.iter().any(|row| row.to_string().contains("~~~")));
        assert!(tilde_rows[2].to_string().starts_with("  print(1)"));
    }

    #[test]
    fn unfinished_fence_keeps_wrapped_narrow_lines_shaded() {
        let view = View {
            transcript: vec![
                "Revex  ```python\n    def really_long_function_name(x, y):\n      return 1".into(),
            ],
            ..View::default()
        };
        let rows = transcript_rows(&view, 16);
        assert!(!rows.iter().any(|row| row.to_string().contains("```")));
        assert!(rows[2].to_string().starts_with("    def"));
        assert!(rows.len() > 5, "the long line should wrap");
        let mut terminal = Terminal::new(TestBackend::new(16, 14)).unwrap();
        terminal.draw(|frame| render(frame, &view, &[])).unwrap();
        let buffer = terminal.backend().buffer();
        let code_rows = rows.len() - 2; // Exclude the assistant's two outer gaps.
        let top = 9usize.saturating_sub(rows.len());
        for row in 1..=code_rows {
            for x in 0..16 {
                assert_eq!(buffer[(x, (top + row) as u16)].bg, CODE_BG);
            }
        }
    }

    #[test]
    fn fenced_code_keywords_follow_declared_language() {
        for (language, word, expected) in [
            ("rust", "fn", Color::Cyan),
            ("python", "fn", Color::White),
            ("python", "def", Color::Cyan),
            ("js", "function", Color::Cyan),
            ("rust", "function", Color::White),
        ] {
            let view = View {
                transcript: vec![format!("Revex  ```{language}\n{word} work() {{}}\n```")],
                ..View::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
            terminal.draw(|frame| render(frame, &view, &[])).unwrap();
            assert_eq!(
                terminal.backend().buffer()[(0, 8)].fg,
                expected,
                "{language} {word}"
            );
        }
    }

    #[test]
    fn very_long_fenced_line_is_bounded_and_marked_truncated() {
        let view = View {
            transcript: vec![format!("Revex  ```rust\n{}\n```", "x".repeat(10_000))],
            ..View::default()
        };
        assert!(transcript_rows(&view, 80).len() < 40);
        let output = screen(&view, &[], 80, 20);
        assert!(output.contains("line truncated"));
    }

    #[test]
    fn pathological_code_row_falls_back_to_plain_style_after_token_limit() {
        let view = View {
            transcript: vec![format!("Revex  ```js\n{}\n```", "!".repeat(1_000))],
            ..View::default()
        };
        let rows = transcript_rows(&view, 1_000);
        assert!(rows[2].spans.len() < 300);
        let mut terminal = Terminal::new(TestBackend::new(1_000, 12)).unwrap();
        terminal.draw(|frame| render(frame, &view, &[])).unwrap();
        assert_eq!(terminal.backend().buffer()[(900, 4)].fg, Color::White);
        assert_eq!(terminal.backend().buffer()[(900, 4)].bg, CODE_BG);
    }

    #[test]
    fn model_in_flight_indicator_animates_and_disappears() {
        let mut view = View {
            model_in_flight: true,
            activity_tick: 0,
            status: "running  |  1 queued".into(),
            ..View::default()
        };
        let first = screen(&view, &[], 80, 20);
        assert!(first.contains("| Thinking 1 queued"));
        view.activity_tick = 1;
        let second = screen(&view, &[], 80, 20);
        assert!(second.contains("/ Thinking"));
        view.model_in_flight = false;
        let done = screen(&view, &[], 80, 20);
        assert!(!done.contains("Thinking"));
    }

    #[test]
    fn approval_modal_shows_preview_and_selected_action() {
        let view = View {
            overlay: Overlay::Approval,
            approval_what: "Run command".into(),
            approval_preview: "rm sample.tmp".into(),
            approval_allow: true,
            ..View::default()
        };
        let output = screen(&view, &[], 70, 18);
        assert!(output.contains("Approval required"));
        assert!(output.contains("Run command"));
        assert!(output.contains("rm sample.tmp"));
        assert!(output.contains("Approve"));
        assert!(output.contains("Deny"));
    }

    #[test]
    fn long_approval_preview_shows_old_and_new_then_scrolls_both() {
        let preview = (1..=8)
            .map(|n| format!("- old line {n}"))
            .chain((1..=8).map(|n| format!("+ new line {n}")))
            .collect::<Vec<_>>()
            .join("\n");
        let mut view = View {
            overlay: Overlay::Approval,
            approval_what: "replace src/main.rs".into(),
            approval_preview: preview,
            ..View::default()
        };
        let first = screen(&view, &[], 80, 20);
        assert!(first.contains("- old line 1"));
        assert!(first.contains("+ new line 1"));
        assert!(first.contains("Deny"));
        assert!(first.contains("Approve"));
        view.approval_scroll = 6;
        let later = screen(&view, &[], 80, 20);
        assert!(later.contains("- old line 7"));
        assert!(later.contains("+ new line 7"));
        assert!(later.contains("Deny"));
        assert!(later.contains("Approve"));
    }

    #[test]
    fn command_approval_preview_can_scroll_without_losing_actions() {
        let mut view = View {
            overlay: Overlay::Approval,
            approval_what: "Run command".into(),
            approval_preview: (1..=12)
                .map(|n| format!("command line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            ..View::default()
        };
        view.approval_scroll = 9;
        let output = screen(&view, &[], 70, 18);
        assert!(output.contains("command line 10"));
        assert!(output.contains("Deny"));
        assert!(output.contains("Approve"));
    }

    #[test]
    fn approval_wraps_command_suffix_and_scrolls_later_preview() {
        let mut view = View {
            overlay: Overlay::Approval,
            approval_what: format!("sh {}COMMAND_END", "x".repeat(90)),
            approval_preview: (1..=12)
                .map(|n| format!("preview line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            ..View::default()
        };
        let initial = screen(&view, &[], 72, 20);
        assert!(initial.contains("COMMAND_END"));
        assert!(initial.contains("more"));
        view.approval_scroll = usize::MAX;
        let later = screen(&view, &[], 72, 20);
        assert!(later.contains("preview line 12"));
        assert!(later.contains("Deny"));
        assert!(later.contains("Approve"));
    }

    #[test]
    fn long_shell_command_suffix_requires_scroll_and_remains_inspectable() {
        let mut view = View {
            overlay: Overlay::Approval,
            approval_what: format!("sh {}COMMAND_END", "x".repeat(1000)),
            ..View::default()
        };
        let initial = screen(&view, &[], 72, 20);
        assert!(!initial.contains("COMMAND_END"));
        assert!(initial.contains("more"));
        view.approval_scroll = usize::MAX;
        let later = screen(&view, &[], 72, 20);
        assert!(later.contains("COMMAND_END"));
        assert!(later.contains("Deny"));
        assert!(later.contains("Approve"));
    }

    #[test]
    fn bounded_edit_preview_advertises_shortening() {
        let view = View {
            overlay: Overlay::Approval,
            approval_what: "replace file".into(),
            approval_preview: format!("+ {}...", "x".repeat(100)),
            ..View::default()
        };
        let output = screen(&view, &[], 72, 20);
        assert!(output.contains("Preview shortened"));
        assert!(output.contains("Approve"));
    }

    #[test]
    fn approval_wraps_old_and_new_edit_suffixes() {
        let view = View {
            overlay: Overlay::Approval,
            approval_what: "replace file".into(),
            approval_preview: format!("- {}OLD_END\n+ {}NEW_END", "o".repeat(90), "n".repeat(90)),
            ..View::default()
        };
        let output = screen(&view, &[], 72, 20);
        assert!(output.contains("OLD_END"));
        assert!(output.contains("NEW_END"));
        assert!(output.contains("Deny"));
        assert!(output.contains("Approve"));
    }

    #[test]
    fn agent_overlay_shows_model_and_usage_completeness() {
        let view = View {
            overlay: Overlay::Agents,
            cwd: "/tmp/launch".into(),
            ..View::default()
        };
        let agent = AgentInfo {
            id: "a1".into(),
            task: "inspect binary".into(),
            status: Status::Running,
            provider: crate::agents::Provider::Local,
            model: Some("tiny-model".into()),
            cwd: "/tmp/revex".into(),
            usage: crate::UsageTotal {
                input: 12,
                output: 9,
                requests: 2,
                input_missing: 1,
                output_missing: 0,
            },
            cost_usd: None,
        };
        let mut complete = agent.clone();
        complete.id = "a2".into();
        complete.provider = crate::agents::Provider::Codex;
        complete.usage.input_missing = 0;
        let mut unavailable = agent.clone();
        unavailable.id = "a3".into();
        unavailable.provider = crate::agents::Provider::Claude;
        unavailable.usage = Default::default();
        let output = screen(&view, &[agent, complete, unavailable], 80, 24);
        assert!(output.contains("Agents"));
        assert!(output.contains("a1"));
        assert!(output.contains("running"));
        assert!(output.contains("local"));
        assert!(output.contains("codex"));
        assert!(output.contains("claude"));
        assert!(output.contains("tiny-model"));
        assert!(output.contains("partial"));
        assert!(output.contains("complete"));
        assert!(output.contains("unavailable"));
        assert!(output.contains("12+?"));
        assert!(output.contains("inspect binary"));
    }

    #[test]
    fn workspace_confirmation_uses_launch_cwd_and_defaults_to_continue() {
        let view = View {
            page: Page::Workspace,
            cwd: "/tmp/launch".into(),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        print_snapshot_if_requested("workspace confirmation 80x24", &output);
        assert!(output.contains(concat!("Revex v", env!("CARGO_PKG_VERSION"))));
        assert!(output.contains("Use this workspace?"));
        assert!(output.contains("/tmp/launch"));
        assert!(output.contains("> Yes / Continue"));
        assert!(output.contains("No / Exit"));
        assert!(output.contains("cd there and restart Revex"));
        assert!(!output.contains("Repository root"));
        assert!(!output.contains("Custom path"));
    }

    #[test]
    fn workspace_confirmation_can_select_exit() {
        let view = View {
            page: Page::Workspace,
            cwd: "/tmp/launch".into(),
            workspace_selected: 1,
            ..View::default()
        };
        let output = screen(&view, &[], 80, 18);
        assert!(output.contains("> No / Exit"));
        assert!(!output.contains("> Yes / Continue"));
    }

    #[test]
    fn transcript_scroll_moves_away_from_latest_message() {
        let mut view = View {
            transcript: (0..20).map(|i| format!("message {i:02}")).collect(),
            ..View::default()
        };
        let latest = screen(&view, &[], 50, 12);
        assert!(latest.contains("message 19"));
        assert!(!latest.contains("message 00"));
        view.scroll = 18;
        let earlier = screen(&view, &[], 50, 12);
        assert!(earlier.contains("message 00"));
        assert!(!earlier.contains("message 19"));
    }

    #[test]
    fn one_character_setup_key_does_not_change_other_text() {
        let view = View {
            setup_key: "a".into(),
            transcript: vec!["\u{1b}[31ma valid answer\u{1b}[0m".into()],
            ..View::default()
        };
        let output = screen(&view, &[], 60, 12);
        assert!(output.contains("a valid answer"));
        assert!(!output.contains("[31m"));
    }

    #[test]
    fn one_character_key_is_masked_only_in_setup_field() {
        let view = View {
            page: Page::Setup,
            setup_key: "a".into(),
            setup_status: "a valid API response".into(),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("API key: *"));
        assert!(output.contains("a valid API response"));
        assert!(!output.contains("API key: a"));
    }

    #[test]
    fn setup_key_field_explains_keep_and_explicit_clear() {
        let view = View {
            page: Page::Setup,
            setup_field: 2,
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("Blank keeps saved key; Ctrl+K clears it"));
        let view = View {
            setup_clear_key: true,
            ..view
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("API key: (clear on Save)"));
    }

    #[test]
    fn setup_url_cursor_tracks_unicode_cell_width() {
        let view = View {
            page: Page::Setup,
            setup_editing: true,
            setup_field: 1,
            setup_url: "ab界d".into(),
            cursor: 3,
            ..View::default()
        };
        let (output, cursor) = screen_with_cursor(&view, &[], 80, 24);
        assert!(output.contains("URL: ab界"));
        assert_eq!(cursor, (12, 10));
    }

    #[test]
    fn setup_key_cursor_tracks_masked_characters_without_revealing_them() {
        let view = View {
            page: Page::Setup,
            setup_editing: true,
            setup_field: 2,
            setup_key: "a界b".into(),
            cursor: 2,
            ..View::default()
        };
        let (output, cursor) = screen_with_cursor(&view, &[], 80, 24);
        assert!(output.contains("API key: ***"));
        assert!(!output.contains("a界b"));
        assert_eq!(cursor, (14, 11));
    }

    #[test]
    fn manual_model_cursor_tracks_unicode_cell_width() {
        let view = View {
            page: Page::Setup,
            setup_editing: true,
            setup_tab: 1,
            setup_field: 3,
            setup_manual: true,
            setup_model: "m界x".into(),
            cursor: 2,
            ..View::default()
        };
        let (output, cursor) = screen_with_cursor(&view, &[], 80, 24);
        assert!(output.contains("Model: m界"));
        assert_eq!(cursor, (13, 8));
    }

    #[test]
    fn narrow_setup_field_scrolls_horizontally_and_keeps_cursor_visible() {
        let view = View {
            page: Page::Setup,
            setup_editing: true,
            setup_field: 1,
            setup_url: "https://example.test/v1".into(),
            cursor: 23,
            ..View::default()
        };
        let (output, (x, y)) = screen_with_cursor(&view, &[], 25, 12);
        assert!(output.contains("/v1"));
        assert!(x < 24, "cursor must stay inside panel: {x}");
        assert!(y < 10, "cursor must stay above footer: {y}");
    }

    #[test]
    fn long_chat_input_keeps_cursor_at_visible_insertion_point() {
        let view = View {
            input: "abcdefghijklmnopqrstuvwxyz".into(),
            cursor: 26,
            ..View::default()
        };
        let (output, cursor) = screen_with_cursor(&view, &[], 20, 10);
        assert!(output.contains("uvwxyz"));
        assert_eq!(cursor.1, 7);
        assert!(cursor.0 < 20);
    }

    #[test]
    fn multiline_chat_editor_shows_lines_and_insertion_cursor() {
        let view = View {
            input: "first line\nsecond line".into(),
            cursor: "first line\nsecond".chars().count(),
            ..View::default()
        };
        let (output, cursor) = screen_with_cursor(&view, &[], 80, 20);
        let rows: Vec<_> = output.lines().collect();
        assert!(rows[16].starts_with("> first line"));
        assert!(rows[17].starts_with("  second line"));
        assert_eq!(cursor, (8, 17));
    }

    #[test]
    fn editor_cursor_tracks_wrapped_wide_character() {
        let (rows, insertion) = editor_rows("abc界", 3, 4);
        assert_eq!(rows, ["abc", "界"]);
        assert_eq!(insertion, (1, 0));
    }

    #[test]
    fn trailing_newline_shows_empty_editor_row() {
        let view = View {
            input: "one\n".into(),
            cursor: 4,
            ..View::default()
        };
        let (output, cursor) = screen_with_cursor(&view, &[], 80, 20);
        assert!(output.lines().nth(16).unwrap().starts_with("> one"));
        assert_eq!(cursor, (2, 17));
    }

    #[test]
    fn multiline_editor_keeps_cursor_visible_in_small_terminal() {
        let value = "a\nb\nc\nd\ne\nf\ng";
        let view = View {
            input: value.into(),
            cursor: value.chars().count(),
            ..View::default()
        };
        let (output, cursor) = screen_with_cursor(&view, &[], 20, 12);
        assert!(output.contains("  g"));
        assert!(!output.contains("> a"));
        assert_eq!(cursor, (3, 9));
    }

    #[test]
    fn tiny_terminals_do_not_panic_or_leak_secrets() {
        let view = View {
            page: Page::Setup,
            setup_key: "sensitive".into(),
            setup_status: "\u{1b}[31mfailed\u{1b}[0m".into(),
            ..View::default()
        };
        for (width, height) in [(0, 0), (1, 1), (4, 2), (12, 4)] {
            let output = screen(&view, &[], width, height);
            assert!(!output.contains("sensitive"));
            assert!(!output.contains('\u{1b}'));
        }
    }
}
