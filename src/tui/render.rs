//! Cached, width-aware rendering. Completed messages are highlighted once;
//! redraws clone only visible lines, and streaming updates invalidate one entry.
use super::{
    app::{App, Entry},
    commands::HELP,
    picker::{Picker, PickerKind},
};
use crate::config::Theme;
use ratatui::{
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use std::sync::OnceLock;
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, ThemeSet},
    parsing::SyntaxSet,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Default)]
pub(super) struct Renderer {
    cache: Vec<CachedEntry>,
    width: usize,
    theme: Option<Theme>,
    generation: u64,
    #[cfg(test)]
    rebuilds: usize,
}
struct CachedEntry {
    revision: u64,
    lines: Vec<Line<'static>>,
}

impl Renderer {
    fn history(&mut self, app: &App, width: usize, height: usize) -> Vec<Line<'static>> {
        if self.width != width
            || self.theme.as_ref() != Some(&app.theme)
            || self.generation != app.history_generation
        {
            self.cache.clear();
            self.width = width;
            self.theme = Some(app.theme.clone());
            self.generation = app.history_generation;
        }
        self.cache.truncate(app.entries.len());
        for (index, entry) in app.entries.iter().enumerate() {
            if self
                .cache
                .get(index)
                .is_some_and(|c| c.revision == entry.revision)
            {
                continue;
            }
            let cached = CachedEntry {
                revision: entry.revision,
                lines: render_entry(entry, width, &app.theme),
            };
            if index == self.cache.len() {
                self.cache.push(cached);
            } else {
                self.cache[index] = cached;
            }
            #[cfg(test)]
            {
                self.rebuilds += 1;
            }
        }
        let total: usize = self.cache.iter().map(|c| c.lines.len()).sum();
        let max_scroll = total.saturating_sub(height);
        let mut skip = max_scroll.saturating_sub(app.scroll.min(max_scroll));
        let mut visible = Vec::with_capacity(height);
        for cached in &self.cache {
            if skip >= cached.lines.len() {
                skip -= cached.lines.len();
                continue;
            }
            let count = (height - visible.len()).min(cached.lines.len() - skip);
            visible.extend_from_slice(&cached.lines[skip..skip + count]);
            skip = 0;
            if visible.len() == height {
                break;
            }
        }
        visible
    }

    pub fn draw(&mut self, frame: &mut Frame, app: &App) {
        let area = frame.area();
        let theme = &app.theme;
        let base = Style::default()
            .bg(color(&theme.background))
            .fg(color(&theme.foreground));
        frame.render_widget(Block::default().style(base), area);
        // Portable terminals expose cells, not pixel-sized CSS padding.
        let area = area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        let input_lines = wrap_lines(
            vec![Line::raw(app.input.text.clone())],
            area.width.saturating_sub(2) as usize,
        );
        let input_height = (input_lines.len() as u16 + 2)
            .clamp(3, 8)
            .min(area.height.saturating_sub(4));
        let regions = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(input_height),
            Constraint::Length(2),
        ])
        .split(area);
        draw_header(frame, app, regions[0]);
        let history = self.history(
            app,
            regions[1].width.saturating_sub(2) as usize,
            regions[1].height as usize,
        );
        frame.render_widget(
            Paragraph::new(history)
                .block(border_block(theme).borders(Borders::LEFT | Borders::RIGHT)),
            regions[1],
        );
        draw_input(frame, app, regions[2]);
        let footer =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(regions[3]);
        frame.render_widget(
            Paragraph::new(app.status.clone()).style(Style::default().fg(color(&theme.muted))),
            footer[0],
        );
        let buttons = if app.busy.is_some() {
            vec![
                button(" Ctrl+C Cancel ", theme),
                Span::raw("  /tools and /mcp work during a run"),
            ]
        } else {
            vec![
                button(" Enter Send ", theme),
                Span::raw("  "),
                button(" /help Commands ", theme),
                Span::raw("  Tab mode | Alt+Enter newline"),
            ]
        };
        frame.render_widget(Paragraph::new(Line::from(buttons)), footer[1]);
        if let Some(picker) = &app.picker {
            draw_picker(
                frame,
                picker,
                theme,
                area,
                app.approval.is_none() && !app.help,
            );
        }
        if app.workflow_complete {
            draw_overlay(
                frame,
                "Workflow complete",
                "n: start a new workflow\nr: repeat this workflow\nq: exit workflow mode",
                None,
                app,
                area,
            );
        }
        if app.help {
            draw_overlay(frame, "Help", HELP, None, app, area);
        }
        if let Some(approval) = &app.approval {
            let choices = if approval.workflow {
                " y Continue | r Retry | s Skip | q Abort "
            } else {
                " y Approve | n Reject | q Abort "
            };
            draw_overlay(
                frame,
                &approval.title,
                &approval.detail,
                Some(choices),
                app,
                area,
            );
        }
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(2)]).split(area);
    let columns = Layout::horizontal([Constraint::Min(10), Constraint::Length(24)]).split(rows[0]);
    frame.render_widget(
        Paragraph::new(format!(" {}", app.model_label)).style(
            Style::default()
                .fg(color(&theme.accent))
                .add_modifier(Modifier::BOLD),
        ),
        columns[0],
    );
    let spend_color = if app.spend.unpriced_requests == 0 {
        &theme.success
    } else {
        &theme.warning
    };
    frame.render_widget(
        Paragraph::new(app.spend.display()).style(Style::default().fg(color(spend_color))),
        columns[1],
    );
    let agent = app.selection.agent.as_deref().unwrap_or("default");
    let detail = format!(" agent: {agent} | effort: {}", app.effort_label);
    frame.render_widget(
        Paragraph::new(detail)
            .style(Style::default().fg(color(&theme.muted)))
            .block(border_block(theme).borders(Borders::BOTTOM)),
        rows[1],
    );
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let block = border_block(theme)
        .title(" Input ")
        .border_style(Style::default().fg(color(&theme.accent)));
    let inner = block.inner(area);
    let mut lines = wrap_lines(
        vec![Line::raw(app.input.text.clone())],
        inner.width as usize,
    );
    let prefix = wrap_lines(
        vec![Line::raw(app.input.text[..app.input.cursor].to_owned())],
        inner.width as usize,
    );
    let mut row = prefix.len().saturating_sub(1);
    let mut column = prefix.last().map(Line::width).unwrap_or(0);
    if column >= inner.width as usize && inner.width > 0 {
        row += 1;
        column = 0;
        lines.push(Line::raw(""));
    }
    let offset = row.saturating_sub(inner.height.saturating_sub(1) as usize);
    if app.input.text.starts_with('/') || app.input.text == ":q" {
        for line in &mut lines {
            line.style = Style::default().fg(color(&theme.accent));
        }
    }
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(lines.into_iter().skip(offset).collect::<Vec<_>>()),
        inner,
    );
    if app.approval.is_none()
        && !app.help
        && app.picker.is_none()
        && inner.width > 0
        && inner.height > 0
    {
        frame.set_cursor_position((
            inner.x + (column as u16).min(inner.width - 1),
            inner.y + ((row - offset) as u16).min(inner.height - 1),
        ));
    }
}

fn draw_picker(frame: &mut Frame, picker: &Picker, theme: &Theme, area: Rect, focused: bool) {
    let width = area.width.min(100);
    let height = area.height.min(24);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = border_block(theme)
        .title(format!(
            " {} ({}/{}) ",
            picker.title(),
            picker.matches.len(),
            picker.choices.len()
        ))
        .style(
            Style::default()
                .bg(color(&theme.background))
                .fg(color(&theme.foreground)),
        )
        .border_style(Style::default().fg(color(&theme.accent)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let search = border_block(theme).title(" Search ");
    let search_area = search.inner(rows[0]);
    frame.render_widget(search, rows[0]);
    if search_area.width > 0 && search_area.height > 0 {
        let prefix = &picker.query.text[..picker.query.cursor];
        let cursor_column = UnicodeWidthStr::width(prefix);
        let desired_skip = cursor_column.saturating_sub(search_area.width as usize - 1);
        let mut skipped = 0;
        let visible: String = picker
            .query
            .text
            .chars()
            .skip_while(|c| {
                if skipped < desired_skip {
                    skipped += c.width().unwrap_or(0);
                    true
                } else {
                    false
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(visible), search_area);
        if focused {
            frame.set_cursor_position((
                search_area.x + cursor_column.saturating_sub(skipped) as u16,
                search_area.y,
            ));
        }
    }
    if picker.matches.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching entries").style(Style::default().fg(color(&theme.muted))),
            rows[1],
        );
    } else {
        // Center selection in the viewport; only render visible labels.
        let height = rows[1].height as usize;
        let start = picker
            .selected
            .saturating_sub(height / 2)
            .min(picker.matches.len().saturating_sub(height));
        let lines = picker
            .matches
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(position, &index)| {
                let choice = &picker.choices[index];
                let mut spans = vec![];
                if let Some(enabled) = choice.enabled {
                    spans.push(Span::styled(
                        if enabled { "[on ] " } else { "[off] " },
                        Style::default().fg(color(if enabled {
                            &theme.success
                        } else {
                            &theme.muted
                        })),
                    ));
                }
                spans.push(if position == picker.selected {
                    button(&choice.label, theme)
                } else {
                    Span::raw(choice.label.as_str())
                });
                Line::from(spans)
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), rows[1]);
    }
    frame.render_widget(
        Paragraph::new(picker.hint()).style(Style::default().fg(color(&theme.muted))),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            button(
                if matches!(picker.kind, PickerKind::Mcps) {
                    " Enter Toggle "
                } else {
                    " Enter Select "
                },
                theme,
            ),
            Span::raw(" Up/Down browse | PgUp/PgDn | Esc close"),
        ])),
        rows[3],
    );
}

fn render_entry(entry: &Entry, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let role_color = match entry.role.as_str() {
        "user" => &theme.user,
        "assistant" => &theme.assistant,
        "tool" => &theme.tool,
        "error" => &theme.error,
        _ => &theme.muted,
    };
    let label = if entry.context == "main" {
        entry.role.clone()
    } else {
        format!("{} | {}", entry.role, entry.context)
    };
    let (top_left, top_right, vertical, bottom_left, bottom_right) = if theme.ascii {
        ('+', '+', '|', '+', '+')
    } else {
        ('┌', '┐', '│', '└', '┘')
    };
    let header_style = Style::default()
        .fg(color(role_color))
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::styled(
        format!("{top_left} {label} {top_right}"),
        header_style,
    )];
    let content = if entry.role == "tool" && entry.text.len() <= 100_000 {
        serde_json::from_str::<serde_json::Value>(&entry.text)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .map(|s| format!("```json\n{s}\n```"))
    } else {
        None
    };
    let content_lines = markdown(
        content.as_deref().unwrap_or(&entry.text),
        theme,
        color(role_color),
    );
    for line in content_lines {
        let mut spans = vec![Span::styled(
            format!("{vertical} "),
            Style::default().fg(color(&theme.border)),
        )];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
    lines.push(Line::styled(
        format!("{bottom_left} {bottom_right}"),
        Style::default().fg(color(&theme.border)),
    ));
    wrap_lines(lines, width)
}

fn draw_overlay(
    frame: &mut Frame,
    title: &str,
    text: &str,
    choices: Option<&str>,
    app: &App,
    area: Rect,
) {
    let theme = &app.theme;
    let rect = Rect {
        x: area.x + area.width / 12,
        y: area.y + area.height / 12,
        width: area.width * 5 / 6,
        height: area.height * 5 / 6,
    };
    frame.render_widget(Clear, rect);
    let block = border_block(theme)
        .title(title)
        .title_bottom(" PgUp/PgDn scroll ")
        .style(Style::default().bg(color(&theme.background)))
        .border_style(Style::default().fg(color(if choices.is_some() {
            &theme.warning
        } else {
            &theme.accent
        })));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let regions = Layout::vertical([
        Constraint::Length(u16::from(choices.is_some())),
        Constraint::Min(0),
    ])
    .split(inner);
    if let Some(choices) = choices {
        frame.render_widget(
            Paragraph::new(Line::from(button(choices, theme))),
            regions[0],
        );
    }
    let lines = wrap_lines(
        markdown(text, theme, color(&theme.foreground)),
        regions[1].width as usize,
    );
    let offset = app
        .overlay_scroll
        .min(lines.len().saturating_sub(regions[1].height as usize));
    frame.render_widget(
        Paragraph::new(lines.into_iter().skip(offset).collect::<Vec<_>>()),
        regions[1],
    );
}

fn button<'a>(label: &'a str, theme: &Theme) -> Span<'a> {
    Span::styled(
        label,
        Style::default()
            .fg(color(&theme.cta_foreground))
            .bg(color(&theme.cta_background))
            .add_modifier(Modifier::BOLD),
    )
}
fn border_block(theme: &Theme) -> Block<'static> {
    let symbols = if theme.ascii {
        border::Set {
            top_left: "+",
            top_right: "+",
            bottom_left: "+",
            bottom_right: "+",
            vertical_left: "|",
            vertical_right: "|",
            horizontal_top: "-",
            horizontal_bottom: "-",
        }
    } else {
        border::ROUNDED
    };
    Block::default()
        .borders(Borders::ALL)
        .border_set(symbols)
        .border_style(Style::default().fg(color(&theme.border)))
}
fn color(hex: &str) -> Color {
    let n = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0xffffff);
    Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)
}

/// Fenced code uses bundled grammars. Unknown languages and pathological long
/// lines fall back to plain text; no network, font, or grammar install is needed.
fn markdown(text: &str, theme: &Theme, foreground: Color) -> Vec<Line<'static>> {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    static THEMES: OnceLock<ThemeSet> = OnceLock::new();
    let mut highlighter = None;
    let mut in_code = false;
    let mut lines = vec![];
    for line in text.lines() {
        if let Some(language) = line.trim_start().strip_prefix("```") {
            in_code = !in_code;
            highlighter = None;
            if in_code && theme.syntax_highlighting {
                let syntaxes = SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines);
                let themes = THEMES.get_or_init(crate::config::themes::syntax_themes);
                let syntax = syntaxes
                    .find_syntax_by_token(language.trim())
                    .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
                if let Some(palette) = themes.themes.get(&theme.syntax_theme) {
                    highlighter = Some(HighlightLines::new(syntax, palette));
                }
            }
            lines.push(Line::styled(
                line.to_owned(),
                Style::default().fg(color(&theme.muted)),
            ));
            continue;
        }
        if let Some(highlighter) = highlighter.as_mut().filter(|_| line.len() <= 4096) {
            if let Ok(tokens) =
                highlighter.highlight_line(&format!("{line}\n"), SYNTAXES.get().unwrap())
            {
                let spans = tokens
                    .into_iter()
                    .map(|(style, text)| {
                        let mut rendered = Style::default().fg(Color::Rgb(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                        ));
                        if style.font_style.contains(FontStyle::BOLD) {
                            rendered = rendered.add_modifier(Modifier::BOLD);
                        }
                        if style.font_style.contains(FontStyle::ITALIC) {
                            rendered = rendered.add_modifier(Modifier::ITALIC);
                        }
                        Span::styled(text.trim_end_matches('\n').to_owned(), rendered)
                    })
                    .collect::<Vec<_>>();
                lines.push(Line::from(spans));
                continue;
            }
        }
        let style = Style::default().fg(foreground);
        if !in_code && line.starts_with('#') {
            lines.push(Line::styled(
                line.to_owned(),
                style.fg(color(&theme.accent)).add_modifier(Modifier::BOLD),
            ));
        } else if !in_code && line.contains('`') {
            let spans = line
                .split_inclusive('`')
                .enumerate()
                .map(|(i, part)| {
                    Span::styled(
                        part.to_owned(),
                        if i % 2 == 1 {
                            style.fg(color(&theme.accent))
                        } else {
                            style
                        },
                    )
                })
                .collect::<Vec<_>>();
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::styled(line.to_owned(), style));
        }
    }
    lines
}

/// Wrap by terminal cell width, retaining each token's style and UTF-8 boundary.
fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut output = vec![];
    for line in lines {
        let mut spans = vec![];
        let mut used = 0;
        for span in line.spans {
            let mut text = String::new();
            let style = line.style.patch(span.style);
            for character in span.content.chars() {
                if character == '\n' {
                    spans.push(Span::styled(std::mem::take(&mut text), style));
                    output.push(Line::from(std::mem::take(&mut spans)));
                    used = 0;
                    continue;
                }
                if character.is_control() && character != '\t' {
                    continue;
                }
                let character = if character == '\t' { ' ' } else { character };
                let cells = character.width().unwrap_or(0);
                if used + cells > width && used > 0 {
                    spans.push(Span::styled(std::mem::take(&mut text), style));
                    output.push(Line::from(std::mem::take(&mut spans)));
                    used = 0;
                }
                text.push(character);
                used += cells;
            }
            if !text.is_empty() {
                spans.push(Span::styled(text, style));
            }
        }
        output.push(Line::from(spans));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        engine::Selection,
        model::{Message, UiEvent},
    };
    use ratatui::{backend::TestBackend, Terminal};

    fn screen(renderer: &mut Renderer, app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| renderer.draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn highlights_code_and_reuses_unchanged_history() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.message(
            "main".into(),
            Message::new(
                "assistant",
                "```rust\nfn main() { let text = \"hello\"; }\n```",
            ),
        );
        let mut renderer = Renderer::default();
        let output = screen(&mut renderer, &app, 90, 24);
        assert!(output.contains("fn main()"));
        assert!(output.contains("Enter Send"));
        assert!(output.contains("┌ assistant ┐"));
        assert!(output.contains("│ "));
        let colors: std::collections::HashSet<_> = renderer.cache[0]
            .lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter_map(|s| s.style.fg)
            .collect();
        assert!(colors.len() >= 3);
        let rebuilds = renderer.rebuilds;
        screen(&mut renderer, &app, 90, 24);
        assert_eq!(renderer.rebuilds, rebuilds);
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "next".into(),
        });
        screen(&mut renderer, &app, 90, 24);
        assert_eq!(renderer.rebuilds, rebuilds + 1);
    }
    #[test]
    fn small_terminals_and_plain_fonts_work() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.theme.ascii = true;
        app.input.insert("a漢é\nsecond line");
        for (width, height) in [(1, 1), (8, 3), (30, 8)] {
            screen(&mut Renderer::default(), &app, width, height);
        }
        assert!(screen(&mut Renderer::default(), &app, 90, 24).contains("+"));
    }

    #[test]
    fn model_picker_scrolls_selection_and_keeps_search_cursor_inside_small_viewports() {
        let mut config = Config::default();
        for i in 0..40 {
            let mut model = config.model.clone();
            model.model = format!("model-{i:02}");
            config.models.insert(format!("alias-{i:02}"), model);
        }
        let mut app = App::new(&config, Selection::default());
        app.picker = Some(Picker::models(&config, &config.model, None));
        app.picker
            .as_mut()
            .unwrap()
            .key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::End,
                crossterm::event::KeyModifiers::CONTROL,
            ));
        let selected = app
            .picker
            .as_ref()
            .unwrap()
            .current()
            .unwrap()
            .label
            .clone();
        let output = screen(&mut Renderer::default(), &app, 90, 24);
        assert!(output.contains("Search"));
        assert!(output.contains(&selected));
        assert!(output.lines().next().unwrap().trim().is_empty());
        assert!(output.lines().last().unwrap().trim().is_empty());
        assert!(output
            .lines()
            .all(|line| line.starts_with(' ') && line.ends_with(' ')));
        app.paste(&"漢é".repeat(40));
        for (width, height) in [(1, 1), (8, 3), (30, 12), (90, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| Renderer::default().draw(frame, &app))
                .unwrap();
            let cursor = terminal.get_cursor_position().unwrap();
            assert!(cursor.x < width && cursor.y < height);
        }
    }

    #[test]
    fn settings_pickers_render_states_and_theme_changes_invalidate_cached_syntax() {
        let config = Config { mcp_servers: serde_json::from_value(serde_json::json!({
            "browser":{"uuid":"browser-id","transport":"stdio","command":"never-started","enabled":false},
            "search":{"uuid":"search-id","transport":"stdio","command":"never-started","enabled":true}
        })).unwrap(), ..Config::default() };
        let mut app = App::new(&config, Selection::default());
        app.picker = Some(Picker::mcps(&config, &crate::tools::Switches::default()));
        let output = screen(&mut Renderer::default(), &app, 100, 24);
        assert!(output.contains("[off] browser"));
        assert!(output.contains("[on ] search"));
        app.picker = Some(Picker::themes(&app.theme, &config.theme));
        for (width, height) in [(1, 1), (8, 3), (30, 8), (100, 24)] {
            screen(&mut Renderer::default(), &app, width, height);
        }
        app.message(
            "main".into(),
            Message::new(
                "assistant",
                "```rust\nfn main() { let n = 42; println!(\"hello\"); }\n```",
            ),
        );
        let mut renderer = Renderer::default();
        screen(&mut renderer, &app, 100, 24);
        let before = renderer.rebuilds;
        for (name, _) in crate::config::themes::PRESETS {
            app.theme = crate::config::themes::preset(name).unwrap();
            screen(&mut renderer, &app, 100, 24);
            assert_eq!(renderer.theme.as_ref(), Some(&app.theme));
            if *name == "mama_j" {
                for rgb in renderer.cache[0]
                    .lines
                    .iter()
                    .flat_map(|l| &l.spans)
                    .filter_map(|s| s.style.fg)
                {
                    if let Color::Rgb(r, g, b) = rgb {
                        assert_eq!((r, g), (g, b));
                    }
                }
            }
        }
        assert_eq!(renderer.rebuilds, before + 6);
        let stable = renderer.rebuilds;
        screen(&mut renderer, &app, 100, 24);
        assert_eq!(renderer.rebuilds, stable);
    }
}
