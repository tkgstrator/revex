//! Pure terminal drawing for the persistent interactive UI.

use crate::agents::AgentInfo;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

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
    pub(crate) input: String,
    pub(crate) cursor: usize,
    pub(crate) scroll: usize,
    pub(crate) status: String,
    pub(crate) workspace_selected: usize,
    pub(crate) setup_provider: usize,
    pub(crate) setup_field: usize,
    pub(crate) setup_url: String,
    pub(crate) setup_key: String,
    pub(crate) setup_model: String,
    pub(crate) setup_models: Vec<String>,
    pub(crate) setup_model_selected: usize,
    pub(crate) setup_manual: bool,
    pub(crate) setup_reasoning: String,
    pub(crate) setup_reasoning_selected: usize,
    pub(crate) setup_reasoning_manual: bool,
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
const USER_BG: Color = Color::Rgb(25, 31, 40);
const CODE_BG: Color = Color::Rgb(18, 23, 30);
const MAX_TRANSCRIPT_LINE_CHARS: usize = 2_048;
const MAX_CODE_TOKENS_PER_ROW: usize = 256;

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
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
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
        "{}  ·  {}  ·  {}",
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
        Page::Workspace => "↑↓ choose  ·  Enter confirm",
        Page::Setup if view.setup_field == 5 && area.width >= 60 => {
            "↑↓ choose helper  ·  Space toggle  ·  Enter continue"
        }
        Page::Setup if view.setup_field == 5 => "↑↓ Space toggle Enter",
        Page::Setup => "Tab/↑↓ move  ·  Enter confirm  ·  Esc back",
        Page::Chat => "Enter send  ·  /agents  /status  /config  /exit",
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
    let providers = ["Ollama", "LM Studio", "oMLX", "Custom API"];
    let provider = providers
        .get(view.setup_provider)
        .copied()
        .unwrap_or("Custom API");
    let block = panel(" Setup ");
    let inner = block.inner(area);
    let key = if view.setup_key.is_empty() {
        "(optional)".to_owned()
    } else {
        "•".repeat(view.setup_key.chars().count())
    };
    let model = if view.setup_manual {
        single(view, &view.setup_model)
    } else {
        view.setup_models
            .get(view.setup_model_selected)
            .map(|m| single(view, m))
            .unwrap_or_else(|| single(view, &view.setup_model))
    };
    let model = if model.is_empty() {
        "(choose or enter a model)".into()
    } else {
        model
    };
    let mut lines = vec![
        Line::raw("Connect an OpenAI-compatible model server"),
        Line::raw(""),
    ];
    lines.push(focused(
        format!("Provider: {provider}"),
        view.setup_field == 0,
    ));
    let choices = providers
        .iter()
        .enumerate()
        .map(|(i, name)| {
            if i == view.setup_provider {
                format!("[{name}]")
            } else {
                name.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    lines.push(Line::from(Span::styled(
        format!("    {choices}"),
        Style::default().fg(MUTED),
    )));
    let url_active = view.setup_field == 1;
    let (url, url_cursor) = field_window(
        &single(view, &view.setup_url),
        if url_active { view.cursor } else { 0 },
        inner.width.saturating_sub(7) as usize,
    );
    lines.push(focused(format!("URL: {url}"), url_active));
    let key_active = view.setup_field == 2;
    let (key, key_cursor) = field_window(
        &key,
        if key_active { view.cursor } else { 0 },
        inner.width.saturating_sub(11) as usize,
    );
    lines.push(focused(format!("API key: {key}"), key_active));
    let model_active = view.setup_field == 3;
    let (model, model_cursor) = field_window(
        &model,
        if model_active && view.setup_manual {
            view.cursor
        } else {
            0
        },
        inner.width.saturating_sub(9) as usize,
    );
    lines.push(focused(format!("Model: {model}"), model_active));
    if view.setup_manual {
        lines.push(Line::from(Span::styled(
            "    Manual model ID (edit here)",
            Style::default().fg(MUTED),
        )));
    } else if view.setup_models.is_empty() {
        lines.push(Line::from(Span::styled(
            "    Model list unavailable; enter an ID manually",
            Style::default().fg(MUTED),
        )));
    } else if view.setup_field == 3 {
        let selected = view.setup_model_selected.min(view.setup_models.len() - 1);
        let start = selected.saturating_sub(1);
        for (index, name) in view.setup_models.iter().enumerate().skip(start).take(3) {
            lines.push(focused(
                format!("  {}", single(view, name)),
                index == selected && view.setup_field == 3,
            ));
        }
        lines.push(Line::from(Span::styled(
            "    Type to use a manual model ID",
            Style::default().fg(MUTED),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(
                "    {} models available; Enter to choose",
                view.setup_models.len()
            ),
            Style::default().fg(MUTED),
        )));
    }
    let reasoning_choices = ["Default", "low", "medium", "high", "xhigh", "max"];
    let reasoning = if view.setup_reasoning_manual {
        let value = single(view, &view.setup_reasoning);
        if value.is_empty() {
            "(enter effort)".to_owned()
        } else {
            value
        }
    } else {
        reasoning_choices
            .get(view.setup_reasoning_selected)
            .copied()
            .unwrap_or("Default")
            .to_owned()
    };
    let reasoning_active = view.setup_field == 4;
    let (reasoning, reasoning_cursor) = field_window(
        &reasoning,
        if reasoning_active && view.setup_reasoning_manual {
            view.cursor
        } else {
            0
        },
        inner.width.saturating_sub(13) as usize,
    );
    let reasoning_row = lines.len();
    lines.push(focused(format!("Reasoning: {reasoning}"), reasoning_active));
    lines.push(Line::from(Span::styled(
        "    Default / low / medium / high / xhigh / max · Tab manual",
        Style::default().fg(MUTED),
    )));
    let helpers_row = lines.len();
    lines.push(Line::raw("Helper providers (select any)"));
    for (index, name) in ["Local", "Codex", "Claude"].iter().enumerate() {
        let mark = if view.setup_helpers[index] { 'x' } else { ' ' };
        let availability = if index == 0 {
            if view.setup_helper_available[index] {
                "built in"
            } else {
                "unavailable"
            }
        } else if view.setup_helper_available[index] {
            "CLI available"
        } else {
            "CLI unavailable"
        };
        lines.push(focused(
            format!("[{mark}] {name} ({availability})"),
            view.setup_field == 5 && view.setup_helper_selected == index,
        ));
    }
    let connect_row = lines.len();
    lines.push(focused("Confirm / Continue".into(), view.setup_field == 6));
    if !view.setup_status.is_empty() {
        lines.push(Line::from(Span::styled(
            single(view, &view.setup_status),
            Style::default().fg(ACCENT),
        )));
    }
    let active_row = match view.setup_field {
        0 => 2,
        1 => 4,
        2 => 5,
        3 => 6,
        4 => reasoning_row,
        5 => helpers_row + 1 + view.setup_helper_selected.min(2),
        _ => connect_row,
    };
    let scroll = active_row.saturating_sub(inner.height.saturating_sub(1) as usize);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        area,
    );
    if view.overlay == Overlay::None && inner.width > 0 && inner.height > 0 {
        let editable = match view.setup_field {
            1 => Some((7, url_cursor)),
            2 => Some((11, key_cursor)),
            3 if view.setup_manual => Some((9, model_cursor)),
            4 if view.setup_reasoning_manual => Some((13, reasoning_cursor)),
            _ => None,
        };
        if let Some((prefix, offset)) = editable
            && active_row >= scroll
            && active_row - scroll < inner.height as usize
        {
            let x = inner.x.saturating_add(
                (prefix + offset).min(inner.width.saturating_sub(1) as usize) as u16,
            );
            frame.set_cursor_position((x, inner.y + (active_row - scroll) as u16));
        }
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

fn transcript_rows(view: &View, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for entry in &view.transcript {
        let entry = safe(view, entry);
        let user = entry.starts_with("You  ");
        let assistant = entry.starts_with("Revex  ");
        let message_block = user || assistant;
        let source: Vec<&str> = entry.lines().collect();
        let start = source
            .iter()
            .take_while(|line| line.trim().is_empty())
            .count();
        let end = source
            .iter()
            .rposition(|line| !line.trim().is_empty())
            .map_or(start, |index| index + 1);
        if message_block && rows.last().is_none_or(|line: &Line<'_>| line.width() != 0) {
            rows.push(Line::raw(""));
        }
        let mut in_code = None;
        for &line in &source[start..end] {
            let content = line.strip_prefix("Revex  ").unwrap_or(line).trim_start();
            let fence = assistant && (content.starts_with("```") || content.starts_with("~~~"));
            let language = in_code.unwrap_or_else(|| fence_language(content));
            let mut chars = line.chars();
            let bounded: String = chars.by_ref().take(MAX_TRANSCRIPT_LINE_CHARS).collect();
            let truncated = chars.next().is_some();
            for visual in wrap_visual(&bounded, width) {
                if user {
                    let padding = width.saturating_sub(Line::raw(&visual).width());
                    rows.push(Line::styled(
                        format!("{visual}{}", " ".repeat(padding)),
                        Style::default().bg(USER_BG),
                    ));
                } else if assistant && (in_code.is_some() || fence) {
                    rows.push(code_row(&visual, width, fence, language));
                } else {
                    rows.push(Line::raw(visual));
                }
            }
            if truncated {
                let marker = "… (line truncated)";
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
        if message_block {
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

fn draw_chat(frame: &mut Frame, view: &View, area: Rect) {
    let title = Line::from(Span::styled(
        format!("Revex v{}", env!("CARGO_PKG_VERSION")),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));
    let model = view.model.trim();
    let known_model = !model.is_empty() && model != "server default";
    let model_line = if known_model {
        let model = single(view, model);
        match view.reasoning.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(reasoning) => format!("{model} with {}", single(view, reasoning)),
            None => model,
        }
    } else if model == "server default" {
        "Model selected by server".into()
    } else {
        "No model selected".into()
    };
    let location = if view.connection.is_empty() {
        single(view, &view.cwd)
    } else {
        format!(
            "{}  ·  {}",
            single(view, &view.cwd),
            single(view, &view.connection)
        )
    };
    frame.render_widget(
        Paragraph::new(vec![
            title,
            Line::from(Span::styled(model_line, Style::default().fg(MUTED))),
            Line::from(Span::styled(location, Style::default().fg(MUTED))),
        ]),
        rect(area, 0, 3),
    );

    let input = safe(view, &view.input);
    let (editor_lines, insertion) =
        editor_rows(&input, view.cursor, area.width.saturating_sub(2) as usize);
    let editor_cap = area.height.saturating_sub(7).clamp(1, 6) as usize;
    let editor_height = editor_lines.len().min(editor_cap) as u16;
    let editor_y = area.height.saturating_sub(1 + editor_height);
    let transcript_area = rect(area, 4, editor_y.saturating_sub(4));
    if transcript_area.height > 0 {
        let lines = transcript_rows(view, transcript_area.width as usize);
        let top = lines
            .len()
            .saturating_sub(transcript_area.height as usize)
            .saturating_sub(view.scroll)
            .min(u16::MAX as usize) as u16;
        frame.render_widget(
            Paragraph::new(Text::from(lines)).scroll((top, 0)),
            transcript_area,
        );
    }

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
        let prefix = if original_row == 0 { "› " } else { "  " };
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
            Paragraph::new(editor).style(Style::default().bg(Color::Rgb(28, 34, 44))),
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
    let hints = "/agents · /help";
    let status = single(view, &view.status);
    let status = status.trim();
    let quiet = status.is_empty()
        || status == "Ready"
        || status.starts_with("idle")
        || status.starts_with("done");
    let mut content = hints.to_owned();
    if view.model_in_flight {
        let frames = ["◐", "◓", "◑", "◒"];
        let indicator = format!(
            "{hints}  {} Thinking",
            frames[view.activity_tick as usize % frames.len()]
        );
        let queued = status.rsplit('·').next().unwrap_or("").trim();
        let with_queue = format!("{indicator}  ·  {queued}");
        if queued != "0 queued"
            && queued.ends_with(" queued")
            && Line::raw(&with_queue).width() <= footer.width as usize
        {
            content = with_queue;
        } else if Line::raw(&indicator).width() <= footer.width as usize {
            content = indicator;
        }
    } else if !quiet {
        let full = format!("{hints}  {status}");
        if Line::raw(&full).width() <= footer.width as usize {
            content = full;
        } else if let Some(short) = status.split_whitespace().next() {
            let short = format!("{hints}  {short}");
            if Line::raw(&short).width() <= footer.width as usize {
                content = short;
            }
        }
    }
    frame.render_widget(
        Paragraph::new(content).style(Style::default().fg(MUTED)),
        footer,
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
            format!("↓ more  ·  ↑↓ scroll {}/{}", scroll + 1, max_scroll + 1)
        } else {
            format!("↑ earlier  ·  ↑↓ scroll {}/{}", scroll + 1, max_scroll + 1)
        };
        let message = if preview_shortened {
            if max_scroll == 0 {
                "Preview shortened".to_owned()
            } else {
                format!("Shortened · {message}")
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
        Paragraph::new("Enter confirm  ·  Esc deny").style(Style::default().fg(MUTED)),
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
                let model = agent.model.as_deref().unwrap_or("—");
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
        draw_chat(frame, view, area);
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
        assert!(output.contains("••••"));
    }

    #[test]
    fn setup_defaults_to_ollama_and_offers_four_providers() {
        let view = View {
            page: Page::Setup,
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("Provider: Ollama"));
        assert!(output.contains("[Ollama]  LM Studio  oMLX  Custom API"));
    }

    #[test]
    fn setup_can_select_omlx() {
        let view = View {
            page: Page::Setup,
            setup_provider: 2,
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
            setup_field: 4,
            setup_reasoning_selected: 3,
            ..View::default()
        };
        let output = screen(&view, &[], 80, 24);
        assert!(output.contains("> Reasoning: high"));
        assert!(output.contains("Default / low / medium / high / xhigh / max"));
        assert!(output.contains("Confirm / Continue"));
    }

    #[test]
    fn manual_reasoning_field_shows_insertion_cursor() {
        let view = View {
            page: Page::Setup,
            setup_field: 4,
            setup_reasoning_manual: true,
            setup_reasoning: "ultra".into(),
            cursor: 2,
            ..View::default()
        };
        let (output, cursor) = screen_with_cursor(&view, &[], 80, 24);
        assert!(output.contains("Reasoning: ultra"));
        assert_eq!(cursor, (16, 12));
    }

    #[test]
    fn setup_confirm_is_field_six() {
        let view = View {
            page: Page::Setup,
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
    fn config_start_shows_saved_helpers_even_with_cached_models() {
        let view = View {
            page: Page::Setup,
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
        for expected in [
            concat!("Revex v", env!("CARGO_PKG_VERSION")),
            "/tmp/project",
            "connected",
            "hello",
            "answer",
            "› next question",
            "/agents",
            "/help",
        ] {
            assert!(output.contains(expected), "missing {expected}:\n{output}");
        }
        let footer = output.lines().last().unwrap().trim_end();
        assert_eq!(footer, "/agents · /help");
        assert!(!footer.contains("my-model"));
        assert!(!footer.contains("/tmp/project"));
        assert!(
            !output.contains('┌'),
            "chat should not have bordered panels:\n{output}"
        );
        assert!(!output.contains(">_ Revex"));
    }

    #[test]
    fn empty_chat_shows_prominent_prompt_placeholder() {
        let view = View {
            cwd: "/tmp/project".into(),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 20);
        assert!(output.contains("› Ask Revex to do anything"));
        assert!(!output.contains("Start a conversation."));
    }

    #[test]
    fn idle_chat_footer_stays_sparse_with_long_cwd_and_model() {
        let view = View {
            cwd: "/Users/someone/work/revex/target/release".into(),
            model: "server default".into(),
            status: "idle  ·  0 queued".into(),
            ..View::default()
        };
        for width in [24, 80] {
            let output = screen(&view, &[], width, 20);
            assert_eq!(output.lines().last().unwrap().trim_end(), "/agents · /help");
        }
    }

    #[test]
    fn active_chat_footer_shows_status_only_when_it_fits() {
        let view = View {
            status: "running  ·  2 queued".into(),
            ..View::default()
        };
        let wide = screen(&view, &[], 80, 20);
        assert_eq!(
            wide.lines().last().unwrap().trim_end(),
            "/agents · /help  running  ·  2 queued"
        );
        let narrow = screen(&view, &[], 24, 20);
        assert_eq!(
            narrow.lines().last().unwrap().trim_end(),
            "/agents · /help  running"
        );
    }

    #[test]
    fn chat_header_shows_known_model_and_reasoning_on_separate_line() {
        let view = View {
            model: "local-model".into(),
            reasoning: Some("high".into()),
            cwd: "/tmp/project".into(),
            connection: "connected".into(),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 20);
        let lines: Vec<_> = output.lines().collect();
        assert!(lines[0].starts_with(concat!("Revex v", env!("CARGO_PKG_VERSION"))));
        assert!(lines[1].starts_with("local-model with high"));
        assert!(lines[2].starts_with("/tmp/project"));
        assert!(!lines[19].contains("local-model"));
    }

    #[test]
    fn chat_header_never_fabricates_server_model_or_reasoning() {
        let view = View {
            model: "server default".into(),
            reasoning: Some("high".into()),
            cwd: "/tmp/project".into(),
            ..View::default()
        };
        let output = screen(&view, &[], 80, 20);
        let lines: Vec<_> = output.lines().collect();
        assert!(lines[1].starts_with("Model selected by server"));
        assert!(!output.contains("server default"));
        assert!(!output.contains("with high"));
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
        assert_eq!(buffer[(0, 4)].bg, Color::Reset);
        assert_eq!(buffer[(0, 5)].bg, Color::Rgb(25, 31, 40));
        assert_eq!(buffer[(39, 5)].bg, Color::Rgb(25, 31, 40));
        assert_eq!(buffer[(0, 6)].bg, Color::Reset);
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
        for y in [5, 6] {
            assert_eq!(buffer[(0, y)].bg, Color::Rgb(25, 31, 40));
            assert_eq!(buffer[(19, y)].bg, Color::Rgb(25, 31, 40));
        }
        assert_eq!(buffer[(0, 4)].bg, Color::Reset);
        assert_eq!(buffer[(0, 7)].bg, Color::Reset);
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
        let output = screen(&view, &[], 40, 20);
        let rows: Vec<_> = output.lines().map(str::trim_end).collect();
        assert_eq!(
            rows[4..14],
            [
                "",
                "You  question",
                "",
                "Revex  first line",
                "second line",
                "",
                "tool finished",
                "",
                "Revex  next answer",
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
        let output = screen(&view, &[], 40, 20);
        let rows: Vec<_> = output.lines().map(str::trim_end).collect();
        assert_eq!(
            rows[4..11],
            [
                "",
                "You  live question",
                "",
                "You  restored question",
                "",
                "Revex  answer",
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
        let output = screen(&view, &[], 40, 18);
        let rows: Vec<_> = output.lines().map(str::trim_end).collect();
        assert_eq!(rows[4..8], ["tool finished", "", "You  next task", ""]);
    }

    #[test]
    fn consecutive_assistant_blocks_share_one_blank_row() {
        let view = View {
            transcript: vec!["Revex  answer one".into(), "Revex  answer two".into()],
            ..View::default()
        };
        let output = screen(&view, &[], 40, 18);
        let rows: Vec<_> = output.lines().map(str::trim_end).collect();
        assert_eq!(
            rows[4..10],
            ["", "Revex  answer one", "", "Revex  answer two", "", ""]
        );
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
        assert_eq!(buffer[(0, 7)].fg, Color::Cyan);
        assert_eq!(buffer[(11, 7)].fg, Color::Green);
        assert_eq!(buffer[(17, 7)].fg, Color::DarkGray);
        assert_eq!(buffer[(59, 7)].bg, Color::Rgb(18, 23, 30));
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
                terminal.backend().buffer()[(0, 6)].fg,
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
        assert_eq!(terminal.backend().buffer()[(900, 6)].fg, Color::White);
        assert_eq!(
            terminal.backend().buffer()[(900, 6)].bg,
            Color::Rgb(18, 23, 30)
        );
    }

    #[test]
    fn model_in_flight_indicator_animates_and_disappears() {
        let mut view = View {
            model_in_flight: true,
            activity_tick: 0,
            status: "running  ·  1 queued".into(),
            ..View::default()
        };
        let first = screen(&view, &[], 80, 20);
        assert!(first.lines().last().unwrap().contains("◐ Thinking"));
        assert!(first.lines().last().unwrap().contains("1 queued"));
        view.activity_tick = 1;
        let second = screen(&view, &[], 80, 20);
        assert!(second.lines().last().unwrap().contains("◓ Thinking"));
        view.model_in_flight = false;
        let done = screen(&view, &[], 80, 20);
        assert!(!done.lines().last().unwrap().contains("Thinking"));
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
        assert!(output.contains("API key: •"));
        assert!(output.contains("a valid API response"));
        assert!(!output.contains("API key: a"));
    }

    #[test]
    fn setup_url_cursor_tracks_unicode_cell_width() {
        let view = View {
            page: Page::Setup,
            setup_field: 1,
            setup_url: "ab界d".into(),
            cursor: 3,
            ..View::default()
        };
        let (output, cursor) = screen_with_cursor(&view, &[], 80, 24);
        assert!(output.contains("URL: ab界"));
        assert_eq!(cursor, (12, 8));
    }

    #[test]
    fn setup_key_cursor_tracks_masked_characters_without_revealing_them() {
        let view = View {
            page: Page::Setup,
            setup_field: 2,
            setup_key: "a界b".into(),
            cursor: 2,
            ..View::default()
        };
        let (output, cursor) = screen_with_cursor(&view, &[], 80, 24);
        assert!(output.contains("API key: •••"));
        assert!(!output.contains("a界b"));
        assert_eq!(cursor, (14, 9));
    }

    #[test]
    fn manual_model_cursor_tracks_unicode_cell_width() {
        let view = View {
            page: Page::Setup,
            setup_field: 3,
            setup_manual: true,
            setup_model: "m界x".into(),
            cursor: 2,
            ..View::default()
        };
        let (output, cursor) = screen_with_cursor(&view, &[], 80, 24);
        assert!(output.contains("Model: m界"));
        assert_eq!(cursor, (13, 10));
    }

    #[test]
    fn narrow_setup_field_scrolls_horizontally_and_keeps_cursor_visible() {
        let view = View {
            page: Page::Setup,
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
        assert_eq!(cursor.1, 8);
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
        assert!(rows[17].starts_with("› first line"));
        assert!(rows[18].starts_with("  second line"));
        assert_eq!(cursor, (8, 18));
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
        assert!(output.lines().nth(17).unwrap().starts_with("› one"));
        assert_eq!(cursor, (2, 18));
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
        assert!(!output.contains("› a"));
        assert_eq!(cursor, (3, 10));
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
