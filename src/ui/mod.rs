#![allow(dead_code)]

//! Interfaz de terminal (T3): layout con ratatui y loop de eventos no bloqueante.

use std::io;
use std::path::Path;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Gauge, Paragraph};
use ratatui::Frame;

use crate::audio::AudioEngine;

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const VOLUME_STEP: f32 = 0.05;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Playing,
    Paused,
    Ended,
}

struct App {
    track_label: String,
    status: Status,
    error: Option<String>,
    should_quit: bool,
}

impl App {
    fn new(track_label: String, status: Status) -> Self {
        Self {
            track_label,
            status,
            error: None,
            should_quit: false,
        }
    }
}

/// Guard de terminal: raw mode + alternate screen vía `ratatui::try_init`.
/// La restauración está garantizada por `Drop` (y por el panic hook que instala `try_init`).
struct Tui {
    terminal: ratatui::DefaultTerminal,
    restored: bool,
}

impl Tui {
    fn new() -> io::Result<Self> {
        match ratatui::try_init() {
            Ok(terminal) => Ok(Self {
                terminal,
                restored: false,
            }),
            Err(err) => {
                // try_init puede fallar con raw mode ya activo; revertir igualmente.
                let _ = ratatui::try_restore();
                Err(err)
            }
        }
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let _ = self.terminal.show_cursor();
        let _ = ratatui::try_restore();
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Loop principal de la TUI. Sale con la terminal restaurada y el audio detenido.
pub fn run(engine: &mut AudioEngine, path: &Path) -> io::Result<()> {
    let status = status_of(engine);
    let mut app = App::new(path.to_string_lossy().into_owned(), status);
    let mut tui = Tui::new()?;

    let result = event_loop(&mut tui, &mut app, engine, path);

    engine.stop();
    tui.restore();
    result
}

fn event_loop(
    tui: &mut Tui,
    app: &mut App,
    engine: &mut AudioEngine,
    path: &Path,
) -> io::Result<()> {
    let mut needs_redraw = true;

    loop {
        let previous = app.status;
        app.status = status_of(engine);
        if app.status != previous {
            needs_redraw = true;
        }

        if needs_redraw {
            tui.terminal.draw(|frame| render(frame, app, engine))?;
            needs_redraw = false;
        }

        if !event::poll(FRAME_INTERVAL)? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                handle_key(key, app, engine, path);
                if app.should_quit {
                    break;
                }
                needs_redraw = true;
            }
            Event::Resize(..) => needs_redraw = true,
            _ => {}
        }
    }

    Ok(())
}

fn status_of(engine: &AudioEngine) -> Status {
    if engine.is_paused() {
        Status::Paused
    } else if engine.is_playing() {
        Status::Playing
    } else {
        Status::Ended
    }
}

fn handle_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine, path: &Path) {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        app.should_quit = true;
        return;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char(' ') => {
            if engine.is_playing() {
                engine.pause();
            } else if engine.is_paused() {
                engine.resume();
            } else if let Err(err) = engine.play(path) {
                app.error = Some(err.to_string());
            }
        }
        KeyCode::Up => engine.set_volume((engine.volume() + VOLUME_STEP).clamp(0.0, 1.0)),
        KeyCode::Down => engine.set_volume((engine.volume() - VOLUME_STEP).clamp(0.0, 1.0)),
        _ => {}
    }
}

fn render(frame: &mut Frame<'_>, app: &App, engine: &AudioEngine) {
    let zones = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(4),
            Constraint::Length(3),
        ])
        .split(frame.area());

    render_info(frame, zones[0], app, engine);
    render_visualizer(frame, zones[1]);
    render_footer(frame, zones[2]);
}

fn render_info(frame: &mut Frame<'_>, area: Rect, app: &App, engine: &AudioEngine) {
    let block = Block::bordered().title(Span::styled(
        " mp3tui ",
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let dim = Style::default().fg(Color::DarkGray);
    let file_line = Line::from(vec![
        Span::styled("Archivo: ", dim),
        Span::raw(app.track_label.as_str()),
    ]);

    let (status_text, status_color) = match app.status {
        Status::Playing => ("Reproduciendo", Color::Green),
        Status::Paused => ("Pausado", Color::Yellow),
        Status::Ended => ("Fin de la pista", Color::DarkGray),
    };
    let status_line = Line::from(vec![
        Span::styled("Estado:  ", dim),
        Span::styled(
            status_text,
            Style::default().fg(status_color).add_modifier(Modifier::BOLD),
        ),
    ]);

    let error_line = match &app.error {
        Some(err) => Line::from(Span::styled(
            format!("Error:  {err}"),
            Style::default().fg(Color::Red),
        )),
        None => Line::from(""),
    };

    frame.render_widget(
        Paragraph::new(vec![file_line, status_line, error_line]),
        rows[0],
    );

    let volume = engine.volume().clamp(0.0, 1.0);
    let percent = (volume * 100.0).round() as u16;
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(f64::from(volume))
            .label(format!("Volumen {percent}%")),
        rows[1],
    );
}

fn render_visualizer(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new("(T4) espectro FFT proximamente")
            .block(Block::bordered().title(" Visualizador "))
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(" espacio: play/pausa   flechas arriba/abajo: volumen   q/Esc: salir ")
            .block(Block::bordered().title(" Atajos "))
            .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}
