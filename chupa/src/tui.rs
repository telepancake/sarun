//! Standalone terminal GUI for mirror administration and local reading.

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::reader::{KeyResult, Reader};
use crate::supervisor::{self, Job};

enum Mode {
    Jobs,
    Reader(Reader),
    Add {
        field: usize,
        values: [String; 4],
    },
    ConfirmDelete {
        id: i64,
        data: bool,
    },
}

struct App {
    jobs: Vec<Job>,
    selected: usize,
    status: String,
    mode: Mode,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            jobs: Vec::new(),
            selected: 0,
            status: String::new(),
            mode: Mode::Jobs,
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        match supervisor::jobs_list() {
            Ok(jobs) => {
                self.jobs = jobs;
                self.selected = self.selected.min(self.jobs.len().saturating_sub(1));
            }
            Err(error) => self.status = error,
        }
    }

    fn selected(&self) -> Option<&Job> {
        self.jobs.get(self.selected)
    }

    fn read_selected(&mut self) {
        let Some(job) = self.selected().cloned() else {
            return;
        };
        let result = match job.kind.as_str() {
            "wiki" => Reader::open_wiki(PathBuf::from(job.dest), None),
            "ietf" => Reader::open_ietf(PathBuf::from(job.dest), None),
            kind => {
                self.status = format!("{kind} mirrors are browsed through their native readout");
                return;
            }
        };
        match result {
            Ok(reader) => self.mode = Mode::Reader(reader),
            Err(error) => self.status = error.to_string(),
        }
    }
}

pub fn run() -> Result<(), String> {
    run_app(App::new())
}

pub fn run_reader(reader: Reader) -> Result<(), String> {
    let mut app = App::new();
    app.mode = Mode::Reader(reader);
    run_app(app)
}

fn run_app(mut app: App) -> Result<(), String> {
    enable_raw_mode().map_err(|error| error.to_string())?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|error| error.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|error| error.to_string())?;
    let result = run_loop(&mut terminal, &mut app);
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<(), String> {
    loop {
        terminal
            .draw(|frame| draw(frame, app))
            .map_err(|error| error.to_string())?;
        if !event::poll(Duration::from_millis(250)).map_err(|error| error.to_string())? {
            if matches!(app.mode, Mode::Jobs) {
                app.refresh();
            }
            continue;
        }
        let Event::Key(key) = event::read().map_err(|error| error.to_string())? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match &mut app.mode {
            Mode::Reader(reader) => match reader.handle_key(key.code) {
                KeyResult::Close => app.mode = Mode::Jobs,
                KeyResult::OpenPrompt => {
                    reader.status = "open a mirror from the Chupa list".into()
                }
                _ => {}
            },
            Mode::Add { field, values } => match key.code {
                KeyCode::Esc => app.mode = Mode::Jobs,
                KeyCode::Backspace => {
                    values[*field].pop();
                }
                KeyCode::Tab => *field = (*field + 1) % values.len(),
                KeyCode::BackTab => *field = (*field + values.len() - 1) % values.len(),
                KeyCode::Enter if *field + 1 < values.len() => *field += 1,
                KeyCode::Enter => {
                    let interval = values[3].parse::<i64>().unwrap_or(86_400);
                    match supervisor::job_add(&values[0], &values[1], &values[2], interval) {
                        Ok(id) => {
                            app.status = format!("registered mirror job {id}");
                            app.mode = Mode::Jobs;
                            app.refresh();
                        }
                        Err(error) => app.status = error,
                    }
                }
                KeyCode::Char(character) => values[*field].push(character),
                _ => {}
            },
            Mode::ConfirmDelete { id, data } => match key.code {
                KeyCode::Char('y' | 'Y') => {
                    let result = if *data {
                        supervisor::job_remove_with_data(*id)
                    } else {
                        supervisor::job_remove(*id)
                    };
                    app.status = result.unwrap_or_else(|error| error);
                    app.mode = Mode::Jobs;
                    app.refresh();
                }
                _ => app.mode = Mode::Jobs,
            },
            Mode::Jobs => match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => {
                    app.selected = (app.selected + 1).min(app.jobs.len().saturating_sub(1));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.selected = app.selected.saturating_sub(1);
                }
                KeyCode::Char('a') => {
                    app.mode = Mode::Add {
                        field: 0,
                        values: [String::new(), String::new(), String::new(), "86400".into()],
                    };
                }
                KeyCode::Char('r') => {
                    if let Some(id) = app.selected().map(|job| job.id) {
                        app.status = supervisor::job_run(id)
                            .map(|()| format!("started job {id}"))
                            .unwrap_or_else(|error| error);
                    }
                }
                KeyCode::Char('R') => {
                    app.status = supervisor::run_pending()
                        .map(|ids| format!("started {} pending jobs", ids.len()))
                        .unwrap_or_else(|error| error);
                }
                KeyCode::Char('c') => {
                    if let Some(id) = app.selected().map(|job| job.id) {
                        app.status = supervisor::job_cancel(id)
                            .map(|()| format!("stopping job {id}"))
                            .unwrap_or_else(|error| error);
                    }
                }
                KeyCode::Char(' ') => {
                    if let Some((id, paused)) = app.selected().map(|job| (job.id, job.paused)) {
                        app.status = supervisor::job_set_paused(id, !paused)
                            .map(|()| if paused { "scheduling resumed" } else { "scheduling paused" }.into())
                            .unwrap_or_else(|error| error);
                    }
                }
                KeyCode::Enter | KeyCode::Char('v') => app.read_selected(),
                KeyCode::Char('d') => {
                    if let Some(id) = app.selected().map(|job| job.id) {
                        app.mode = Mode::ConfirmDelete { id, data: false };
                    }
                }
                KeyCode::Char('D') => {
                    if let Some(id) = app.selected().map(|job| job.id) {
                        app.mode = Mode::ConfirmDelete { id, data: true };
                    }
                }
                _ => {}
            },
        }
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    match &mut app.mode {
        Mode::Reader(reader) => {
            reader.render(frame, frame.area(), true);
        }
        Mode::Add { field, values } => {
            let labels = ["kind (git/wiki/ietf/cmd)", "source", "destination", "interval seconds"];
            let lines = labels
                .iter()
                .enumerate()
                .map(|(index, label)| {
                    let style = if index == *field {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default()
                    };
                    Line::from(vec![Span::raw(format!("{label}: ")), Span::styled(values[index].clone(), style)])
                })
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().title("Add mirror · Tab next · Enter save · Esc cancel").borders(Borders::ALL)),
                frame.area(),
            );
        }
        Mode::ConfirmDelete { data, .. } => {
            let operation = if *data { "delete registration and owned mirror data" } else { "remove registration only" };
            frame.render_widget(
                Paragraph::new(format!("{operation}? Press y to confirm; any other key cancels."))
                    .block(Block::default().title("Confirm").borders(Borders::ALL)),
                frame.area(),
            );
        }
        Mode::Jobs => draw_jobs(frame, app),
    }
}

fn draw_jobs(frame: &mut ratatui::Frame<'_>, app: &App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(4), Constraint::Length(3)])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new("Local mirrors · a add · r run · Space pause · c cancel · v read · d remove · D delete data · q quit")
            .block(Block::default().title("Chupa").borders(Borders::ALL)),
        areas[0],
    );
    let items = app.jobs.iter().map(|job| {
        let detail = format!(
            "#{:<3} {:<5} {:<11} {} → {}",
            job.id, job.kind, job.state, job.src, job.dest
        );
        ListItem::new(detail)
    });
    let mut state = ListState::default().with_selected((!app.jobs.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
            .block(Block::default().title("Mirrors").borders(Borders::ALL)),
        areas[1],
        &mut state,
    );
    frame.render_widget(
        Paragraph::new(app.status.as_str()).block(Block::default().title("Status").borders(Borders::ALL)),
        areas[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn standalone_gui_renders_its_mirror_workflow() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App {
            jobs: Vec::new(),
            selected: 0,
            status: "ready".into(),
            mode: Mode::Jobs,
        };
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("Chupa"), "{rendered}");
        assert!(rendered.contains("Local mirrors"), "{rendered}");
    }
}
