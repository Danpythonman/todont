//! The TUI's add/edit task form: title, due date, reminder and nag fields.
//!
//! Editing is pure state manipulation (`Form::apply`), so it's testable
//! without a terminal; saving is left to the caller, which owns the `App`.

use jiff::{Timestamp, Zoned};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
};

use crate::alerts::{self, Nag, Remind};
use crate::core::Task;
use crate::due;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Due,
    Remind,
    Nag,
}

const FIELDS: [Field; 4] =
    [Field::Title, Field::Due, Field::Remind, Field::Nag];

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::Title => "Title",
            Field::Due => "Due",
            Field::Remind => "Remind",
            Field::Nag => "Nag",
        }
    }

    fn is_text(self) -> bool {
        matches!(self, Field::Title | Field::Due)
    }
}

/// What a key does inside the form. Submit and Cancel are handled by the
/// caller; everything else by [`Form::apply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormAction {
    Next,
    Prev,
    Left,
    Right,
    Home,
    End,
    Char(char),
    Backspace,
    Delete,
    Clear,
    Submit,
    Cancel,
}

/// A single-line text input. `cursor` counts chars, not bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    text: String,
    cursor: usize,
}

impl TextInput {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        TextInput { text, cursor }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map_or(self.text.len(), |(i, _)| i)
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn insert(&mut self, c: char) {
        let at = self.byte_at(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let at = self.byte_at(self.cursor);
            self.text.remove(at);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.len() {
            let at = self.byte_at(self.cursor);
            self.text.remove(at);
        }
    }
}

/// The values a valid form produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub title: String,
    pub due: Option<Timestamp>,
    pub remind: Remind,
    pub nag: Nag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    /// The task being edited, or `None` for a new one.
    pub editing: Option<i64>,
    pub title: TextInput,
    pub due: TextInput,
    pub remind: Remind,
    pub nag: Nag,
    pub focus: Field,
    /// Why the last submit failed, shown until the next edit.
    pub error: Option<String>,
}

impl Form {
    pub fn new() -> Self {
        Form {
            editing: None,
            title: TextInput::default(),
            due: TextInput::default(),
            remind: Remind::Default,
            nag: Nag::Default,
            focus: Field::Title,
            error: None,
        }
    }

    pub fn edit(task: &Task, now: &Zoned) -> Self {
        let due = task
            .due
            .and_then(|d| Timestamp::from_second(d).ok())
            // A format due::parse reads back, so it can be tweaked.
            .map(|d| {
                d.to_zoned(now.time_zone().clone())
                    .strftime("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        Form {
            editing: Some(task.id),
            title: TextInput::new(task.title.clone()),
            due: TextInput::new(due),
            remind: task.remind,
            nag: task.nag,
            ..Form::new()
        }
    }

    fn focused_text(&mut self) -> Option<&mut TextInput> {
        match self.focus {
            Field::Title => Some(&mut self.title),
            Field::Due => Some(&mut self.due),
            Field::Remind | Field::Nag => None,
        }
    }

    fn move_focus(&mut self, step: isize) {
        let i = FIELDS.iter().position(|&f| f == self.focus).unwrap_or(0);
        let n = FIELDS.len() as isize;
        self.focus = FIELDS[(i as isize + step).rem_euclid(n) as usize];
    }

    fn cycle(&mut self, forward: bool) {
        match self.focus {
            Field::Remind => {
                self.remind =
                    alerts::cycle(Remind::PRESETS, self.remind, forward)
            }
            Field::Nag => {
                self.nag = alerts::cycle(Nag::PRESETS, self.nag, forward)
            }
            Field::Title | Field::Due => {}
        }
    }

    /// Applies an editing action. Submit and Cancel are no-ops here.
    pub fn apply(&mut self, action: FormAction) {
        if !matches!(action, FormAction::Submit | FormAction::Cancel) {
            self.error = None;
        }
        match action {
            FormAction::Next => self.move_focus(1),
            FormAction::Prev => self.move_focus(-1),
            FormAction::Submit | FormAction::Cancel => {}
            _ if !self.focus.is_text() => match action {
                FormAction::Left => self.cycle(false),
                FormAction::Right | FormAction::Char(' ') => self.cycle(true),
                FormAction::Backspace
                | FormAction::Delete
                | FormAction::Clear => match self.focus {
                    Field::Remind => self.remind = Remind::Default,
                    _ => self.nag = Nag::Default,
                },
                _ => {}
            },
            _ => {
                let Some(input) = self.focused_text() else {
                    return;
                };
                match action {
                    FormAction::Left => {
                        input.cursor = input.cursor.saturating_sub(1)
                    }
                    FormAction::Right => {
                        input.cursor = (input.cursor + 1).min(input.len())
                    }
                    FormAction::Home => input.cursor = 0,
                    FormAction::End => input.cursor = input.len(),
                    FormAction::Char(c) => input.insert(c),
                    FormAction::Backspace => input.backspace(),
                    FormAction::Delete => input.delete(),
                    FormAction::Clear => *input = TextInput::default(),
                    _ => {}
                }
            }
        }
    }

    /// The due date as currently typed: `Ok(None)` when blank.
    pub fn due_preview(
        &self,
        now: &Zoned,
    ) -> Result<Option<Timestamp>, due::DueError> {
        let text = self.due.text().trim();
        if text.is_empty() {
            return Ok(None);
        }
        due::parse(text, now).map(Some)
    }

    /// Validates the form. On failure, records the error and moves focus
    /// to the field at fault.
    pub fn submit(&mut self, now: &Zoned) -> Option<Submission> {
        if self.title.text().trim().is_empty() {
            self.focus = Field::Title;
            self.error = Some("title must not be empty".into());
            return None;
        }
        let due = match self.due_preview(now) {
            Ok(due) => due,
            Err(e) => {
                self.focus = Field::Due;
                self.error = Some(e.to_string());
                return None;
            }
        };
        Some(Submission {
            title: self.title.text().trim().to_string(),
            due,
            remind: self.remind,
            nag: self.nag,
        })
    }
}

// ---- Rendering -----------------------------------------------------------

const LABEL_WIDTH: u16 = 8;

/// Draws the form centred in `area`.
pub fn draw(frame: &mut Frame, area: Rect, form: &Form, now: &Zoned) {
    let width = area.width.min(64);
    let height = area.height.min(11);
    let area = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 3,
        width,
        height,
    };
    let title = match form.editing {
        None => " New task ".to_string(),
        Some(id) => format!(" Edit task {id} "),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Cyan))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [
        title_row,
        due_row,
        preview_row,
        _,
        remind_row,
        nag_row,
        _,
        error_row,
    ] = Layout::vertical([Constraint::Length(1); 8]).areas(inner);

    let dim = Style::default().fg(Color::DarkGray);
    text_row(frame, title_row, form, Field::Title, &form.title);
    text_row(frame, due_row, form, Field::Due, &form.due);

    let preview = match form.due_preview(now) {
        Ok(None) => {
            Span::styled("optional: fri 5pm, sep 30, 2026-10-01, +2h", dim)
        }
        Ok(Some(ts)) => {
            let z = ts.to_zoned(now.time_zone().clone());
            Span::styled(
                format!("→ {}", z.strftime("%a %b %d %Y, %H:%M")),
                Style::default().fg(Color::Green),
            )
        }
        Err(_) => Span::styled(
            "✗ not a date I understand",
            Style::default().fg(Color::Red),
        ),
    };
    frame.render_widget(
        Paragraph::new(Line::from(preview)),
        indent(preview_row),
    );

    // Alerts only mean something with a due date.
    let no_due = !matches!(form.due_preview(now), Ok(Some(_)));
    choice_row(
        frame,
        remind_row,
        form,
        Field::Remind,
        form.remind.label(),
        no_due,
    );
    choice_row(frame, nag_row, form, Field::Nag, form.nag.label(), no_due);

    if let Some(e) = &form.error {
        frame.render_widget(
            Paragraph::new(e.as_str()).style(Style::default().fg(Color::Red)),
            error_row,
        );
    }
}

fn indent(row: Rect) -> Rect {
    Rect {
        x: row.x + LABEL_WIDTH,
        width: row.width.saturating_sub(LABEL_WIDTH),
        ..row
    }
}

fn label(field: Field, focused: bool) -> Span<'static> {
    let style = if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Span::styled(
        format!("{:<w$}", field.label(), w = LABEL_WIDTH as usize),
        style,
    )
}

fn text_row(
    frame: &mut Frame,
    row: Rect,
    form: &Form,
    field: Field,
    input: &TextInput,
) {
    let focused = form.focus == field;
    frame.render_widget(Paragraph::new(label(field, focused)), row);
    let area = indent(row);
    let width = area.width.max(1) as usize;
    // Scroll horizontally so the cursor stays visible.
    let offset = (input.cursor + 1).saturating_sub(width);
    let visible: String =
        input.text.chars().skip(offset).take(width).collect();
    let style = if focused {
        Style::default().bg(Color::DarkGray)
    } else {
        Style::default().add_modifier(Modifier::UNDERLINED)
    };
    let padded = format!("{visible:<width$}");
    frame.render_widget(Paragraph::new(padded).style(style), area);
    if focused {
        let x = area.x + (input.cursor - offset) as u16;
        frame.set_cursor_position((x, area.y));
    }
}

fn choice_row(
    frame: &mut Frame,
    row: Rect,
    form: &Form,
    field: Field,
    value: String,
    inactive: bool,
) {
    let focused = form.focus == field;
    let value_style = match (focused, inactive) {
        (true, _) => Style::default().add_modifier(Modifier::REVERSED),
        (false, true) => Style::default().fg(Color::DarkGray),
        (false, false) => Style::default(),
    };
    let mut spans = vec![
        label(field, focused),
        Span::raw(if focused { "‹ " } else { "  " }),
        Span::styled(value, value_style),
        Span::raw(if focused { " ›" } else { "  " }),
    ];
    if inactive {
        spans.push(Span::styled(
            "  (needs a due date)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), row);
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::*;

    fn now() -> Zoned {
        jiff::civil::date(2026, 9, 27)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
    }

    fn type_str(form: &mut Form, s: &str) {
        for c in s.chars() {
            form.apply(FormAction::Char(c));
        }
    }

    #[test]
    fn text_editing_with_cursor() {
        let mut f = Form::new();
        type_str(&mut f, "héllo");
        f.apply(FormAction::Home);
        f.apply(FormAction::Delete);
        type_str(&mut f, "H");
        f.apply(FormAction::End);
        f.apply(FormAction::Left);
        f.apply(FormAction::Backspace);
        assert_eq!(f.title.text(), "Hélo");
        f.apply(FormAction::Clear);
        assert_eq!(f.title.text(), "");
    }

    #[test]
    fn tab_cycles_fields_and_wraps() {
        let mut f = Form::new();
        let mut seen = vec![f.focus];
        for _ in 0..4 {
            f.apply(FormAction::Next);
            seen.push(f.focus);
        }
        assert_eq!(
            seen,
            [
                Field::Title,
                Field::Due,
                Field::Remind,
                Field::Nag,
                Field::Title
            ]
        );
        f.apply(FormAction::Prev);
        assert_eq!(f.focus, Field::Nag);
    }

    #[test]
    fn arrows_and_space_change_choices() {
        let mut f = Form::new();
        f.focus = Field::Remind;
        f.apply(FormAction::Right);
        assert_eq!(f.remind, Remind::Before(0));
        f.apply(FormAction::Char(' '));
        assert_eq!(f.remind, Remind::Before(5));
        f.apply(FormAction::Left);
        f.apply(FormAction::Left);
        assert_eq!(f.remind, Remind::Default);
        f.focus = Field::Nag;
        f.apply(FormAction::Left);
        assert_eq!(f.nag, Nag::Off);
        // Typing letters on a choice does nothing.
        f.apply(FormAction::Char('x'));
        assert_eq!(f.nag, Nag::Off);
    }

    #[test]
    fn submit_validates_and_focuses_the_bad_field() {
        let mut f = Form::new();
        f.focus = Field::Nag;
        assert!(f.submit(&now()).is_none());
        assert_eq!(f.focus, Field::Title);
        assert!(f.error.as_deref().unwrap().contains("empty"));

        type_str(&mut f, "dentist");
        f.apply(FormAction::Next);
        type_str(&mut f, "someday");
        f.focus = Field::Title;
        assert!(f.submit(&now()).is_none());
        assert_eq!(f.focus, Field::Due);

        f.apply(FormAction::Clear);
        assert!(f.error.is_none(), "editing clears the error");
        type_str(&mut f, "fri 10am");
        let s = f.submit(&now()).unwrap();
        assert_eq!(s.title, "dentist");
        assert!(s.due.is_some());
    }

    fn render(form: &Form, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, f.area(), form, &now())).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_at_any_size() {
        let mut f = Form::new();
        type_str(&mut f, "a fairly long title that will need to scroll");
        f.apply(FormAction::Next);
        type_str(&mut f, "fri 5pm");
        f.remind = Remind::Before(15);
        for (w, h) in [(80, 24), (40, 12), (12, 4), (1, 1)] {
            render(&f, w, h);
        }
        let screen = render(&f, 70, 13);
        println!("{screen}");
        assert!(screen.contains("New task"));
        assert!(screen.contains("→ Fri Oct 02 2026, 17:00"));
        assert!(screen.contains("15m before"));
    }

    #[test]
    fn edit_prefills_every_field() {
        let task = Task {
            id: 7,
            uuid: "u".into(),
            title: "call".into(),
            done: false,
            due: Some(now().timestamp().as_second() + 3600),
            created: 0,
            updated: 0,
            remind: Remind::Before(15),
            nag: Nag::Off,
            dirty: false,
        };
        let mut f = Form::edit(&task, &now());
        assert_eq!(f.due.text(), "2026-09-27 11:00");
        let s = f.submit(&now()).unwrap();
        assert_eq!(s.due.map(|t| t.as_second()), task.due);
        assert_eq!((s.remind, s.nag), (task.remind, task.nag));
    }
}
