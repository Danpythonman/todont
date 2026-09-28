//! TUI frontend. Owns view state and rendering; all mutation goes through
//! core.

use std::{
    cell::Cell,
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use jiff::{Timestamp, Zoned};
use ratatui::{
    Frame,
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
        execute,
        terminal::{
            EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
            enable_raw_mode,
        },
    },
    layout::{Constraint, Layout},
    prelude::Terminal,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::config::SyncConfig;
use crate::core::{App, Edit, NewTask, Task};
use crate::due;
use crate::form::{self, Form, FormAction, Submission};
use crate::sync::{self, Stats};

/// Leaves raw mode and the alternate screen, returning the terminal to its
/// normal state. Best-effort: called both on graceful shutdown and from the
/// panic hook, where there's no way to handle a second failure.
fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)
}

/// Puts the terminal into raw/alternate-screen mode for the lifetime of the
/// value and guarantees it's restored afterwards.
///
/// A panic hook is installed (and the previous one chained) so that a panic
/// while the TUI is active doesn't leave the user's shell stuck in raw mode
/// with a garbled alternate screen.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Creates a new ratatui terminal (i.e., set terminal in raw mode and
    /// entering an alternate screen), installing a panic hook so that raw
    /// mode is disabled on panic and previous panic messages still go through.
    pub fn new() -> io::Result<Self> {
        // Chain onto the existing panic hook rather than replacing it, so
        // default panic messages (and any other hooks) still run.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal();
            previous(info);
        }));
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
        let _ = self.terminal.show_cursor();
    }
}

// ---- State + Action + update: keys produce Actions; update() calls core --

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    /// The add/edit form, shown instead of the list.
    Form(Form),
    /// Waiting for y/n before doing something to a task.
    Confirm(Pending),
}

/// A change that needs confirming first.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    Complete { id: i64, title: String },
    Delete { id: i64, title: String },
}

impl Pending {
    fn prompt(&self) -> String {
        match self {
            Pending::Complete { title, .. } => format!("Mark “{title}” done?"),
            Pending::Delete { title, .. } => format!("Delete “{title}”?"),
        }
    }
}

struct State {
    mode: Mode,
    tasks: Vec<Task>,
    list: ListState,
    show_done: bool,
    status: String,
    /// Shown in the list's title, e.g. "synced 14:02" or "offline".
    sync_label: String,
    /// Set when a task changed, so the run loop can ask for a sync.
    changed: bool,
    /// Whether sync is configured; if not, there's no status column.
    sync_on: bool,
    /// A sync is in flight, so unpushed tasks show a spinner.
    syncing: bool,
    /// Which spinner frame to draw; advanced by the run loop.
    spinner: usize,
    /// Already told the user the server runs a different version.
    version_noted: bool,
    quit: bool,
}

impl State {
    fn new() -> Self {
        State {
            mode: Mode::Normal,
            tasks: Vec::new(),
            list: ListState::default(),
            show_done: false,
            status: String::new(),
            sync_label: String::new(),
            changed: false,
            sync_on: false,
            syncing: false,
            spinner: 0,
            version_noted: false,
            quit: false,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Action {
    None,
    Quit,
    Up,
    Down,
    Toggle,
    Delete,
    ToggleShowDone,
    BeginAdd,
    BeginEdit,
    Form(FormAction),
    Confirm,
    Decline,
}

fn key_to_action(state: &State, key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char('c')
    {
        return Action::Quit;
    }
    match &state.mode {
        Mode::Form(_) => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            Action::Form(match key.code {
                KeyCode::Tab | KeyCode::Down => FormAction::Next,
                KeyCode::BackTab | KeyCode::Up => FormAction::Prev,
                KeyCode::Left => FormAction::Left,
                KeyCode::Right => FormAction::Right,
                KeyCode::Home => FormAction::Home,
                KeyCode::End => FormAction::End,
                KeyCode::Backspace => FormAction::Backspace,
                KeyCode::Delete => FormAction::Delete,
                KeyCode::Enter => FormAction::Submit,
                KeyCode::Esc => FormAction::Cancel,
                KeyCode::Char('u') if ctrl => FormAction::Clear,
                KeyCode::Char('a') if ctrl => FormAction::Home,
                KeyCode::Char('e') if ctrl => FormAction::End,
                KeyCode::Char(c) if !ctrl => FormAction::Char(c),
                _ => return Action::None,
            })
        }
        Mode::Confirm(_) => match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => Action::Confirm,
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Action::Decline,
            _ => Action::None,
        },
        Mode::Normal => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('j') | KeyCode::Down => Action::Down,
            KeyCode::Char('k') | KeyCode::Up => Action::Up,
            KeyCode::Char(' ') => Action::Toggle,
            KeyCode::Enter => Action::BeginEdit,
            KeyCode::Char('d') => Action::Delete,
            KeyCode::Char('a') => Action::BeginAdd,
            KeyCode::Char('e') => Action::BeginEdit,
            KeyCode::Char('h') => Action::ToggleShowDone,
            _ => Action::None,
        },
    }
}

/// The only place the TUI touches core. Errors become a status line, never
/// a process exit, so an interactive user gets to try again.
fn update(state: &mut State, app: &mut App, action: Action, now: &Zoned) {
    match action {
        Action::None => {}
        Action::Quit => state.quit = true,

        Action::Down => {
            let i = state.list.selected().map_or(0, |i| i + 1);
            state.list.select(Some(i));
            clamp_selection(state);
        }
        Action::Up => {
            let i = state.list.selected().unwrap_or(0);
            state.list.select(Some(i.saturating_sub(1)));
        }

        Action::Toggle => {
            if let Some(t) = selected(state) {
                let (id, title) = (t.id, t.title.clone());
                if t.done {
                    // Reopening loses nothing, so it doesn't ask.
                    set_done(state, app, id, false);
                } else {
                    state.mode =
                        Mode::Confirm(Pending::Complete { id, title });
                }
            }
        }

        Action::Delete => {
            if let Some(t) = selected(state) {
                let (id, title) = (t.id, t.title.clone());
                state.mode = Mode::Confirm(Pending::Delete { id, title });
            }
        }

        Action::Confirm => {
            let Mode::Confirm(pending) =
                std::mem::replace(&mut state.mode, Mode::Normal)
            else {
                return;
            };
            match pending {
                Pending::Complete { id, .. } => set_done(state, app, id, true),
                Pending::Delete { id, .. } => {
                    state.status = match app.remove(id) {
                        Ok(t) => {
                            state.changed = true;
                            format!("removed: {}", t.title)
                        }
                        Err(e) => format!("error: {e}"),
                    };
                    refresh(state, app);
                }
            }
        }
        Action::Decline => {
            state.mode = Mode::Normal;
            state.status = "cancelled".into();
        }

        Action::ToggleShowDone => {
            state.show_done = !state.show_done;
            refresh(state, app);
        }

        Action::BeginAdd => {
            state.mode = Mode::Form(Form::new());
            state.status.clear();
        }
        Action::BeginEdit => {
            if let Some(t) = selected(state) {
                state.mode = Mode::Form(Form::edit(t, now));
                state.status.clear();
            }
        }
        Action::Form(FormAction::Cancel) => {
            state.mode = Mode::Normal;
            state.status = "cancelled".into();
        }
        Action::Form(FormAction::Submit) => {
            let Mode::Form(form) = &mut state.mode else {
                return;
            };
            // Invalid input stays in the form, with the error shown there.
            let Some(submission) = form.submit(now) else {
                return;
            };
            match save(app, form.editing, submission) {
                Ok(t) => {
                    state.changed = true;
                    state.status = format!("saved: {}", t.title);
                    state.mode = Mode::Normal;
                    refresh(state, app);
                    select_id(state, t.id);
                }
                Err(e) => form.error = Some(e.to_string()),
            }
        }
        Action::Form(a) => {
            if let Mode::Form(form) = &mut state.mode {
                form.apply(a);
            }
        }
    }
}

fn set_done(state: &mut State, app: &mut App, id: i64, done: bool) {
    state.status = match app.set_done(id, done) {
        Ok(t) => {
            state.changed = true;
            let verb = if t.done { "done" } else { "reopened" };
            format!("{verb}: {}", t.title)
        }
        Err(e) => format!("error: {e}"),
    };
    refresh(state, app);
}

fn save(
    app: &mut App,
    editing: Option<i64>,
    s: Submission,
) -> crate::core::Result<Task> {
    match editing {
        None => app.create(NewTask {
            title: &s.title,
            due: s.due,
            remind: s.remind,
            nag: s.nag,
        }),
        Some(id) => app.edit(
            id,
            Edit {
                title: Some(s.title),
                due: Some(s.due),
                remind: Some(s.remind),
                nag: Some(s.nag),
            },
        ),
    }
}

/// Reloads the list from core, keeping the cursor on the same task if it's
/// still visible.
fn refresh(state: &mut State, app: &App) {
    let keep = selected(state).map(|t| t.id);
    match app.tasks(state.show_done) {
        Ok(tasks) => state.tasks = tasks,
        Err(e) => state.status = format!("error: {e}"),
    }
    if let Some(id) = keep {
        select_id(state, id);
    }
    clamp_selection(state);
}

fn selected(state: &State) -> Option<&Task> {
    state.list.selected().and_then(|i| state.tasks.get(i))
}

fn select_id(state: &mut State, id: i64) {
    if let Some(i) = state.tasks.iter().position(|t| t.id == id) {
        state.list.select(Some(i));
    }
}

fn clamp_selection(state: &mut State) {
    let len = state.tasks.len();
    if len == 0 {
        state.list.select(None);
    } else {
        let i = state.list.selected().unwrap_or(0);
        state.list.select(Some(i.min(len - 1)));
    }
}

// ---- Render + event loop -------------------------------------------------

/// Renders one frame of the UI: the task list (or the form in its place),
/// a line of key hints, and a single-line status bar at the bottom.
fn draw(frame: &mut Frame, state: &mut State, now: &Zoned) {
    let hints = hint_lines(hints(&state.mode), frame.area().width);
    let [body, help, status] = Layout::vertical([
        Constraint::Min(1), // body fills whatever space is left
        Constraint::Length(hints.len() as u16), // every key, wrapped
        Constraint::Length(1), // status bar is a single line
    ])
    .areas(frame.area());

    let dim = Style::default().fg(Color::DarkGray);
    frame.render_widget(Paragraph::new(hints), help);
    let status_line = match &state.mode {
        Mode::Confirm(p) => Line::from(vec![
            Span::styled(
                p.prompt(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" (y/n)"),
        ]),
        _ => Line::styled(
            state.status.as_str(),
            Style::default().fg(Color::Yellow),
        ),
    };
    frame.render_widget(Paragraph::new(status_line), status);

    if let Mode::Form(f) = &state.mode {
        form::draw(frame, body, f, now);
        return;
    }

    let now_secs = now.timestamp().as_second();
    let items: Vec<ListItem> = state
        .tasks
        .iter()
        .map(|t| {
            let mark = if t.done { "[x] " } else { "[ ] " };
            let title_style = if t.done {
                Style::default()
                    .add_modifier(Modifier::CROSSED_OUT | Modifier::DIM)
            } else {
                Style::default()
            };
            let (due, due_style) = match t.due {
                None => (String::new(), dim),
                Some(d) => {
                    let text = Timestamp::from_second(d)
                        .map(|ts| due::describe(ts, now))
                        .unwrap_or_default();
                    let style = if !t.done && d <= now_secs {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default().fg(Color::Cyan)
                    };
                    (text, style)
                }
            };
            let mut spans = vec![Span::styled(format!("{:>4} ", t.id), dim)];
            if state.sync_on {
                spans.push(sync_mark(state, t));
            }
            spans.extend([
                Span::raw(mark),
                Span::styled(format!("{due:<16} "), due_style),
                Span::styled(t.title.as_str(), title_style),
            ]);
            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut title = if state.show_done {
        " all tasks "
    } else {
        " tasks "
    }
    .to_string();
    if !state.sync_label.is_empty() {
        title = format!("{title}· {} ", state.sync_label);
    }
    let mut block = Block::default().borders(Borders::ALL).title(title);
    if state.sync_on {
        block = block.title_bottom(sync_legend());
    }
    let list = List::new(items)
        .block(block)
        .highlight_symbol("▌")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_stateful_widget(list, body, &mut state.list);
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A task's sync status: on the server, being pushed now, or waiting.
fn sync_mark(state: &State, t: &Task) -> Span<'static> {
    if !t.dirty {
        Span::styled("✓ ", Style::default().fg(Color::Green))
    } else if state.syncing {
        let frame = SPINNER[state.spinner % SPINNER.len()];
        Span::styled(format!("{frame} "), Style::default().fg(Color::Cyan))
    } else {
        Span::styled("○ ", Style::default().fg(Color::Yellow))
    }
}

fn sync_legend() -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    Line::from(vec![
        Span::raw(" "),
        Span::styled("✓", Style::default().fg(Color::Green)),
        Span::styled(" synced  ", dim),
        Span::styled(SPINNER[0], Style::default().fg(Color::Cyan)),
        Span::styled(" syncing  ", dim),
        Span::styled("○", Style::default().fg(Color::Yellow)),
        Span::styled(" not synced yet ", dim),
    ])
}

/// Every key that does something in `mode`, as (keys, what) pairs.
fn hints(mode: &Mode) -> &'static [(&'static str, &'static str)] {
    match mode {
        Mode::Normal => &[
            ("j/k ↑/↓", "move"),
            ("a", "add"),
            ("e/enter", "edit"),
            ("space", "done/reopen"),
            ("d", "delete"),
            ("h", "show/hide done"),
            ("q/esc/^c", "quit"),
        ],
        Mode::Form(_) => &[
            ("tab/↓", "next field"),
            ("shift-tab/↑", "prev field"),
            ("←/→", "move cursor/change choice"),
            ("space", "change choice"),
            ("home/end ^a/^e", "start/end"),
            ("bksp/del", "delete char/reset choice"),
            ("^u", "clear field"),
            ("enter", "save"),
            ("esc", "cancel"),
            ("^c", "quit"),
        ],
        Mode::Confirm(_) => {
            &[("y/enter", "confirm"), ("n/esc", "cancel"), ("^c", "quit")]
        }
    }
}

/// Lays hints out in as many lines as `width` needs, never splitting one.
fn hint_lines(
    hints: &[(&'static str, &'static str)],
    width: u16,
) -> Vec<Line<'static>> {
    const GAP: &str = "   ";
    let key_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Vec<Span>> = vec![Vec::new()];
    let mut used = 0;
    for &(key, desc) in hints {
        let len = key.chars().count() + 1 + desc.chars().count();
        let line = lines.last_mut().expect("never empty");
        if !line.is_empty() && used + GAP.len() + len > width as usize {
            lines.push(Vec::new());
            used = 0;
        }
        let line = lines.last_mut().expect("never empty");
        if !line.is_empty() {
            line.push(Span::raw(GAP));
            used += GAP.len();
        }
        line.push(Span::styled(key, key_style));
        line.push(Span::styled(format!(" {desc}"), desc_style));
        used += len;
    }
    lines.into_iter().map(Line::from).collect()
}

/// Entry point for the TUI.
pub fn run(
    app: &mut App,
    db_path: &Path,
    sync_cfg: Option<SyncConfig>,
) -> io::Result<()> {
    let mut state = State::new();
    refresh(&mut state, app);
    let syncer = sync_cfg.map(|cfg| Syncer::spawn(db_path.to_path_buf(), cfg));
    if syncer.is_some() {
        state.sync_label = "syncing…".into();
        state.sync_on = true;
    }
    let started = std::time::Instant::now();

    let mut guard = TerminalGuard::new()?;
    while !state.quit {
        if let Some(syncer) = &syncer {
            if std::mem::take(&mut state.changed) {
                syncer.nudge();
            }
            while let Some(result) = syncer.poll() {
                on_synced(&mut state, app, result);
            }
            state.syncing = syncer.busy();
            state.spinner = (started.elapsed().as_millis() / 80) as usize;
        }
        let now = Zoned::now();
        guard.terminal.draw(|f| draw(f, &mut state, &now))?;
        // Timeout keeps the loop alive for ticks/resizes instead of blocking;
        // shorter while the spinner is turning.
        let tick = if state.syncing { 80 } else { 250 };
        if event::poll(Duration::from_millis(tick))? {
            match event::read()? {
                // Windows sends Press *and* Release; ignoring Release avoids
                // every keystroke firing twice.
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let action = key_to_action(&state, key);
                    update(&mut state, app, action, &now);
                }
                _ => {}
            }
        }
    }
    if let Some(syncer) = &syncer {
        if std::mem::take(&mut state.changed) {
            syncer.nudge();
        }
        // Give the last change a moment to reach the server (and so the
        // phone), without hanging if we're offline.
        drop(guard);
        syncer.wait_idle(sync::TIMEOUT + Duration::from_secs(1));
    }
    Ok(())
}

fn on_synced(state: &mut State, app: &App, result: Result<Stats, String>) {
    let now = Zoned::now().strftime("%H:%M").to_string();
    match result {
        Ok(stats) => {
            state.sync_label = format!("synced {now}");
            if !state.version_noted
                && let Some(note) = stats.version_note()
            {
                state.status = note;
                state.version_noted = true;
            }
            // Even with nothing pulled, pushed tasks are no longer dirty.
            refresh(state, app);
        }
        Err(e) => {
            state.sync_label = "offline".into();
            state.status = format!("sync: {e}");
        }
    }
}

/// Syncs on a background thread with its own database connection, so a
/// slow or unreachable server never blocks the UI.
///
/// Every nudge carries a generation number, and every result reports the
/// newest generation it covers; `wait_idle` uses that to know when the
/// last change has been pushed.
struct Syncer {
    nudges: mpsc::Sender<u64>,
    results: mpsc::Receiver<(u64, Result<Stats, String>)>,
    sent: Cell<u64>,
    covered: Cell<u64>,
    /// No result yet: the startup sync is still running.
    started: Cell<bool>,
}

impl Syncer {
    const EVERY: Duration = Duration::from_secs(60);

    fn spawn(path: PathBuf, cfg: SyncConfig) -> Self {
        let (nudges, nudge_rx) = mpsc::channel::<u64>();
        let (result_tx, results) = mpsc::channel();
        std::thread::spawn(move || {
            let mut app = match App::open(&path) {
                Ok(app) => app,
                Err(e) => {
                    let _ = result_tx.send((u64::MAX, Err(e.to_string())));
                    return;
                }
            };
            let mut generation = 0;
            loop {
                let r = sync::sync(&mut app, &cfg).map_err(|e| e.to_string());
                if result_tx.send((generation, r)).is_err() {
                    return;
                }
                match nudge_rx.recv_timeout(Self::EVERY) {
                    Ok(g) => generation = generation.max(g),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
                // Coalesce a burst of edits into one sync.
                while let Ok(g) = nudge_rx.try_recv() {
                    generation = generation.max(g);
                }
            }
        });
        Syncer {
            nudges,
            results,
            sent: Cell::new(0),
            covered: Cell::new(0),
            started: Cell::new(false),
        }
    }

    fn nudge(&self) {
        self.sent.set(self.sent.get() + 1);
        let _ = self.nudges.send(self.sent.get());
    }

    fn poll(&self) -> Option<Result<Stats, String>> {
        let (g, r) = self.results.try_recv().ok()?;
        self.covered.set(self.covered.get().max(g));
        self.started.set(true);
        Some(r)
    }

    /// A sync is running or about to: the startup one, or one nudged by
    /// an edit that hasn't reported back yet.
    fn busy(&self) -> bool {
        !self.started.get() || self.covered.get() < self.sent.get()
    }

    /// Blocks until every nudge so far has been synced, or `timeout`.
    fn wait_idle(&self, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while self.covered.get() < self.sent.get() {
            let left =
                deadline.saturating_duration_since(std::time::Instant::now());
            match self.results.recv_timeout(left) {
                Ok((g, _)) => self.covered.set(self.covered.get().max(g)),
                Err(_) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::*;
    use crate::alerts::{Nag, Remind};

    fn now() -> Zoned {
        jiff::civil::date(2026, 9, 27)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
    }

    fn fixture() -> (State, App) {
        let mut app = App::open_in_memory().unwrap();
        app.add("one", None).unwrap();
        app.add("two", None).unwrap();
        let mut state = State::new();
        refresh(&mut state, &app);
        (state, app)
    }

    fn type_str(state: &mut State, app: &mut App, s: &str) {
        for c in s.chars() {
            form_key(state, app, FormAction::Char(c));
        }
    }

    fn form_key(state: &mut State, app: &mut App, a: FormAction) {
        update(state, app, Action::Form(a), &now());
    }

    fn form(state: &State) -> &Form {
        match &state.mode {
            Mode::Form(f) => f,
            other => panic!("not in the form: {other:?}"),
        }
    }

    // update() is pure w.r.t. the terminal, so the whole TUI is testable
    // without a terminal attached.
    #[test]
    fn toggle_hides_done_task() {
        let (mut state, mut app) = fixture();
        update(&mut state, &mut app, Action::Down, &now());
        update(&mut state, &mut app, Action::Toggle, &now());
        update(&mut state, &mut app, Action::Confirm, &now());
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].title, "one");
        assert_eq!(state.list.selected(), Some(0));
        update(&mut state, &mut app, Action::ToggleShowDone, &now());
        assert_eq!(state.tasks.len(), 2);
    }

    #[test]
    fn completing_and_deleting_ask_first() {
        let (mut state, mut app) = fixture();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for start in [Action::Toggle, Action::Delete] {
            update(&mut state, &mut app, start, &now());
            assert!(matches!(state.mode, Mode::Confirm(_)));
            // Unrelated keys do nothing while asking.
            assert_eq!(
                key_to_action(&state, key(KeyCode::Char('d'))),
                Action::None
            );
            let no = key_to_action(&state, key(KeyCode::Char('n')));
            update(&mut state, &mut app, no, &now());
            assert_eq!(state.mode, Mode::Normal);
        }
        let tasks = app.tasks(true).unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| !t.done));
    }

    #[test]
    fn reopening_does_not_ask() {
        let (mut state, mut app) = fixture();
        update(&mut state, &mut app, Action::ToggleShowDone, &now());
        update(&mut state, &mut app, Action::Toggle, &now());
        update(&mut state, &mut app, Action::Confirm, &now());
        assert!(selected(&state).unwrap().done);
        update(&mut state, &mut app, Action::Toggle, &now());
        assert_eq!(state.mode, Mode::Normal);
        assert!(!selected(&state).unwrap().done);
    }

    #[test]
    fn enter_edits_instead_of_completing() {
        let (state, _app) = fixture();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(key_to_action(&state, enter), Action::BeginEdit);
    }

    #[test]
    fn hints_wrap_without_splitting() {
        let text = |l: &Line| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        let wide = hint_lines(hints(&Mode::Normal), 200);
        assert_eq!(wide.len(), 1);
        let narrow = hint_lines(hints(&Mode::Normal), 40);
        assert!(narrow.len() > 1);
        for line in &narrow {
            assert!(text(line).chars().count() <= 40, "{:?}", text(line));
        }
        let all: String = narrow.iter().map(text).collect();
        for (key, desc) in hints(&Mode::Normal) {
            assert!(all.contains(&format!("{key} {desc}")));
        }
    }

    fn screen(state: &mut State, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, state, &now())).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                let row: String =
                    (0..w).map(|x| buf[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_mode_renders_its_keys() {
        let (mut state, mut app) = fixture();
        assert!(screen(&mut state, 60, 8).contains("e/enter edit"));
        update(&mut state, &mut app, Action::Toggle, &now());
        let s = screen(&mut state, 60, 8);
        assert!(s.contains("Mark “one” done? (y/n)"));
        assert!(s.contains("y/enter confirm"));
        update(&mut state, &mut app, Action::Decline, &now());
        update(&mut state, &mut app, Action::BeginAdd, &now());
        let s = screen(&mut state, 60, 20);
        assert!(s.contains("^u clear field"));
    }

    /// Records that `app`'s pending changes reached the server.
    fn mark_synced(app: &mut App) {
        let pushed = app.pending_changes().unwrap();
        let resp = crate::proto::SyncResponse {
            server_id: "s".into(),
            cursor: 1,
            changes: Vec::new(),
        };
        app.apply_sync(&pushed, &resp).unwrap();
    }

    #[test]
    fn sync_column_shows_each_tasks_state() {
        let (mut state, mut app) = fixture();
        // No sync configured: no column, no legend.
        assert!(!screen(&mut state, 60, 6).contains('○'));

        state.sync_on = true;
        mark_synced(&mut app);
        app.add("fresh", None).unwrap();
        refresh(&mut state, &app);
        let s = screen(&mut state, 60, 8);
        assert!(s.contains("✓ [ ]") && s.contains("○ [ ]"), "{s}");
        assert!(s.contains("✓ synced"), "legend missing:\n{s}");

        state.syncing = true;
        state.spinner = 3;
        let s = screen(&mut state, 60, 8);
        assert!(s.contains("⠸ [ ]") && !s.contains("○ [ ]"), "{s}");

        // Editing a synced task makes it unsynced again.
        let one = state.tasks.iter().find(|t| t.title == "one").unwrap().id;
        app.set_done(one, true).unwrap();
        app.set_done(one, false).unwrap();
        mark_synced(&mut app);
        let t = app.edit(one, Edit::default()).unwrap();
        assert!(t.dirty);
    }

    #[test]
    fn delete_clamps_selection() {
        let (mut state, mut app) = fixture();
        update(&mut state, &mut app, Action::Down, &now());
        update(&mut state, &mut app, Action::Delete, &now());
        update(&mut state, &mut app, Action::Confirm, &now());
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.list.selected(), Some(0));
    }

    #[test]
    fn add_through_the_form() {
        let (mut state, mut app) = fixture();
        update(&mut state, &mut app, Action::BeginAdd, &now());
        type_str(&mut state, &mut app, "call mom");
        form_key(&mut state, &mut app, FormAction::Next);
        type_str(&mut state, &mut app, "today 17:00");
        form_key(&mut state, &mut app, FormAction::Next);
        form_key(&mut state, &mut app, FormAction::Right); // at due time
        form_key(&mut state, &mut app, FormAction::Next);
        form_key(&mut state, &mut app, FormAction::Left); // nag off
        form_key(&mut state, &mut app, FormAction::Submit);
        assert_eq!(state.mode, Mode::Normal);
        assert!(state.changed);
        // Dated tasks sort first, and the cursor follows the new task.
        let t = selected(&state).unwrap();
        assert_eq!(t.title, "call mom");
        assert!(t.due.is_some());
        assert_eq!((t.remind, t.nag), (Remind::Before(0), Nag::Off));
    }

    #[test]
    fn edit_round_trips_every_field() {
        let (mut state, mut app) = fixture();
        update(&mut state, &mut app, Action::BeginAdd, &now());
        type_str(&mut state, &mut app, "x");
        form_key(&mut state, &mut app, FormAction::Next);
        type_str(&mut state, &mut app, "tomorrow 5pm");
        form_key(&mut state, &mut app, FormAction::Next);
        form_key(&mut state, &mut app, FormAction::Left); // remind off
        form_key(&mut state, &mut app, FormAction::Submit);
        let before = selected(&state).unwrap().clone();

        update(&mut state, &mut app, Action::BeginEdit, &now());
        assert_eq!(form(&state).editing, Some(before.id));
        form_key(&mut state, &mut app, FormAction::Submit);
        let after = selected(&state).unwrap();
        assert_eq!(
            (&after.title, after.due, after.remind, after.nag),
            (&before.title, before.due, before.remind, before.nag)
        );
    }

    #[test]
    fn invalid_form_stays_open_and_cancel_discards() {
        let (mut state, mut app) = fixture();
        update(&mut state, &mut app, Action::BeginAdd, &now());
        form_key(&mut state, &mut app, FormAction::Submit); // empty title
        assert!(form(&state).error.as_deref().unwrap().contains("empty"));

        type_str(&mut state, &mut app, "x");
        form_key(&mut state, &mut app, FormAction::Next);
        type_str(&mut state, &mut app, "someday");
        form_key(&mut state, &mut app, FormAction::Submit);
        assert!(form(&state).error.as_deref().unwrap().contains("due date"));

        form_key(&mut state, &mut app, FormAction::Cancel);
        assert_eq!(state.mode, Mode::Normal);
        assert_eq!(app.tasks(true).unwrap().len(), 2);
        assert!(!state.changed);
    }

    #[test]
    fn form_keys_dont_trigger_list_shortcuts() {
        let (mut state, _app) = fixture();
        state.mode = Mode::Form(Form::new());
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert_eq!(
            key_to_action(&state, key('q')),
            Action::Form(FormAction::Char('q'))
        );
        assert_eq!(
            key_to_action(&state, key('d')),
            Action::Form(FormAction::Char('d'))
        );
    }
}
