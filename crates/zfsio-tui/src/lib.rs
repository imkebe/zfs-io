use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph, Row, Table};
use zfsio_model::UiSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    Pools,
    Arc,
    Events,
    Help,
}

#[derive(Debug)]
pub struct AppState {
    pub screen: Screen,
    pub paused: bool,
    pub last_snapshot: Option<UiSnapshot>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            screen: Screen::Dashboard,
            paused: false,
            last_snapshot: None,
        }
    }
}

impl AppState {
    pub fn handle_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('?') => self.screen = Screen::Help,
            KeyCode::Char('1') => self.screen = Screen::Dashboard,
            KeyCode::Char('2') => self.screen = Screen::Pools,
            KeyCode::Char('3') => self.screen = Screen::Arc,
            KeyCode::Char('4') => self.screen = Screen::Events,
            KeyCode::Char(' ') => self.paused = !self.paused,
            KeyCode::Tab => self.screen = next_screen(self.screen),
            _ => {}
        }
        false
    }
}

fn next_screen(screen: Screen) -> Screen {
    match screen {
        Screen::Dashboard => Screen::Pools,
        Screen::Pools => Screen::Arc,
        Screen::Arc => Screen::Events,
        Screen::Events | Screen::Help => Screen::Dashboard,
    }
}

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn draw_once(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    match state.screen {
        Screen::Dashboard => draw_dashboard(frame, area, state),
        Screen::Pools => draw_pools(frame, area, state),
        Screen::Arc => draw_arc(frame, area, state),
        Screen::Events => draw_events(frame, area, state),
        Screen::Help => draw_help(frame, area),
    }
}

pub fn poll_exit_key(state: &mut AppState, timeout: Duration) -> Result<bool> {
    if event::poll(timeout)?
        && let Event::Key(key) = event::read()?
        && key.kind == KeyEventKind::Press
    {
        return Ok(state.handle_key(key.code));
    }
    Ok(false)
}

fn draw_dashboard(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Percentage(45),
        Constraint::Percentage(35),
        Constraint::Min(3),
    ])
    .split(area);

    draw_header(frame, chunks[0], state, "Dashboard");
    draw_chart(frame, chunks[1], state, true);
    draw_pools(frame, chunks[2], state);
    draw_events(frame, chunks[3], state);
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, state: &AppState, title: &str) {
    let status = match (&state.last_snapshot, state.paused) {
        (_, true) => "paused".to_string(),
        (Some(snapshot), false) if snapshot.is_stale() => "stale".to_string(),
        (Some(_), false) => "live".to_string(),
        (None, false) => "waiting for samples".to_string(),
    };
    let header = Paragraph::new(format!(
        "zfs-io | {title} | {status} | 1 dashboard 2 pools 3 arc 4 events ? help q quit"
    ))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, area);
}

fn draw_chart(frame: &mut Frame<'_>, area: Rect, state: &AppState, with_header: bool) {
    let Some(snapshot) = &state.last_snapshot else {
        frame.render_widget(Paragraph::new("waiting for telemetry"), area);
        return;
    };

    let read = snapshot.read_history.minute_values();
    let write = snapshot.write_history.minute_values();
    let datasets = vec![
        Dataset::default()
            .name("read")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Cyan))
            .data(&read),
        Dataset::default()
            .name("write")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Yellow))
            .data(&write),
    ];
    let max_y = read
        .iter()
        .chain(write.iter())
        .map(|(_, value)| *value)
        .fold(1.0, f64::max);
    let block = if with_header {
        Block::default().title("Throughput").borders(Borders::ALL)
    } else {
        Block::default().borders(Borders::ALL)
    };
    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(Axis::default().bounds([0.0, 60.0]).labels(["0s", "60s"]))
        .y_axis(Axis::default().bounds([0.0, max_y]).labels(["0", "max"]));
    frame.render_widget(chart, area);
}

fn draw_pools(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let Some(snapshot) = &state.last_snapshot else {
        frame.render_widget(Paragraph::new("waiting for pool telemetry"), area);
        return;
    };
    let rows = snapshot.pools.iter().map(|pool| {
        Row::new(vec![
            pool.name.clone(),
            pool.state.as_str().to_string(),
            format!("{:.1}%", pool.capacity_percent),
            format!("{:.0}", pool.read_iops),
            format!("{:.0}", pool.write_iops),
            pool.error_count.to_string(),
            pool.status.clone(),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Min(12),
        ],
    )
    .header(Row::new(vec![
        "pool", "state", "cap", "r iops", "w iops", "errors", "status",
    ]))
    .block(Block::default().title("Pools").borders(Borders::ALL));
    frame.render_widget(table, area);
}

fn draw_arc(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let Some(snapshot) = &state.last_snapshot else {
        frame.render_widget(Paragraph::new("waiting for ARC telemetry"), area);
        return;
    };
    let arc = &snapshot.arc;
    let text = format!(
        "ARC size: {:.1} GiB\nARC target: {:.1} GiB\nHit ratio: {:.1}%\nMiss ratio: {:.1}%",
        arc.size_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
        arc.target_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
        arc.hit_ratio * 100.0,
        arc.miss_ratio * 100.0
    );
    frame.render_widget(
        Paragraph::new(text).block(Block::default().title("ARC/L2ARC").borders(Borders::ALL)),
        area,
    );
}

fn draw_events(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let Some(snapshot) = &state.last_snapshot else {
        frame.render_widget(Paragraph::new("waiting for events"), area);
        return;
    };
    let text = snapshot
        .events
        .iter()
        .rev()
        .take(6)
        .map(|event| format!("{}: {}", event.severity.as_str(), event.message))
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(
        Paragraph::new(text).block(Block::default().title("Events").borders(Borders::ALL)),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let text = "q/Esc quit\nSpace pause/resume\nTab next screen\n1 dashboard\n2 pools\n3 arc\n4 events\n? help\n\nzfs-io is read-only: collectors must prefer kstats, use bounded work, and skip late results.";
    frame.render_widget(
        Paragraph::new(text).block(Block::default().title("Help").borders(Borders::ALL)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_bindings_change_screen_and_pause() {
        let mut state = AppState::default();
        assert!(!state.handle_key(KeyCode::Char('2')));
        assert_eq!(state.screen, Screen::Pools);
        assert!(!state.handle_key(KeyCode::Char(' ')));
        assert!(state.paused);
        assert!(state.handle_key(KeyCode::Char('q')));
    }
}
