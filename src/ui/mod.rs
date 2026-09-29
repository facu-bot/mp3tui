#![allow(dead_code)]

//! Interfaz de terminal (T3): layout con ratatui y loop de eventos no bloqueante.

use std::io;
use std::path::Path;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Gauge, Paragraph};
use ratatui::Frame;

use crate::audio::AudioEngine;
use crate::visualizer::{SpectrumAnalyzer, NUM_BANDS};

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
    analyzer: SpectrumAnalyzer,
}

impl App {
    fn new(track_label: String, status: Status) -> Self {
        Self {
            track_label,
            status,
            error: None,
            should_quit: false,
            analyzer: SpectrumAnalyzer::new(),
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
    loop {
        app.status = status_of(engine);

        // Lock corto: solo copiar muestras del tap; la FFT corre después, sin lock.
        {
            let tap = engine.visualizer();
            let mut tap = tap.lock().unwrap();
            app.analyzer.ingest(&mut tap);
        }
        app.analyzer.analyze();

        tui.terminal.draw(|frame| render(frame, app, engine))?;

        if !event::poll(FRAME_INTERVAL)? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                handle_key(key, app, engine, path);
                if app.should_quit {
                    break;
                }
            }
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
    render_visualizer(frame, zones[1], app.analyzer.bands());
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

fn render_visualizer(frame: &mut Frame<'_>, area: Rect, bands: &[f32; NUM_BANDS]) {
    let block = Block::bordered().title(" Visualizador ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    paint_bands(frame.buffer_mut(), inner, bands);
}

/// Pinta las 32 barras dentro del área interior del bloque, sin widgets:
/// cada celda recibe el simbolo y el color de su altura en el gradiente.
fn paint_bands(buffer: &mut ratatui::buffer::Buffer, inner: Rect, bands: &[f32; NUM_BANDS]) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // 32 slots de `slot_w` columnas: 1 para la barra + 1 de separación.
    let slot_width = (inner.width / NUM_BANDS as u16).max(1);
    let bar_width = slot_width.saturating_sub(1).max(1);
    let right = inner.x + inner.width;
    let bottom = inner.y + inner.height;

    for (band, &magnitude) in bands.iter().enumerate() {
        let x0 = inner.x + band as u16 * slot_width;
        if x0 >= right {
            break;
        }
        let width = bar_width.min(right - x0);
        let height = ((magnitude.clamp(0.0, 1.0)) * inner.height as f32).round() as u16;

        for row in 0..inner.height {
            let y = bottom - 1 - row;
            let (symbol, color) = if row < height {
                let t = if inner.height > 1 {
                    row as f32 / (inner.height - 1) as f32
                } else {
                    1.0
                };
                let (red, green, blue) = spectrum_color(t);
                ('█', Color::Rgb(red, green, blue))
            } else {
                (' ', Color::Reset)
            };
            for dx in 0..width {
                if let Some(cell) = buffer.cell_mut((x0 + dx, y)) {
                    cell.set_char(symbol).set_fg(color);
                }
            }
        }
    }
}

/// Verde (abajo) → amarillo (medio) → rojo (arriba), estilo cava.
fn spectrum_color(t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        let u = t * 2.0;
        ((u * 255.0) as u8, 255, 0)
    } else {
        let u = (t - 0.5) * 2.0;
        (255, ((1.0 - u) * 255.0) as u8, 0)
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(" espacio: play/pausa   flechas arriba/abajo: volumen   q/Esc: salir ")
            .block(Block::bordered().title(" Atajos "))
            .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}


#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;

    #[test]
    fn spectrum_color_extremes() {
        assert_eq!(spectrum_color(0.0), (0, 255, 0));
        assert_eq!(spectrum_color(0.5), (255, 255, 0));
        assert_eq!(spectrum_color(1.0), (255, 0, 0));
    }

    #[test]
    fn paint_bands_full_bar_is_green_at_bottom_and_red_at_top() {
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 1.0;
        paint_bands(&mut buffer, area, &bands);

        let bottom = &buffer[(0u16, 9u16)];
        assert_eq!(bottom.symbol(), "\u{2588}");
        assert_eq!(bottom.fg, Color::Rgb(0, 255, 0));

        let top = &buffer[(0u16, 0u16)];
        assert_eq!(top.symbol(), "\u{2588}");
        assert_eq!(top.fg, Color::Rgb(255, 0, 0));

        let mid = &buffer[(0u16, 5u16)];
        assert_eq!(mid.symbol(), "\u{2588}");
        match mid.fg {
            Color::Rgb(red, green, blue) => {
                assert_eq!(green, 255);
                assert_eq!(blue, 0);
                assert!(red >= 100, "a mitad de altura debe ir hacia amarillo, red={red}");
            }
            other => panic!("esperaba Rgb, obtenido {other:?}"),
        }
    }

    #[test]
    fn paint_bands_zero_leaves_empty_reset_cells() {
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        paint_bands(&mut buffer, area, &[0.0f32; NUM_BANDS]);

        let cell = &buffer[(0u16, 9u16)];
        assert_eq!(cell.symbol(), " ");
        assert_eq!(cell.fg, Color::Reset);
    }

    #[test]
    fn paint_bands_half_height_covers_bottom_half_only() {
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 0.5;
        paint_bands(&mut buffer, area, &bands);

        // 5 filas de 10: y=9..5 son barra, y=4 queda vacia.
        assert_eq!(buffer[(0u16, 5u16)].symbol(), "\u{2588}");
        assert_eq!(buffer[(0u16, 4u16)].symbol(), " ");
        assert_eq!(buffer[(0u16, 4u16)].fg, Color::Reset);
    }
}
