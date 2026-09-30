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
use crate::playback::{filter_tracks, next_track, seek_target, shuffle_order, RepeatMode};
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
    /// Índices de la biblioteca que se muestran (filtrados por la búsqueda).
    view: Vec<usize>,
    /// Selección dentro de `view` (nunca un índice directo de la biblioteca).
    selected: usize,
    /// Filtro de búsqueda confirmado (Enter); la lista visible lo respeta.
    filter: Option<String>,
    /// Texto de la búsqueda en curso; `Some` = modo búsqueda activo.
    search: Option<String>,
    /// Shuffle activo: next/previous recorren `shuffle_order` en vez del
    /// orden de archivo.
    shuffle: bool,
    repeat: RepeatMode,
    /// Permutación de `0..view.len()`; válida mientras `shuffle` esté activo
    /// y `shuffle_order.len() == view.len()`.
    shuffle_order: Vec<usize>,
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
        view: Vec::new(),
        selected: 0,
        filter: None,
        search: None,
        shuffle: false,
        repeat: RepeatMode::Off,
        shuffle_order: Vec::new(),
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
        // La transición Playing → Ended es el fin natural de la pista: ahí
        // decide repeat (uno la relanza, todo salta y envuelve).
        let previous_status = app.status;
        app.status = status_of(engine);
        if previous_status == Status::Playing && app.status == Status::Ended {
            handle_natural_end(app, engine);
        }
        // Un seek fallido en el decodificador se toma una sola vez para
        // mostrarlo; no corta la reproducción.
        if let Some(err) = engine.take_seek_error() {
            app.error = Some(err.to_string());
        }
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

    // Con la búsqueda abierta todo lo demás se escribe en el query: ni 'q'
    // sale ni las flechas mueven la selección.
    if app.search.is_some() {
        handle_search_key(key, app);
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
        KeyCode::Left => seek_by(app, engine, -1),
        KeyCode::Right => seek_by(app, engine, 1),
        KeyCode::Char('b') | KeyCode::Char('B') | KeyCode::Tab => enter_browser(app),
        KeyCode::Char('n') | KeyCode::Char('N') => play_step(app, engine, 1),
        KeyCode::Char('p') | KeyCode::Char('P') => play_step(app, engine, -1),
        KeyCode::Char('s') | KeyCode::Char('S') => toggle_shuffle(app),
        KeyCode::Char('r') | KeyCode::Char('R') => app.repeat = app.repeat.cycle(),
        KeyCode::Char('/') => {
            enter_browser(app);
            open_search(app);
        }
        _ => {}
    }
}

fn handle_browser_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    let view_len = app.view.len();
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
        KeyCode::Char('n') | KeyCode::Char('N') => play_step(app, engine, 1),
        KeyCode::Char('p') | KeyCode::Char('P') => play_step(app, engine, -1),
        KeyCode::Char('s') | KeyCode::Char('S') => toggle_shuffle(app),
        KeyCode::Char('r') | KeyCode::Char('R') => app.repeat = app.repeat.cycle(),
        KeyCode::Char('/') => open_search(app),
        KeyCode::Up => app.selected = move_selection(app.selected, -1, view_len),
        KeyCode::Down => app.selected = move_selection(app.selected, 1, view_len),
        KeyCode::Left => seek_by(app, engine, -1),
        KeyCode::Right => seek_by(app, engine, 1),
        KeyCode::PageUp => app.selected = move_selection(app.selected, -PAGE_STEP, view_len),
        KeyCode::PageDown => app.selected = move_selection(app.selected, PAGE_STEP, view_len),
        KeyCode::Home => app.selected = move_selection(app.selected, isize::MIN, view_len),
        KeyCode::End => app.selected = move_selection(app.selected, isize::MAX, view_len),
        _ => {}
    }
}

/// Modo búsqueda incremental: '/' lo abre, y mientras está activo la lista se
/// filtra en vivo. Enter confirma el filtro, Esc cancela y restaura la lista
/// completa.
fn handle_search_key(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Esc => {
            app.search = None;
            app.filter = None;
        }
        KeyCode::Enter => {
            let text = app.search.take().unwrap_or_default();
            app.filter = (!text.trim().is_empty()).then_some(text);
        }
        KeyCode::Backspace => {
            if let Some(query) = app.search.as_mut() {
                query.pop();
            }
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(query) = app.search.as_mut() {
                query.push(c);
            }
        }
        _ => {}
    }
    refresh_view(app);
}

fn open_search(app: &mut App) {
    // El texto arranca del filtro actual para poder editarlo.
    app.search = Some(app.filter.clone().unwrap_or_default());
    refresh_view(app);
}

fn library_get(app: &App, index: usize) -> Option<&Track> {
    app.library.as_ref().and_then(|library| library.get(index))
}

fn view_get(app: &App, index: usize) -> Option<&Track> {
    app.view
        .get(index)
        .and_then(|&library_index| library_get(app, library_index))
}

/// Posición de un path dentro de la lista visible.
fn position_in_view(app: &App, path: &Path) -> Option<usize> {
    let library_index = app.library.as_ref()?.index_of(path)?;
    app.view.iter().position(|&index| index == library_index)
}

/// Posición de la pista sonando dentro de la lista visible (la que recorren
/// next/previous con shuffle y repeat).
fn current_view_position(app: &App) -> Option<usize> {
    let path = app.current_path.as_ref()?;
    position_in_view(app, path)
}

/// Recalcula la lista visible (biblioteca filtrada por búsqueda/filtro) sin
/// perder la selección: se preserva la pista seleccionada por path, si ya no
/// existe se usa la sonando y si ninguna está en la vista se acota a rango.
/// Con shuffle activo regenera el orden mezclado para la nueva lista.
fn refresh_view(app: &mut App) {
    let previous_path = app
        .view
        .get(app.selected)
        .and_then(|&library_index| library_get(app, library_index))
        .map(|track| track.path.clone());

    let needle = app
        .search
        .as_ref()
        .or(app.filter.as_ref())
        .map(String::as_str);
    app.view = app
        .library
        .as_ref()
        .map(|library| filter_tracks(library.tracks(), needle))
        .unwrap_or_default();

    let new_selected = previous_path
        .as_ref()
        .and_then(|path| position_in_view(app, path))
        .or_else(|| current_view_position(app))
        .unwrap_or(0)
        .min(app.view.len().saturating_sub(1));
    app.selected = new_selected;

    if app.shuffle {
        app.shuffle_order = shuffle_order(app.view.len());
    }
}

/// Ubica la selección en la pista que está sonando (si está en la biblioteca).
fn resync_selection(app: &mut App) {
    if let Some(position) = current_view_position(app) {
        app.selected = position;
    }
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
            app.library = Some(playlist);
            app.scan_rx = None;
            // Reaplica filtro y, si había shuffle activo, regenera el orden
            // mezclado; la selección se preserva por pista.
            refresh_view(app);
        }
        Some(Err(TryRecvError::Disconnected)) => {
            // El thread murió sin entregar: biblioteca vacía para no dejar la
            // UI en "Escaneando…" para siempre.
            app.library = Some(Playlist::new());
            app.scan_rx = None;
            refresh_view(app);
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
    let Some(library_index) = app.view.get(app.selected).copied() else {
        return;
    };
    let Some(track) = library_get(app, library_index) else {
        return;
    };
    let path = track.path.clone();
    let label = path.to_string_lossy().into_owned();
    match engine.play(&path) {
        Ok(()) => {
            if let Some(library) = app.library.as_mut() {
                library.set_current(Some(library_index));
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

/// 'n' / 'p': salta a la siguiente/anterior pista de la playlist visible,
/// respetando shuffle y repeat, sin salir del modo actual (en el explorador
/// además mueve la selección). No envuelve con repeat Off/One.
fn play_step(app: &mut App, engine: &mut AudioEngine, step: isize) {
    if app.shuffle && app.shuffle_order.len() != app.view.len() {
        // La lista cambió desde que se armó el orden (filtro/rescann).
        app.shuffle_order = shuffle_order(app.view.len());
    }
    let current = current_view_position(app);
    let Some(target) = next_track(
        app.view.len(),
        current,
        app.repeat,
        app.shuffle,
        &app.shuffle_order,
        step,
    ) else {
        return;
    };
    let Some(library_index) = app.view.get(target).copied() else {
        return;
    };
    let Some(track) = library_get(app, library_index) else {
        return;
    };
    let path = track.path.clone();
    let label = path.to_string_lossy().into_owned();
    match engine.play(&path) {
        Ok(()) => {
            if let Some(library) = app.library.as_mut() {
                library.set_current(Some(library_index));
            }
            app.current_path = Some(path);
            app.track_label = label;
            app.has_player = true;
            app.selected = target;
            app.error = None;
        }
        Err(err) => app.error = Some(err.to_string()),
    }
}

fn toggle_shuffle(app: &mut App) {
    app.shuffle = !app.shuffle;
    if app.shuffle {
        app.shuffle_order = shuffle_order(app.view.len());
    }
}

/// ← / →: seek relativo (±5 s) sobre la pista que está sonando. Sin pista en
/// reproducción no hace nada; el motor valida que haya sesión activa.
fn seek_by(app: &mut App, engine: &mut AudioEngine, direction: i64) {
    if !app.has_player {
        return;
    }
    let duration = engine.duration().or_else(|| current_track_duration(app));
    let target = seek_target(engine.position(), direction, duration);
    match engine.seek(target) {
        Ok(()) => app.error = None,
        Err(err) => app.error = Some(err.to_string()),
    }
}

/// Duración declarada en los metadatos de la pista actual (fallback cuando el
/// contenedor no reporta la suya).
fn current_track_duration(app: &App) -> Option<Duration> {
    let path = app.current_path.as_ref()?;
    let library = app.library.as_ref()?;
    let index = library.index_of(path)?;
    library.get(index)?.duration
}

/// Fin natural de la pista (transición Playing → Ended): decide el repeat
/// sin esperar que el usuario apriete espacio.
fn handle_natural_end(app: &mut App, engine: &mut AudioEngine) {
    match app.repeat {
        RepeatMode::Off => {}
        RepeatMode::One => {
            if let Some(path) = app.current_path.clone()
                && let Err(err) = engine.play(&path)
            {
                app.error = Some(err.to_string());
            }
        }
        RepeatMode::All => play_step(app, engine, 1),
    }
}

/// 'b' / Tab desde el reproductor: abre el explorador, apunta la selección a
/// la pista sonando y reescanea la raíz en background (si ya hay un escaneo en
/// curso, no lanza otro).
fn enter_browser(app: &mut App) {
    app.mode = Mode::Browser;
    refresh_view(app);
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

fn status_line(status: Status, repeat: RepeatMode, shuffle: bool) -> Line<'static> {
    let (text, color) = status_parts(status);
    let mut spans = vec![
        Span::styled("Estado:  ", Style::default().fg(DIM)),
        Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(repeat_shuffle_spans(repeat, shuffle));
    Line::from(spans)
}

/// Indicadores de estado compartidos: solo lo que está activo ("repeat: todo",
/// "shuffle: sí"), así en los defaults la pantalla queda limpia.
fn repeat_shuffle_spans(repeat: RepeatMode, shuffle: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if let Some(label) = repeat.label() {
        spans.push(Span::styled("  ·  ", Style::default().fg(DIM)));
        spans.push(Span::styled(
            format!("repeat: {label}"),
            Style::default().fg(ACCENT),
        ));
    }
    if shuffle {
        spans.push(Span::styled("  ·  ", Style::default().fg(DIM)));
        spans.push(Span::styled(
            "shuffle: sí",
            Style::default().fg(ACCENT),
        ));
    }
    spans
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
            Constraint::Length(4),
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
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let file_line = Line::from(vec![
        Span::styled("Archivo: ", Style::default().fg(DIM)),
        Span::styled(app.track_label.as_str(), Style::default().fg(TEXT)),
    ]);

    frame.render_widget(
        Paragraph::new(vec![
            file_line,
            status_line(app.status, app.repeat, app.shuffle),
            error_line(app),
        ]),
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

    frame.render_widget(progress_gauge(app, engine), rows[2]);
}

/// Barra de progreso de la pista: posición tomada del motor (se recalibra en
/// cada seek) sobre la duración conocida; con duración desconocida se muestra
/// la posición sin relleno.
fn progress_gauge(app: &App, engine: &AudioEngine) -> Gauge<'static> {
    let position = engine.position();
    let duration = engine.duration().or_else(|| current_track_duration(app));
    let label = match duration {
        Some(total) => format!(
            "Progreso {} / {}",
            format_duration(position),
            format_duration(total)
        ),
        None => format!("Progreso {} / —", format_duration(position)),
    };

    let mut gauge = Gauge::default().label(label);
    match duration.filter(|total| !total.is_zero()) {
        Some(total) => {
            let ratio = position.as_secs_f64() / total.as_secs_f64();
            gauge = gauge.gauge_style(Style::default().fg(ACCENT)).ratio(ratio.clamp(0.0, 1.0));
        }
        None => {
            gauge = gauge.gauge_style(Style::default().fg(DIM)).ratio(0.0);
        }
    }
    gauge
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
                Constraint::Length(4),
            ])
            .split(frame.area())
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),
                Constraint::Min(6),
                Constraint::Length(4),
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
        status_line(app.status, app.repeat, app.shuffle)
    } else {
        let mut spans = vec![
            Span::styled("Estado:  ", Style::default().fg(DIM)),
            Span::styled("Explorando", Style::default().fg(DIM)),
        ];
        spans.extend(repeat_shuffle_spans(app.repeat, app.shuffle));
        Line::from(spans)
    };

    frame.render_widget(Paragraph::new(vec![now_line, status, error_line(app)]), inner);
}

fn render_library(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let folder = folder_label(&app.root);
    let block = themed_block(Span::styled(
        library_title(app, &folder),
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

    if app.view.is_empty() {
        let (message, style) = if library.is_empty() {
            (
                " Sin archivos de audio (mp3, flac, ogg, wav) en esta carpeta ",
                Style::default().fg(DIM),
            )
        } else {
            (
                " Sin coincidencias para la búsqueda ",
                Style::default().fg(ACCENT),
            )
        };
        frame.render_widget(Paragraph::new(Line::styled(message, style)), inner);
        return;
    }

    let view_height = inner.height as usize;
    let first = scroll_offset(app.selected, app.view.len(), view_height);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(view_height);
    for (position, library_index) in app.view.iter().enumerate().skip(first) {
        if lines.len() >= view_height {
            break;
        }
        let Some(track) = library.get(*library_index) else {
            break;
        };
        let is_selected = position == app.selected;
        let is_playing = app.current_path.as_deref() == Some(track.path.as_path());
        lines.push(row_line(
            track,
            position + 1,
            is_selected,
            is_playing,
            inner.width,
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Título del bloque biblioteca: lista visible, filtro, búsqueda activa y
/// modos de reproducción (solo cuando están prendidos, para no ensuciar).
fn library_title(app: &App, folder: &str) -> String {
    let mut title = if let Some(query) = &app.search {
        format!(" Buscar: {query} · {} coincidencias ", app.view.len())
    } else {
        match &app.library {
            None => format!(" Biblioteca · {folder} "),
            Some(library) => {
                let count = if app.filter.is_some() {
                    format!("{}/{}", app.view.len(), library.len())
                } else {
                    format!("{}", library.len())
                };
                let mut base = if app.scan_rx.is_some() {
                    format!(" Biblioteca · {count} pistas · {folder} · actualizando… ")
                } else {
                    format!(" Biblioteca · {count} pistas · {folder} ")
                };
                if let Some(filter) = &app.filter {
                    base.push_str(&format!("· filtro \"{filter}\" "));
                }
                base
            }
        }
    };
    if let Some(label) = app.repeat.label() {
        title.push_str(&format!("· repeat: {label} "));
    }
    if app.shuffle {
        title.push_str("· shuffle ");
    }
    title
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

/// El pie de atajos creció a dos líneas (T6): shuffle/repeat/n-p/buscar no
/// entraban en una. Con la búsqueda abierta muestra su propio contexto.
fn footer_lines(app: &App) -> Vec<Line<'static>> {
    if app.search.is_some() {
        return vec![
            Line::from(" Escribí para filtrar por título, artista, álbum o archivo "),
            Line::from(
                " Backspace: borrar   Enter: aplicar   Esc: cancelar (lista completa) ",
            ),
        ];
    }
    match app.mode {
        Mode::Player => vec![
            Line::from(
                " espacio: play/pausa   ↑↓: volumen   ←/→: ±5s   n/p: pista sig/ant   /: buscar ",
            ),
            Line::from(" s: shuffle   r: repeat   b/Tab: biblioteca   q/Esc: salir "),
        ],
        Mode::Browser => vec![
            Line::from(
                " ↑/↓ PgUp/PgDn: mover   Enter: reproducir   n/p: pista sig/ant   /: buscar ",
            ),
            Line::from(
                " s: shuffle   r: repeat   espacio: pausa   ←/→: ±5s   Esc/Tab: volver   q/Esc: salir ",
            ),
        ],
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(
        Paragraph::new(footer_lines(app))
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
            view: Vec::new(),
            selected: 0,
            filter: None,
            search: None,
            shuffle: false,
            repeat: RepeatMode::Off,
            shuffle_order: Vec::new(),
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

        assert_eq!(app.view.len(), 0);
        assert!(view_get(&app, 0).is_none());
        app.selected = move_selection(app.selected, 5, app.view.len());
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

    fn library_with_three() -> App {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_named("alpha"),
            track_named("beta"),
            track_named("gamma"),
        ]));
        refresh_view(&mut app);
        app
    }

    #[test]
    fn refresh_view_filters_and_keeps_the_selected_track() {
        let mut app = library_with_three();
        app.selected = 1; // beta

        app.filter = Some("bet".to_string());
        refresh_view(&mut app);
        assert_eq!(app.view, vec![1], "filtro por prefijo del título");
        assert_eq!(app.selected, 0, "la seleccionada sigue visible");

        app.filter = Some("no-existe".to_string());
        refresh_view(&mut app);
        assert!(app.view.is_empty(), "sin coincidencias la lista queda vacia");
        assert_eq!(app.selected, 0, "seleccion acotada al rango vacio");

        app.filter = None;
        refresh_view(&mut app);
        assert_eq!(app.view, vec![0, 1, 2], "sin filtro vuelve la lista completa");
    }

    #[test]
    fn refresh_view_syncs_selection_to_playing_track_on_first_scan() {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_named("alpha"),
            track_named("beta"),
        ]));
        app.current_path = Some(PathBuf::from(r"C:\x\beta.mp3"));
        refresh_view(&mut app);
        assert_eq!(app.selected, 1, "seleccion en la pista sonando");
    }

    #[test]
    fn search_mode_types_into_query_enter_applies_and_esc_clears() {
        let mut app = library_with_three();

        open_search(&mut app);
        assert!(app.search.is_some(), "'/' abre la búsqueda");

        handle_search_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &mut app);
        assert_eq!(app.search.as_deref(), Some("g"));
        assert_eq!(app.view, vec![2], "filtrado en vivo");

        handle_search_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut app,
        );
        assert_eq!(app.view, vec![0, 1, 2], "backspace amplía de vuelta");

        handle_search_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), &mut app);
        handle_search_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE), &mut app);
        handle_search_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), &mut app);
        handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut app);
        assert!(app.search.is_none(), "Enter cierra el modo búsqueda");
        assert_eq!(app.filter.as_deref(), Some("bet"), "Enter confirma el filtro");
        assert_eq!(app.view, vec![1]);

        open_search(&mut app);
        assert_eq!(app.search.as_deref(), Some("bet"), "reabre con el filtro actual");
        handle_search_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut app);
        assert!(app.search.is_none());
        assert!(app.filter.is_none(), "Esc cancela y limpia el filtro");
        assert_eq!(app.view, vec![0, 1, 2], "lista completa restaurada");
    }

    #[test]
    fn shuffle_order_is_regenerated_when_view_changes() {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_named("alpha"),
            track_named("beta"),
            track_named("gamma"),
            track_named("delta"),
        ]));
        refresh_view(&mut app);

        assert!(!app.shuffle, "arranca sin shuffle");
        toggle_shuffle(&mut app);
        assert!(app.shuffle, "'s' activa el shuffle");
        assert_eq!(app.shuffle_order.len(), 4, "orden mezclado de la lista visible");

        app.filter = Some("ta".to_string()); // "ta" matchea beta y delta
        refresh_view(&mut app);
        assert_eq!(app.view, vec![1, 3]);
        assert_eq!(app.shuffle_order.len(), 2, "orden re-armado al cambiar el filtro");
        let mut sorted = app.shuffle_order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1], "sigue siendo una permutación");
    }

    #[test]
    fn footer_shows_the_new_shortcuts_in_each_mode() {
        let mut app = library_with_three();
        app.mode = Mode::Player;
        let player: String = footer_lines(&app)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        for hint in ["s: shuffle", "r: repeat", "n/p: pista", "/: buscar", "q/Esc: salir", "←/→: ±5s"] {
            assert!(player.contains(hint), "footer del player falta '{hint}': {player}");
        }

        app.mode = Mode::Browser;
        let browser: String = footer_lines(&app)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        for hint in ["s: shuffle", "r: repeat", "n/p: pista", "/: buscar", "q/Esc: salir", "←/→: ±5s"] {
            assert!(browser.contains(hint), "footer del explorador falta '{hint}': {browser}");
        }

        app.search = Some(String::new());
        let search: String = footer_lines(&app)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(search.contains("Backspace"));
        assert!(search.contains("Esc: cancelar"));
    }

    #[test]
    fn repeat_and_shuffle_show_up_in_the_status_line_only_when_active() {
        let plain = status_line(Status::Playing, RepeatMode::Off, false);
        let plain_text = line_text(&plain);
        assert!(plain_text.contains("Reproduciendo"));
        assert!(!plain_text.contains("repeat"));
        assert!(!plain_text.contains("shuffle"));

        let active = status_line(Status::Playing, RepeatMode::All, true);
        let active_text = line_text(&active);
        assert!(active_text.contains("repeat: todo"), "{active_text}");
        assert!(active_text.contains("shuffle: sí"), "{active_text}");
    }
}
