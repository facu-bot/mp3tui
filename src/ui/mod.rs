#![allow(dead_code)]

//! Interfaz de terminal: layout con ratatui y loop de eventos no bloqueante.
//! Reproducción (T3+T4) y explorador de biblioteca (T5). Con sesión de audio
//! el reproductor (info + visualizador) es el fondo permanente y el
//! explorador se superpone como panel lateral con Ctrl+Tab/b/Tab (T13); a
//! pantalla completa solo cuando no hay player (arranque por directorio).

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

use crate::audio::{AudioEngine, SPEED_STEP};
use crate::config::{self, key_labels, Theme};
use crate::playback::{filter_tracks, next_track, seek_target, shuffle_order, RepeatMode};
use crate::playlist::{move_selection, scroll_offset, Playlist, Track};
use crate::visualizer::{SpectrumAnalyzer, NUM_BANDS};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const VOLUME_STEP: f32 = 0.05;
const PAGE_STEP: isize = 10;

// Paleta compartida por todo el programa (T9): viene de la config, con
// defaults pulidos en `config::Theme::default()`. Se lee en cada estilizado
// (es un par de loads atómicos por frame, nada relevante).
fn theme() -> &'static Theme {
    &config::config().theme
}

const CURSOR: &str = "\u{258c}"; // ▌ fila seleccionada
const PLAYING: &str = "\u{25b6}"; // ▶ pista en reproducción

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Playing,
    Paused,
    Ended,
    /// No suena y además falló (apertura, decode o device): distinto de
    /// `Ended`, que es la pista que llegó a su fin natural.
    Stopped,
}

/// Cómo arranca la TUI según el argumento de CLI.
pub enum StartMode {
    /// Archivo por CLI: reproducción directa (comportamiento original).
    PlayFile(PathBuf),
    /// Sin argumento (T15: carpetas conocidas + `[library]` roots) o con
    /// carpeta: explorador de la biblioteca, agrupado por carpeta de origen.
    Browse(Vec<PathBuf>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
/// Pantalla de fondo. Con sesión de audio siempre es `Player`; el explorador
/// se superpone como panel lateral (`App::explorer_open`, T13) sin
/// reemplazarlo. `Browser` a pantalla completa es el caso sin player
/// (arranque por directorio): no hay fondo que mostrar detrás de la lista.
enum Mode {
    Player,
    Browser,
}

struct App {
    mode: Mode,
    /// Panel lateral del explorador superpuesto sobre el reproductor (T13).
    /// Solo tiene efecto en `Mode::Player`.
    explorer_open: bool,
    /// Raíces escaneadas (T14/T15): CLI, o bien las carpetas conocidas del
    /// sistema unidas a las `[library]` roots de la config.
    roots: Vec<PathBuf>,
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
    let (mode, roots, current_path, has_player, track_label) = match start {
        StartMode::PlayFile(path) => {
            let root = path
                .parent()
                .filter(|parent| parent.is_dir())
                .map(Path::to_path_buf)
                .unwrap_or_else(fallback_cwd);
            let label = path.to_string_lossy().into_owned();
            (Mode::Player, vec![root], Some(path), true, label)
        }
        StartMode::Browse(roots) => (Mode::Browser, roots, None, false, String::new()),
    };

    // La biblioteca se escanea en background: con miles de archivos el parseo
    // de tags (lofty, I/O por archivo) no puede congelar el hilo de la UI.
    let mut app = App {
        mode,
        explorer_open: false,
        roots,
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
        // Avisos de la config (T9): en la primera pantalla, además del
        // stderr que ya imprimió main. Con config limpia queda `None`.
        error: (!config::config().warnings.is_empty())
            .then(|| config::config().warnings.join(" · ")),
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
        // Error en curso del decoder o del device (archivo corrupto a
        // mitad, device desconectado): se muestra una vez y corta el
        // auto-advance de fin natural — la pista no terminó, falló. Se lee
        // ANTES de la transición de estado: el hilo de decode escribe el
        // error primero que el state, así que si el estado ya cambió, el
        // error está disponible.
        let playback_error = engine.take_error();
        if let Some(err) = &playback_error {
            app.error = Some(err.to_string());
        }

        // La transición Playing → Ended es el fin natural de la pista: ahí
        // decide repeat (uno la relanza, todo salta y envuelve).
        let previous_status = app.status;
        app.status = status_of(engine);
        if playback_error.is_none()
            && previous_status == Status::Playing
            && app.status == Status::Ended
        {
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
    } else if engine.had_playback_error() {
        // No terminó: falló. «Fin de la pista» para un archivo que nunca
        // llegó al final sería mentira (y dispararía el auto-advance).
        Status::Stopped
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
        // Panel lateral abierto (T13): la lista manda las flechas, pero los
        // atajos de reproducción (pausa, seek, n/p…) siguen activos.
        Mode::Player if app.explorer_open => handle_list_key(key, app, engine),
        Mode::Player => handle_player_key(key, app, engine),
        Mode::Browser => handle_list_key(key, app, engine),
    }
}

fn handle_player_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    let keys = &config::config().keys;
    let code = key.code;
    if config::pressed(&keys.quit, code) {
        app.should_quit = true;
    } else if config::pressed(&keys.play_pause, code) {
        toggle_play(app, engine);
    } else if config::pressed(&keys.volume_up, code) {
        engine.set_volume((engine.volume() + VOLUME_STEP).clamp(0.0, 1.0));
    } else if config::pressed(&keys.volume_down, code) {
        engine.set_volume((engine.volume() - VOLUME_STEP).clamp(0.0, 1.0));
    } else if config::pressed(&keys.seek_back, code) {
        seek_by(app, engine, -1);
    } else if config::pressed(&keys.seek_fwd, code) {
        seek_by(app, engine, 1);
    } else if config::pressed(&keys.speed_down, code) {
        step_speed(app, engine, -1.0);
    } else if config::pressed(&keys.speed_up, code) {
        step_speed(app, engine, 1.0);
    } else if config::pressed(&keys.explorer, code) {
        toggle_explorer(app);
    } else if config::pressed(&keys.next_track, code) {
        play_step(app, engine, 1);
    } else if config::pressed(&keys.prev_track, code) {
        play_step(app, engine, -1);
    } else if config::pressed(&keys.shuffle, code) {
        toggle_shuffle(app);
    } else if config::pressed(&keys.repeat, code) {
        app.repeat = app.repeat.cycle();
    } else if config::pressed(&keys.search, code) {
        toggle_explorer(app);
        open_search(app);
    }
}

/// Teclas de la lista del explorador: navegar y reproducir. Con el panel
/// lateral abierto (T13) corre en `Mode::Player` y suma los atajos de
/// reproducción (pausa, seek, n/p, velocidad…) sobre la lista; con el
/// explorador a pantalla completa (sin player) es el modo viejo de T5.
fn handle_list_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    let keys = &config::config().keys;
    let view_len = app.view.len();
    let code = key.code;
    // El orden importa: «back» antes que «quit» — Esc cierra el panel (o
    // sale del explorador sin player) aunque Esc también esté en «quit».
    // Tab no está en «back» (T14): cae en «explorer» y no-opea sin player.
    if config::pressed(&keys.back, code) {
        go_back(app);
    } else if config::pressed(&keys.explorer, code) {
        toggle_explorer(app);
    } else if config::pressed(&keys.quit, code) {
        app.should_quit = true;
    } else if config::pressed(&keys.select, code) {
        play_selected(app, engine);
    } else if config::pressed(&keys.play_pause, code) {
        toggle_play(app, engine);
    } else if config::pressed(&keys.next_track, code) {
        play_step(app, engine, 1);
    } else if config::pressed(&keys.prev_track, code) {
        play_step(app, engine, -1);
    } else if config::pressed(&keys.shuffle, code) {
        toggle_shuffle(app);
    } else if config::pressed(&keys.repeat, code) {
        app.repeat = app.repeat.cycle();
    } else if config::pressed(&keys.search, code) {
        open_search(app);
    } else if config::pressed(&keys.move_up, code) {
        app.selected = move_selection(app.selected, -1, view_len);
    } else if config::pressed(&keys.move_down, code) {
        app.selected = move_selection(app.selected, 1, view_len);
    } else if config::pressed(&keys.seek_back, code) {
        seek_by(app, engine, -1);
    } else if config::pressed(&keys.seek_fwd, code) {
        seek_by(app, engine, 1);
    } else if config::pressed(&keys.speed_down, code) {
        step_speed(app, engine, -1.0);
    } else if config::pressed(&keys.speed_up, code) {
        step_speed(app, engine, 1.0);
    } else if config::pressed(&keys.page_up, code) {
        app.selected = move_selection(app.selected, -PAGE_STEP, view_len);
    } else if config::pressed(&keys.page_down, code) {
        app.selected = move_selection(app.selected, PAGE_STEP, view_len);
    } else if config::pressed(&keys.home, code) {
        app.selected = move_selection(app.selected, isize::MIN, view_len);
    } else if config::pressed(&keys.end, code) {
        app.selected = move_selection(app.selected, isize::MAX, view_len);
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

/// Lanza el escaneo de `app.roots` en un thread aparte (T14: una o varias
/// carpetas, agrupadas por carpeta de origen). Si ya hay un escaneo en
/// curso no lanza otro (el resultado viejo sigue siendo válido).
fn start_scan(app: &mut App) {
    if app.scan_rx.is_some() {
        return;
    }
    let roots = app.roots.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let playlist = Playlist::from_dirs_scan(&roots);
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

/// Enter en la lista: reproduce la pista seleccionada. Con el explorador a
/// pantalla completa pasa al modo reproducción; con el panel lateral abierto
/// (T13) el panel se mantiene y el reproductor de fondo arranca la pista. Si
/// el archivo no se puede abrir, seguimos donde estábamos con el error visible.
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

/// `[` / `]`: baja o sube la velocidad en pasos de 0.25x (0.5x..2.0x). Sin
/// pista en reproducción no hace nada, igual que el seek.
fn step_speed(app: &App, engine: &mut AudioEngine, direction: f32) {
    if !app.has_player {
        return;
    }
    engine.set_speed(engine.speed() + direction * SPEED_STEP);
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

/// Acción de «back» (Esc, T14) desde la lista: con el panel lateral abierto
/// (T13) lo cierra y deja el reproductor de fondo; con el explorador a
/// pantalla completa vuelve al player si hay sesión y si no sale de la app.
fn go_back(app: &mut App) {
    if app.explorer_open {
        app.explorer_open = false;
    } else if app.has_player {
        app.mode = Mode::Player;
    } else {
        app.should_quit = true;
    }
}

/// 'b' / Tab / Ctrl+Tab: alterna el panel lateral del explorador (T13).
/// Al abrirlo re-sincroniza la selección con la pista sonando y reescanea
/// las raíces en background (si ya hay un escaneo en curso, no lanza otro).
/// Sin sesión de audio no hace nada: el explorador ya ocupa toda la
/// pantalla (T14: así Tab sin player no sale de la app).
fn toggle_explorer(app: &mut App) {
    if !app.has_player {
        return;
    }
    app.explorer_open = !app.explorer_open;
    if app.explorer_open {
        refresh_view(app);
        resync_selection(app);
        start_scan(app);
    }
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
        .border_style(Style::default().fg(theme().dim))
        .title(title)
}

fn accent_title(text: &str) -> Span<'static> {
    Span::styled(
        format!(" {text} "),
        Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
    )
}

fn status_parts(status: Status) -> (&'static str, Color) {
    match status {
        Status::Playing => ("Reproduciendo", theme().ok),
        Status::Paused => ("Pausado", theme().warn),
        Status::Ended => ("Fin de la pista", theme().dim),
        Status::Stopped => ("Detenido", theme().error),
    }
}

fn status_line(status: Status, repeat: RepeatMode, shuffle: bool) -> Line<'static> {
    let (text, color) = status_parts(status);
    let mut spans = vec![
        Span::styled("Estado:  ", Style::default().fg(theme().dim)),
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
        spans.push(Span::styled("  ·  ", Style::default().fg(theme().dim)));
        spans.push(Span::styled(
            format!("repeat: {label}"),
            Style::default().fg(theme().accent),
        ));
    }
    if shuffle {
        spans.push(Span::styled("  ·  ", Style::default().fg(theme().dim)));
        spans.push(Span::styled(
            "shuffle: sí",
            Style::default().fg(theme().accent),
        ));
    }
    spans
}

fn error_line(app: &App) -> Line<'static> {
    match &app.error {
        Some(err) => Line::from(Span::styled(
            format!("Error:  {err}"),
            Style::default().fg(theme().error),
        )),
        None => Line::raw(""),
    }
}

/// Áreas del reproductor: info (8 filas), visualizador (resto) y pie. Con el
/// panel lateral abierto (T13) info + visualizador se achican al 64% del
/// ancho y el explorador ocupa el 36% derecho superpuesto (más barras
/// visibles pero el espectro sigue animando); el pie crece a 5 filas (3 de
/// texto) para los atajos del panel y sigue a todo lo ancho para no cortarse.
fn player_areas(area: Rect, explorer_open: bool) -> (Rect, Rect, Rect, Option<Rect>) {
    let zones = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(4),
            Constraint::Length(if explorer_open { 5 } else { 4 }),
        ])
        .split(area);
    let footer = zones[2];
    if !explorer_open {
        return (zones[0], zones[1], footer, None);
    }
    let upper = Rect {
        height: zones[0].height + zones[1].height,
        ..zones[0]
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(64),
            Constraint::Percentage(36),
        ])
        .split(upper);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(4),
        ])
        .split(columns[0]);
    (left[0], left[1], footer, Some(columns[1]))
}

fn render_player(frame: &mut Frame<'_>, app: &App, engine: &AudioEngine) {
    let (info_area, visual_area, footer_area, panel) =
        player_areas(frame.area(), app.explorer_open);

    render_player_info(frame, info_area, app, engine);
    render_visualizer(frame, visual_area, app.analyzer.bands());
    if let Some(panel_area) = panel {
        render_explorer_panel(frame, panel_area, app);
    }
    render_footer(frame, footer_area, app);
}

/// Panel lateral del explorador (T13): la biblioteca superpuesta sobre el
/// costado derecho, con el mismo bloque, título, scroll y marcadores de
/// selección/reproducción que el explorador a pantalla completa.
fn render_explorer_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    render_library(frame, area, app);
}

fn render_player_info(frame: &mut Frame<'_>, area: Rect, app: &App, engine: &AudioEngine) {
    let block = themed_block(accent_title("mp3tui"));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // La fila de velocidad aparece solo cuando no es 1.0x: en velocidad
    // normal el panel conserva su forma y el espacio no muestra ruido.
    let show_speed = (engine.speed() - 1.0).abs() > f32::EPSILON;
    let mut constraints = vec![
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ];
    if show_speed {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(0));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let file_line = Line::from(vec![
        Span::styled("Archivo: ", Style::default().fg(theme().dim)),
        Span::styled(app.track_label.as_str(), Style::default().fg(theme().text)),
    ]);

    frame.render_widget(
        Paragraph::new(vec![
            file_line,
            status_line(app.status, app.repeat, app.shuffle),
            error_line(app),
        ]),
        rows[0],
    );

    let mut row = 1;
    if show_speed {
        frame.render_widget(Paragraph::new(speed_line(engine)), rows[row]);
        row += 1;
    }

    let volume = engine.volume().clamp(0.0, 1.0);
    let percent = (volume * 100.0).round() as u16;
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(theme().accent))
            .ratio(f64::from(volume))
            .label(format!("Volumen {percent}%")),
        rows[row],
    );

    frame.render_widget(progress_gauge(app, engine), rows[row + 1]);
}

/// Fila "Velocidad: 1.25x" del panel de info (solo se renderiza si != 1.0x).
fn speed_line(engine: &AudioEngine) -> Line<'static> {
    Line::from(vec![
        Span::styled("Velocidad: ", Style::default().fg(theme().dim)),
        Span::styled(speed_label(engine.speed()), Style::default().fg(theme().accent)),
    ])
}

fn speed_label(speed: f32) -> String {
    format!("{speed:.2}x")
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
            gauge = gauge.gauge_style(Style::default().fg(theme().accent)).ratio(ratio.clamp(0.0, 1.0));
        }
        None => {
            gauge = gauge.gauge_style(Style::default().fg(theme().dim)).ratio(0.0);
        }
    }
    gauge
}

/// Explorador a pantalla completa: solo cuando no hay sesión de audio (T13);
/// con player el explorador va como panel lateral en `render_player`. Sin
/// sesión no hay nada que visualizar, así que la lista se queda con toda la
/// altura en vez de mostrar un bloque Visualizador vacío.
fn render_browser(frame: &mut Frame<'_>, app: &App) {
    debug_assert!(
        !app.has_player,
        "con sesión de audio el explorador es panel lateral, no pantalla completa"
    );
    let zones = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(6),
            Constraint::Length(5),
        ])
        .split(frame.area());

    render_browser_header(frame, zones[0], app);
    render_library(frame, zones[1], app);
    render_footer(frame, zones[2], app);
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
            Span::styled("Sonando: ", Style::default().fg(theme().dim)),
            Span::styled(PLAYING, Style::default().fg(theme().ok)),
            Span::raw(" "),
            Span::styled(name, Style::default().fg(theme().text)),
        ]),
        None => Line::from(vec![
            Span::styled("Sonando: ", Style::default().fg(theme().dim)),
            Span::styled("—", Style::default().fg(theme().dim)),
        ]),
    };

    // Con !has_player (único caso que llega acá, T13) no hay sesión: la
    // línea de estado es siempre «Explorando» + repeat/shuffle si están.
    let mut spans = vec![
        Span::styled("Estado:  ", Style::default().fg(theme().dim)),
        Span::styled("Explorando", Style::default().fg(theme().dim)),
    ];
    spans.extend(repeat_shuffle_spans(app.repeat, app.shuffle));

    frame.render_widget(
        Paragraph::new(vec![now_line, Line::from(spans), error_line(app)]),
        inner,
    );
}

fn render_library(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let folder = library_folder_label(app);
    let block = themed_block(Span::styled(
        library_title(app, &folder),
        Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(library) = &app.library else {
        // Primer escaneo en curso: estado de carga en vez de lista vacía.
        frame.render_widget(
            Paragraph::new(Line::styled(
                " Escaneando biblioteca… ",
                Style::default().fg(theme().accent),
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
                Style::default().fg(theme().dim),
            )
        } else {
            (
                " Sin coincidencias para la búsqueda ",
                Style::default().fg(theme().accent),
            )
        };
        frame.render_widget(Paragraph::new(Line::styled(message, style)), inner);
        return;
    }

    let lines = library_rows(app, inner.width, inner.height);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// ¿La vista agrupa más de una carpeta (T14)? Con una sola carpeta en
/// pantalla los encabezados solo ensucian, así que no se dibujan.
fn view_is_grouped(app: &App) -> bool {
    let mut first: Option<&str> = None;
    for position in 0..app.view.len() {
        let Some(track) = view_get(app, position) else {
            break;
        };
        match first {
            None => first = Some(track.folder.as_str()),
            Some(folder) if folder != track.folder.as_str() => return true,
            Some(_) => {}
        }
    }
    false
}

/// Filas que ocupan `[from..=to]` (T14): una por track más un encabezado
/// cada vez que cambia de carpeta; el grupo de `from` también cuenta si es
/// distinto del track anterior, que queda fuera de la vista.
fn rows_between(app: &App, from: usize, to: usize, grouped: bool) -> usize {
    if from > to {
        return 0;
    }
    let mut rows = 0;
    let mut previous: Option<&str> = None;
    if grouped && from > 0 {
        previous = view_get(app, from - 1).map(|track| track.folder.as_str());
    }
    for position in from..=to {
        let Some(track) = view_get(app, position) else {
            break;
        };
        if grouped && previous != Some(track.folder.as_str()) {
            rows += 1;
        }
        rows += 1;
        previous = Some(track.folder.as_str());
    }
    rows
}

/// Primer track visible para que `selected` entre en `height` filas con los
/// encabezados incluidos (T14): arranca del candidato centrado clásico
/// (`scroll_offset`) y sube mientras `[start..=selected]` no entre en el
/// alto. Nunca supera `selected`, así que el cursor siempre queda a la vista.
fn visible_start(app: &App, height: usize, grouped: bool) -> usize {
    let len = app.view.len();
    if len == 0 {
        return 0;
    }
    let mut start = scroll_offset(app.selected, len, height);
    while start < app.selected
        && rows_between(app, start, app.selected, grouped) > height
    {
        start += 1;
    }
    start
}

/// Filas visibles de la biblioteca (T14): encabezados de carpeta
/// intercalados cuando la vista agrupa más de una carpeta. Los encabezados
/// no son seleccionables — `selected` sigue siendo un índice sobre `view`
/// y las flechas los saltan solas.
fn library_rows(app: &App, width: u16, height: u16) -> Vec<Line<'static>> {
    let Some(library) = &app.library else {
        return Vec::new();
    };
    let height = height as usize;
    if height == 0 {
        return Vec::new();
    }
    let grouped = view_is_grouped(app);
    let start = visible_start(app, height, grouped);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
    let mut previous: Option<&str> = None;
    if grouped && start > 0 {
        previous = view_get(app, start - 1).map(|track| track.folder.as_str());
    }
    for (position, library_index) in app.view.iter().enumerate().skip(start) {
        if lines.len() >= height {
            break;
        }
        let Some(track) = library.get(*library_index) else {
            break;
        };
        if grouped && previous != Some(track.folder.as_str()) {
            // Dejan entrar encabezado + fila; si no, se corta la vista.
            if lines.len() + 2 > height {
                break;
            }
            lines.push(folder_header_line(&track.folder, width));
            previous = Some(track.folder.as_str());
        }
        let is_selected = position == app.selected;
        let is_playing = app.current_path.as_deref() == Some(track.path.as_path());
        lines.push(row_line(
            track,
            position + 1,
            is_selected,
            is_playing,
            width,
        ));
    }
    lines
}

/// Encabezado de carpeta (T14): nombre en acento y negrita seguido de un
/// guion fino que completa el ancho de la lista.
fn folder_header_line(folder: &str, width: u16) -> Line<'static> {
    let width = width as usize;
    let label = truncate_chars(&format!(" {folder} "), width);
    let used = label.chars().count();
    let mut spans = vec![Span::styled(
        label,
        Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
    )];
    if width > used {
        spans.push(Span::styled(
            "\u{2500}".repeat(width - used),
            Style::default().fg(theme().dim),
        ));
    }
    Line::from(spans)
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

/// Carpeta que nombra a la biblioteca en el título (T14): con una raíz, su
/// nombre; con varias, cuántas hay.
fn library_folder_label(app: &App) -> String {
    match app.roots.as_slice() {
        [root] => folder_label(root),
        roots => format!("{} carpetas", roots.len()),
    }
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
        let bg = theme().select_bg;
        (
            Style::default().bg(bg).fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().ok),
            Style::default().bg(bg).fg(theme().text).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().dim),
        )
    } else {
        (
            Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().fg(theme().ok),
            Style::default().fg(theme().text),
            Style::default().fg(theme().dim),
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
    paint_bands(frame.buffer_mut(), inner, bands, theme());
}

/// Pinta las 32 barras dentro del área interior del bloque, sin widgets:
/// cada celda recibe el simbolo y el color de su altura en el gradiente.
fn paint_bands(
    buffer: &mut ratatui::buffer::Buffer,
    inner: Rect,
    bands: &[f32; NUM_BANDS],
    theme: &Theme,
) {
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
                let (red, green, blue) = spectrum_color(t, theme);
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

/// Verde (abajo) → amarillo (medio) → rojo (arriba), estilo cava: interpola
/// linealmente entre los tres colores del tema.
fn spectrum_color(t: f32, theme: &Theme) -> (u8, u8, u8) {
    fn channel(from: u8, to: u8, u: f32) -> u8 {
        (f64::from(from) + (f64::from(to) - f64::from(from)) * f64::from(u)).round() as u8
    }

    fn mix(from: Color, to: Color, u: f32) -> (u8, u8, u8) {
        let (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) = (from, to) else {
            return (0, 0, 0);
        };
        (
            channel(r0, r1, u),
            channel(g0, g1, u),
            channel(b0, b1, u),
        )
    }

    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        mix(theme.spectrum_low, theme.spectrum_mid, t * 2.0)
    } else {
        mix(theme.spectrum_mid, theme.spectrum_high, (t - 0.5) * 2.0)
    }
}

/// El pie de atajos creció a dos líneas (T6): shuffle/repeat/n-p/buscar no
/// entraban en una. Con la búsqueda abierta muestra su propio contexto.
/// Pie de atajos: se arma desde la config para que muestre siempre las
/// teclas reales aunque el usuario las remapee.
fn footer_lines(app: &App) -> Vec<Line<'static>> {
    if app.search.is_some() {
        return vec![
            Line::from(" Escribí para filtrar por título, artista, álbum o archivo "),
            Line::from(
                " Backspace: borrar   Enter: aplicar   Esc: cancelar (lista completa) ",
            ),
        ];
    }

    let keys = &config::config().keys;
    match app.mode {
        // Panel lateral abierto (T13): 3 líneas — lista, reproducción y el
        // recordatorio de que ↑/↓ son volumen solo con el panel cerrado.
        Mode::Player if app.explorer_open => vec![
            Line::from(format!(
                " {}/{}: mover   {}: reproducir   {}: buscar   {}/{}: cerrar",
                key_labels(&keys.move_up),
                key_labels(&keys.move_down),
                key_labels(&keys.select),
                key_labels(&keys.search),
                key_labels(&keys.back),
                key_labels(&keys.explorer),
            )),
            Line::from(format!(
                " {}: pausa   {}/{}: ±5s   {}/{}: vel   {}: shuffle   {}: repeat   {}: salir",
                key_labels(&keys.play_pause),
                key_labels(&keys.seek_back),
                key_labels(&keys.seek_fwd),
                key_labels(&keys.speed_down),
                key_labels(&keys.speed_up),
                key_labels(&keys.shuffle),
                key_labels(&keys.repeat),
                key_labels(&keys.quit),
            )),
            Line::from(format!(
                " {}/{}: pista sig/ant   volumen {}/{}: con el panel cerrado",
                key_labels(&keys.next_track),
                key_labels(&keys.prev_track),
                key_labels(&keys.volume_up),
                key_labels(&keys.volume_down),
            )),
        ],
        Mode::Player => vec![
            Line::from(format!(
                " {}: pausa   {}/{}: volumen   {}/{}: ±5s   \
                 {}/{}: pista sig/ant   {}: buscar",
                key_labels(&keys.play_pause),
                key_labels(&keys.volume_up),
                key_labels(&keys.volume_down),
                key_labels(&keys.seek_back),
                key_labels(&keys.seek_fwd),
                key_labels(&keys.next_track),
                key_labels(&keys.prev_track),
                key_labels(&keys.search),
            )),
            Line::from(format!(
                " {}: shuffle   {}: repeat   {}/{}: vel   {}: biblioteca   {}: salir",
                key_labels(&keys.shuffle),
                key_labels(&keys.repeat),
                key_labels(&keys.speed_down),
                key_labels(&keys.speed_up),
                key_labels(&keys.explorer),
                key_labels(&keys.quit),
            )),
        ],
        Mode::Browser => vec![
            Line::from(format!(
                " {}/{} {}/{}: mover   {}: reproducir   {}/{}: pista sig/ant   \
                 {}: buscar",
                key_labels(&keys.move_up),
                key_labels(&keys.move_down),
                key_labels(&keys.page_up),
                key_labels(&keys.page_down),
                key_labels(&keys.select),
                key_labels(&keys.next_track),
                key_labels(&keys.prev_track),
                key_labels(&keys.search),
            )),
            Line::from(format!(
                " {}: pausa   {}/{}: ±5s   {}/{}: vel   {}: shuffle   {}: repeat",
                key_labels(&keys.play_pause),
                key_labels(&keys.seek_back),
                key_labels(&keys.seek_fwd),
                key_labels(&keys.speed_down),
                key_labels(&keys.speed_up),
                key_labels(&keys.shuffle),
                key_labels(&keys.repeat),
            )),
            Line::from(format!(
                " {}: volver   {}: salir",
                key_labels(&keys.back),
                key_labels(&keys.quit),
            )),
        ],
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(
        Paragraph::new(footer_lines(app))
            .block(
                Block::bordered()
                    .border_style(Style::default().fg(theme().dim))
                    .title(accent_title("Atajos")),
            )
            .style(Style::default().fg(theme().dim))
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
            folder: "music".to_string(),
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
        let theme = Theme::default();
        assert_eq!(
            spectrum_color(0.0, &theme),
            (0x89, 0x92, 0xA7),
            "base del gradiente = spectrum_low"
        );
        assert_eq!(
            spectrum_color(0.5, &theme),
            (0x7F, 0xB4, 0xCA),
            "mitad = spectrum_mid"
        );
        assert_eq!(
            spectrum_color(1.0, &theme),
            (0xC4, 0x74, 0x6E),
            "pico = spectrum_high"
        );
    }

    #[test]
    fn paint_bands_full_bar_is_violet_at_bottom_and_red_at_top() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 1.0;
        paint_bands(&mut buffer, area, &bands, &theme);

        let bottom = &buffer[(0u16, 9u16)];
        assert_eq!(bottom.symbol(), "\u{2588}");
        assert_eq!(
            bottom.fg,
            Color::Rgb(0x89, 0x92, 0xA7),
            "violeta de la base"
        );

        let top = &buffer[(0u16, 0u16)];
        assert_eq!(top.symbol(), "\u{2588}");
        assert_eq!(top.fg, Color::Rgb(0xC4, 0x74, 0x6E), "rojo del pico");

        let mid = &buffer[(0u16, 5u16)];
        assert_eq!(mid.symbol(), "\u{2588}");
        // La barra mapea t = (alto - 1 - y) / (alto - 1); fila 5 de 10 → 4/9.
        let expected = spectrum_color(4.0 / 9.0, &theme);
        assert_eq!(mid.fg, Color::Rgb(expected.0, expected.1, expected.2));
    }

    #[test]
    fn paint_bands_zero_leaves_empty_reset_cells() {
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        paint_bands(&mut buffer, area, &[0.0f32; NUM_BANDS], &Theme::default());

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
        paint_bands(&mut buffer, area, &bands, &Theme::default());

        // 5 filas de 10: y=9..5 son barra, y=4 queda vacia.
        assert_eq!(buffer[(0u16, 5u16)].symbol(), "\u{2588}");
        assert_eq!(buffer[(0u16, 4u16)].symbol(), " ");
        assert_eq!(buffer[(0u16, 4u16)].fg, Color::Reset);
    }

    fn test_app() -> App {
        App {
            mode: Mode::Browser,
            explorer_open: false,
            roots: vec![PathBuf::from(".")],
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
            folder: "x".to_string(),
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
    fn toggle_explorer_never_starts_a_second_scan() {
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        let (tx, rx) = mpsc::channel();
        app.scan_rx = Some(rx);

        // Segundo 'b' con un escaneo ya en curso: conserva el receiver original.
        toggle_explorer(&mut app);
        assert!(app.explorer_open, "primer 'b' abre el panel");
        assert!(app.scan_rx.is_some());

        toggle_explorer(&mut app);
        assert!(!app.explorer_open, "segundo 'b' cierra el panel");
        assert!(app.scan_rx.is_some(), "cerrar tampoco toca el escaneo");

        // Sin sesión de audio el explorador ya está a pantalla completa.
        let mut bare = test_app();
        toggle_explorer(&mut bare);
        assert!(!bare.explorer_open, "sin player no hay panel que abrir");

        // El escaneo original es el que entrega el resultado.
        tx.send(crate::playlist::from_tracks_for_test(vec![track_named("alpha")]))
            .unwrap();
        poll_scan(&mut app);
        assert_eq!(app.library.as_ref().map(Playlist::len), Some(1));
        assert!(app.scan_rx.is_none());
    }

    #[test]
    fn player_areas_shrinks_the_main_column_and_keeps_the_footer_full_width() {
        let area = Rect::new(0, 0, 100, 30);

        let (info, visual, footer, panel) = player_areas(area, false);
        assert_eq!(panel, None, "panel cerrado = layout clásico");
        assert_eq!((info.x, info.width), (0, 100));
        assert_eq!((visual.x, visual.width), (0, 100));
        assert_eq!(footer.height, 4, "pie clásico con 2 líneas");

        let (info, visual, footer, panel) = player_areas(area, true);
        let panel = panel.expect("panel abierto");
        assert_eq!((panel.x, panel.width), (64, 36), "36% derecho para la lista");
        assert_eq!(
            info.x + info.width,
            64,
            "la info se achica antes del panel"
        );
        assert_eq!(
            visual.x + visual.width,
            64,
            "el visualizador sigue visible (comprimido) a la izquierda"
        );
        assert_eq!(
            visual.y,
            info.y + info.height,
            "el visualizador arranca debajo de la info"
        );
        assert_eq!(footer.width, 100, "el pie sigue a todo lo ancho");
        assert_eq!(footer.height, 5, "pie con 3 líneas para los atajos del panel");
        assert_eq!(
            info.y + info.height + visual.height,
            footer.y,
            "sin solapes entre zonas"
        );
    }

    #[test]
    fn go_back_closes_the_panel_and_only_quits_from_the_bare_explorer() {
        // Panel abierto: Esc cierra el panel, no sale de la app (T14: Esc
        // es la única tecla de «back»; Tab vive en «explorer»).
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        go_back(&mut app);
        assert!(!app.explorer_open, "back cierra el panel");
        assert!(app.mode == Mode::Player, "el player de fondo queda");
        assert!(!app.should_quit, "no sale de la app");

        // Explorador a pantalla completa sin player: sale (como antes de T13).
        let mut bare = test_app();
        go_back(&mut bare);
        assert!(bare.should_quit, "sin player Esc sale");

        // Player sin panel (estado clásico): back vuelve al modo player.
        let mut classic = test_app();
        classic.mode = Mode::Browser;
        classic.has_player = true;
        go_back(&mut classic);
        assert!(classic.mode == Mode::Player);
        assert!(!classic.should_quit);
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
        for hint in ["s: shuffle", "r: repeat", "n/p: pista", "/: buscar", "q/Esc: salir", "←/→: ±5s", "[/]: vel"] {
            assert!(player.contains(hint), "footer del player falta '{hint}': {player}");
        }

        app.mode = Mode::Browser;
        let browser: String = footer_lines(&app)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        for hint in ["s: shuffle", "r: repeat", "n/p: pista", "/: buscar", "q/Esc: salir", "←/→: ±5s", "[/]: vel"] {
            assert!(browser.contains(hint), "footer del explorador falta '{hint}': {browser}");
        }

        // Panel lateral abierto (T13): lista + playback + aviso de volumen.
        app.mode = Mode::Player;
        app.explorer_open = true;
        let panel: String = footer_lines(&app)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        for hint in [
            "↑/↓: mover",
            "Enter: reproducir",
            "Esc/b/Tab: cerrar",
            "espacio: pausa",
            "←/→: ±5s",
            "[/]: vel",
            "n/p: pista sig/ant",
            "q/Esc: salir",
            "volumen ↑/↓: con el panel cerrado",
        ] {
            assert!(panel.contains(hint), "footer del panel falta '{hint}': {panel}");
        }
        assert_eq!(
            footer_lines(&app).len(),
            3,
            "el pie con panel usa sus 3 líneas"
        );
        app.explorer_open = false;

        // Ninguna línea del pie puede exceder el ancho útil (78): si no, se
        // trunca en pantalla.
        for (mode, open) in [
            (Mode::Player, false),
            (Mode::Player, true),
            (Mode::Browser, false),
        ] {
            app.mode = mode;
            app.explorer_open = open;
            for line in footer_lines(&app) {
                assert!(
                    line_width(&line) <= 78,
                    "línea del pie demasiado ancha ({}): {}",
                    line_width(&line),
                    line_text(&line)
                );
            }
        }
        app.explorer_open = false;

        app.search = Some(String::new());
        let search: String = footer_lines(&app)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(search.contains("Backspace"));
        assert!(search.contains("Esc: cancelar"));
    }

    fn track_in(name: &str, folder: &str) -> Track {
        Track {
            path: PathBuf::from(format!(r"C:\lib\{folder}\{name}.mp3")),
            title: name.to_string(),
            artist: None,
            album: None,
            duration: None,
            folder: folder.to_string(),
        }
    }

    fn library_two_folders() -> App {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_in("alpha", "rock"),
            track_in("beta", "rock"),
            track_in("gamma", "jazz"),
            track_in("delta", "jazz"),
        ]));
        refresh_view(&mut app);
        app
    }

    #[test]
    fn library_rows_interleave_headers_and_keep_the_cursor_visible() {
        let mut app = library_two_folders();
        let lines = library_rows(&app, 40, 10);
        let texts: Vec<String> = lines.iter().map(|line| line_text(line)).collect();
        assert_eq!(texts.len(), 6, "2 encabezados + 4 pistas: {texts:?}");
        assert!(texts[0].contains("rock"), "encabezado de grupo: {:?}", texts[0]);
        assert!(
            texts[0].contains('\u{2500}'),
            "relleno de guiones: {:?}",
            texts[0]
        );
        assert_eq!(line_width(&lines[0]), 40, "el encabezado ocupa el ancho");
        assert!(!texts[0].contains('\u{258c}'), "el encabezado no selecciona");
        assert!(texts[1].contains("alpha"), "{:?}", texts[1]);
        assert!(texts[3].contains("jazz"), "segundo grupo: {:?}", texts[3]);

        // Selección al final con alto chico: el scroll cuenta los
        // encabezados y el cursor queda en la última fila visible.
        app.selected = 3;
        let lines = library_rows(&app, 40, 3);
        let texts: Vec<String> = lines.iter().map(|line| line_text(line)).collect();
        assert_eq!(texts.len(), 3, "{texts:?}");
        assert!(
            texts[2].starts_with('\u{258c}'),
            "cursor a la vista: {texts:?}"
        );
        assert!(texts[2].contains("delta"), "{:?}", texts[2]);
    }

    #[test]
    fn single_folder_library_renders_without_headers() {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_in("alpha", "rock"),
            track_in("beta", "rock"),
        ]));
        refresh_view(&mut app);

        let lines = library_rows(&app, 40, 10);
        let texts: Vec<String> = lines.iter().map(|line| line_text(line)).collect();
        assert_eq!(texts.len(), 2, "una sola carpeta no muestra encabezados");
        assert!(texts[0].contains("1. "), "fila normal: {:?}", texts[0]);
        assert!(
            texts.iter().all(|text| !text.contains('\u{2500}')),
            "sin guiones de encabezado"
        );
    }

    #[test]
    fn folder_label_shows_the_single_root_or_the_root_count() {
        let mut app = test_app();
        app.roots = vec![PathBuf::from(r"C:\musica\rock")];
        assert_eq!(library_folder_label(&app), "rock");

        app.roots = vec![PathBuf::from(r"C:\a"), PathBuf::from(r"C:\b")];
        assert_eq!(library_folder_label(&app), "2 carpetas");
    }

    #[test]
    fn speed_label_shows_two_decimals_with_x() {
        assert_eq!(speed_label(1.25), "1.25x");
        assert_eq!(speed_label(0.75), "0.75x");
        assert_eq!(speed_label(2.0), "2.00x");
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

    #[test]
    fn playback_failure_shows_detenido_not_fin_de_la_pista() {
        let failed = status_line(Status::Stopped, RepeatMode::Off, false);
        let failed_text = line_text(&failed);
        assert!(failed_text.contains("Detenido"), "{failed_text}");
        assert!(
            !failed_text.contains("Fin de la pista"),
            "{failed_text}"
        );

        let ended = status_line(Status::Ended, RepeatMode::Off, false);
        let ended_text = line_text(&ended);
        assert!(ended_text.contains("Fin de la pista"), "{ended_text}");
        assert!(!ended_text.contains("Detenido"), "{ended_text}");
    }
}
