//! Terminal sniffer. Drawing is capped well below the frame rate to save battery.

use crate::can::Frame;
use crate::daemon::{socket_path, valid_label};
use crate::wire::{self, Record, Status, RECORD};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Terminal;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const DRAW_EVERY: Duration = Duration::from_millis(100);
const CHANGE_HOLD: Duration = Duration::from_millis(800);
/// One key per action, because the keyboard is awkward for typing labels. The standard
/// test: press to start, do the action as many times as you like, press to stop.
const ACTIONS: [(char, &str); 5] = [('1', "brake"), ('2', "throttle"), ('3', "steering"), ('4', "clutch"), ('5', "lights")];
/// Asks what is being tested, then behaves like the keys above.
const CUSTOM_KEY: char = '6';
const CUSTOM_DEFAULT: &str = "other";
const MAX_LABEL: usize = 32;

#[derive(Debug, PartialEq)]
pub enum KeyResult {
    Nothing,
    Quit,
    Mark(&'static str, String),
}

#[derive(Clone, Copy, PartialEq)]
pub enum Sort {
    Id,
    Rate,
}

struct IdRow {
    dlc: u8,
    data: [u8; 8],
    changed: [Option<Instant>; 8],
    count: u64,
    window: u64,
    rate: u64,
}

pub struct App {
    rows: BTreeMap<(bool, u32), IdRow>,
    status: Status,
    errors: u64,
    total_rate: u64,
    window: u64,
    window_start: Instant,
    paused: bool,
    sort: Sort,
    active: Vec<String>,
    /// The typed action currently running, which key 6 stops.
    custom: Option<String>,
    /// Text being typed for a new action. Keys go here instead of acting as commands.
    typing: Option<String>,
}

impl App {
    pub fn new(now: Instant) -> Self {
        Self { rows: BTreeMap::new(), status: Status::default(), errors: 0, total_rate: 0, window: 0, window_start: now, paused: false, sort: Sort::Id, active: Vec::new(), custom: None, typing: None }
    }

    pub fn apply(&mut self, f: &Frame, now: Instant) {
        if f.err {
            self.errors += 1;
            return;
        }
        self.window += 1;
        let row = self.rows.entry((f.ext, f.id)).or_insert(IdRow { dlc: f.dlc, data: f.data, changed: [None; 8], count: 0, window: 0, rate: 0 });
        for i in 0..8 {
            if row.count > 0 && row.data[i] != f.data[i] {
                row.changed[i] = Some(now);
            }
        }
        row.data = f.data;
        row.dlc = f.dlc;
        row.count += 1;
        row.window += 1;
    }

    /// Starts or stops an action and returns the marker verb to send.
    pub fn toggle(&mut self, label: &str) -> &'static str {
        if let Some(i) = self.active.iter().position(|a| a == label) {
            self.active.remove(i);
            "stop"
        } else {
            self.active.push(label.to_string());
            "start"
        }
    }

    pub fn on_key(&mut self, code: KeyCode) -> KeyResult {
        if let Some(text) = &mut self.typing {
            match code {
                KeyCode::Enter => {
                    let typed = self.typing.take().unwrap_or_default();
                    let label = if typed.is_empty() { CUSTOM_DEFAULT.to_string() } else { typed };
                    // Already running under a number key, so that key still stops it.
                    if self.active.contains(&label) {
                        return KeyResult::Nothing;
                    }
                    self.custom = Some(label.clone());
                    return KeyResult::Mark(self.toggle(&label), label);
                }
                KeyCode::Esc => self.typing = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) if text.len() < MAX_LABEL => {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        text.push(c);
                    } else if c == ' ' && !text.is_empty() {
                        text.push('-');
                    }
                }
                _ => {}
            }
            return KeyResult::Nothing;
        }

        match code {
            KeyCode::Char('q') | KeyCode::Esc => KeyResult::Quit,
            KeyCode::Char('s') => {
                self.sort = if self.sort == Sort::Id { Sort::Rate } else { Sort::Id };
                KeyResult::Nothing
            }
            KeyCode::Char(' ') => {
                self.paused = !self.paused;
                KeyResult::Nothing
            }
            KeyCode::Char(CUSTOM_KEY) => match self.custom.take() {
                Some(label) => KeyResult::Mark(self.toggle(&label), label),
                None => {
                    self.typing = Some(String::new());
                    KeyResult::Nothing
                }
            },
            KeyCode::Char(c) => match ACTIONS.iter().find(|a| a.0 == c) {
                Some(&(_, label)) => {
                    if self.custom.as_deref() == Some(label) {
                        self.custom = None;
                    }
                    KeyResult::Mark(self.toggle(label), label.to_string())
                }
                None => KeyResult::Nothing,
            },
            _ => KeyResult::Nothing,
        }
    }

    pub fn tick(&mut self, now: Instant) {
        if now.duration_since(self.window_start) < Duration::from_secs(1) {
            return;
        }
        let secs = now.duration_since(self.window_start).as_secs_f64();
        self.total_rate = (self.window as f64 / secs).round() as u64;
        self.window = 0;
        self.window_start = now;
        for row in self.rows.values_mut() {
            row.rate = (row.window as f64 / secs).round() as u64;
            row.window = 0;
        }
    }

    fn draw(&self, frame: &mut ratatui::Frame, now: Instant) {
        let [head, body, foot] = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
        let lost = self.status.kernel_drops as u64 + self.status.log_drops;
        let lost_style = if lost > 0 { Style::new().fg(Color::Red).add_modifier(Modifier::BOLD) } else { Style::new() };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(format!("{} fps  {} ids  bus errors {}  ", self.total_rate, self.rows.len(), self.errors)),
                Span::styled(format!("capture lost {lost}"), lost_style),
                Span::raw(format!("  ui lost {}", self.status.client_drops)),
                Span::styled(if self.paused { "  PAUSED" } else { "" }, Style::new().fg(Color::Yellow)),
                Span::styled(
                    if self.active.is_empty() { String::new() } else { format!("  MARKING {}", self.active.join(" ")) },
                    Style::new().fg(Color::Black).bg(Color::Green),
                ),
            ])),
            head,
        );

        let mut keys: Vec<_> = self.rows.keys().copied().collect();
        if self.sort == Sort::Rate {
            keys.sort_by_key(|k| std::cmp::Reverse(self.rows[k].rate));
        }
        let rows = keys.iter().map(|key| {
            let r = &self.rows[key];
            let id = if key.0 { format!("{:08X}", key.1) } else { format!("{:03X}", key.1) };
            let mut spans = Vec::with_capacity(8);
            for i in 0..(r.dlc as usize).min(8) {
                let hot = r.changed[i].is_some_and(|t| now.duration_since(t) < CHANGE_HOLD);
                let style = if hot { Style::new().fg(Color::Black).bg(Color::Yellow) } else { Style::new() };
                spans.push(Span::styled(format!("{:02X}", r.data[i]), style));
                spans.push(Span::raw(" "));
            }
            Row::new(vec![Cell::from(id), Cell::from(r.rate.to_string()), Cell::from(r.dlc.to_string()), Cell::from(Line::from(spans)), Cell::from(r.count.to_string())])
        });
        let widths = [Constraint::Length(9), Constraint::Length(6), Constraint::Length(4), Constraint::Length(25), Constraint::Min(8)];
        let table = Table::new(rows, widths).header(Row::new(["ID", "Hz", "DLC", "Data", "Count"]).style(Style::new().add_modifier(Modifier::BOLD)));
        frame.render_widget(table, body);
        let footer = match &self.typing {
            Some(text) => format!("What are you testing? {text}_   (Enter to start, Esc to cancel)"),
            None => format!(
                "1 brake 2 throttle 3 steer 4 clutch 5 lights 6 {} | s sort space pause q quit",
                if self.custom.is_some() { "stop" } else { "other" }
            ),
        };
        let footer_style = if self.typing.is_some() { Style::new().fg(Color::Black).bg(Color::Green) } else { Style::new() };
        frame.render_widget(Paragraph::new(footer).style(footer_style), foot);
    }
}

fn open_control() -> Result<UnixStream> {
    let path = socket_path();
    UnixStream::connect(&path).with_context(|| format!("connecting to {}. Is `candeck daemon` running?", path.display()))
}

fn send_mark(control: &mut UnixStream, verb: &str, label: &str) -> Result<()> {
    writeln!(control, "MARK {verb} {label}").context("sending marker to the daemon")
}

/// Marks an action from outside the sniffer.
pub fn mark(verb: &str, label: &str) -> Result<()> {
    if !valid_label(label) {
        anyhow::bail!("label must be 1-32 letters, digits, - or _");
    }
    send_mark(&mut open_control()?, verb, label)
}

fn connect() -> Result<(mpsc::Receiver<Record>, UnixStream)> {
    let mut stream = open_control()?;
    let control = stream.try_clone()?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; RECORD];
        while stream.read_exact(&mut buf).is_ok() {
            if let Some(rec) = wire::decode(&buf) {
                if tx.send(rec).is_err() {
                    return;
                }
            }
        }
    });
    Ok((rx, control))
}

fn drain(app: &mut App, rx: &mpsc::Receiver<Record>, now: Instant) {
    while let Ok(rec) = rx.try_recv() {
        match rec {
            Record::Frame(f) if !app.paused => app.apply(&f, now),
            Record::Frame(_) => {}
            Record::Status(s) => app.status = s,
        }
    }
    if !app.paused {
        app.tick(now);
    }
}

pub fn run() -> Result<()> {
    let (rx, mut control) = connect()?;
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &rx, &mut control);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, rx: &mpsc::Receiver<Record>, control: &mut UnixStream) -> Result<()> {
    let mut app = App::new(Instant::now());
    loop {
        let now = Instant::now();
        drain(&mut app, rx, now);
        terminal.draw(|f| app.draw(f, now))?;
        if event::poll(DRAW_EVERY)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match app.on_key(key.code) {
                        KeyResult::Quit => {
                            // Close open actions so the compare can still use them.
                            for label in std::mem::take(&mut app.active) {
                                send_mark(control, "stop", &label)?;
                            }
                            return Ok(());
                        }
                        KeyResult::Mark(verb, label) => send_mark(control, verb, &label)?,
                        KeyResult::Nothing => {}
                    }
                }
            }
        }
    }
}

/// Renders what the sniffer would show after `secs` seconds, as plain text.
pub fn snapshot(secs: u64) -> Result<String> {
    let (rx, _control) = connect()?;
    let mut app = App::new(Instant::now());
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        drain(&mut app, &rx, Instant::now());
        thread::sleep(DRAW_EVERY);
    }
    let now = Instant::now();
    drain(&mut app, &rx, now);
    Ok(render_text(&app, now, 80, 16))
}

fn render_text(app: &App, now: Instant, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test backend");
    terminal.draw(|f| app.draw(f, now)).expect("draw");
    let buf = terminal.backend().buffer();
    (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u32, first: u8) -> Frame {
        Frame { id, dlc: 2, data: [first, 0x11, 0, 0, 0, 0, 0, 0], ..Default::default() }
    }

    #[test]
    fn marks_only_changed_bytes() {
        let t = Instant::now();
        let mut app = App::new(t);
        app.apply(&frame(0x100, 1), t);
        assert!(app.rows[&(false, 0x100)].changed[0].is_none());
        app.apply(&frame(0x100, 2), t);
        let row = &app.rows[&(false, 0x100)];
        assert!(row.changed[0].is_some());
        assert!(row.changed[1].is_none());
        assert_eq!(row.count, 2);
    }

    #[test]
    fn error_frames_are_counted_not_listed() {
        let t = Instant::now();
        let mut app = App::new(t);
        app.apply(&Frame { err: true, ..Default::default() }, t);
        assert_eq!(app.errors, 1);
        assert!(app.rows.is_empty());
    }

    #[test]
    fn rate_is_per_second() {
        let t = Instant::now();
        let mut app = App::new(t);
        (0..50).for_each(|i| app.apply(&frame(0x100, i), t));
        app.tick(t + Duration::from_secs(2));
        assert_eq!(app.rows[&(false, 0x100)].rate, 25);
        assert_eq!(app.total_rate, 25);
    }

    #[test]
    fn toggle_starts_then_stops() {
        let mut app = App::new(Instant::now());
        assert_eq!(app.toggle("brake"), "start");
        assert_eq!(app.toggle("clutch"), "start");
        assert_eq!(app.toggle("brake"), "stop");
        assert_eq!(app.active, vec!["clutch".to_string()]);
        assert!(render_text(&app, Instant::now(), 90, 8).contains("MARKING clutch"));
    }

    fn press(app: &mut App, text: &str) {
        text.chars().for_each(|c| {
            app.on_key(KeyCode::Char(c));
        });
    }

    #[test]
    fn number_key_starts_and_stops_a_block() {
        let mut app = App::new(Instant::now());
        assert_eq!(app.on_key(KeyCode::Char('1')), KeyResult::Mark("start", "brake".into()));
        assert_eq!(app.on_key(KeyCode::Char('1')), KeyResult::Mark("stop", "brake".into()));
        assert!(app.active.is_empty());
    }

    #[test]
    fn key_six_asks_then_runs_like_the_others() {
        let mut app = App::new(Instant::now());
        assert_eq!(app.on_key(KeyCode::Char('6')), KeyResult::Nothing);
        assert!(render_text(&app, Instant::now(), 90, 8).contains("What are you testing?"));
        // Command letters are text while typing, so this does not quit or sort.
        press(&mut app, "qs horn");
        assert_eq!(app.on_key(KeyCode::Backspace), KeyResult::Nothing);
        assert_eq!(app.on_key(KeyCode::Enter), KeyResult::Mark("start", "qs-hor".into()));
        assert!(render_text(&app, Instant::now(), 90, 8).contains("MARKING qs-hor"));
        assert_eq!(app.on_key(KeyCode::Char('6')), KeyResult::Mark("stop", "qs-hor".into()));
        assert!(app.active.is_empty());
    }

    #[test]
    fn typed_labels_are_safe_and_bounded() {
        let mut app = App::new(Instant::now());
        app.on_key(KeyCode::Char('6'));
        press(&mut app, " ../a$b");
        press(&mut app, &"x".repeat(50));
        let KeyResult::Mark(_, label) = app.on_key(KeyCode::Enter) else { panic!("no mark") };
        assert!(crate::daemon::valid_label(&label), "{label}");
        assert_eq!(label.len(), MAX_LABEL);
        assert!(label.starts_with("ab"));
    }

    #[test]
    fn escape_cancels_typing_without_marking_or_quitting() {
        let mut app = App::new(Instant::now());
        app.on_key(KeyCode::Char('6'));
        press(&mut app, "horn");
        assert_eq!(app.on_key(KeyCode::Esc), KeyResult::Nothing);
        assert!(app.active.is_empty() && app.typing.is_none());
        assert_eq!(app.on_key(KeyCode::Esc), KeyResult::Quit);
    }

    #[test]
    fn empty_name_falls_back_to_other() {
        let mut app = App::new(Instant::now());
        app.on_key(KeyCode::Char('6'));
        assert_eq!(app.on_key(KeyCode::Enter), KeyResult::Mark("start", "other".into()));
    }

    #[test]
    fn renders_row() {
        let t = Instant::now();
        let mut app = App::new(t);
        app.apply(&frame(0x1A0, 0xAB), t);
        let text = render_text(&app, t, 80, 8);
        assert!(text.contains("1A0"), "{text}");
        assert!(text.contains("AB 11"), "{text}");
    }
}
