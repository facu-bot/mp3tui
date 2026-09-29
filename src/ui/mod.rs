#![allow(dead_code)]

//! Interfaz de terminal: layout con ratatui y loop de eventos no bloqueante.
//! Dos modos: reproducción (T3+T4) y explorador de biblioteca (T5).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Gauge, Paragraph};
use ratatui::Frame;

use crate::audio::AudioEngine;
use crate::playlist::{move_selection, scroll_offset, Playlist, Track};
use crate::visualizer::{SpectrumAnalyzer, NUM_BANDS};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const VOLUME_STEP: f32 = 0.05;
const PAGE_STEP: isize = 10;

// Paleta compartida por todo el programa: un acento (cian) para títulos y
// selección, texto claro para valores, apagado para etiquetas. Los colores de
// estado (verde/amarillo/rojo) solo se usan donde expresan estado.
const ACCENT: Color = Color::Cyan;
const TEXT: Color = Color::White;
const DIM: Color = Color::DarkGray;
const SELECT_BG: Color = Color::Rgb(13, 27, 42);

const CURSOR: &str = "\u{258c}"; // ▌ fila seleccionada
const PLAYING: &str = "\u{25b6}"; // ▶ pista en reproducción

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Playing,
    Paused,
    Ended,
}

/// Cómo arranca la TUI según el argumento de CLI.
pub enum StartMode {
    /// Archivo por CLI: reproducción directa (comportamiento original).
    PlayFile(PathBuf),
    /// Sin argumento o con carpeta: explorador de la biblioteca.
    Browse(PathBuf),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Player,
    Browser,
}

struct App {
    mode: Mode,
    root: PathBuf,
    /// Última biblioteca conocida; `None` mientras no terminó el primer escaneo.
    library: Option<Playlist>,
    /// Resultado del escaneo en curso (thread de background). `Some` = escaneando.
    scan_rx: Option<Receiver<Playlist>>,
    selected: usize,
    current_path: Option<PathBuf>,
    has_player: bool,
    track_label: String,
    status: Status,
    error: Option<String>,
    should_quit: bool,
    analyzer: SpectrumAnalyzer,
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
pub fn run(engine: &mut AudioEngine, start: StartMode) -> io::Result<()> {
    let (mode, root, current_path, has_player, track_label) = match start {
        StartMode::PlayFile(path) => {
            let root = path
                .parent()
                .filter(|parent| parent.is_dir())
                .map(Path::to_path_buf)
                .unwrap_or_else(fallback_cwd);
            let label = path.to_string_lossy().into_owned();
            (Mode::Player, root, Some(path), true, label)
        }
        StartMode::Browse(root) => (Mode::Browser, root, None, false, String::new()),
    };

    // La biblioteca se escanea en background: con miles de archivos el parseo
    // de tags (lofty, I/O por archivo) no puede congelar el hilo de la UI.
    let mut app = App {
        mode,
        root,
        library: None,
        scan_rx: None,
        selected: 0,
        current_path,
        has_player,
        track_label,
        status: status_of(engine),
        error: None,
        should_quit: false,
        analyzer: SpectrumAnalyzer::new(),
    };
    start_scan(&mut app);
    let mut tui = Tui::new()?;

    let result = event_loop(&mut tui, &mut app, engine);

    engine.stop();
    tui.restore();
    result
}

fn fallback_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn event_loop(tui: &mut Tui, app: &mut App, engine: &mut AudioEngine) -> io::Result<()> {
    loop {
        app.status = status_of(engine);
        poll_scan(app);

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
                handle_key(key, app, engine);
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

fn handle_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        app.should_quit = true;
        return;
    }

    match app.mode {
        Mode::Player => handle_player_key(key, app, engine),
        Mode::Browser => handle_browser_key(key, app, engine),
    }
}

fn handle_player_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char(' ') => toggle_play(app, engine),
        KeyCode::Up => engine.set_volume((engine.volume() + VOLUME_STEP).clamp(0.0, 1.0)),
        KeyCode::Down => engine.set_volume((engine.volume() - VOLUME_STEP).clamp(0.0, 1.0)),
        KeyCode::Char('b') | KeyCode::Char('B') | KeyCode::Tab => enter_browser(app),
        _ => {}
    }
}

fn handle_browser_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => app.should_quit = true,
        KeyCode::Esc | KeyCode::Tab => {
            if app.has_player {
                app.mode = Mode::Player;
            } else {
                app.should_quit = true;
            }
        }
        KeyCode::Char('b') | KeyCode::Char('B') => {
            // 'b' = biblioteca: ya estamos en la biblioteca.
        }
        KeyCode::Enter => play_selected(app, engine),
        KeyCode::Char(' ') => toggle_play(app, engine),
        KeyCode::Up => app.selected = move_selection(app.selected, -1, library_len(app)),
        KeyCode::Down => app.selected = move_selection(app.selected, 1, library_len(app)),
        KeyCode::PageUp => app.selected = move_selection(app.selected, -PAGE_STEP, library_len(app)),
        KeyCode::PageDown => app.selected = move_selection(app.selected, PAGE_STEP, library_len(app)),
        KeyCode::Home => app.selected = move_selection(app.selected, isize::MIN, library_len(app)),
        KeyCode::End => app.selected = move_selection(app.selected, isize::MAX, library_len(app)),
        _ => {}
    }
}

fn library_len(app: &App) -> usize {
    app.library.as_ref().map_or(0, Playlist::len)
}

fn library_get(app: &App, index: usize) -> Option<&Track> {
    app.library.as_ref().and_then(|library| library.get(index))
}

/// Ubica la selección en la pista que está sonando (si está en la biblioteca).
fn resync_selection(app: &mut App) {
    app.selected = app
        .current_path
        .as_ref()
        .and_then(|path| app.library.as_ref().and_then(|library| library.index_of(path)))
        .unwrap_or(0);
}

/// Lanza el escaneo de `app.root` en un thread aparte. Si ya hay un escaneo en
/// curso no lanza otro (el resultado viejo sigue siendo válido).
fn start_scan(app: &mut App) {
    if app.scan_rx.is_some() {
        return;
    }
    let root = app.root.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let playlist = Playlist::from_dir_scan(&root);
        // Si la UI ya salió, el receiver está caído y el resultado se descarta.
        let _ = tx.send(playlist);
    });
    app.scan_rx = Some(rx);
}

/// Chequeo sin bloquear, una vez por frame (mismo espíritu que
/// ingest()/analyze() del visualizador): `try_recv` nunca espera.
fn poll_scan(app: &mut App) {
    let result = app.scan_rx.as_ref().map(|rx| rx.try_recv());
    match result {
        None | Some(Err(TryRecvError::Empty)) => {}
        Some(Ok(playlist)) => {
            let had_library = app.library.is_some();
            app.library = Some(playlist);
            app.scan_rx = None;
            if had_library {
                // Rescann: no patear la selección del usuario, solo acotarla.
                app.selected = app.selected.min(library_len(app).saturating_sub(1));
            } else {
                resync_selection(app);
            }
        }
        Some(Err(TryRecvError::Disconnected)) => {
            // El thread murió sin entregar: biblioteca vacía para no dejar la
            // UI en "Escaneando…" para siempre.
            app.library = Some(Playlist::new());
            app.scan_rx = None;
        }
    }
}

/// Play / pausa / relanzar al terminar la pista.
fn toggle_play(app: &mut App, engine: &mut AudioEngine) {
    if engine.is_playing() {
        engine.pause();
    } else if engine.is_paused() {
        engine.resume();
    } else if let Some(path) = app.current_path.clone()
        && let Err(err) = engine.play(&path)
    {
        app.error = Some(err.to_string());
    }
}

/// Enter en el explorador: reproduce la pista seleccionada y pasa al modo
/// reproducción. Si el archivo no se puede abrir, seguimos en el explorador
/// con el error visible.
fn play_selected(app: &mut App, engine: &mut AudioEngine) {
    let Some(track) = library_get(app, app.selected) else {
        return;
    };
    let path = track.path.clone();
    let label = track.path.to_string_lossy().into_owned();
    match engine.play(&path) {
        Ok(()) => {
            if let Some(library) = app.library.as_mut() {
                library.set_current(Some(app.selected));
            }
            app.current_path = Some(path);
            app.track_label = label;
            app.has_player = true;
            app.mode = Mode::Player;
            app.error = None;
        }
        Err(err) => app.error = Some(err.to_string()),
    }
}

/// 'b' / Tab desde el reproductor: abre el explorador y reescanea la raíz en
/// background (si ya hay un escaneo en curso, no lanza otro).
fn enter_browser(app: &mut App) {
    app.mode = Mode::Browser;
    resync_selection(app);
    start_scan(app);
}

fn render(frame: &mut Frame<'_>, app: &App, engine: &AudioEngine) {
    match app.mode {
        Mode::Player => render_player(frame, app, engine),
        Mode::Browser => render_browser(frame, app),
    }
}

/// Bloque con borde apagado y título acentuado: el patrón de todo el programa.
fn themed_block<'a>(title: Span<'a>) -> Block<'a> {
    Block::bordered()
        .border_style(Style::default().fg(DIM))
        .title(title)
}

fn accent_title(text: &str) -> Span<'static> {
    Span::styled(
        format!(" {text} "),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )
}

fn status_parts(status: Status) -> (&'static str, Color) {
    match status {
        Status::Playing => ("Reproduciendo", Color::Green),
        Status::Paused => ("Pausado", Color::Yellow),
        Status::Ended => ("Fin de la pista", Color::DarkGray),
    }
}

fn status_line(status: Status) -> Line<'static> {
    let (text, color) = status_parts(status);
    Line::from(vec![
        Span::styled("Estado:  ", Style::default().fg(DIM)),
        Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn error_line(app: &App) -> Line<'static> {
    match &app.error {
        Some(err) => Line::from(Span::styled(
            format!("Error:  {err}"),
            Style::default().fg(Color::Red),
        )),
        None => Line::raw(""),
    }
}

fn render_player(frame: &mut Frame<'_>, app: &App, engine: &AudioEngine) {
    let zones = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(4),
            Constraint::Length(3),
        ])
        .split(frame.area());

    render_player_info(frame, zones[0], app, engine);
    render_visualizer(frame, zones[1], app.analyzer.bands());
    render_footer(frame, zones[2], app);
}

fn render_player_info(frame: &mut Frame<'_>, area: Rect, app: &App, engine: &AudioEngine) {
    let block = themed_block(accent_title("mp3tui"));
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

    let file_line = Line::from(vec![
        Span::styled("Archivo: ", Style::default().fg(DIM)),
        Span::styled(app.track_label.as_str(), Style::default().fg(TEXT)),
    ]);

    frame.render_widget(
        Paragraph::new(vec![file_line, status_line(app.status), error_line(app)]),
        rows[0],
    );

    let volume = engine.volume().clamp(0.0, 1.0);
    let percent = (volume * 100.0).round() as u16;
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(ACCENT))
            .ratio(f64::from(volume))
            .label(format!("Volumen {percent}%")),
        rows[1],
    );
}

fn render_browser(frame: &mut Frame<'_>, app: &App) {
    // Sin sesión de audio no hay nada que visualizar: la lista se queda con
    // toda la altura en vez de mostrar un bloque Visualizador vacío.
    let zones = if app.has_player {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),
                Constraint::Min(6),
                Constraint::Length(6),
                Constraint::Length(3),
            ])
            .split(frame.area())
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),
                Constraint::Min(6),
                Constraint::Length(3),
            ])
            .split(frame.area())
    };

    render_browser_header(frame, zones[0], app);
    render_library(frame, zones[1], app);
    if app.has_player {
        render_visualizer(frame, zones[2], app.analyzer.bands());
        render_footer(frame, zones[3], app);
    } else {
        render_footer(frame, zones[2], app);
    }
}

fn render_browser_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = themed_block(accent_title("mp3tui"));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let now_playing = app
        .current_path
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned());

    let now_line = match &now_playing {
        Some(name) => Line::from(vec![
            Span::styled("Sonando: ", Style::default().fg(DIM)),
            Span::styled(PLAYING, Style::default().fg(Color::Green)),
            Span::raw(" "),
            Span::styled(name, Style::default().fg(TEXT)),
        ]),
        None => Line::from(vec![
            Span::styled("Sonando: ", Style::default().fg(DIM)),
            Span::styled("—", Style::default().fg(DIM)),
        ]),
    };

    let status = if app.has_player {
        status_line(app.status)
    } else {
        Line::from(vec![
            Span::styled("Estado:  ", Style::default().fg(DIM)),
            Span::styled("Explorando", Style::default().fg(DIM)),
        ])
    };

    frame.render_widget(Paragraph::new(vec![now_line, status, error_line(app)]), inner);
}

fn render_library(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let folder = folder_label(&app.root);
    let title = match (&app.library, app.scan_rx.is_some()) {
        (Some(library), true) => format!(
            " Biblioteca · {} pistas · {folder} · actualizando… ",
            library.len()
        ),
        (Some(library), false) => {
            format!(" Biblioteca · {} pistas · {folder} ", library.len())
        }
        (None, _) => format!(" Biblioteca · {folder} "),
    };
    let block = themed_block(Span::styled(
        title,
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(library) = &app.library else {
        // Primer escaneo en curso: estado de carga en vez de lista vacía.
        frame.render_widget(
            Paragraph::new(Line::styled(
                " Escaneando biblioteca… ",
                Style::default().fg(ACCENT),
            ))
            .alignment(Alignment::Center),
            inner,
        );
        return;
    };

    if library.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                " Sin archivos de audio (mp3, flac, ogg, wav) en esta carpeta ",
                Style::default().fg(DIM),
            )),
            inner,
        );
        return;
    }

    let view_height = inner.height as usize;
    let first = scroll_offset(app.selected, library.len(), view_height);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(view_height);
    for index in first..first + view_height {
        let Some(track) = library.get(index) else {
            break;
        };
        let is_selected = index == app.selected;
        let is_playing = app.current_path.as_deref() == Some(track.path.as_path());
        lines.push(row_line(
            track,
            index + 1,
            is_selected,
            is_playing,
            inner.width,
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn folder_label(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

/// Una fila de la biblioteca, medida para ocupar exactamente `width`
/// celdas: cursor + indicador + título/meta (recortado) + duración a la derecha.
fn row_line(
    track: &Track,
    number: usize,
    selected: bool,
    playing: bool,
    width: u16,
) -> Line<'static> {
    let cursor = if selected { CURSOR } else { " " };
    let marker = if playing { PLAYING } else { " " };
    let mut duration = track
        .duration
        .map(format_duration)
        .map(|value| format!(" {value:>5}"))
        .unwrap_or_default();

    // Tope: en un terminal muy angosto una duración larga (1:02:05) no debe
    // desbordar la fila; el título tiene prioridad sobre la duración.
    if duration.chars().count() + 2 > width as usize {
        duration = String::new();
    }

    // Presupuesto del bloque central (número + título + meta); el cursor, el
    // indicador y la duración ya ocupan lo suyo.
    let budget = (width as usize).saturating_sub(2 + duration.chars().count());

    let mut mid = format!("{number:>2}. ");
    mid.push_str(&track.title);
    if let Some(secondary) = track.secondary() {
        mid.push_str("  \u{b7} ");
        mid.push_str(&secondary);
    }
    let mut mid = truncate_chars(&mid, budget);
    mid.push_str(&" ".repeat(budget.saturating_sub(mid.chars().count())));

    let (cursor_style, marker_style, title_style, duration_style) = if selected {
        let bg = SELECT_BG;
        (
            Style::default().bg(bg).fg(ACCENT).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(Color::Green),
            Style::default().bg(bg).fg(TEXT).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(DIM),
        )
    } else {
        (
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Green),
            Style::default().fg(TEXT),
            Style::default().fg(DIM),
        )
    };

    Line::from(vec![
        Span::styled(cursor, cursor_style),
        Span::styled(marker, marker_style),
        Span::styled(mid, title_style),
        Span::styled(duration, duration_style),
    ])
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('\u{2026}'); // …
    out
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn render_visualizer(frame: &mut Frame<'_>, area: Rect, bands: &[f32; NUM_BANDS]) {
    let block = themed_block(accent_title("Visualizador"));
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

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let hints = match app.mode {
        Mode::Player => {
            " espacio: play/pausa   flechas arriba/abajo: volumen   b/Tab: biblioteca   q/Esc: salir "
        }
        Mode::Browser if app.has_player => {
            " ↑/↓ PgUp/PgDn: mover   Enter: reproducir   espacio: pausa   Esc/Tab: volver   q: salir "
        }
        Mode::Browser => " ↑/↓ PgUp/PgDn: mover   Enter: reproducir   q/Esc: salir ",
    };

    frame.render_widget(
        Paragraph::new(hints)
            .block(
                Block::bordered()
                    .border_style(Style::default().fg(DIM))
                    .title(accent_title("Atajos")),
            )
            .style(Style::default().fg(DIM))
            .alignment(Alignment::Center),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;

    fn sample_track() -> Track {
        Track {
            path: PathBuf::from(r"C:\music\cancion.mp3"),
            title: "Una canción con un título bastante largo".to_string(),
            artist: Some("Artista".to_string()),
            album: Some("Álbum".to_string()),
            duration: Some(Duration::from_secs(65)),
        }
    }

    fn line_width(line: &Line<'_>) -> usize {
        line.spans.iter().map(|span| span.content.chars().count()).sum()
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|span| span.content.to_string()).collect()
    }

    #[test]
    fn row_fills_exact_width_and_duration_at_right() {
        let track = sample_track();
        let line = row_line(&track, 1, true, false, 40);
        assert_eq!(line_width(&line), 40, "la fila debe ocupar todo el ancho");
        let text = line_text(&line);
        assert!(text.starts_with('▌'), "cursor de selección: {text}");
        assert!(text.ends_with(" 1:05"), "duración a la derecha: {text}");
    }

    #[test]
    fn row_shows_playing_marker_and_number() {
        let track = sample_track();
        let line = row_line(&track, 12, false, true, 60);
        let text = line_text(&line);
        assert!(text.contains('▶'), "marcador de reproducción: {text}");
        assert!(text.contains("12. "), "número de pista: {text}");
        assert!(!text.starts_with('▌'), "sin selección no hay cursor");
    }

    #[test]
    fn row_truncates_very_long_titles_instead_of_overflowing() {
        let mut track = sample_track();
        track.title = "x".repeat(300);
        for width in [10usize, 20, 40] {
            let line = row_line(&track, 1, false, false, width as u16);
            assert_eq!(line_width(&line), width, "width={width}");
            assert!(line_text(&line).contains('\u{2026}'), "debe recortar con …");
        }
    }

    #[test]
    fn duration_format_is_human_readable() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0:00");
        assert_eq!(format_duration(Duration::from_secs(65)), "1:05");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1:02:05");
    }

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

    fn test_app() -> App {
        App {
            mode: Mode::Browser,
            root: PathBuf::from("."),
            library: None,
            scan_rx: None,
            selected: 0,
            current_path: None,
            has_player: false,
            track_label: String::new(),
            status: Status::Ended,
            error: None,
            should_quit: false,
            analyzer: SpectrumAnalyzer::new(),
        }
    }

    fn track_named(name: &str) -> Track {
        Track {
            path: PathBuf::from(format!(r"C:\x\{name}.mp3")),
            title: name.to_string(),
            artist: None,
            album: None,
            duration: None,
        }
    }

    #[test]
    fn poll_scan_waits_then_delivers_and_syncs_selection() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.scan_rx = Some(rx);
        app.current_path = Some(PathBuf::from(r"C:\x\beta.mp3"));

        // Todavia escaneando: no hay biblioteca, pero el receiver sigue vivo.
        poll_scan(&mut app);
        assert!(app.library.is_none(), "espera sin resultados");
        assert!(app.scan_rx.is_some());

        tx.send(crate::playlist::from_tracks_for_test(vec![
            track_named("alpha"),
            track_named("beta"),
        ]))
        .unwrap();
        poll_scan(&mut app);

        assert!(app.scan_rx.is_none(), "scan terminado");
        assert_eq!(app.library.as_ref().map(Playlist::len), Some(2));
        assert_eq!(app.selected, 1, "seleccion en la pista sonando");
    }

    #[test]
    fn poll_scan_disconnected_yields_empty_library_not_hang() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.scan_rx = Some(rx);
        drop(tx);

        poll_scan(&mut app);
        assert!(app.library.is_some(), "no queda en Escaneando eterno");
        assert!(app.library.unwrap().is_empty());
        assert!(app.scan_rx.is_none());
    }

    #[test]
    fn enter_browser_never_starts_a_second_scan() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.scan_rx = Some(rx);

        // Segundo 'b' con un escaneo ya en curso: conserva el receiver original.
        enter_browser(&mut app);
        assert!(app.mode == Mode::Browser);
        assert!(app.scan_rx.is_some());

        // El escaneo original es el que entrega el resultado.
        tx.send(crate::playlist::from_tracks_for_test(vec![track_named("alpha")]))
            .unwrap();
        poll_scan(&mut app);
        assert_eq!(app.library.as_ref().map(Playlist::len), Some(1));
        assert!(app.scan_rx.is_none());
    }

    #[test]
    fn browser_is_safe_while_first_scan_runs() {
        let mut app = test_app();
        let (_tx, rx) = mpsc::channel(); // tx vivo: escaneo "en curso"
        app.scan_rx = Some(rx);

        assert_eq!(library_len(&app), 0);
        assert!(library_get(&app, 0).is_none());
        app.selected = move_selection(app.selected, 5, library_len(&app));
        assert_eq!(app.selected, 0, "la seleccion no sale del rango vacio");

        poll_scan(&mut app);
        assert!(app.library.is_none(), "el scan sigue corriendo");
        assert!(app.scan_rx.is_some());
    }

    #[test]
    fn row_line_survives_narrow_width_with_hour_long_duration() {
        let mut track = sample_track();
        track.duration = Some(Duration::from_secs(3725)); // 1:02:05
        for width in [3u16, 6, 10, 20, 40] {
            let line = row_line(&track, 1, false, false, width);
            assert_eq!(line_width(&line), width as usize, "width={width}");
        }
        // Con muy poco ancho la duración se descarta y el título manda.
        let line = row_line(&track, 1, false, false, 6);
        assert!(!line_text(&line).contains("1:02:05"));
    }
}
