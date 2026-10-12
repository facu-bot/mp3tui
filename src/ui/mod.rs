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
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::audio::{AudioEngine, SPEED_STEP};
use crate::config::{self, key_labels, Theme};
use crate::playback::{
    filter_tracks, natural_end_action, next_track, seek_target, shuffle_order, NextAction,
    RepeatMode, URL_REPLAY_TOO_SOON_MSG,
};
use crate::playlist::{move_selection, scroll_offset, Playlist, Track};
use crate::visualizer::{SpectrumAnalyzer, NUM_BANDS};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const VOLUME_STEP: f32 = 0.05;
const PAGE_STEP: isize = 10;

/// Bloques de octavo de celda para el visualizador sub-celda (mejora 1).
const EIGHTH_BLOCKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

// Paleta compartida por todo el programa (T9/T12): la celda runtime de
// config, que cambia en caliente al previsualizar/aplicar temas desde el menú
// (default: la del `[theme] name` cargado al arrancar). Copia de 11 `Color`
// por llamada; barato por frame.
fn theme() -> Theme {
    config::active_theme()
}

const CURSOR: &str = "\u{258c}"; // ▌ fila seleccionada
const PLAYING: &str = "\u{25b6}"; // ▶ pista en reproducción
const SEARCH_CARET: &str = "\u{258f}"; // ▏ cursor de texto en modo búsqueda

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Playing,
    Paused,
    Ended,
    /// No suena y además falló (apertura, decode o device): distinto de
    /// `Ended`, que es la pista que llegó a su fin natural.
    Stopped,
    /// Resolviendo URL de página con yt-dlp en segundo plano.
    Resolving,
    /// Conectando a URL de audio remota en segundo plano.
    Connecting,
}

/// Cómo arranca la TUI según el argumento de CLI.
pub enum StartMode {
    /// Archivo por CLI: reproducción directa (comportamiento original).
    PlayFile(PathBuf),
    /// URL por CLI: conexión/resolución directa en background con biblioteca scan_roots.
    PlayUrl {
        url: String,
        kind: crate::source::UrlKind,
        roots: Vec<PathBuf>,
    },
    /// Sin argumento (T15: carpetas conocidas + `[library]` roots) o con
    /// carpeta: explorador de la biblioteca, agrupado por carpeta de origen.
    Browse(Vec<PathBuf>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitAction {
    Local(PathBuf),
    DirectAudio(String),
    Page(String),
    Unknown(String),
    Empty,
}

pub fn classify_submit_input(input: &str) -> SubmitAction {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return SubmitAction::Empty;
    }
    match crate::source::classify(trimmed) {
        crate::source::Input::Local(path) => SubmitAction::Local(path),
        crate::source::Input::Url { url, kind } => match kind {
            crate::source::UrlKind::DirectAudio => SubmitAction::DirectAudio(url),
            crate::source::UrlKind::Page => SubmitAction::Page(url),
            crate::source::UrlKind::Unknown => SubmitAction::Unknown(url),
        },
    }
}

pub fn url_display_label(url: &str) -> String {
    let trimmed = url.trim();
    let without_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else {
        trimmed
    };

    let before_fragment = without_scheme.split('#').next().unwrap_or(without_scheme);
    let before_query = before_fragment.split('?').next().unwrap_or(before_fragment);

    let mut parts: Vec<&str> = before_query
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if parts.len() > 1 {
        // Has host and path segments: return the last non-empty path segment
        parts.pop().unwrap().to_string()
    } else if let Some(host) = parts.first() {
        host.to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn handle_prompt_char(prompt: &mut String, c: char) {
    if !c.is_control() {
        prompt.push(c);
    }
}

pub fn handle_prompt_backspace(prompt: &mut String) {
    prompt.pop();
}

pub fn handle_prompt_paste(prompt: &mut String, pasted: &str) {
    for c in pasted.chars() {
        if c == '\r' || c == '\n' {
            continue;
        }
        if !c.is_control() {
            prompt.push(c);
        }
    }
}

fn handle_ytdlp_result(
    app: &mut App,
    request_id: u64,
    result: Result<crate::source::ytdlp::Resolved, crate::source::ytdlp::YtDlpError>,
) -> Option<(String, String)> {
    if request_id != app.ytdlp_request_id {
        return None;
    }
    match result {
        Ok(resolved) => {
            app.status = Status::Connecting;
            app.track_label = resolved.title.clone();
            app.error = None;
            app.ytdlp_failed = false;
            Some((resolved.audio_url, resolved.title))
        }
        Err(err) => {
            app.status = Status::Stopped;
            app.error = Some(err.to_string());
            app.ytdlp_failed = true;
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Pantalla de fondo. Con sesión de audio siempre es `Player`; el explorador
/// se superpone como panel lateral (`App::explorer_open`, T13) sin
/// reemplazarlo. `Browser` a pantalla completa es el caso sin player
/// (arranque por directorio): no hay fondo que mostrar detrás de la lista.
enum Mode {
    Player,
    Browser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewItem {
    History(usize),
    Track(usize),
}

impl From<usize> for ViewItem {
    fn from(index: usize) -> Self {
        ViewItem::Track(index)
    }
}

impl PartialEq<usize> for ViewItem {
    fn eq(&self, other: &usize) -> bool {
        match self {
            ViewItem::Track(idx) => idx == other,
            ViewItem::History(_) => false,
        }
    }
}

impl PartialEq<ViewItem> for usize {
    fn eq(&self, other: &ViewItem) -> bool {
        other == self
    }
}

type YtDlpMessage = (
    u64,
    String,
    Result<crate::source::ytdlp::Resolved, crate::source::ytdlp::YtDlpError>,
);

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
    /// Índices de la biblioteca y del historial que se muestran (filtrados por la búsqueda).
    view: Vec<ViewItem>,
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
    url_input: Option<String>,
    ytdlp_rx: Option<Receiver<YtDlpMessage>>,
    ytdlp_request_id: u64,
    is_url: bool,
    /// URL original (nunca la resuelta) + etiqueta de la pista cuando la
    /// reproducción actual viene de una URL; `None` en reproducción local.
    /// Guarda el original para repetir (RepeatMode::One) vía `submit_url_prompt`:
    /// las URLs que resuelve yt-dlp vencen, así que una página se re-resuelve.
    current_url: Option<(String, String)>,
    ytdlp_failed: bool,
    pub history: crate::history::History,
    pub pending_history: Option<(String, String)>,
    pub history_path: Option<PathBuf>,
    /// Menú de temas abierto (T12): `Some` mientras el modal está activo.
    theme_menu: Option<ThemeMenuState>,
}

/// Estado del menú de temas (T12): selección con previsualización en vivo y
/// el tema con el que se abrió (para el marcador y para restaurar con Esc).
struct ThemeMenuState {
    /// Índice en `config::builtin_themes()`.
    selected: usize,
    /// Id del tema activo al abrir (fila «actual» marcada con ●).
    original_name: String,
    /// Colores activos al abrir: Esc los restaura.
    original_theme: Theme,
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
            Ok(terminal) => {
                let _ = crossterm::execute!(io::stdout(), crossterm::event::EnableBracketedPaste);
                Ok(Self {
                    terminal,
                    restored: false,
                })
            }
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
        let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
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
    let is_url_start = matches!(start, StartMode::PlayUrl { .. });
    let (mode, roots, current_path, has_player, track_label, start_url_info) = match start {
        StartMode::PlayFile(path) => {
            let root = path
                .parent()
                .filter(|parent| parent.is_dir())
                .map(Path::to_path_buf)
                .unwrap_or_else(fallback_cwd);
            let label = path.to_string_lossy().into_owned();
            (Mode::Player, vec![root], Some(path), true, label, None)
        }
        StartMode::PlayUrl { url, kind, roots } => {
            let label = url_display_label(&url);
            (Mode::Player, roots, None, true, label, Some((url, kind)))
        }
        StartMode::Browse(roots) => (Mode::Browser, roots, None, false, String::new(), None),
    };

    let (history, history_warn) = crate::history::load();
    let mut initial_error = (!config::config().warnings.is_empty())
        .then(|| config::config().warnings.join(" · "));
    if let Some(warn) = history_warn {
        initial_error = Some(match initial_error {
            Some(prev) => format!("{prev} · {warn}"),
            None => warn,
        });
    }

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
        // Avisos de la config (T9) e historial: en la primera pantalla, además del
        // stderr que ya imprimió main. Con config limpia queda `None`.
        error: initial_error,
        should_quit: false,
        analyzer: SpectrumAnalyzer::new(),
        url_input: None,
        ytdlp_rx: None,
        ytdlp_request_id: 0,
        is_url: is_url_start,
        current_url: None,
        ytdlp_failed: false,
        history,
        pending_history: None,
        history_path: crate::history::history_path(),
        theme_menu: None,
    };

    if let Some((url, kind)) = start_url_info {
        match kind {
            crate::source::UrlKind::DirectAudio | crate::source::UrlKind::Unknown => {
                let label = url_display_label(&url);
                start_url_playback(&mut app, engine, url.clone(), url, label);
            }
            crate::source::UrlKind::Page => {
                app.status = Status::Resolving;
                app.error = None;
                app.current_url = Some((url.clone(), url_display_label(&url)));
                app.ytdlp_request_id = 1;
                app.ytdlp_rx = Some(spawn_ytdlp_resolution(url, 1));
            }
        }
    }
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
        let playback_failed = engine.had_playback_error() || playback_error.is_some() || app.ytdlp_failed;
        let snapshot = EngineSnapshot::from_engine(engine, playback_failed);
        app.status = apply_engine_snapshot(app, snapshot);
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

        if let Some(rx) = &app.ytdlp_rx {
            match rx.try_recv() {
                Ok((req_id, orig_url, result)) => {
                    app.ytdlp_rx = None;
                    if let Some((audio_url, title)) = handle_ytdlp_result(app, req_id, result) {
                        start_url_playback(app, engine, audio_url, orig_url, title);
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    app.ytdlp_rx = None;
                }
            }
        }

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
            Event::Paste(pasted) if app.url_input.is_some() => {
                if let Some(buf) = app.url_input.as_mut() {
                    handle_prompt_paste(buf, &pasted);
                }
            }
            _ => {}
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineSnapshot {
    pub is_connecting: bool,
    pub is_playing: bool,
    pub is_paused: bool,
    pub had_playback_error: bool,
}

impl EngineSnapshot {
    pub fn from_engine(engine: &AudioEngine, app_error: bool) -> Self {
        Self {
            is_connecting: engine.is_connecting(),
            is_playing: engine.is_playing(),
            is_paused: engine.is_paused(),
            had_playback_error: engine.had_playback_error() || app_error,
        }
    }
}

pub fn compute_status(pending_resolution: bool, snapshot: EngineSnapshot) -> Status {
    if pending_resolution {
        Status::Resolving
    } else if snapshot.had_playback_error {
        Status::Stopped
    } else if snapshot.is_connecting {
        Status::Connecting
    } else if snapshot.is_paused {
        Status::Paused
    } else if snapshot.is_playing {
        Status::Playing
    } else {
        Status::Ended
    }
}

fn init_url_playback(app: &mut App, original_url: String, label: String) {
    cancel_pending_ytdlp(app);
    app.is_url = true;
    app.current_path = None;
    app.track_label = label.clone();
    app.has_player = true;
    app.status = Status::Connecting;
    app.error = None;
    app.current_url = Some((original_url.clone(), label.clone()));
    app.pending_history = Some((original_url, label));
}

fn apply_engine_snapshot(app: &mut App, snapshot: EngineSnapshot) -> Status {
    let pending_resolution = app.ytdlp_rx.is_some();
    let new_status = compute_status(pending_resolution, snapshot);
    let previous_status = app.status;
    app.status = new_status;

    if snapshot.had_playback_error || new_status == Status::Stopped {
        app.pending_history = None;
    } else if previous_status == Status::Connecting
        && new_status == Status::Playing
        && app.is_url
        && !snapshot.had_playback_error
        && let Some((original_url, label)) = app.pending_history.take()
    {
        record_history_entry(app, &original_url, &label);
    }
    new_status
}

fn compute_frame_status(
    app: &App,
    engine: &AudioEngine,
    this_frame_error: bool,
) -> Status {
    let pending_resolution = app.ytdlp_rx.is_some();
    let playback_failed = engine.had_playback_error() || this_frame_error || app.ytdlp_failed;
    let snapshot = EngineSnapshot::from_engine(engine, playback_failed);
    compute_status(pending_resolution, snapshot)
}

fn status_of(engine: &AudioEngine) -> Status {
    compute_status(false, EngineSnapshot::from_engine(engine, false))
}

fn handle_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        app.should_quit = true;
        return;
    }

    if app.url_input.is_some() {
        handle_url_prompt_key(key, app, engine);
        return;
    }

    // Modal de temas (T12): con el menú abierto las teclas van al menú.
    if app.theme_menu.is_some() {
        handle_theme_menu_key(key, app, config::persist_theme_name);
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
    } else if config::pressed(&keys.open_url, code) {
        open_url_prompt(app);
    } else if config::pressed(&keys.theme_menu, code) {
        open_theme_menu(app);
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
    } else if config::pressed(&keys.open_url, code) {
        open_url_prompt(app);
    } else if config::pressed(&keys.theme_menu, code) {
        open_theme_menu(app);
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
    match app.view.get(index)? {
        ViewItem::Track(library_index) => library_get(app, *library_index),
        ViewItem::History(_) => None,
    }
}

/// Posición de un path dentro de la lista visible.
fn position_in_view(app: &App, path: &Path) -> Option<usize> {
    let library_index = app.library.as_ref()?.index_of(path)?;
    app.view.iter().position(|item| match item {
        ViewItem::Track(index) => *index == library_index,
        ViewItem::History(_) => false,
    })
}

/// Posición de la pista sonando dentro de la lista visible (la que recorren
/// next/previous con shuffle y repeat).
fn current_view_position(app: &App) -> Option<usize> {
    let path = app.current_path.as_ref()?;
    position_in_view(app, path)
}

fn matches_history_filter(entry: &crate::history::HistoryEntry, needle: &str) -> bool {
    let trimmed = needle.trim().to_lowercase();
    if trimmed.is_empty() {
        return true;
    }
    let title = if entry.title.is_empty() {
        &entry.url
    } else {
        &entry.title
    };
    title.to_lowercase().contains(&trimmed)
}

/// Recalcula la lista visible (biblioteca filtrada por búsqueda/filtro) sin
/// perder la selección: se preserva la pista seleccionada por path, si ya no
/// existe se usa la sonando y si ninguna está en la vista se acota a rango.
/// Con shuffle activo regenera el orden mezclado para la nueva lista.
fn refresh_view(app: &mut App) {
    refresh_view_with_hint(app, None);
}

fn refresh_view_with_hint(app: &mut App, preserved_history_url: Option<&str>) {
    let previous_item = app.view.get(app.selected).copied();
    let previous_path = previous_item.and_then(|item| match item {
        ViewItem::Track(library_index) => library_get(app, library_index).map(|track| track.path.clone()),
        ViewItem::History(_) => None,
    });
    let previous_history_url = preserved_history_url
        .map(String::from)
        .or_else(|| {
            previous_item.and_then(|item| match item {
                ViewItem::History(hist_idx) => app.history.get(hist_idx).map(|entry| entry.url.clone()),
                ViewItem::Track(_) => None,
            })
        });

    let needle = app
        .search
        .as_ref()
        .or(app.filter.as_ref())
        .map(String::as_str);

    let mut new_view = Vec::new();
    for (idx, entry) in app.history.entries().iter().enumerate() {
        if matches_history_filter(entry, needle.unwrap_or_default()) {
            new_view.push(ViewItem::History(idx));
        }
    }
    if let Some(library) = &app.library {
        for track_idx in filter_tracks(library.tracks(), needle) {
            new_view.push(ViewItem::Track(track_idx));
        }
    }
    app.view = new_view;

    let new_selected = if let Some(url) = &previous_history_url {
        app.view.iter().position(|item| match item {
            ViewItem::History(idx) => app.history.get(*idx).map(|entry| &entry.url) == Some(url),
            _ => false,
        })
    } else if let Some(path) = &previous_path {
        position_in_view(app, path)
    } else {
        None
    }
    .or_else(|| current_view_position(app))
    .unwrap_or(0)
    .min(app.view.len().saturating_sub(1));
    app.selected = new_selected;

    if app.shuffle {
        let track_count = app
            .view
            .iter()
            .filter(|item| matches!(item, ViewItem::Track(_)))
            .count();
        app.shuffle_order = shuffle_order(track_count);
    }
}

/// Ubica la selección en la pista que está sonando (si está en la biblioteca).
fn resync_selection(app: &mut App) {
    if let Some(position) = current_view_position(app) {
        app.selected = position;
    }
}

fn open_url_prompt(app: &mut App) {
    app.url_input = Some(String::new());
}

fn handle_url_prompt_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
    {
        // El pegado del portapapeles llega vía Event::Paste o como secuencia de caracteres del terminal.
        return;
    }

    match key.code {
        KeyCode::Esc => {
            app.url_input = None;
        }
        KeyCode::Enter => {
            let input = app.url_input.take().unwrap_or_default();
            submit_url_prompt(input, app, engine);
        }
        KeyCode::Backspace => {
            if let Some(buf) = app.url_input.as_mut() {
                handle_prompt_backspace(buf);
            }
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(buf) = app.url_input.as_mut() {
                handle_prompt_char(buf, c);
            }
        }
        _ => {}
    }
}

fn cancel_pending_ytdlp(app: &mut App) {
    app.ytdlp_request_id = app.ytdlp_request_id.wrapping_add(1);
    app.ytdlp_rx = None;
    app.ytdlp_failed = false;
    app.pending_history = None;
}

// --- Menú de temas (T12) ----------------------------------------------------

fn open_theme_menu(app: &mut App) {
    let name = config::active_theme_name();
    let selected = config::builtin_themes()
        .iter()
        .position(|(id, _)| *id == name)
        .unwrap_or(0);
    app.theme_menu = Some(ThemeMenuState {
        selected,
        original_name: name,
        original_theme: config::active_theme(),
    });
}

fn handle_theme_menu_key(
    key: KeyEvent,
    app: &mut App,
    persist: impl FnOnce(&str) -> Result<(), String>,
) {
    let len = config::builtin_themes().len();
    match key.code {
        KeyCode::Up | KeyCode::Down => {
            let Some(menu) = app.theme_menu.as_mut() else {
                return;
            };
            let delta = if matches!(key.code, KeyCode::Up) { -1 } else { 1 };
            menu.selected = move_selection(menu.selected, delta, len);
            let (id, _) = config::builtin_themes()[menu.selected];
            // Previsualización en vivo: cambia los colores sin tocar el id.
            config::preview_theme(config::resolve_theme(id));
        }
        KeyCode::Enter => {
            let Some(menu) = app.theme_menu.take() else {
                return;
            };
            let (id, _) = config::builtin_themes()[menu.selected];
            // Aplica primero (queda en la sesión aunque falle el guardado)
            // y recién después persiste; un fallo solo deja el aviso.
            config::activate_theme(id, config::resolve_theme(id));
            if let Err(err) = persist(id) {
                app.error = Some(err);
            }
        }
        KeyCode::Esc => {
            let Some(menu) = app.theme_menu.take() else {
                return;
            };
            // Restaura el tema con el que se abrió (colores + id).
            config::activate_theme(&menu.original_name, menu.original_theme);
        }
        _ => {}
    }
}

/// Filas del modal: una por tema builtin; ● marca el tema activo al abrir y
/// la selección va resaltada con los colores vivos del tema previsualizado.
fn theme_menu_lines(menu: &ThemeMenuState) -> Vec<Line<'static>> {
    let colors = theme(); // snapshot coherente para todas las filas
    config::builtin_themes()
        .iter()
        .enumerate()
        .map(|(i, (id, _))| {
            let marker = if *id == menu.original_name { "● " } else { "  " };
            let style = if i == menu.selected {
                Style::default()
                    .bg(colors.select_bg)
                    .fg(colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(colors.text)
            };
            Line::from(Span::styled(format!(" {}{} ", marker, id), style))
        })
        .collect()
}

fn start_local_playback(app: &mut App, engine: &mut AudioEngine, path: PathBuf) -> bool {
    cancel_pending_ytdlp(app);
    app.is_url = false;
    app.current_url = None;
    let label = path.to_string_lossy().into_owned();
    match engine.play(&path) {
        Ok(()) => {
            app.current_path = Some(path);
            app.track_label = label;
            app.has_player = true;
            app.status = Status::Playing;
            app.error = None;
            true
        }
        Err(err) => {
            app.status = Status::Stopped;
            app.error = Some(err.to_string());
            false
        }
    }
}

fn record_history_entry(app: &mut App, original_url: &str, label: &str) {
    let selected_history_url = match app.view.get(app.selected) {
        Some(ViewItem::History(idx)) => app.history.get(*idx).map(|e| e.url.clone()),
        _ => None,
    };
    let clean_url = crate::history::sanitize_url(original_url);
    app.history.add(label.to_string(), clean_url);
    if let Some(path) = &app.history_path
        && let Err(err) = crate::history::save_to(&app.history, path)
    {
        app.error = Some(format!("Error al guardar historial: {err}"));
    }
    refresh_view_with_hint(app, selected_history_url.as_deref());
}

fn start_url_playback(
    app: &mut App,
    engine: &mut AudioEngine,
    playable_url: String,
    original_url: String,
    label: String,
) {
    init_url_playback(app, original_url, label);
    if let Err(err) = engine.play_url(&playable_url) {
        app.status = Status::Stopped;
        app.error = Some(err.to_string());
        app.pending_history = None;
    }
}

/// Único punto del programa que lanza el proceso yt-dlp para resolver una URL
/// de página: devuelve el receiver del resultado. Centralizarlo acá permite
/// que los tests inyecten un doble y nunca toquen el proceso ni la red.
fn spawn_ytdlp_resolution(url: String, req_id: u64) -> Receiver<YtDlpMessage> {
    let (tx, rx) = mpsc::channel();
    let url_clone = url.clone();
    thread::spawn(move || {
        let result =
            crate::source::ytdlp::resolve(&url, &crate::source::ytdlp::ProcessCommandRunner);
        let _ = tx.send((req_id, url_clone, result));
    });
    rx
}

fn submit_url_prompt(input: String, app: &mut App, engine: &mut AudioEngine) {
    submit_url_prompt_with(input, app, engine, spawn_ytdlp_resolution)
}

/// Seam inyectable de [`submit_url_prompt`]: la rama de página recibe el
/// resolver (`FnOnce(url, req_id) -> receiver`), que en producción es
/// [`spawn_ytdlp_resolution`] y en tests un doble que no lanza yt-dlp.
fn submit_url_prompt_with(
    input: String,
    app: &mut App,
    engine: &mut AudioEngine,
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
) {
    match classify_submit_input(&input) {
        SubmitAction::Empty => {}
        SubmitAction::Local(path) => {
            if path.is_file() {
                if start_local_playback(app, engine, path.clone()) {
                    app.mode = Mode::Player;
                    if let Some(lib) = app.library.as_mut() {
                        let idx = lib.index_of(&path);
                        lib.set_current(idx);
                    }
                }
            } else {
                app.error = Some(format!("'{}' no existe o no es un archivo.", path.display()));
            }
        }
        SubmitAction::DirectAudio(url) | SubmitAction::Unknown(url) => {
            let label = url_display_label(&url);
            start_url_playback(app, engine, url.clone(), url, label);
            app.mode = Mode::Player;
        }
        SubmitAction::Page(url) => {
            cancel_pending_ytdlp(app);
            app.is_url = true;
            app.current_path = None;
            app.track_label = url_display_label(&url);
            app.has_player = true;
            app.mode = Mode::Player;
            app.status = Status::Resolving;
            app.error = None;
            // La URL original queda guardada desde el input: si la pista que
            // estaba sonando termina mientras resuelve, One repite ésta.
            app.current_url = Some((url.clone(), url_display_label(&url)));

            let req_id = app.ytdlp_request_id;
            app.ytdlp_rx = Some(resolve_page(url, req_id));
        }
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
    } else if let Some(path) = app.current_path.clone() {
        start_local_playback(app, engine, path);
    }
}

/// Enter en la lista: reproduce la pista seleccionada. Con el explorador a
/// pantalla completa pasa al modo reproducción; con el panel lateral abierto
/// (T13) el panel se mantiene y el reproductor de fondo arranca la pista. Si
/// el archivo no se puede abrir, seguimos donde estábamos con el error visible.
fn play_selected(app: &mut App, engine: &mut AudioEngine) {
    let Some(item) = app.view.get(app.selected).copied() else {
        return;
    };
    match item {
        ViewItem::History(hist_idx) => {
            if let Some(entry) = app.history.get(hist_idx) {
                let url = entry.url.clone();
                submit_url_prompt(url, app, engine);
            }
        }
        ViewItem::Track(library_index) => {
            let Some(track) = library_get(app, library_index) else {
                return;
            };
            if start_local_playback(app, engine, track.path.clone()) {
                if let Some(library) = app.library.as_mut() {
                    library.set_current(Some(library_index));
                }
                app.mode = Mode::Player;
            }
        }
    }
}

/// Posiciones de la vista que son pistas de la biblioteca (lo que recorren
/// `n`/`p` y el fin natural; las entradas del historial quedan fuera).
/// Regenera el orden de shuffle si la lista cambió desde que se armó
/// (filtro/rescann).
fn track_positions_in_view(app: &mut App) -> Vec<usize> {
    let track_positions: Vec<usize> = app
        .view
        .iter()
        .enumerate()
        .filter_map(|(pos, item)| match item {
            ViewItem::Track(_) => Some(pos),
            ViewItem::History(_) => None,
        })
        .collect();
    if app.shuffle && !track_positions.is_empty() && app.shuffle_order.len() != track_positions.len()
    {
        app.shuffle_order = shuffle_order(track_positions.len());
    }
    track_positions
}

/// Índice de `track_positions` de la pista actualmente en reproducción
/// (`None` si no está en la lista, p. ej. mientras suena una URL).
fn current_track_index(app: &App, track_positions: &[usize]) -> Option<usize> {
    current_view_position(app)
        .and_then(|view_pos| track_positions.iter().position(|&p| p == view_pos))
}

/// Arranca la pista de una posición de la vista por el único camino de
/// reproducción local (limpia `is_url`/`current_url` y pendientes de URL,
/// sincroniza biblioteca y selección). Devuelve false si esa posición no es
/// una pista o el archivo no pudo abrirse.
fn play_view_position(app: &mut App, engine: &mut AudioEngine, target: usize) -> bool {
    let Some(ViewItem::Track(library_index)) = app.view.get(target).copied() else {
        return false;
    };
    let Some(track) = library_get(app, library_index) else {
        return false;
    };
    if start_local_playback(app, engine, track.path.clone()) {
        if let Some(library) = app.library.as_mut() {
            library.set_current(Some(library_index));
        }
        app.selected = target;
        true
    } else {
        false
    }
}

/// 'n' / 'p': salta a la siguiente/anterior pista de la playlist visible,
/// respetando shuffle y repeat, sin salir del modo actual (en el explorador
/// además mueve la selección). No envuelve con repeat Off/One.
fn play_step(app: &mut App, engine: &mut AudioEngine, step: isize) {
    let track_positions = track_positions_in_view(app);
    if track_positions.is_empty() {
        return;
    }
    let current = current_track_index(app, &track_positions);
    let Some(target_idx) = next_track(
        track_positions.len(),
        current,
        app.repeat,
        app.shuffle,
        &app.shuffle_order,
        step,
    ) else {
        return;
    };
    play_view_position(app, engine, track_positions[target_idx]);
}

fn toggle_shuffle(app: &mut App) {
    app.shuffle = !app.shuffle;
    if app.shuffle {
        let count = app
            .view
            .iter()
            .filter(|item| matches!(item, ViewItem::Track(_)))
            .count();
        app.shuffle_order = shuffle_order(count);
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

/// Orden pura de relanzamiento de URL que pide el fin natural con repetir
/// «una»: la URL original de la sesión. La ejecuta el dispatcher inyectable
/// de [`handle_natural_end_with`] (en producción, [`dispatch_url_replay`]);
/// los tests la capturan sin lanzar yt-dlp ni tocar la red.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlReplayRequest {
    original_url: String,
}

/// Fin natural de la pista (transición Playing → Ended): decide el repeat
/// sin esperar que el usuario apriete espacio. La regla única de avance vive
/// en [`natural_end_action`] y vale igual para pistas locales y para URL.
fn handle_natural_end(app: &mut App, engine: &mut AudioEngine) {
    handle_natural_end_with(app, engine, dispatch_url_replay)
}

/// Efecto real del replay de URL: re-lanza la URL ORIGINAL por el camino
/// normal de URL (las páginas se resuelven de nuevo con yt-dlp porque las
/// URLs resueltas vencen).
fn dispatch_url_replay(app: &mut App, engine: &mut AudioEngine, request: UrlReplayRequest) {
    submit_url_prompt(request.original_url, app, engine);
}

/// Seam inyectable del fin natural: el replay de URL pasa por `replay` en
/// lugar de disparar yt-dlp directamente, de modo que un test puede capturar
/// la orden y verificarla sin procesos ni red.
fn handle_natural_end_with(
    app: &mut App,
    engine: &mut AudioEngine,
    mut replay: impl FnMut(&mut App, &mut AudioEngine, UrlReplayRequest),
) {
    let track_positions = track_positions_in_view(app);
    let current = current_track_index(app, &track_positions);

    match natural_end_action(
        app.repeat,
        app.shuffle,
        track_positions.len(),
        current,
        &app.shuffle_order,
        app.is_url,
        engine.position(),
    ) {
        NextAction::Replay => {
            if app.is_url {
                // Re-lanza la URL ORIGINAL por el camino normal de URL: las
                // páginas se resuelven de nuevo con yt-dlp porque las URLs
                // resueltas vencen. Sin URL original guardada no hay qué
                // repetir.
                if let Some((original_url, _label)) = app.current_url.clone() {
                    replay(
                        app,
                        engine,
                        UrlReplayRequest { original_url },
                    );
                }
            } else if let Some(path) = app.current_path.clone() {
                start_local_playback(app, engine, path);
            }
        }
        NextAction::Play(target_idx) => {
            if let Some(&target) = track_positions.get(target_idx) {
                play_view_position(app, engine, target);
            }
        }
        NextAction::Stop => {}
        NextAction::StopTooSoon => {
            // URL repetir «una» que terminó bajo el mínimo: no se re-lanza
            // (evita el bucle yt-dlp + red) y se avisa en la línea de estado.
            app.error = Some(URL_REPLAY_TOO_SOON_MSG.to_string());
        }
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

    if let Some(buf) = &app.url_input {
        render_url_prompt(frame, buf);
    }

    if let Some(menu) = &app.theme_menu {
        render_theme_menu(frame, menu);
    }
}

fn render_url_prompt(frame: &mut Frame<'_>, input: &str) {
    let area = frame.area();
    let prompt_width = 72.min(area.width.saturating_sub(4)).max(20);
    let prompt_height = 5.min(area.height.saturating_sub(2)).max(3);

    let x = area.x + (area.width.saturating_sub(prompt_width)) / 2;
    let y = area.y + (area.height.saturating_sub(prompt_height)) / 2;
    let prompt_rect = Rect::new(x, y, prompt_width, prompt_height);

    frame.render_widget(Clear, prompt_rect);

    let block = Block::bordered()
        .border_style(Style::default().fg(theme().accent))
        .title(Span::styled(
            " Abrir URL / Archivo ",
            Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(prompt_rect);
    frame.render_widget(block, prompt_rect);

    let display_text = format!(" {input}▌");
    let line = Line::from(Span::styled(display_text, Style::default().fg(theme().text)));
    frame.render_widget(Paragraph::new(vec![line]), inner);
}

/// Modal centrado de temas (T12), al estilo del prompt de URL.
fn render_theme_menu(frame: &mut Frame<'_>, menu: &ThemeMenuState) {
    let area = frame.area();
    let width = 44.min(area.width.saturating_sub(4)).max(24);
    let height = (config::builtin_themes().len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(3);

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    frame.render_widget(Clear, rect);

    let block = Block::bordered()
        .border_style(Style::default().fg(theme().accent))
        .title(Span::styled(
            " Temas ",
            Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(Paragraph::new(theme_menu_lines(menu)), inner);
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

/// Mejora 3: iconos Unicode en los estados de reproducción.
fn status_parts(status: Status) -> (&'static str, Color) {
    match status {
        Status::Playing => ("▶  Reproduciendo", theme().ok),
        Status::Paused  => ("⏸  Pausado",       theme().warn),
        Status::Ended   => ("⏹  Fin de pista",  theme().dim),
        Status::Stopped => ("⏹  Detenido",      theme().error),
        Status::Resolving => ("⏳  Resolviendo con yt-dlp…", theme().accent),
        Status::Connecting => ("⏳  Conectando…", theme().accent),
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
///
/// Mejora 6: footer comprimible — si `area.height < 30`, el pie se reduce a
/// 2 filas (1 de borde + 1 de contenido) independientemente del panel.
fn player_areas(area: Rect, explorer_open: bool) -> (Rect, Rect, Rect, Option<Rect>) {
    // Umbral de compresión: con exactamente 30 filas se usa el layout completo.
    let compact = area.height < 30;
    let footer_height = if compact {
        2
    } else if explorer_open {
        5
    } else {
        4
    };

    let zones = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(4),
            Constraint::Length(footer_height),
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

    // Mejora 5: el foco visual va al panel cuando está abierto; info y
    // visualizador quedan con borde apagado en ese caso.
    let player_focused = !app.explorer_open;

    render_player_info(frame, info_area, app, engine, player_focused);
    render_visualizer(frame, visual_area, app.analyzer.bands(), player_focused);
    if let Some(panel_area) = panel {
        render_explorer_panel(frame, panel_area, app);
    }
    let compact = frame.area().height < 30;
    render_footer(frame, footer_area, app, compact);
}

/// Panel lateral del explorador (T13): la biblioteca superpuesta sobre el
/// costado derecho, con el mismo bloque, título, scroll y marcadores de
/// selección/reproducción que el explorador a pantalla completa.
fn render_explorer_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    render_library(frame, area, app, true);
}

fn render_player_info(frame: &mut Frame<'_>, area: Rect, app: &App, engine: &AudioEngine, focused: bool) {
    // Mejora 5: borde acentuado cuando el player tiene el foco.
    let border_color = if focused { theme().accent } else { theme().dim };
    let block = Block::bordered()
        .border_style(Style::default().fg(border_color))
        .title(accent_title("mp3tui"));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Mejora 8: buscar metadatos del track actual en la biblioteca.
    let track_meta: Option<(String, Option<String>)> = app.current_path.as_ref()
        .and_then(|path| app.library.as_ref()?.index_of(path))
        .and_then(|idx| app.library.as_ref()?.get(idx))
        .and_then(|track| {
            track.artist.as_ref().map(|a| (a.clone(), track.album.clone()))
        });

    // La fila de velocidad aparece solo cuando no es 1.0x: en velocidad
    // normal el panel conserva su forma y el espacio no muestra ruido.
    let show_speed = (engine.speed() - 1.0).abs() > f32::EPSILON;
    let show_meta = track_meta.is_some();

    // Bloque inicial: archivo + (meta opcional) + estado + error.
    let first_block_lines = if show_meta { 4 } else { 3 };

    let mut constraints = vec![
        Constraint::Length(first_block_lines),
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

    let mut info_lines = vec![file_line];

    // Mejora 8: línea de artista/álbum (solo si hay artista conocido).
    if let Some((artist, album_opt)) = &track_meta {
        let mut spans = vec![
            Span::styled("Meta:    ", Style::default().fg(theme().dim)),
            Span::styled(artist.clone(), Style::default().fg(theme().text)),
        ];
        if let Some(album) = album_opt {
            spans.push(Span::styled("  ·  ", Style::default().fg(theme().dim)));
            spans.push(Span::styled(album.clone(), Style::default().fg(theme().dim)));
        }
        info_lines.push(Line::from(spans));
    }

    info_lines.push(status_line(app.status, app.repeat, app.shuffle));
    info_lines.push(error_line(app));

    frame.render_widget(Paragraph::new(info_lines), rows[0]);

    let mut row = 1;
    if show_speed {
        frame.render_widget(Paragraph::new(speed_line(engine)), rows[row]);
        row += 1;
    }

    // Mejora 4: barras de volumen y progreso con caracteres ━/╸/░.
    let volume = engine.volume().clamp(0.0, 1.0);
    let percent = (volume * 100.0).round() as u16;
    let vol_suffix = format!("{percent}%");
    frame.render_widget(
        Paragraph::new(build_bar(
            "Volumen  ",
            volume as f64,
            &vol_suffix,
            theme().accent,
            theme().dim,
            inner.width as usize,
            theme().buffered,
            &[],
        )),
        rows[row],
    );

    let position = engine.position();
    let duration = engine.duration().or_else(|| current_track_duration(app));
    // Secciones precargadas de la sesión de URL actual; se consultan en cada
    // frame para que la barra se actualice mientras descarga.
    let buffered: Vec<(f32, f32)> = engine.buffered_fractions().unwrap_or_default();
    match duration.filter(|total| !total.is_zero()) {
        Some(total) => {
            let ratio = (position.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0);
            let prog_suffix = format!("{} / {}", format_duration(position), format_duration(total));
            frame.render_widget(
                Paragraph::new(build_bar(
                    "Progreso ",
                    ratio,
                    &prog_suffix,
                    theme().ok,
                    theme().dim,
                    inner.width as usize,
                    theme().buffered,
                    &buffered,
                )),
                rows[row + 1],
            );
        }
        None => {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Progreso ", Style::default().fg(theme().dim)),
                    Span::styled(format_duration(position), Style::default().fg(theme().text)),
                ])),
                rows[row + 1],
            );
        }
    }
}

/// Estado de una celda de la barra de progreso (T2 precarga).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BarCellKind {
    /// Parte ya reproducida (━).
    Played,
    /// Punta de la posición actual (╸).
    Tip,
    /// Sección descargada pero no reproducida (░ con el color `buffered`).
    Buffered,
    /// Track vacío (░ gris).
    Empty,
}

/// Mapea (ancho, fracción reproducida, secciones precargadas) a un estilo por
/// celda, de izquierda a derecha. Función pura: la barra la usa para pintar y
/// los tests la cubren sin tocar terminales.
///
/// Reglas:
/// - `played` y cada fracción `[s, e)` fuera de `[0,1]` se clampan.
/// - Un rango degenerado (e <= s tras clampar) no pinta nada.
/// - La celda `i` cubre el intervalo `[i/ancho, (i+1)/ancho)`: se pinta
///   `Buffered` si algún rango la toca (overlap no vacío). Un rango diminuto
///   no vacío pinta al menos la celda que toca; un rango vacío, ninguna.
/// - Las celdas reproducidas conservan `Played`/`Tip` aunque un rango las cubra.
/// - `ancho == 0` devuelve un vec vacío (nunca panica).
fn progress_bar_cells(width: usize, played: f64, buffered: &[(f32, f32)]) -> Vec<BarCellKind> {
    if width == 0 {
        return Vec::new();
    }
    let played = played.clamp(0.0, 1.0);
    let filled = (played * width as f64).round() as usize;
    let have_tip = filled > 0 && filled < width;

    let mut cells = Vec::with_capacity(width);
    for i in 0..width {
        if i < filled {
            cells.push(if have_tip && i == filled - 1 {
                BarCellKind::Tip
            } else {
                BarCellKind::Played
            });
            continue;
        }
        let covered = buffered.iter().any(|&(s, e)| {
            let s = (s as f64).clamp(0.0, 1.0);
            let e = (e as f64).clamp(0.0, 1.0);
            e > s && s < (i + 1) as f64 / width as f64 && e > i as f64 / width as f64
        });
        cells.push(if covered {
            BarCellKind::Buffered
        } else {
            BarCellKind::Empty
        });
    }
    cells
}

/// Mejora 4: construye una barra de progreso visual con ━, ╸ y ░.
///
/// Formato: `label [━━━━╸░░░░]  suffix`
/// `filled_ratio` en `[0.0, 1.0]`. `total_width` es el ancho disponible de
/// la fila completa (en celdas de terminal). Las secciones `buffered`
/// (fracciones `[0.0, 1.0]`) se pintan con `buffered_color` sobre el track
/// vacío, sin tocar la parte ya reproducida.
#[allow(clippy::too_many_arguments)]
fn build_bar(
    label: &str,
    filled_ratio: f64,
    suffix: &str,
    filled_color: Color,
    empty_color: Color,
    total_width: usize,
    buffered_color: Color,
    buffered: &[(f32, f32)],
) -> Line<'static> {
    // Ancho de la barra interna: descontar etiqueta + " [" + "] " + sufijo.
    let overhead = label.chars().count() + 2 + 2 + suffix.chars().count();
    let bar_chars = if total_width > overhead + 2 {
        total_width - overhead
    } else {
        0
    };

    let label_span = Span::styled(label.to_string(), Style::default().fg(empty_color));

    if bar_chars == 0 {
        // Sin espacio para la barra, mostrar solo etiqueta + sufijo.
        return Line::from(vec![
            label_span,
            Span::styled(suffix.to_string(), Style::default().fg(theme().text)),
        ]);
    }

    // Función pura: decide el estilo de cada celda (reproducida / punta /
    // precargada / vacía) y agrupa contiguos en spans.
    let kinds = progress_bar_cells(bar_chars, filled_ratio, buffered);

    let mut spans = vec![
        label_span,
        Span::styled(" [", Style::default().fg(empty_color)),
    ];
    let mut i = 0;
    while i < kinds.len() {
        let kind = kinds[i];
        let mut j = i + 1;
        while j < kinds.len() && kinds[j] == kind {
            j += 1;
        }
        let (ch, color) = match kind {
            BarCellKind::Played => ("━", filled_color),
            BarCellKind::Tip => ("╸", filled_color),
            BarCellKind::Buffered => ("░", buffered_color),
            BarCellKind::Empty => ("░", empty_color),
        };
        let count = j - i;
        spans.push(Span::styled(ch.repeat(count), Style::default().fg(color)));
        i = j;
    }

    spans.push(Span::styled("] ", Style::default().fg(empty_color)));
    spans.push(Span::styled(suffix.to_string(), Style::default().fg(theme().text)));

    Line::from(spans)
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
    render_library(frame, zones[1], app, false);
    let compact = frame.area().height < 30;
    render_footer(frame, zones[2], app, compact);
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

/// Mejora 5: `focused` controla el color del borde (acento cuando tiene foco,
/// apagado cuando el panel del explorador tiene el foco).
fn render_library(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let folder = library_folder_label(app);
    // Buscando: borde y título en el color de aviso (warn) en vez del acento
    // normal, para que el modo búsqueda se note de un vistazo y no sólo por
    // el texto "Buscar:" (que puede pasar desapercibido).
    let searching = app.search.is_some();
    let highlight = if searching {
        theme().warn
    } else if focused {
        theme().accent
    } else {
        theme().dim
    };
    let border_color = if searching {
        theme().warn
    } else if focused {
        theme().accent
    } else {
        theme().dim
    };
    let block = Block::bordered()
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            library_title(app, &folder),
            Style::default().fg(highlight).add_modifier(Modifier::BOLD),
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

fn item_group<'a>(app: &'a App, item: &ViewItem) -> Option<&'a str> {
    match item {
        ViewItem::History(_) => Some("Agregadas por URL"),
        ViewItem::Track(idx) => library_get(app, *idx).map(|t| t.folder.as_str()),
    }
}

/// ¿La vista agrupa más de una carpeta o incluye historial (T14/T1)?
/// Con una sola carpeta en pantalla (y sin historial) los encabezados solo ensucian,
/// así que no se dibujan. Si hay historial, siempre se dibuja "Agregadas por URL"
/// y el grupo correspondiente a las pistas locales.
fn view_is_grouped(app: &App) -> bool {
    let has_history = app.view.iter().any(|item| matches!(item, ViewItem::History(_)));
    if has_history {
        return true;
    }
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

/// Filas que ocupan `[from..=to]` (T14): una por item más un encabezado
/// cada vez que cambia de grupo; el grupo de `from` también cuenta si es
/// distinto del item anterior, que queda fuera de la vista.
fn rows_between(app: &App, from: usize, to: usize, grouped: bool) -> usize {
    if from > to {
        return 0;
    }
    let mut rows = 0;
    let mut previous: Option<&str> = None;
    if grouped && from > 0 {
        previous = app.view.get(from - 1).and_then(|item| item_group(app, item));
    }
    for position in from..=to {
        let Some(item) = app.view.get(position) else {
            break;
        };
        let group = item_group(app, item);
        if grouped && group.is_some() && previous != group {
            rows += 1;
        }
        rows += 1;
        previous = group;
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

fn history_row_line(
    entry: &crate::history::HistoryEntry,
    selected: bool,
    playing: bool,
    width: u16,
) -> Line<'static> {
    let cursor = if selected { CURSOR } else { " " };
    let marker = if playing { PLAYING } else { " " };
    let badge = "[URL] ";

    let budget = (width as usize).saturating_sub(2 + badge.chars().count());

    let title_raw = if entry.title.is_empty() {
        &entry.url
    } else {
        &entry.title
    };

    let title_chars = title_raw.chars().count();
    let (title_display, padding) = if title_chars <= budget {
        (title_raw.to_string(), budget - title_chars)
    } else {
        let truncated = truncate_chars(title_raw, budget);
        let pad = budget.saturating_sub(truncated.chars().count());
        (truncated, pad)
    };

    let (cursor_style, marker_style, badge_style, title_style) = if selected {
        let bg = theme().select_bg;
        (
            Style::default().bg(bg).fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().ok),
            Style::default().bg(bg).fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().text).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().fg(theme().ok),
            Style::default().fg(theme().accent),
            Style::default().fg(theme().text),
        )
    };

    let spaces = " ".repeat(padding);
    let title_with_pad = format!("{title_display}{spaces}");

    let spans = vec![
        Span::styled(cursor, cursor_style),
        Span::styled(marker, marker_style),
        Span::styled(badge, badge_style),
        Span::styled(title_with_pad, title_style),
    ];

    Line::from(spans)
}

/// Filas visibles de la biblioteca (T14/T1): encabezados de grupo
/// intercalados ("Agregadas por URL" y carpetas). Los encabezados
/// no son seleccionables — `selected` sigue siendo un índice sobre `view`
/// y las flechas los saltan solas.
fn library_rows(app: &App, width: u16, height: u16) -> Vec<Line<'static>> {
    let height = height as usize;
    if height == 0 {
        return Vec::new();
    }
    let grouped = view_is_grouped(app);
    let start = visible_start(app, height, grouped);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
    let mut previous: Option<&str> = None;
    if grouped && start > 0 {
        previous = app.view.get(start - 1).and_then(|item| item_group(app, item));
    }
    for (position, item) in app.view.iter().enumerate().skip(start) {
        if lines.len() >= height {
            break;
        }
        let group = item_group(app, item);
        if grouped && previous != group {
            if let Some(grp) = group {
                // Dejan entrar encabezado + fila; si no, se corta la vista.
                if lines.len() + 2 > height {
                    break;
                }
                lines.push(folder_header_line(grp, width));
            }
            previous = group;
        }
        let is_selected = position == app.selected;
        match item {
            ViewItem::History(hist_idx) => {
                let Some(entry) = app.history.get(*hist_idx) else {
                    break;
                };
                let is_playing = app.is_url
                    && ((!entry.title.is_empty() && app.track_label == entry.title)
                        || app.track_label == entry.url);
                lines.push(history_row_line(
                    entry,
                    is_selected,
                    is_playing,
                    width,
                ));
            }
            ViewItem::Track(library_index) => {
                let Some(track) = library_get(app, *library_index) else {
                    break;
                };
                let is_playing = app.current_path.as_deref() == Some(track.path.as_path());
                lines.push(row_line(
                    track,
                    position + 1,
                    is_selected,
                    is_playing,
                    width,
                ));
            }
        }
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
        format!(
            " Buscar: {query}{SEARCH_CARET} · {} coincidencias ",
            app.view.len()
        )
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

/// Mejora 7: una fila de la biblioteca con título en `theme().text` y
/// artista/álbum en `theme().dim`. Ocupa exactamente `width` celdas.
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

    // Parte principal: número + título.
    let main_part = format!("{number:>2}. {}", track.title);
    // Parte dim: separador + artista/álbum (solo si existe).
    let dim_part_raw: String = track
        .secondary()
        .map(|s| format!("  \u{b7} {s}"))
        .unwrap_or_default();

    let main_chars = main_part.chars().count();
    let dim_chars = dim_part_raw.chars().count();
    let total_chars = main_chars + dim_chars;

    // Calcular cuántos chars van a cada parte dentro del budget.
    let (main_display, dim_display, padding) = if total_chars <= budget {
        // Caben ambas; el relleno va al final del bloque dim (o al final del main si no hay dim).
        let pad = budget - total_chars;
        (main_part.clone(), dim_part_raw.clone(), pad)
    } else if main_chars >= budget {
        // El título solo ya supera el budget: truncar el título, sin dim.
        let truncated = truncate_chars(&main_part, budget);
        let pad = budget.saturating_sub(truncated.chars().count());
        (truncated, String::new(), pad)
    } else {
        // El título cabe; el dim se trunca a lo que queda.
        let dim_budget = budget - main_chars;
        let truncated_dim = if dim_chars > dim_budget {
            truncate_chars(&dim_part_raw, dim_budget)
        } else {
            dim_part_raw.clone()
        };
        let used = main_chars + truncated_dim.chars().count();
        let pad = budget.saturating_sub(used);
        (main_part.clone(), truncated_dim, pad)
    };

    let (cursor_style, marker_style, title_style, dim_style, duration_style) = if selected {
        let bg = theme().select_bg;
        (
            Style::default().bg(bg).fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().ok),
            Style::default().bg(bg).fg(theme().text).add_modifier(Modifier::BOLD),
            Style::default().bg(bg).fg(theme().dim),
            Style::default().bg(bg).fg(theme().dim),
        )
    } else {
        (
            Style::default().fg(theme().accent).add_modifier(Modifier::BOLD),
            Style::default().fg(theme().ok),
            Style::default().fg(theme().text),
            Style::default().fg(theme().dim),
            Style::default().fg(theme().dim),
        )
    };

    // El relleno de espacios se adjunta al último span del bloque central.
    let spaces = " ".repeat(padding);
    let dim_with_pad = format!("{dim_display}{spaces}");

    let mut spans = vec![
        Span::styled(cursor, cursor_style),
        Span::styled(marker, marker_style),
        Span::styled(main_display, title_style),
    ];
    // Si dim_with_pad está vacío (ni dim ni padding) no agregar el span.
    if !dim_with_pad.is_empty() {
        spans.push(Span::styled(dim_with_pad, dim_style));
    } else {
        // Sin parte dim: el padding va adjunto al span del título si el
        // presupuesto no fue exacto (esto puede ocurrir cuando dim_display
        // está vacío y padding > 0 pero la rama anterior lo puso en dim_with_pad).
        // La rama ya cubre esto: dim_with_pad = spaces en ese caso.
    }
    if !duration.is_empty() {
        spans.push(Span::styled(duration, duration_style));
    }

    Line::from(spans)
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

/// Mejora 5: `focused` controla el color del borde del visualizador.
fn render_visualizer(frame: &mut Frame<'_>, area: Rect, bands: &[f32; NUM_BANDS], focused: bool) {
    let border_color = if focused { theme().accent } else { theme().dim };
    let block = Block::bordered()
        .border_style(Style::default().fg(border_color))
        .title(accent_title("Visualizador"));
    let inner = block.inner(area);
    frame.render_widget(block, area);
        paint_bands(frame.buffer_mut(), inner, bands, &theme());
}

/// Pinta las 32 barras dentro del área interior del bloque, sin widgets.
/// Mejora 1: usa bloques de octavo de celda para la celda parcial superior.
/// Mejora 2: el gap entre barras está garantizado cuando el slot mide >= 2:
///   - slot >= 2 → bar_width = slot - 1 ≥ 1 (gap de 1 col)
///   - slot == 1 → bar_width = 1 (sin espacio: no hay gap posible)
///
/// Los slots se reparten proporcionalmente, así las barras ocupan todo el ancho.
fn paint_bands(
    buffer: &mut ratatui::buffer::Buffer,
    inner: Rect,
    bands: &[f32; NUM_BANDS],
    theme: &Theme,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let right = inner.x + inner.width;
    let bottom = inner.y + inner.height;
    let total = u32::from(inner.width);
    let bands_count = NUM_BANDS as u32;

    for (band, &magnitude) in bands.iter().enumerate() {
        // Los slots se reparten proporcionalmente para cubrir todo el ancho
        // (sin dejar el resto de la división entera sin pintar). Si hay menos
        // columnas que bandas, cada banda ocupa una columna y se corta al llegar.
        let (slot_start, slot_end) = if total >= bands_count {
            (
                (total * band as u32 / bands_count) as u16,
                (total * (band as u32 + 1) / bands_count) as u16,
            )
        } else {
            (band as u16, band as u16 + 1)
        };
        let x0 = inner.x + slot_start;
        if x0 >= right {
            break;
        }
        // slot >= 2 → barra = slot - 1 (1 col de gap); slot == 1 → sin gap posible.
        let bar_width = (slot_end - slot_start).saturating_sub(1).max(1);
        let width = bar_width.min(right - x0);

        // Sub-celda: altura continua → celdas completas + fracción.
        let height_f = magnitude.clamp(0.0, 1.0) * inner.height as f32;
        let full_cells = height_f.floor() as u16;
        let frac = height_f - height_f.floor();

        // Pintar las celdas completas (█ con gradiente de color).
        for row in 0..full_cells {
            let y = bottom - 1 - row;
            let t = if inner.height > 1 {
                row as f32 / (inner.height - 1) as f32
            } else {
                1.0
            };
            let (red, green, blue) = spectrum_color(t, theme);
            let color = Color::Rgb(red, green, blue);
            for dx in 0..width {
                if let Some(cell) = buffer.cell_mut((x0 + dx, y)) {
                    cell.set_symbol("█").set_fg(color);
                }
            }
        }

        // Pintar la celda parcial con el bloque de octavo correspondiente.
        if frac > 0.0 && full_cells < inner.height {
            let y = bottom - full_cells - 1;
            let idx = ((frac * 8.0) as usize).min(7);
            let symbol = EIGHTH_BLOCKS[idx];
            // Color: el mismo gradiente que tendría si fuera una celda completa.
            let t = if inner.height > 1 {
                full_cells as f32 / (inner.height - 1) as f32
            } else {
                1.0
            };
            let (red, green, blue) = spectrum_color(t, theme);
            let color = Color::Rgb(red, green, blue);
            for dx in 0..width {
                if let Some(cell) = buffer.cell_mut((x0 + dx, y)) {
                    cell.set_symbol(symbol).set_fg(color);
                }
            }
        }

        // Limpiar las celdas vacías por encima de la barra.
        let painted_rows = full_cells + if frac > 0.0 { 1 } else { 0 };
        for row in painted_rows..inner.height {
            let y = bottom - 1 - row;
            for dx in 0..width {
                if let Some(cell) = buffer.cell_mut((x0 + dx, y)) {
                    cell.set_symbol(" ").set_fg(Color::Reset);
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
///
/// Mejora 6: `compact` activo cuando `area.height < 30`; muestra una sola
/// línea con los atajos críticos.
fn footer_lines(app: &App, compact: bool) -> Vec<Line<'static>> {
    if app.url_input.is_some() {
        return vec![
            Line::from(" Pegá o escribí una URL (directa o YouTube) o ruta local "),
            Line::from(" Enter: reproducir   Esc: cancelar   (pegar con el terminal) "),
        ];
    }

    if app.theme_menu.is_some() {
        return vec![Line::from(
            " ↑/↓: previsualizar tema   Enter: guardar   Esc: cancelar ",
        )];
    }

    if app.search.is_some() {
        return vec![
            Line::from(" Escribí para filtrar por título, artista, álbum o archivo "),
            Line::from(
                " Backspace: borrar   Enter: aplicar   Esc: cancelar (lista completa) ",
            ),
        ];
    }

    if compact {
        let keys = &config::config().keys;
        return vec![Line::from(format!(
            " {}: pausa   {}: salir",
            key_labels(&keys.play_pause),
            key_labels(&keys.quit),
        ))];
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
                " {}: pausa   {}/{}: ±5s   {}: shuffle   {}: repeat   {}: temas   {}: salir",
                key_labels(&keys.play_pause),
                key_labels(&keys.seek_back),
                key_labels(&keys.seek_fwd),
                key_labels(&keys.shuffle),
                key_labels(&keys.repeat),
                key_labels(&keys.theme_menu),
                key_labels(&keys.quit),
            )),
            Line::from(format!(
                " {}/{}: pista sig/ant   volumen {}/{}: con el panel cerrado   {}/{}: vel",
                key_labels(&keys.next_track),
                key_labels(&keys.prev_track),
                key_labels(&keys.volume_up),
                key_labels(&keys.volume_down),
                key_labels(&keys.speed_down),
                key_labels(&keys.speed_up),
            )),
        ],
        Mode::Player => vec![
            Line::from(format!(
                " {}: pausa   {}/{}: volumen   {}/{}: ±5s   \
                 {}/{}: pista   {}: buscar   {}: URL",
                key_labels(&keys.play_pause),
                key_labels(&keys.volume_up),
                key_labels(&keys.volume_down),
                key_labels(&keys.seek_back),
                key_labels(&keys.seek_fwd),
                key_labels(&keys.next_track),
                key_labels(&keys.prev_track),
                key_labels(&keys.search),
                key_labels(&keys.open_url),
            )),
            Line::from(format!(
                " {}: shuffle   {}: repeat   {}/{}: vel   {}: temas   {}: biblio   {}: salir",
                key_labels(&keys.shuffle),
                key_labels(&keys.repeat),
                key_labels(&keys.speed_down),
                key_labels(&keys.speed_up),
                key_labels(&keys.theme_menu),
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
                " {}: URL   {}: temas   {}: volver   {}: salir",
                key_labels(&keys.open_url),
                key_labels(&keys.theme_menu),
                key_labels(&keys.back),
                key_labels(&keys.quit),
            )),
        ],
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App, compact: bool) {
    frame.render_widget(
        Paragraph::new(footer_lines(app, compact))
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

        // 0.5 * 10 = 5.0 → full_cells = 5, frac = 0.0 → no partial cell.
        // Rows y=9..5 (bottom 5) are full █, y=4 is empty.
        assert_eq!(buffer[(0u16, 5u16)].symbol(), "\u{2588}");
        assert_eq!(buffer[(0u16, 4u16)].symbol(), " ");
        assert_eq!(buffer[(0u16, 4u16)].fg, Color::Reset);
    }

    #[test]
    fn paint_bands_partial_cell_uses_eighth_block() {
        // magnitude = 0.45, inner.height = 10
        // height_f = 0.45 * 10 = 4.5
        // full_cells = 4, frac = 0.5
        // idx = (0.5 * 8.0) as usize = 4 → EIGHTH_BLOCKS[4] = "▅"
        // partial cell y = bottom - full_cells - 1 = 10 - 4 - 1 = 5
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 0.45;
        paint_bands(&mut buffer, area, &bands, &Theme::default());

        let bottom = area.y + area.height; // = 10
        let partial_y = bottom - 4 - 1; // = 5
        assert_eq!(
            buffer[(0u16, partial_y)].symbol(),
            "▅",
            "frac=0.5 → EIGHTH_BLOCKS[4] = ▅"
        );
        // The full cells below the partial cell should be █
        assert_eq!(buffer[(0u16, bottom - 1)].symbol(), "█", "bottom full cell");
        // The cell above the partial cell should be empty
        assert_eq!(buffer[(0u16, partial_y - 1)].symbol(), " ", "above partial = empty");
    }

    #[test]
    fn paint_bands_spreads_bars_across_full_width_when_not_multiple_of_bands() {
        // 113 columnas / 32 bandas: la división entera dejaba ~17 columnas vacías.
        let width = 113u16;
        let area = Rect::new(0, 0, width, 3);
        let mut buffer = Buffer::empty(area);
        paint_bands(&mut buffer, area, &[1.0f32; NUM_BANDS], &Theme::default());

        let rightmost = (0..width)
            .rev()
            .find(|&x| buffer[(x, 2u16)].symbol() == "\u{2588}")
            .expect("alguna columna pintada");
        // La última barra deja a lo sumo 1 columna de gap antes del borde.
        assert!(
            rightmost >= width - 2,
            "las barras deben llegar al borde derecho, última columna pintada: {rightmost}"
        );
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
            url_input: None,
            ytdlp_rx: None,
            ytdlp_request_id: 0,
            is_url: false,
            current_url: None,
            ytdlp_failed: false,
            history: crate::history::History::default(),
            pending_history: None,
            history_path: Some(test_temp_history_path()),
            theme_menu: None,
        }
    }

    static TEST_HIST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn test_temp_history_path() -> PathBuf {
        let id = TEST_HIST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("mp3tui_test_hist_{}_{}.toml", std::process::id(), id))
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
        let player: String = footer_lines(&app, false)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        for hint in ["s: shuffle", "r: repeat", "n/p: pista", "/: buscar", "q/Esc: salir", "←/→: ±5s", "[/]: vel"] {
            assert!(player.contains(hint), "footer del player falta '{hint}': {player}");
        }

        app.mode = Mode::Browser;
        let browser: String = footer_lines(&app, false)
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
        let panel: String = footer_lines(&app, false)
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
            footer_lines(&app, false).len(),
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
            for line in footer_lines(&app, false) {
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
        let search: String = footer_lines(&app, false)
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
            !failed_text.contains("Fin de pista"),
            "stopped no debe mostrar fin de pista: {failed_text}"
        );

        let ended = status_line(Status::Ended, RepeatMode::Off, false);
        let ended_text = line_text(&ended);
        assert!(ended_text.contains("Fin de pista"), "{ended_text}");
        assert!(!ended_text.contains("Detenido"), "{ended_text}");
    }

    #[test]
    fn test_url_display_label() {
        assert_eq!(
            url_display_label("https://example.com/audio/song.mp3"),
            "song.mp3"
        );
        assert_eq!(
            url_display_label("http://audio.example.org/path/stream.m4a?token=1#part"),
            "stream.m4a"
        );
        assert_eq!(
            url_display_label("https://radio.example.com/"),
            "radio.example.com"
        );
        assert_eq!(
            url_display_label("http://localhost:8080"),
            "localhost:8080"
        );
    }

    #[test]
    fn test_classify_submit_input() {
        assert_eq!(classify_submit_input(""), SubmitAction::Empty);
        assert_eq!(classify_submit_input("   "), SubmitAction::Empty);
        assert_eq!(
            classify_submit_input("https://example.com/audio.mp3"),
            SubmitAction::DirectAudio("https://example.com/audio.mp3".to_string())
        );
        assert_eq!(
            classify_submit_input("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            SubmitAction::Page("https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            classify_submit_input("https://example.com/unknown_stream"),
            SubmitAction::Unknown("https://example.com/unknown_stream".to_string())
        );
    }

    #[test]
    fn test_prompt_editing_and_paste() {
        let mut prompt = String::new();
        handle_prompt_char(&mut prompt, 'h');
        handle_prompt_char(&mut prompt, 't');
        handle_prompt_char(&mut prompt, 't');
        handle_prompt_char(&mut prompt, 'p');
        assert_eq!(prompt, "http");

        handle_prompt_backspace(&mut prompt);
        assert_eq!(prompt, "htt");

        handle_prompt_paste(&mut prompt, "p://example.com/song.mp3\r\n");
        assert_eq!(prompt, "http://example.com/song.mp3");
    }

    #[test]
    fn test_status_text_transitions() {
        let (resolving_text, _) = status_parts(Status::Resolving);
        assert!(
            resolving_text.contains("Resolviendo con yt-dlp…"),
            "texto: {resolving_text}"
        );

        let (connecting_text, _) = status_parts(Status::Connecting);
        assert!(
            connecting_text.contains("Conectando…"),
            "texto: {connecting_text}"
        );
    }

    #[test]
    fn test_footer_shows_open_url_key() {
        let app = test_app();
        let lines = footer_lines(&app, false);
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join(" ");
        assert!(
            joined.contains("u: URL") || joined.contains("u: url") || joined.contains("URL"),
            "footer debe mencionar atajo de URL: {joined}"
        );
    }

    // -------------------------------------------------------------------------
    // T12: menú de temas
    // -------------------------------------------------------------------------

    static THEME_MENU_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ThemeMenuTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        name: String,
        theme: Theme,
    }

    impl Drop for ThemeMenuTestGuard {
        fn drop(&mut self) {
            config::activate_theme(&self.name, self.theme.clone());
        }
    }

    /// Serializa los tests que mutan el tema global y restaura el estado
    /// activo al salir (incluso si el test falla).
    fn theme_menu_test_setup() -> ThemeMenuTestGuard {
        let lock = THEME_MENU_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ThemeMenuTestGuard {
            _lock: lock,
            name: config::active_theme_name(),
            theme: config::active_theme(),
        }
    }

    fn theme_menu_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn theme_menu_open_snapshots_current_theme() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();

        open_theme_menu(&mut app);

        let menu = app
            .theme_menu
            .as_ref()
            .expect("open_theme_menu debe abrir el menú");
        let current = config::active_theme_name();
        let expected = config::builtin_themes()
            .iter()
            .position(|(id, _)| *id == current)
            .expect("el tema activo es un id builtin");
        assert_eq!(menu.selected, expected, "selección = tema actual");
        assert_eq!(menu.original_name, current, "guarda el id original");
        assert_eq!(
            menu.original_theme,
            config::active_theme(),
            "guarda los colores originales para Esc"
        );
        // Abrir no cambia el tema.
        assert_eq!(config::active_theme_name(), current, "solo previsualiza al mover");
    }

    #[test]
    fn theme_menu_renders_inside_tiny_terminals() {
        // Más angosta que 24 columnas o más baja que 3 filas: el modal no puede
        // salirse del frame (ratatui entra en pánico al escribir fuera del buffer).
        let menu = ThemeMenuState {
            selected: 0,
            original_name: config::active_theme_name(),
            original_theme: config::active_theme(),
        };
        for (w, h) in [(10, 2), (23, 10), (40, 2), (1, 1), (0, 0)] {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
            terminal
                .draw(|frame| render_theme_menu(frame, &menu))
                .unwrap_or_else(|e| panic!("{w}x{h}: {e}"));
        }
    }

    #[test]
    fn theme_menu_navigation_previews_live() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        app.theme_menu = Some(ThemeMenuState {
            selected: 0,
            original_name: config::active_theme_name(),
            original_theme: config::active_theme(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        let menu = app
            .theme_menu
            .as_ref()
            .expect("el menú sigue abierto al navegar");
        assert_eq!(menu.selected, 1, "↓ mueve la selección");
        let second_id = config::builtin_themes()[1].0;
        assert_eq!(
            config::active_theme(),
            config::resolve_theme(second_id),
            "↓ previsualiza la paleta en vivo"
        );

        handle_theme_menu_key(theme_menu_key(KeyCode::Up), &mut app, |_| Ok(()));
        assert_eq!(
            app.theme_menu.as_ref().expect("menú abierto").selected,
            0,
            "↑ vuelve a la primera fila"
        );
        assert_eq!(
            config::active_theme(),
            config::resolve_theme(config::builtin_themes()[0].0),
            "↑ restaura la previsualización anterior"
        );
    }

    #[test]
    fn theme_menu_enter_applies_and_persists_injected_path() {
        let _setup = theme_menu_test_setup();
        let path = std::env::temp_dir()
            .join(format!("mp3tui_theme_menu_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut app = test_app();
        let last = config::builtin_themes().len() - 1;
        app.theme_menu = Some(ThemeMenuState {
            selected: last,
            original_name: config::active_theme_name(),
            original_theme: config::active_theme(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |id| {
            config::persist_theme_name_at(&path, id)
        });

        let expected = config::builtin_themes()[last].0;
        assert!(app.theme_menu.is_none(), "Enter cierra el menú");
        assert_eq!(
            config::active_theme_name(),
            expected,
            "el id aplicado queda activo"
        );
        assert_eq!(
            config::active_theme(),
            config::resolve_theme(expected),
            "la paleta aplicada queda activa"
        );
        assert!(app.error.is_none(), "sin error: {:?}", app.error);
        let text = std::fs::read_to_string(&path).expect("archivo persistido");
        assert_eq!(
            text,
            format!("[theme]\nname = \"{expected}\"\n"),
            "config.toml temporal con el name nuevo"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn theme_menu_enter_persist_failure_keeps_applied_theme() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        app.theme_menu = Some(ThemeMenuState {
            selected: 2,
            original_name: config::active_theme_name(),
            original_theme: config::active_theme(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |_| {
            Err("no se pudo guardar el tema: disco lleno".to_string())
        });

        let expected = config::builtin_themes()[2].0;
        assert!(app.theme_menu.is_none(), "Enter cierra igual");
        assert_eq!(
            config::active_theme_name(),
            expected,
            "el tema queda aplicado en la sesión pese al fallo"
        );
        assert_eq!(config::active_theme(), config::resolve_theme(expected));
        let err = app.error.as_deref().expect("error visible en la app");
        assert!(!err.is_empty(), "mensaje corto no vacío");
    }

    #[test]
    fn theme_menu_escape_restores_previous_theme() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        let before_name = config::active_theme_name();
        let before_theme = config::active_theme();
        app.theme_menu = Some(ThemeMenuState {
            selected: 0,
            original_name: before_name.clone(),
            original_theme: before_theme.clone(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| {
            Ok(())
        });
        handle_theme_menu_key(theme_menu_key(KeyCode::Esc), &mut app, |_| {
            panic!("Esc no debe persistir")
        });

        assert!(app.theme_menu.is_none(), "Esc cierra el menú");
        assert_eq!(
            config::active_theme(),
            before_theme,
            "Esc restaura los colores originales"
        );
        assert_eq!(
            config::active_theme_name(),
            before_name,
            "Esc restaura el id original"
        );
    }

    #[test]
    fn theme_menu_key_opens_menu_from_list() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(engine) => engine,
            Err(_) => return,
        };

        handle_list_key(
            theme_menu_key(KeyCode::Char('t')),
            &mut app,
            &mut engine,
        );

        assert!(
            app.theme_menu.is_some(),
            "'t' por defecto abre el menú desde la lista"
        );
    }

    #[test]
    fn handle_key_routes_to_theme_menu_when_open() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(engine) => engine,
            Err(_) => return,
        };
        app.theme_menu = Some(ThemeMenuState {
            selected: 1,
            original_name: config::active_theme_name(),
            original_theme: config::active_theme(),
        });

        handle_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);

        assert_eq!(
            app.theme_menu
                .as_ref()
                .expect("el menú sigue abierto")
                .selected,
            2,
            "con el menú abierto las teclas van al menú, no a la lista"
        );
    }

    #[test]
    fn footer_shows_theme_menu_key() {
        let app = test_app();
        let lines = footer_lines(&app, false);
        let joined: String =
            lines.iter().map(line_text).collect::<Vec<_>>().join(" ");
        assert!(
            joined.contains("t: temas"),
            "footer debe mencionar el atajo del menú de temas: {joined}"
        );
    }

    #[test]
    fn theme_menu_lines_list_all_themes_and_mark_current() {
        let _setup = theme_menu_test_setup();
        let menu = ThemeMenuState {
            selected: 3,
            original_name: "nord".to_string(),
            original_theme: config::active_theme(),
        };

        let lines = theme_menu_lines(&menu);
        let texts: Vec<String> =
            lines.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(texts.len(), 7, "una fila por tema: {texts:?}");
        for (id, _) in config::builtin_themes() {
            assert!(
                texts.iter().any(|t| t.contains(id)),
                "falta la fila de {id}: {texts:?}"
            );
        }
        let nord_row = config::builtin_themes()
            .iter()
            .position(|(id, _)| *id == "nord")
            .expect("nord builtin");
        assert!(
            texts[nord_row].contains('●'),
            "● marca el tema actual: {:?}",
            texts[nord_row]
        );
        assert!(
            !texts[0].contains('●'),
            "solo la fila actual lleva el marcador: {:?}",
            texts[0]
        );
        let selected = &lines[3].spans[0];
        assert_eq!(
            selected.style.bg,
            Some(config::active_theme().select_bg),
            "la fila seleccionada va resaltada"
        );
    }

    #[test]
    fn test_stale_resolution_ignored_by_request_id() {
        let mut app = test_app();
        app.ytdlp_request_id = 42;
        app.status = Status::Resolving;
        app.error = Some("error previo".to_string());

        // Stale result (request_id 41 != 42) must be ignored
        let stale_result = Ok(crate::source::ytdlp::Resolved {
            title: "Stale Song".to_string(),
            audio_url: "https://example.com/stale.m4a".to_string(),
        });
        let action = handle_ytdlp_result(&mut app, 41, stale_result);
        assert_eq!(action, None, "stale result must be discarded");
        assert_eq!(app.status, Status::Resolving);
        assert_eq!(app.error.as_deref(), Some("error previo"));

        // Matching result (request_id 42 == 42) must be accepted
        let matching_result = Ok(crate::source::ytdlp::Resolved {
            title: "Fresh Song".to_string(),
            audio_url: "https://example.com/fresh.m4a".to_string(),
        });
        let action = handle_ytdlp_result(&mut app, 42, matching_result);
        assert_eq!(
            action,
            Some(("https://example.com/fresh.m4a".to_string(), "Fresh Song".to_string()))
        );
        assert_eq!(app.status, Status::Connecting);
        assert_eq!(app.track_label, "Fresh Song");
        assert_eq!(app.error, None);
    }

    #[test]
    fn test_compute_status_transitions() {
        let snapshot_stopped = EngineSnapshot {
            is_connecting: false,
            is_playing: false,
            is_paused: false,
            had_playback_error: false,
        };
        assert_eq!(
            compute_status(true, snapshot_stopped),
            Status::Resolving,
            "must stay Resolving while yt-dlp is pending even if engine is stopped"
        );

        let snapshot_connecting = EngineSnapshot {
            is_connecting: true,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        assert_eq!(
            compute_status(true, snapshot_connecting),
            Status::Resolving,
            "must stay Resolving while yt-dlp is pending even if engine is connecting"
        );

        assert_eq!(
            compute_status(false, snapshot_connecting),
            Status::Connecting,
            "must stay Connecting while engine.is_connecting is true"
        );

        let snapshot_playing = EngineSnapshot {
            is_connecting: false,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        assert_eq!(
            compute_status(false, snapshot_playing),
            Status::Playing,
            "must transition to Playing once connection finishes"
        );

        let snapshot_failed = EngineSnapshot {
            is_connecting: false,
            is_playing: false,
            is_paused: false,
            had_playback_error: true,
        };
        assert_eq!(
            compute_status(false, snapshot_failed),
            Status::Stopped,
            "connection or playback failure must result in Stopped"
        );
    }

    #[test]
    fn test_url_track_natural_end_with_repeat_all_advances_to_library() {
        let mut app = test_app();
        app.is_url = true;
        app.current_url = Some((
            "https://example.com/audio.mp3".to_string(),
            "audio.mp3".to_string(),
        ));
        app.status = Status::Playing;
        app.repeat = RepeatMode::All;
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t1_urlall1_{}.wav", std::process::id()));
        let t2 = dir.join(format!("mp3tui_t1_urlall2_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);
        let _ = std::fs::write(&t2, &wav);
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(t1.clone(), "local_1"),
            track_with_path(t2.clone(), "local_2"),
        ]));
        refresh_view(&mut app);
        app.selected = 0;
        app.track_label = "https://example.com/audio.mp3".to_string();

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                let _ = std::fs::remove_file(&t1);
                let _ = std::fs::remove_file(&t2);
                return;
            }
        };

        handle_natural_end(&mut app, &mut engine);

        assert!(
            !app.is_url,
            "URL terminada con repeat All debe entrar a la biblioteca local"
        );
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t1),
            "debe avanzar a la primera pista local"
        );
        assert!(app.current_url.is_none(), "la sesión ya no es de URL");
        assert!(
            app.error.is_none(),
            "no debe fallar al arrancar la pista local: {:?}",
            app.error
        );

        let _ = std::fs::remove_file(&t1);
        let _ = std::fs::remove_file(&t2);
    }

    #[test]
    fn test_non_playback_app_error_does_not_mask_natural_end() {
        let mut app = test_app();
        app.error = Some("config: advertencia de configuración".to_string());
        app.status = Status::Playing;

        let engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        // Engine is not playing, not paused, and has no playback error (ended normally).
        let status = compute_frame_status(&app, &engine, false);
        assert_eq!(
            status,
            Status::Ended,
            "non-playback app.error must not mask natural end into Stopped"
        );
    }

    fn make_test_wav(num_samples: usize) -> Vec<u8> {
        let sample_rate = 44100u32;
        let channels = 1u16;
        let bits_per_sample = 16u16;
        let byte_rate = sample_rate * (channels as u32) * (bits_per_sample as u32 / 8);
        let block_align = channels * (bits_per_sample / 8);
        let data_len = (num_samples * 2) as u32;
        let riff_len = 36 + data_len;

        let mut wav = Vec::with_capacity(44 + num_samples * 2);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&riff_len.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bits_per_sample.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..num_samples {
            let sample = ((i as f32 / 10.0).sin() * 10000.0) as i16;
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    /// Servidor HTTP local de un solo uso que sirve `wav` y devuelve la URL
    /// (`http://127.0.0.1:<puerto>/<filename>`). Permite que las pruebas de la
    /// ruta de URL dialen SOLO loopback, nunca un host externo. El thread
    /// acepta reconexiones del precargador y termina con el proceso.
    fn local_wav_url(filename: &str, wav: &[u8]) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");
        let wav = wav.to_vec();
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else {
                    break;
                };
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    wav.len()
                );
                if socket.write_all(header.as_bytes()).is_err() {
                    continue;
                }
                let _ = socket.write_all(&wav);
            }
        });
        format!("http://{addr}/{filename}")
    }

    fn track_with_path(path: PathBuf, name: &str) -> Track {
        Track {
            path,
            title: name.to_string(),
            artist: None,
            album: None,
            duration: None,
            folder: "x".to_string(),
        }
    }

    #[test]
    fn test_local_playback_clears_is_url_and_allows_natural_end_advance() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t61_adv1_{}.wav", std::process::id()));
        let t2 = dir.join(format!("mp3tui_t61_adv2_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);
        let _ = std::fs::write(&t2, &wav);

        let trk1 = track_with_path(t1.clone(), "track_1");
        let trk2 = track_with_path(t2.clone(), "track_2");

        app.library = Some(crate::playlist::from_tracks_for_test(vec![trk1, trk2]));
        refresh_view(&mut app);
        app.repeat = RepeatMode::All;

        // 1. Simulate prior URL playback
        app.is_url = true;
        app.track_label = "https://example.com/audio.mp3".to_string();
        app.status = Status::Playing;

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                let _ = std::fs::remove_file(&t1);
                let _ = std::fs::remove_file(&t2);
                return;
            }
        };

        // 2. User plays first library track via normal Enter path (play_selected)
        app.selected = 0;
        play_selected(&mut app, &mut engine);

        // Before fix: play_selected left app.is_url = true!
        assert!(!app.is_url, "playing local track must clear is_url");
        assert_eq!(app.current_path.as_ref(), Some(&t1));

        // 3. Track ends naturally: should advance to track_2
        handle_natural_end(&mut app, &mut engine);
        assert_eq!(app.selected, 1, "natural end must auto-advance local track");
        assert_eq!(app.current_path.as_ref(), Some(&t2));

        let _ = std::fs::remove_file(&t1);
        let _ = std::fs::remove_file(&t2);
    }

    #[test]
    fn test_url_natural_end_repeat_off_advances_to_first_library_track() {
        let mut app = test_app();
        app.is_url = true;
        app.current_url = Some((
            "https://example.com/remote.mp3".to_string(),
            "remote.mp3".to_string(),
        ));
        app.status = Status::Playing;
        app.repeat = RepeatMode::Off;
        app.track_label = "https://example.com/remote.mp3".to_string();
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t1_urloff1_{}.wav", std::process::id()));
        let t2 = dir.join(format!("mp3tui_t1_urloff2_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);
        let _ = std::fs::write(&t2, &wav);
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(t1.clone(), "local_1"),
            track_with_path(t2.clone(), "local_2"),
        ]));
        refresh_view(&mut app);

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                let _ = std::fs::remove_file(&t1);
                let _ = std::fs::remove_file(&t2);
                return;
            }
        };

        handle_natural_end(&mut app, &mut engine);

        assert!(
            !app.is_url,
            "repeat Off tras una URL debe avanzar a la biblioteca local"
        );
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t1),
            "la biblioteca local entra desde su primera pista"
        );
        assert_eq!(app.selected, 0);
        assert!(
            app.error.is_none(),
            "no debe fallar al arrancar la pista local: {:?}",
            app.error
        );

        let _ = std::fs::remove_file(&t1);
        let _ = std::fs::remove_file(&t2);
    }

    #[test]
    fn test_url_natural_end_repeat_one_dispatches_replay_without_spawning() {
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        // Sesión URL con posición por encima del mínimo: wav local de 6 s y
        // seek instantáneo (los archivos son seekables y el seek guarda
        // played_frames sincrónicamente, sin esperar reproducción real).
        let t6 = std::env::temp_dir().join(format!(
            "mp3tui_t11_urlok_{}.wav",
            std::process::id()
        ));
        let _ = std::fs::write(&t6, make_test_wav(44100 * 6));
        if engine.play(&t6).is_err() {
            let _ = std::fs::remove_file(&t6);
            return;
        }
        assert!(
            engine.seek(Duration::from_secs(6)).is_ok(),
            "setup: el seek en archivo local debe funcionar"
        );
        assert!(
            engine.position() >= crate::playback::MIN_URL_REPLAY,
            "setup: la posición debe superar el mínimo ({:?})",
            engine.position()
        );

        app.is_url = true;
        app.status = Status::Playing;
        app.repeat = RepeatMode::One;
        let original = "https://www.youtube.com/watch?v=abc123".to_string();
        app.current_url = Some((original.clone(), "Video".to_string()));
        app.track_label = "Video".to_string();

        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        assert_eq!(
            dispatched.as_ref().map(|r| r.original_url.as_str()),
            Some(original.as_str()),
            "One con URL larga debe pedir re-lanzar la URL original"
        );
        assert!(
            app.ytdlp_rx.is_none(),
            "el test no debe lanzar yt-dlp: la orden va por el dispatcher"
        );
        assert_ne!(app.status, Status::Resolving, "sin resolución pendiente");
        assert!(
            app.is_url,
            "la sesión sigue siendo de URL"
        );
        assert_eq!(
            app.current_url.as_ref().map(|(u, _)| u.as_str()),
            Some(original.as_str()),
            "la URL original debe conservarse para el próximo replay"
        );

        let _ = std::fs::remove_file(&t6);
    }

    #[test]
    fn test_url_natural_end_repeat_one_too_short_stops_with_message() {
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        // Motor recién creado: posición 0, por debajo del mínimo.
        assert_eq!(engine.position(), Duration::ZERO);

        app.is_url = true;
        app.status = Status::Playing;
        app.repeat = RepeatMode::One;
        app.current_url = Some((
            "https://www.youtube.com/watch?v=short".to_string(),
            "Short".to_string(),
        ));
        app.track_label = "Short".to_string();

        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        assert_eq!(
            dispatched, None,
            "una URL que duró menos del mínimo no se re-lanza"
        );
        assert_eq!(
            app.error.as_deref(),
            Some("La canción terminó enseguida; no se repite"),
            "debe mostrar el aviso de corte"
        );
        assert!(app.ytdlp_rx.is_none(), "sin yt-dlp en el test");
    }

    #[test]
    fn test_local_natural_end_filtered_out_of_view_stops_without_restarting() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t11_hide1_{}.wav", std::process::id()));
        let t2 = dir.join(format!("mp3tui_t11_hide2_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);
        let _ = std::fs::write(&t2, &wav);
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(t1.clone(), "track_1"),
            track_with_path(t2.clone(), "track_2"),
        ]));
        refresh_view(&mut app);
        app.repeat = RepeatMode::Off;

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                let _ = std::fs::remove_file(&t1);
                let _ = std::fs::remove_file(&t2);
                return;
            }
        };

        // La pista 1 está sonando...
        app.selected = 0;
        play_selected(&mut app, &mut engine);
        assert_eq!(app.current_path.as_ref(), Some(&t1));

        // ...y un filtro la deja FUERA de la vista visible.
        app.filter = Some("track_2".to_string());
        refresh_view(&mut app);
        assert_eq!(app.view.len(), 1, "setup: solo track_2 visible");
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t1),
            "setup: sigue sonando track_1 aunque esté oculto"
        );
        let selected_before = app.selected;

        handle_natural_end(&mut app, &mut engine);

        assert_eq!(
            app.current_path.as_ref(),
            Some(&t1),
            "fuera de la vista no debe reiniciar desde la primera pista visible"
        );
        assert_eq!(
            app.selected, selected_before,
            "la selección no debe moverse"
        );
        assert!(app.error.is_none(), "{:?}", app.error);
        assert!(app.ytdlp_rx.is_none(), "sin yt-dlp en el test");

        let _ = std::fs::remove_file(&t1);
        let _ = std::fs::remove_file(&t2);
    }

    #[test]
    fn test_local_natural_end_repeat_off_advances_then_stops_at_the_end() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t1_offadv1_{}.wav", std::process::id()));
        let t2 = dir.join(format!("mp3tui_t1_offadv2_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);
        let _ = std::fs::write(&t2, &wav);
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(t1.clone(), "track_1"),
            track_with_path(t2.clone(), "track_2"),
        ]));
        refresh_view(&mut app);
        app.repeat = RepeatMode::Off;

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                let _ = std::fs::remove_file(&t1);
                let _ = std::fs::remove_file(&t2);
                return;
            }
        };

        app.selected = 0;
        play_selected(&mut app, &mut engine);
        assert_eq!(app.current_path.as_ref(), Some(&t1));

        // En el medio de la lista: Off avanza a la siguiente.
        handle_natural_end(&mut app, &mut engine);
        assert_eq!(app.selected, 1, "Off debe avanzar en el medio");
        assert_eq!(app.current_path.as_ref(), Some(&t2));

        // En la última pista: Off no envuelve, se queda donde está.
        handle_natural_end(&mut app, &mut engine);
        assert_eq!(
            app.selected, 1,
            "Off en la última pista no debe envolver ni mover la selección"
        );
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "Off en la última pista no debe reemplazar la pista actual"
        );
        assert!(app.error.is_none(), "{:?}", app.error);

        let _ = std::fs::remove_file(&t1);
        let _ = std::fs::remove_file(&t2);
    }

    #[test]
    fn test_playing_library_track_invalidates_pending_ytdlp_resolution() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let t1 = dir.join(format!("mp3tui_t61_inv1_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        let _ = std::fs::write(&t1, &wav);

        let trk1 = track_with_path(t1.clone(), "track_1");

        app.library = Some(crate::playlist::from_tracks_for_test(vec![trk1]));
        refresh_view(&mut app);
        app.selected = 0;

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                let _ = std::fs::remove_file(&t1);
                return;
            }
        };

        // 1. Submit a page URL: resolution is now pending. El resolver se
        // inyecta como doble: captura la orden pero NO lanza yt-dlp ni toca
        // la red desde el test.
        let page_url = "https://youtube.com/watch?v=abc".to_string();
        let mut resolved_url: Option<String> = None;
        submit_url_prompt_with(page_url.clone(), &mut app, &mut engine, |url, _req_id| {
            resolved_url = Some(url);
            let (_tx, rx) = mpsc::channel();
            rx
        });
        assert_eq!(
            resolved_url.as_deref(),
            Some(page_url.as_str()),
            "el resolver debe recibir la URL de la página"
        );
        assert!(app.ytdlp_rx.is_some(), "resolution should be pending");
        let initial_req_id = app.ytdlp_request_id;
        assert_eq!(app.status, Status::Resolving);

        // 2. User plays a library track before resolution finishes
        play_selected(&mut app, &mut engine);

        // In fixed code: ytdlp_rx must be cleared and request_id bumped
        assert!(app.ytdlp_rx.is_none(), "pending resolution must be cleared");
        assert_ne!(app.ytdlp_request_id, initial_req_id, "request id must be bumped");
        assert_ne!(app.status, Status::Resolving, "status must not stay Resolving");

        // 3. Late arrival of old resolution result must be ignored
        let stale_result = Ok(crate::source::ytdlp::Resolved {
            title: "Stale Video".to_string(),
            audio_url: "https://example.com/stale.m4a".to_string(),
        });
        let action = handle_ytdlp_result(&mut app, initial_req_id, stale_result);
        assert_eq!(action, None, "stale ytdlp result must be rejected");

        let _ = std::fs::remove_file(&t1);
    }

    #[test]
    fn test_row_building_with_history_and_local_groups() {
        let mut app = library_with_three();
        app.history.add("Historial Cancion 1".to_string(), "https://example.com/song1.mp3".to_string());
        refresh_view(&mut app);

        assert_eq!(app.view.len(), 4);
        assert_eq!(app.view[0], ViewItem::History(0));
        assert_eq!(app.view[1], ViewItem::Track(0));

        let lines = library_rows(&app, 60, 10);
        let has_history_header = lines.iter().any(|l| {
            l.spans.iter().any(|s| s.content.contains("Agregadas por URL"))
        });
        assert!(has_history_header, "debe contener encabezado 'Agregadas por URL'");

        let has_history_badge = lines.iter().any(|l| {
            l.spans.iter().any(|s| s.content.contains("[URL] "))
        });
        assert!(has_history_badge, "fila de historial debe tener badge '[URL] '");

        let has_history_title = lines.iter().any(|l| {
            l.spans.iter().any(|s| s.content.contains("Historial Cancion 1"))
        });
        assert!(has_history_title, "fila de historial debe mostrar el título");
    }

    #[test]
    fn test_navigation_crossing_boundary_between_history_and_local() {
        let mut app = library_with_three();
        app.history.add("Historial Cancion 1".to_string(), "https://example.com/song1.mp3".to_string());
        refresh_view(&mut app);

        assert_eq!(app.view[0], ViewItem::History(0));
        assert_eq!(app.view[1], ViewItem::Track(0));

        app.selected = 0;
        app.selected = move_selection(app.selected, 1, app.view.len());
        assert_eq!(app.selected, 1);
        assert_eq!(app.view[app.selected], ViewItem::Track(0));

        app.selected = move_selection(app.selected, -1, app.view.len());
        assert_eq!(app.selected, 0);
        assert_eq!(app.view[app.selected], ViewItem::History(0));
    }

    #[test]
    fn test_search_filters_history_by_title_and_local_tracks() {
        let mut app = library_with_three();
        app.history.add("Bohemian Rhapsody".to_string(), "https://example.com/queen.mp3".to_string());
        app.history.add("Yesterday".to_string(), "https://example.com/beatles.mp3".to_string());
        refresh_view(&mut app);

        assert_eq!(app.view.len(), 5);

        app.filter = Some("bohemian".to_string());
        refresh_view(&mut app);
        assert_eq!(app.view.len(), 1);
        assert_eq!(app.view[0], ViewItem::History(1));

        app.filter = Some("beta".to_string());
        refresh_view(&mut app);
        assert_eq!(app.view.len(), 1);
        assert_eq!(app.view[0], ViewItem::Track(1));
    }

    #[test]
    fn test_enter_on_history_row_dispatches_playback() {
        let mut app = library_with_three();
        // URL local vía servidor de loopback: la fila del historial reproduce
        // sin salir a la red externa (el label sigue siendo el filename).
        let url = local_wav_url("stream.mp3", &make_test_wav(4410));
        app.history.add("Historial Cancion".to_string(), url);
        refresh_view(&mut app);

        app.selected = 0;
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        play_selected(&mut app, &mut engine);

        assert_eq!(app.mode, Mode::Player);
        assert!(app.is_url);
        assert_eq!(app.track_label, "stream.mp3");
    }

    #[test]
    fn test_history_recording_happens_only_on_successful_start() {
        let mut app = test_app();
        assert_eq!(app.history.len(), 0);

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        // Reproduce una URL local de loopback: sin red externa, y la entrada
        // registrada debe ser exactamente esa URL.
        let url = local_wav_url("audio.mp3", &make_test_wav(4410));
        start_url_playback(
            &mut app,
            &mut engine,
            url.clone(),
            url.clone(),
            "audio.mp3".to_string(),
        );

        assert_eq!(app.status, Status::Connecting);
        assert_eq!(app.history.len(), 0);
        assert!(app.pending_history.is_some());

        let snapshot_playing = EngineSnapshot {
            is_connecting: false,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        apply_engine_snapshot(&mut app, snapshot_playing);

        assert_eq!(app.status, Status::Playing);
        assert_eq!(app.history.len(), 1);
        assert_eq!(
            app.history.get(0).unwrap().url,
            url,
            "la entrada registrada es la URL local que se reprodujo"
        );
        assert_eq!(app.history.get(0).unwrap().title, "audio.mp3");
        assert!(app.pending_history.is_none());
    }

    #[test]
    fn test_url_playback_error_keeps_history_empty() {
        let mut app = test_app();
        init_url_playback(&mut app, "https://youtube.com/watch?v=123".to_string(), "My Song".to_string());
        assert_eq!(app.status, Status::Connecting);

        let snapshot_error = EngineSnapshot {
            is_connecting: false,
            is_playing: false,
            is_paused: false,
            had_playback_error: true,
        };
        apply_engine_snapshot(&mut app, snapshot_error);
        assert_eq!(app.status, Status::Stopped);
        assert!(app.history.is_empty(), "history must stay empty on playback error");
        assert!(app.pending_history.is_none());
    }

    #[test]
    fn test_url_playback_connecting_keeps_history_empty_until_playing() {
        let mut app = test_app();
        init_url_playback(&mut app, "https://youtube.com/watch?v=123".to_string(), "My Song".to_string());

        let snapshot_connecting = EngineSnapshot {
            is_connecting: true,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        apply_engine_snapshot(&mut app, snapshot_connecting);
        assert_eq!(app.status, Status::Connecting);
        assert!(app.history.is_empty(), "history must be empty while connecting");

        let snapshot_playing = EngineSnapshot {
            is_connecting: false,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        apply_engine_snapshot(&mut app, snapshot_playing);
        assert_eq!(app.status, Status::Playing);
        assert_eq!(app.history.len(), 1, "exactly one history entry recorded on playing");
        assert_eq!(app.history.get(0).unwrap().url, "https://youtube.com/watch?v=123");
        assert_eq!(app.history.get(0).unwrap().title, "My Song");
        assert!(app.pending_history.is_none());
    }

    #[test]
    fn test_superseded_url_playback_is_never_recorded() {
        let mut app = test_app();
        init_url_playback(&mut app, "https://example.com/A.mp3".to_string(), "Track A".to_string());
        init_url_playback(&mut app, "https://example.com/B.mp3".to_string(), "Track B".to_string());

        let snapshot_playing = EngineSnapshot {
            is_connecting: false,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        apply_engine_snapshot(&mut app, snapshot_playing);
        assert_eq!(app.status, Status::Playing);
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history.get(0).unwrap().url, "https://example.com/B.mp3");
        assert!(
            !app.history.entries().iter().any(|e| e.url == "https://example.com/A.mp3"),
            "URL A must never be recorded"
        );
    }

    #[test]
    fn test_ui_app_uses_isolated_temp_history_path() {
        let app = test_app();
        let hist_path = app.history_path.as_ref().expect("history_path must be set");
        assert!(
            hist_path.starts_with(std::env::temp_dir()),
            "history_path must be in temp_dir, got: {}",
            hist_path.display()
        );
        assert_ne!(
            Some(hist_path.clone()),
            crate::history::history_path(),
            "must not be real user path"
        );
    }

    #[test]
    fn test_record_history_preserves_selected_history_item() {
        let mut app = library_with_three();
        app.history.add("Song 1".to_string(), "https://example.com/1.mp3".to_string());
        app.history.add("Song 2".to_string(), "https://example.com/2.mp3".to_string());
        refresh_view(&mut app);

        // View has History(0) ("Song 2") and History(1) ("Song 1")
        assert_eq!(app.view[0], ViewItem::History(0));
        assert_eq!(app.view[1], ViewItem::History(1));
        app.selected = 1; // User selected Song 1 (2nd row)

        record_history_entry(&mut app, "https://example.com/3.mp3", "Song 3");

        // Song 3 was added at top. Song 1 is now at index 2.
        // The selection must remain on Song 1!
        assert_eq!(app.view[app.selected], ViewItem::History(2));
        assert_eq!(app.history.get(2).unwrap().title, "Song 1");
    }

    #[test]
    fn test_record_history_strips_userinfo() {
        let mut app = test_app();
        record_history_entry(&mut app, "http://user:pass@example.com/secret.mp3", "Secret");
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history.get(0).unwrap().url, "http://example.com/secret.mp3");
    }

    #[test]
    fn progress_cells_keeps_today_played_tip_and_gray_track() {
        // Sin secciones precargadas: exactamente el look actual de la barra
        // (Played…, Tip, Empty…), para que lo local no cambie.
        assert_eq!(
            progress_bar_cells(10, 0.5, &[]),
            vec![
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Tip,
                BarCellKind::Empty,
                BarCellKind::Empty,
                BarCellKind::Empty,
                BarCellKind::Empty,
                BarCellKind::Empty,
            ]
        );
    }

    #[test]
    fn progress_cells_paints_buffered_sections_and_keeps_gaps() {
        // Bounds exactamente representables en f32 para evitar que 0.3/0.7
        // toquen la celda del borde (0.3f32 > 0.3 → cubre la celda 3).
        let cells = progress_bar_cells(10, 0.0, &[(0.0, 0.25), (0.75, 1.0)]);
        assert_eq!(
            cells,
            vec![
                BarCellKind::Buffered,
                BarCellKind::Buffered,
                BarCellKind::Buffered,
                BarCellKind::Empty,
                BarCellKind::Empty,
                BarCellKind::Empty,
                BarCellKind::Empty,
                BarCellKind::Buffered,
                BarCellKind::Buffered,
                BarCellKind::Buffered,
            ]
        );
    }

    #[test]
    fn progress_cells_played_wins_over_buffered() {
        // Las celdas ya reproducidas conservan su estilo aunque un rango las cubra.
        let cells = progress_bar_cells(10, 0.6, &[(0.0, 1.0)]);
        assert_eq!(
            cells,
            vec![
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Played,
                BarCellKind::Tip,
                BarCellKind::Buffered,
                BarCellKind::Buffered,
                BarCellKind::Buffered,
                BarCellKind::Buffered,
            ]
        );
    }

    #[test]
    fn progress_cells_tiny_range_paints_the_cell_it_touches() {
        // Rango diminuto no vacío: pinta al menos la celda que toca (0.505 cae
        // dentro de la celda 50 en un ancho 100). Rango degenerado (e <= s): nada.
        let cells = progress_bar_cells(100, 0.0, &[(0.505, 0.5055)]);
        let mut expected = vec![BarCellKind::Empty; 100];
        expected[50] = BarCellKind::Buffered;
        assert_eq!(cells, expected);

        let cells = progress_bar_cells(100, 0.0, &[(0.5, 0.5)]);
        assert!(
            cells.iter().all(|&k| k == BarCellKind::Empty),
            "un rango vacío no debe pintar ninguna celda"
        );
    }

    #[test]
    fn progress_cells_clamps_and_never_panics() {
        // Ancho 0 → sin celdas; ancho 1 → una sola celda bien resuelta.
        assert!(progress_bar_cells(0, 0.5, &[(0.0, 1.0)]).is_empty());
        assert_eq!(
            progress_bar_cells(1, 0.0, &[(0.0, 1.0)]),
            vec![BarCellKind::Buffered]
        );
        assert_eq!(progress_bar_cells(1, 1.0, &[]), vec![BarCellKind::Played]);

        // Fracciones fuera de [0,1] se clampan.
        assert_eq!(
            progress_bar_cells(10, 1.5, &[(0.0, 1.0)]),
            vec![BarCellKind::Played; 10],
            "played > 1 se clampa a barra llena"
        );
        let cells = progress_bar_cells(10, 0.0, &[(-0.5, 0.05), (0.95, 1.5)]);
        let mut expected = vec![BarCellKind::Empty; 10];
        expected[0] = BarCellKind::Buffered;
        expected[9] = BarCellKind::Buffered;
        assert_eq!(cells, expected, "los rangos se clampan a [0,1]");
    }
}
