//! Interfaz de terminal: layout con ratatui y loop de eventos no bloqueante.
//! Reproducción y explorador de biblioteca. Con sesión de audio
//! el reproductor (info + visualizador) es el fondo permanente y el
//! explorador se superpone como panel lateral con Ctrl+Tab/b/Tab; a
//! pantalla completa solo cuando no hay player (arranque por directorio).

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::audio::{AudioEngine, SPEED_STEP};
use crate::config::{self, Theme, key_labels};
use crate::playback::{
    NextAction, QueueItem, RepeatMode, URL_REPLAY_TOO_SOON_MSG, filter_tracks, natural_end_action,
    next_track, seek_target, shuffle_order,
};
use crate::playlist::{Playlist, Track, move_selection, scroll_offset};
use crate::runtime;
use crate::visualizer::{NUM_BANDS, SpectrumAnalyzer};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);

const VOLUME_STEP: f32 = 0.05;

const PAGE_STEP: isize = 10;

/// Bloques de octavo de celda para el visualizador sub-celda (mejora 1).
const EIGHTH_BLOCKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// Rampa ASCII de 10 niveles para el modo ascii (índice 0 = vacío … 9 = lleno).
const ASCII_RAMP: [&str; 10] = [" ", ".", ":", "-", "=", ";", "+", "*", "#", "@"];

// Paleta compartida por todo el programa: la celda runtime del
// módulo `runtime`, que cambia en caliente al previsualizar/aplicar temas desde el menú
// (default: la del `[theme] name` cargado al arrancar). Copia de 11 `Color`
// por llamada; barato por frame.
fn theme() -> Theme {
    runtime::active_theme()
}

const CURSOR: &str = "\u{258c}"; // ▌ fila seleccionada

const PLAYING: &str = "\u{25b6}"; // ▶ pista en reproducción

const SEARCH_CARET: &str = "\u{258f}"; // ▏ cursor de texto en modo búsqueda

/// Ancho fijo (en columnas) de la caja del campo de búsqueda.
const SEARCH_BOX_WIDTH: usize = 24;

/// Texto guía de la caja vacía.
const SEARCH_PLACEHOLDER: &str = "escribí para buscar";

/// Aviso cuando una lista de playlist sale vacía.
const PLAYLIST_EMPTY_MSG: &str = "La playlist no tiene canciones";

/// Aviso cuando un ítem de la cola no arranca y se saltea.
const QUEUE_ITEM_FAILED_MSG: &str = "No se pudo reproducir el ítem de la cola; se saltea";

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
    /// Sin argumento (carpetas conocidas + `[library]` roots) o con
    /// carpeta: explorador de la biblioteca, agrupado por carpeta de origen.
    Browse(Vec<PathBuf>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitAction {
    Local(PathBuf),
    DirectAudio(String),
    Page(String),
    /// URL de playlist: se lista con `--flat-playlist` y se encola el resto.
    Playlist(String),
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
            crate::source::UrlKind::Playlist => SubmitAction::Playlist(url),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Pantalla de fondo. Con sesión de audio siempre es `Player`; el explorador
/// se superpone como panel lateral (`App::explorer_open`) sin
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

/// Mensaje del listado `--flat-playlist`: id de pedido, URL original de la
/// playlist y la lista de entradas (título + URL de página).
type YtDlpListMessage = (
    u64,
    String,
    Result<Vec<crate::source::ytdlp::PlaylistEntry>, crate::source::ytdlp::YtDlpError>,
);

/// Disparador que inició el avance por la cola. Al agotarse la cola tras
/// saltear un ítem fallido, decide por dónde cae el fallback al pool
/// de la biblioteca/historial: el fin natural o el sentido del salto manual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueAdvance {
    /// Fin natural de pista: avanza el pool hacia adelante.
    NaturalEnd,
    /// Salto manual `n` (+1) o `p` (-1).
    Step(isize),
}

struct App {
    mode: Mode,
    /// Panel lateral del explorador superpuesto sobre el reproductor.
    /// Solo tiene efecto en `Mode::Player`.
    explorer_open: bool,
    /// Raíces escaneadas: CLI, o bien las carpetas conocidas del
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
    /// Cola de reproducción: lo que aparece en la solapa Cola del panel.
    /// Mientras queden ítems después de `queue_cursor` manda sobre `repeat`
    /// y `shuffle`.
    queue: Vec<QueueItem>,
    /// Posición de la cola donde está parado el cursor: el ítem que suena (o
    /// el último que sonó desde la cola). `None` mientras no haya sonado
    /// ningún ítem de la cola. Las posiciones por debajo se muestran
    /// atenuadas (ya sonaron) y las de arriba, como próximas.
    queue_cursor: Option<usize>,
    /// Índice de la cola cuya URL se está resolviendo. Si la resolución
    /// falla, el event loop llama a [`skip_failed_queue_item`]: se saltea con
    /// aviso y se intenta el próximo ítem (o el pool si la cola se agota).
    queue_resolving: Option<usize>,
    /// Disparador que inició el recorrido de la cola, para el fallback.
    queue_advance: QueueAdvance,
    /// Solapa activa del panel lateral: `false` = Biblioteca, `true` = Cola.
    /// Solo tiene efecto con el panel abierto.
    queue_panel: bool,
    /// Selección dentro de la Cola (independiente de `selected`, la de la
    /// Biblioteca): cada lista conserva la suya al alternar.
    queue_selected: usize,
    /// «Próximo» a tocar para la franja del panel: el ítem que sigue al
    /// cursor de la cola, y con la cola agotada el siguiente del pool
    /// (biblioteca/historial con shuffle); `None` la oculta.
    next_up: Option<String>,
    /// Reloj de frames del render: alimenta el marquee de la franja.
    ui_frame: u64,
    current_path: Option<PathBuf>,
    has_player: bool,
    track_label: String,
    status: Status,
    error: Option<String>,
    should_quit: bool,
    analyzer: SpectrumAnalyzer,
    url_input: Option<String>,
    ytdlp_rx: Option<Receiver<YtDlpMessage>>,
    /// Receiver del listado `--flat-playlist` en curso.
    playlist_rx: Option<Receiver<YtDlpListMessage>>,
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
    /// Menú de temas abierto: `Some` mientras el modal está activo.
    theme_menu: Option<ThemeMenuState>,
    /// Selector de bandas abierto: `Some` mientras el sub-modal está
    /// activo (el panel sigue abierto debajo).
    bands_menu: Option<BandsMenuState>,
    /// Selector de modo abierto: `Some` mientras el
    /// sub-modal está activo (el panel sigue abierto debajo).
    mode_menu: Option<ModeMenuState>,
    /// Panel de configuración abierto: `Some` mientras el modal está activo.
    settings_panel: Option<SettingsPanelState>,
    /// Captura del mouse en vivo: arranca con el valor de `[ui] mouse`
    /// y el toggle del panel la cambia en el momento (+ persiste).
    /// `handle_mouse` la respeta por encima del chequeo de modales.
    mouse_enabled: bool,
    /// Doble Esc para salir: el primer Esc arma, el segundo sale.
    quit_guard: QuitGuard,
    /// Zonas clickeables del último frame (las anota el render).
    hit: RefCell<HitMap>,
    /// Arrastre con el mouse en curso.
    drag: Option<Drag>,
}

/// Estado genérico del sub-modal de selección (Bandas y Modo; Temas usa el
/// mismo esquema salvo sus dos originales): selección con previsualización en
/// vivo y el valor con el que se abrió (para el marcador ● y para restaurar
/// con Esc). Agregar un selector nuevo es instanciar este estado y agregar
/// data al handler/render genéricos, no copiar el estado.
struct MenuState<T> {
    /// Índice en la lista de opciones.
    selected: usize,
    /// Valor activo al abrir (fila «actual» marcada con ●).
    original: T,
}

/// Estado del menú de temas: selección con previsualización en vivo y
/// el tema con el que se abrió (para el marcador y para restaurar con Esc).
/// Conserva su forma concreta (captura dos originales: id + paleta); el
/// handler, las filas y el render corren por los genéricos.
struct ThemeMenuState {
    /// Índice en `config::builtin_themes()`.
    selected: usize,
    /// Id del tema activo al abrir (fila «actual» marcada con ●).
    original_name: String,
    /// Colores activos al abrir: Esc los restaura.
    original_theme: Theme,
}

/// Selector de bandas: instancia del estado genérico sobre los 3 valores
/// fijos de estilo.
type BandsMenuState = MenuState<config::VisualizerStyle>;

/// Selector de modo: instancia del estado genérico sobre los 5 valores
/// fijos de modo.
type ModeMenuState = MenuState<config::VisualizerMode>;

/// Posición de un sub-modal de selección: el único acceso que el handler
/// genérico necesita del estado (además de la toma del `Option`). Lo
/// implementan `MenuState<T>` y `ThemeMenuState`, que conserva su forma
/// concreta para los tests.
trait MenuSelection {
    fn selected(&self) -> usize;

    fn set_selected(&mut self, selected: usize);
}

impl<T> MenuSelection for MenuState<T> {
    fn selected(&self) -> usize {
        self.selected
    }

    fn set_selected(&mut self, selected: usize) {
        self.selected = selected;
    }
}

impl MenuSelection for ThemeMenuState {
    fn selected(&self) -> usize {
        self.selected
    }

    fn set_selected(&mut self, selected: usize) {
        self.selected = selected;
    }
}

/// Filas fijas del panel de configuración: Temas abre su sub-modal,
/// Bandas el suyo, Mouse su toggle y Modo
/// el suyo. El orden es estable: la cuarta fila va AL FINAL, sin reordenar
/// las existentes.
const SETTINGS_ROWS: [&str; 4] = ["Temas", "Mouse", "Bandas", "Modo"];

/// Estado del panel de configuración (shell sin contenido): selección por
/// índice sobre `SETTINGS_ROWS`.
struct SettingsPanelState {
    /// Índice en `SETTINGS_ROWS`.
    selected: usize,
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
                if config::config().mouse {
                    let _ = crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture);
                }
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
        let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture);
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
    let mut initial_error =
        (!config::config().warnings.is_empty()).then(|| config::config().warnings.join(" · "));
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
        queue: Vec::new(),
        queue_cursor: None,
        queue_resolving: None,
        queue_advance: QueueAdvance::NaturalEnd,
        queue_panel: false,
        queue_selected: 0,
        next_up: None,
        ui_frame: 0,
        current_path,
        has_player,
        track_label,
        status: status_of(engine),
        // Avisos de la config  e historial: en la primera pantalla, además del
        // stderr que ya imprimió main. Con config limpia queda `None`.
        error: initial_error,
        should_quit: false,
        analyzer: SpectrumAnalyzer::new(),
        url_input: None,
        ytdlp_rx: None,
        playlist_rx: None,
        ytdlp_request_id: 0,
        is_url: is_url_start,
        current_url: None,
        ytdlp_failed: false,
        history,
        pending_history: None,
        history_path: crate::history::history_path(),
        theme_menu: None,
        bands_menu: None,
        mode_menu: None,
        settings_panel: None,
        // El estado vivo arranca con lo configurado (el toggle lo cambia en
        // el momento y lo persiste para el próximo arranque).
        mouse_enabled: config::config().mouse,
        quit_guard: QuitGuard::default(),
        hit: RefCell::new(HitMap::default()),
        drag: None,
    };

    if let Some((url, kind)) = start_url_info {
        start_boot_url(&mut app, engine, url, kind);
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
        let playback_failed =
            engine.had_playback_error() || playback_error.is_some() || app.ytdlp_failed;
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

        // Listado de playlist (`--flat-playlist`): al llegar, encola el resto
        // de la lista y resuelve la primera canción.
        if let Some(rx) = &app.playlist_rx {
            match rx.try_recv() {
                Ok((req_id, _orig_playlist_url, result)) => {
                    app.playlist_rx = None;
                    handle_playlist_result(app, req_id, result, &mut spawn_ytdlp_resolution);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    app.playlist_rx = None;
                }
            }
        }

        if let Some(rx) = &app.ytdlp_rx {
            match rx.try_recv() {
                Ok((req_id, orig_url, result)) => {
                    app.ytdlp_rx = None;
                    // Historia: la URL original que se guarda es la de la
                    // sesión actual (`current_url`), que para la primera de
                    // una playlist es la URL de la playlist; para un
                    // replay el `current_url` ya quedó reh aplicado. El valor
                    // que devuelve el hilo solo se usa como respaldo si no
                    // hubiera sesión (no debería ocurrir en esta ruta).
                    let original = app
                        .current_url
                        .as_ref()
                        .map(|(original, _)| original.clone())
                        .unwrap_or(orig_url);
                    if let Some((audio_url, title)) = handle_ytdlp_result(app, req_id, result) {
                        // Resolución OK: ya no es resolución de ítem de cola.
                        app.queue_resolving = None;
                        start_url_playback(app, engine, audio_url, original, title);
                    } else if req_id == app.ytdlp_request_id {
                        // Una URL encolada falló al resolver su turno: se
                        // saltea con aviso y se sigue con el próximo.
                        skip_failed_queue_item(app, engine, &mut dispatch_url_replay);
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
        // Bailarina (modo `miku`): la pose avanza por frame según la energía
        // de graves (o por timer en pausa).
        app.analyzer.tick_dance(app.status == Status::Playing);
        // Reloj del render y franja «Próxima canción»: se actualizan por
        // frame para el marquee y para no calcular el próximo en el draw.
        app.ui_frame = app.ui_frame.wrapping_add(1);
        refresh_next_up(app);

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
            Event::Mouse(mouse) => handle_mouse(mouse, app, engine),
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

fn status_of(engine: &AudioEngine) -> Status {
    compute_status(false, EngineSnapshot::from_engine(engine, false))
}

fn handle_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    app.quit_guard.touched = false;
    dispatch_key(key, app, engine);
    if !app.quit_guard.touched {
        app.quit_guard.disarm();
    }
}

fn dispatch_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
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

    // Menú de temas: con el menú abierto las teclas van al menú —vale igual
    // como sub-modal del panel, que sigue abierto debajo y retoma al
    // cerrar el sub-modal con Enter/Esc—.
    if app.theme_menu.is_some() {
        handle_theme_menu_key(key, app, config::persist_theme_name);
        return;
    }

    // Selector de bandas: sub-modal del panel con la misma prioridad
    // que el de temas (nunca están abiertos los dos a la vez).
    if app.bands_menu.is_some() {
        handle_bands_menu_key(key, app, config::persist_visualizer_style);
        return;
    }

    // Selector de modo: sub-modal del panel con la
    // misma prioridad que los otros dos (nunca hay dos abiertos a la vez).
    if app.mode_menu.is_some() {
        handle_mode_menu_key(key, app, config::persist_visualizer_mode);
        return;
    }

    // Panel de configuración: con el panel abierto las teclas van al
    // panel (los modales son excluyentes: el primero que está abierto gana).
    if app.settings_panel.is_some() {
        handle_settings_panel_key(key, app, config::persist_mouse_enabled);
        return;
    }

    // Con la búsqueda abierta todo lo demás se escribe en el query: ni 'q'
    // sale ni las flechas mueven la selección.
    if app.search.is_some() {
        handle_search_key(key, app);
        return;
    }

    match app.mode {
        // Panel lateral abierto: la lista manda las flechas, pero los
        // atajos de reproducción (pausa, seek, n/p…) siguen activos.
        Mode::Player if app.explorer_open => handle_list_key(key, app, engine),
        Mode::Player => handle_player_key(key, app, engine),
        Mode::Browser => handle_list_key(key, app, engine),
    }
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
        ViewItem::Track(library_index) => {
            library_get(app, library_index).map(|track| track.path.clone())
        }
        ViewItem::History(_) => None,
    });
    let previous_history_url = preserved_history_url.map(String::from).or_else(|| {
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
        // El orden vive sobre el pool MIXTO (pistas + historial), igual que en
        // `toggle_shuffle` y en `pool_positions_in_view`: si el refresco lo
        // dejara con solo las pistas, quedaría a medio armar hasta el próximo
        // salto.
        let pool_count = app
            .view
            .iter()
            .filter(|item| {
                matches!(item, ViewItem::Track(_)) || matches!(item, ViewItem::History(_))
            })
            .count();
        app.shuffle_order = shuffle_order(pool_count);
    }
}

/// Ubica la selección en la pista que está sonando (si está en la biblioteca).
fn resync_selection(app: &mut App) {
    if let Some(position) = current_view_position(app) {
        app.selected = position;
    }
}

fn cancel_pending_ytdlp(app: &mut App) {
    app.ytdlp_request_id = app.ytdlp_request_id.wrapping_add(1);
    app.ytdlp_rx = None;
    app.playlist_rx = None;
    app.queue_resolving = None;
    app.ytdlp_failed = false;
    app.pending_history = None;
}

/// Lanza el escaneo de `app.roots` en un thread aparte (una o varias
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

/// Duración declarada en los metadatos de la pista actual (fallback cuando el
/// contenedor no reporta la suya).
fn current_track_duration(app: &App) -> Option<Duration> {
    let path = app.current_path.as_ref()?;
    let library = app.library.as_ref()?;
    let index = library.index_of(path)?;
    library.get(index)?.duration
}

/// Estado de `position` en la cola: el ítem del cursor es el que suena (o el
/// último que sonó desde la cola), los de abajo ya sonaron y los de arriba
/// faltan.
fn queue_row_state(app: &App, position: usize) -> QueueRowState {
    match app.queue_cursor {
        Some(cursor) if position < cursor => QueueRowState::Played,
        Some(cursor) if position == cursor => QueueRowState::Current,
        _ => QueueRowState::Pending,
    }
}

mod keys;
mod mouse;
mod queue;
mod render;
#[cfg(test)]
mod tests_common;

use self::keys::*;
use self::mouse::*;
use self::queue::*;
use self::render::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tests_common::*;
    use ratatui::buffer::Buffer;

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
        paint_bands(&mut buffer, area, &bands, &theme, false);

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
        paint_bands(
            &mut buffer,
            area,
            &[0.0f32; NUM_BANDS],
            &Theme::default(),
            false,
        );

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
        paint_bands(&mut buffer, area, &bands, &Theme::default(), false);

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
        paint_bands(&mut buffer, area, &bands, &Theme::default(), false);

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
        assert_eq!(
            buffer[(0u16, partial_y - 1)].symbol(),
            " ",
            "above partial = empty"
        );
    }

    #[test]
    fn paint_bands_spreads_bars_across_full_width_when_not_multiple_of_bands() {
        // 113 columnas / 32 bandas: la división entera dejaba ~17 columnas vacías.
        let width = 113u16;
        let area = Rect::new(0, 0, width, 3);
        let mut buffer = Buffer::empty(area);
        paint_bands(
            &mut buffer,
            area,
            &[1.0f32; NUM_BANDS],
            &Theme::default(),
            false,
        );

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

    #[test]
    fn ascii_ramp_has_ten_levels_from_empty_to_full() {
        assert_eq!(ASCII_RAMP.len(), 10, "la rampa ascii tiene 10 niveles");
        assert_eq!(ASCII_RAMP[0], " ", "índice 0 = vacío");
        assert_eq!(ASCII_RAMP[9], "@", "índice 9 = lleno");
        assert_eq!(
            ASCII_RAMP,
            [" ", ".", ":", "-", "=", ";", "+", "*", "#", "@"]
        );
    }

    #[test]
    fn paint_bands_ascii_full_bar_uses_at_with_same_gradient() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 1.0;
        paint_bands(&mut buffer, area, &bands, &theme, true);

        let bottom = &buffer[(0u16, 9u16)];
        assert_eq!(bottom.symbol(), "@", "celda llena en ascii");
        assert_eq!(
            bottom.fg,
            Color::Rgb(0x89, 0x92, 0xA7),
            "el gradiente no cambia en ascii"
        );

        let top = &buffer[(0u16, 0u16)];
        assert_eq!(top.symbol(), "@");
        assert_eq!(top.fg, Color::Rgb(0xC4, 0x74, 0x6E), "rojo del pico");
    }

    #[test]
    fn paint_bands_ascii_empty_cells_are_dim_dots() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        paint_bands(&mut buffer, area, &[0.0f32; NUM_BANDS], &theme, true);

        let cell = &buffer[(0u16, 9u16)];
        assert_eq!(cell.symbol(), ".", "fondo de puntos en ascii");
        assert_eq!(cell.fg, theme.dim);
    }

    #[test]
    fn paint_bands_ascii_partial_cell_uses_ramp() {
        // magnitude = 0.45, alto 10 → height_f = 4.5, full = 4, frac = 0.5
        // idx = (0.5 * 10.0) as usize = 5 → ASCII_RAMP[5] = ";"
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 10);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 0.45;
        paint_bands(&mut buffer, area, &bands, &theme, true);

        let bottom = area.y + area.height; // = 10
        let partial_y = bottom - 4 - 1; // = 5
        assert_eq!(
            buffer[(0u16, partial_y)].symbol(),
            ";",
            "frac=0.5 → ASCII_RAMP[5] = ;"
        );
        // Mismo gradiente que tendría esa celda en unicode.
        let expected = spectrum_color(4.0 / 9.0, &theme);
        assert_eq!(
            buffer[(0u16, partial_y)].fg,
            Color::Rgb(expected.0, expected.1, expected.2)
        );
        assert_eq!(buffer[(0u16, bottom - 1)].symbol(), "@", "llenas = @");
        let above = &buffer[(0u16, partial_y - 1)];
        assert_eq!(above.symbol(), ".", "sobre la barra = punto tenue");
        assert_eq!(above.fg, theme.dim);
    }

    // Osciloscopio `onda` con muestras crudas.
    // Área 8x7 → eje cero en fila 3 (0.5 * 6 = 3.0); la onda tapa al eje
    // donde lo cruza y el eje asoma donde la onda se desvía.
    // ------------------------------------------------------------------

    #[test]
    fn paint_wave_empty_samples_draws_dim_axis_only() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 7);
        // ASCII: eje `-` tenue, resto fondo de puntos.
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &[], &theme, true);
        for x in 0..8u16 {
            let axis = &buffer[(x, 3u16)];
            assert_eq!(axis.symbol(), "-", "eje cero en fila 3 (x={x})");
            assert_eq!(axis.fg, theme.dim);
            for y in [0u16, 1, 2, 4, 5, 6] {
                let cell = &buffer[(x, y)];
                assert_eq!(cell.symbol(), ".", "fondo de puntos en ({x},{y})");
                assert_eq!(cell.fg, theme.dim);
            }
        }
        // Unicode: eje `─` tenue, resto vacío.
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &[], &theme, false);
        for x in 0..8u16 {
            let axis = &buffer[(x, 3u16)];
            assert_eq!(axis.symbol(), "─", "eje cero unicode en fila 3");
            assert_eq!(axis.fg, theme.dim);
            assert_eq!(buffer[(x, 0u16)].symbol(), " ");
        }
    }

    #[test]
    fn paint_wave_ascii_silence_is_flat_hash_line_at_center() {
        // Sin señal la onda ES el cero: tapa al eje con una línea plana.
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 7);
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &[0.0f32; 8], &theme, true);
        for x in 0..8u16 {
            assert_eq!(buffer[(x, 3u16)].symbol(), "#", "plana en el centro");
        }
        // Fila 3 de 7 → t = (6 - 3) / 6 = 0.5 → spectrum_mid.
        let expected = spectrum_color(0.5, &theme);
        assert_eq!(
            buffer[(0u16, 3u16)].fg,
            Color::Rgb(expected.0, expected.1, expected.2),
            "la onda usa el gradiente del tema por altura"
        );
        assert_eq!(buffer[(0u16, 0u16)].symbol(), ".");
    }

    #[test]
    fn paint_wave_newest_sample_at_right() {
        // La más reciente a la derecha: última muestra = 1.0 → arriba, y el
        // salto brusco desde el centro queda conectado (sin hueco) en esa
        // misma columna.
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 7);
        let mut buffer = Buffer::empty(area);
        let samples = [0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        paint_wave(&mut buffer, area, &samples, &theme, true);
        assert_eq!(
            buffer[(7u16, 0u16)].symbol(),
            "#",
            "1.0 mapea a la fila de arriba"
        );
        assert_eq!(
            buffer[(7u16, 3u16)].symbol(),
            "#",
            "el salto top-centro queda conectado, sin hueco, en la misma columna"
        );
        assert_eq!(
            buffer[(0u16, 3u16)].symbol(),
            "#",
            "la primera (más vieja) queda a la izquierda, en el centro"
        );
        // Pico arriba → t = 1.0 → spectrum_high.
        let expected = spectrum_color(1.0, &theme);
        assert_eq!(
            buffer[(7u16, 0u16)].fg,
            Color::Rgb(expected.0, expected.1, expected.2)
        );
    }

    #[test]
    fn paint_wave_clamps_out_of_range_without_panic() {
        // El salto de +2.0 (clampa arriba) a -3.0 (clampa abajo) entre dos
        // columnas consecutivas conecta la columna entera, sin pánico ni
        // hueco.
        let theme = Theme::default();
        let area = Rect::new(0, 0, 2, 5);
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &[2.0f32, -3.0], &theme, true);
        assert_eq!(buffer[(0u16, 0u16)].symbol(), "#", "2.0 se clampa arriba");
        assert_eq!(buffer[(1u16, 0u16)].symbol(), "#", "conectado hasta arriba");
        assert_eq!(buffer[(1u16, 4u16)].symbol(), "#", "-3.0 se clampa abajo");
    }

    #[test]
    fn paint_wave_tiny_and_zero_sized_areas_degrade_without_panic() {
        // Precedente: los menús no entran en pánico en terminales enanas;
        // la onda tampoco (0×0, 1×1, 1–2 filas).
        let theme = Theme::default();
        let samples = [0.5f32, -0.5, 1.0, -1.0, 0.25, -0.25, 0.75, -0.75];

        // 1 fila: con `height <= 1`, toda muestra mapea a la fila 0.
        let area = Rect::new(0, 0, 8, 1);
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &samples, &theme, true);
        for x in 0..8u16 {
            assert_eq!(buffer[(x, 0u16)].symbol(), "#", "1 fila: todo a la fila 0");
        }

        // 2 filas: eje en (0.5 * 1).round() = 1 → fila de abajo.
        let area = Rect::new(0, 0, 8, 2);
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &[0.0f32; 8], &theme, true);
        for x in 0..8u16 {
            assert_eq!(buffer[(x, 1u16)].symbol(), "#", "2 filas: centro abajo");
        }

        // 1×1 y 0×0, ascii y unicode: no deben entrar en pánico.
        let area = Rect::new(0, 0, 1, 1);
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &samples, &theme, true);
        paint_wave(&mut buffer, area, &samples, &theme, false);
        let area = Rect::new(0, 0, 0, 0);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        paint_wave(&mut buffer, area, &samples, &theme, true);
        paint_wave(&mut buffer, area, &[], &theme, false);
    }

    #[test]
    fn paint_wave_unicode_uses_braille_glyphs_for_the_trace() {
        // El trazo en unicode se compone de caracteres Braille (bloque
        // U+2800..=U+28FF): 2×4 puntos por celda.
        let theme = Theme::default();
        let area = Rect::new(0, 0, 12, 8);
        let mut buffer = Buffer::empty(area);
        let samples: Vec<f32> = (0..200)
            .map(|i| (std::f32::consts::TAU * 3.0 * i as f32 / 200.0).sin())
            .collect();
        paint_wave(&mut buffer, area, &samples, &theme, false);
        let mut saw_braille = false;
        for y in 0..8u16 {
            for x in 0..12u16 {
                let symbol = buffer[(x, y)].symbol();
                let Some(ch) = symbol.chars().next() else {
                    continue;
                };
                if symbol == " " || symbol == "─" {
                    continue;
                }
                assert!(
                    ('\u{2800}'..='\u{28FF}').contains(&ch),
                    "la onda en unicode debe ser Braille, obtenido {symbol:?} en ({x},{y})"
                );
                saw_braille = true;
            }
        }
        assert!(
            saw_braille,
            "debe haber al menos una celda con trazo Braille"
        );
    }

    #[test]
    fn wave_column_spans_has_no_column_to_column_gaps_for_a_full_scale_sine() {
        // Continuidad: con un seno a escala completa sobre un área 40x10,
        // toda columna tiene celda pintada y columnas adyacentes se tocan
        // o se superponen verticalmente (ningún hueco).
        let samples: Vec<f32> = (0..400)
            .map(|i| (std::f32::consts::TAU * 7.0 * i as f32 / 400.0).sin())
            .collect();
        let spans = wave_column_spans(&samples, 40, 10);
        assert_eq!(spans.len(), 40);
        for (col, span) in spans.iter().enumerate() {
            assert!(span.is_some(), "columna sin celda pintada: {col}");
        }
        for col in 1..40 {
            let (lo_prev, hi_prev) = spans[col - 1].unwrap();
            let (lo_cur, hi_cur) = spans[col].unwrap();
            assert!(
                lo_cur.max(lo_prev) <= hi_cur.min(hi_prev),
                "hueco entre las columnas {} y {col}: prev=({lo_prev},{hi_prev}) cur=({lo_cur},{hi_cur})",
                col - 1
            );
        }
    }

    #[test]
    fn paint_wave_ascii_uses_hash_and_axis_shows_where_wave_leaves() {
        let theme = Theme::default();
        // Alto 5 → eje en fila 2 (0.5 * 4 = 2.0).
        let area = Rect::new(0, 0, 4, 5);
        let mut buffer = Buffer::empty(area);
        paint_wave(&mut buffer, area, &[1.0f32, -1.0, 0.0, 0.0], &theme, true);
        assert_eq!(buffer[(0u16, 0u16)].symbol(), "#", "1.0 arriba con #");
        assert_eq!(buffer[(1u16, 4u16)].symbol(), "#", "-1.0 abajo con #");
        assert_eq!(buffer[(2u16, 2u16)].symbol(), "#", "el cero tapa al eje");
        assert_eq!(buffer[(0u16, 2u16)].symbol(), "-", "el eje asoma en x=0");
        assert_eq!(buffer[(0u16, 2u16)].fg, theme.dim);
        let empty = &buffer[(0u16, 1u16)];
        assert_eq!(empty.symbol(), ".", "celda vacía = punto tenue");
        assert_eq!(empty.fg, theme.dim);
    }

    // Espejo vertical de las 32 bandas.
    // Área 32x7 → 1 columna por banda; eje central en fila 3 (igual que la
    // onda: `wave_sample_row(0.0, 7)`), 3 filas por lado.
    // ------------------------------------------------------------------

    #[test]
    fn paint_mirror_full_bar_grows_both_ways_from_dim_center_axis() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 7);
        let mut buffer = Buffer::empty(area);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 1.0;
        paint_mirror(&mut buffer, area, &bands, &theme, false);
        for y in [0u16, 1, 2, 4, 5, 6] {
            assert_eq!(
                buffer[(0u16, y)].symbol(),
                "█",
                "barra a ambos lados del eje (y={y})"
            );
        }
        let axis = &buffer[(0u16, 3u16)];
        assert_eq!(axis.symbol(), "─", "línea central tenue");
        assert_eq!(axis.fg, theme.dim);
        // Las demás bandas en cero: solo el eje, resto vacío.
        for x in 1..32u16 {
            assert_eq!(
                buffer[(x, 3u16)].symbol(),
                "─",
                "el eje cruza todo el ancho (x={x})"
            );
            assert_eq!(
                buffer[(x, 0u16)].symbol(),
                " ",
                "sin barra = vacío en unicode"
            );
        }
    }

    #[test]
    fn paint_mirror_gradient_goes_from_low_at_center_to_high_at_tips() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 7);
        let mut buffer = Buffer::empty(area);
        paint_mirror(&mut buffer, area, &[1.0f32; NUM_BANDS], &theme, false);
        let low = spectrum_color(0.0, &theme);
        let high = spectrum_color(1.0, &theme);
        for (y, label) in [(2u16, "arriba junto al eje"), (4u16, "abajo junto al eje")] {
            assert_eq!(
                buffer[(0u16, y)].fg,
                Color::Rgb(low.0, low.1, low.2),
                "{label} = spectrum_low"
            );
        }
        for (y, label) in [(0u16, "punta de arriba"), (6u16, "punta de abajo")] {
            assert_eq!(
                buffer[(0u16, y)].fg,
                Color::Rgb(high.0, high.1, high.2),
                "{label} = spectrum_high"
            );
        }
    }

    #[test]
    fn paint_mirror_reuses_band_glyph_levels_and_dot_background() {
        let theme = Theme::default();
        // Mitad de nivel con lado de 3: 1.5 → 1 llena + parcial 0.5.
        let area = Rect::new(0, 0, 32, 7);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 0.5;
        let mut buffer = Buffer::empty(area);
        paint_mirror(&mut buffer, area, &bands, &theme, false);
        assert_eq!(buffer[(0u16, 2u16)].symbol(), "█", "llena junto al eje");
        assert_eq!(
            buffer[(0u16, 1u16)].symbol(),
            "▅",
            "parcial 0.5 = EIGHTH_BLOCKS[4], como paint_bands"
        );
        assert_eq!(buffer[(0u16, 4u16)].symbol(), "█", "abajo simétrico");
        assert_eq!(buffer[(0u16, 5u16)].symbol(), "▅", "parcial simétrico");
        // ASCII: misma rampa que las barras + fondo de puntos.
        let mut buffer = Buffer::empty(area);
        paint_mirror(&mut buffer, area, &bands, &theme, true);
        assert_eq!(buffer[(0u16, 2u16)].symbol(), "@", "llena ascii = @");
        assert_eq!(
            buffer[(0u16, 1u16)].symbol(),
            ";",
            "parcial 0.5 = ASCII_RAMP[5], como paint_bands"
        );
        let empty = &buffer[(5u16, 0u16)];
        assert_eq!(empty.symbol(), ".", "fondo de puntos en ascii");
        assert_eq!(empty.fg, theme.dim);
        let axis = &buffer[(5u16, 3u16)];
        assert_eq!(axis.symbol(), "-", "eje ascii");
        assert_eq!(axis.fg, theme.dim);
    }

    #[test]
    fn paint_mirror_tiny_heights_degrade_without_panic() {
        let theme = Theme::default();
        let bands = [1.0f32; NUM_BANDS];
        let area = Rect::new(0, 0, 32, 1);
        let mut buffer = Buffer::empty(area);
        paint_mirror(&mut buffer, area, &bands, &theme, false);
        for x in 0..32u16 {
            assert_eq!(buffer[(x, 0u16)].symbol(), "─", "1 fila: solo el eje");
        }
        let area = Rect::new(0, 0, 32, 2);
        let mut buffer = Buffer::empty(area);
        paint_mirror(&mut buffer, area, &bands, &theme, false);
        for x in 0..32u16 {
            assert_eq!(buffer[(x, 1u16)].symbol(), "─", "el eje sigue visible");
            assert_eq!(
                buffer[(x, 0u16)].symbol(),
                "█",
                "arriba de 1 fila pinta sin pánico"
            );
        }
    }

    // VU mono horizontal con RMS.
    // Área 32x5 → barra en filas 0–3, escala tenue en fila 4.
    // ------------------------------------------------------------------

    #[test]
    fn paint_vu_full_level_fills_bar_with_gradient_and_peak_mark() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 5);
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 1.0, 1.0, &theme, false);
        // Barra llena de izquierda a derecha en todas las filas de barra.
        for y in 0..4u16 {
            assert_eq!(buffer[(0u16, y)].symbol(), "█", "llena (y={y})");
            assert_eq!(buffer[(31u16, y)].symbol(), "█", "hasta el borde (y={y})");
        }
        // Gradiente del tema de izquierda (low) a derecha (high).
        let low = spectrum_color(0.0, &theme);
        let high = spectrum_color(1.0, &theme);
        assert_eq!(
            buffer[(0u16, 0u16)].fg,
            Color::Rgb(low.0, low.1, low.2),
            "izquierda = spectrum_low"
        );
        assert_eq!(
            buffer[(31u16, 0u16)].fg,
            Color::Rgb(high.0, high.1, high.2),
            "derecha = spectrum_high"
        );
        // Pico al tope: marca en spectrum_high que tapa la barra.
        assert_eq!(buffer[(31u16, 1u16)].fg, theme.spectrum_high);
        // Escala tenue en la última fila.
        assert_eq!(buffer[(0u16, 4u16)].fg, theme.dim, "escala en dim");
        assert_eq!(buffer[(5u16, 4u16)].fg, theme.dim, "escala en dim");
    }

    #[test]
    fn paint_vu_partial_level_uses_frac_glyph_and_dot_background() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 5);
        // 0.3 * 32 = 9.6 → 9 llenas + parcial 0.6.
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 0.3, 0.0, &theme, false);
        assert_eq!(buffer[(8u16, 0u16)].symbol(), "█", "llenas a la izquierda");
        assert_eq!(
            buffer[(9u16, 0u16)].symbol(),
            "▅",
            "parcial 0.6 = EIGHTH_BLOCKS[4], como paint_bands"
        );
        assert_eq!(buffer[(10u16, 0u16)].symbol(), " ", "vacío en unicode");
        // Sin pico no hay marca en spectrum_high.
        for x in 0..32u16 {
            assert_ne!(
                buffer[(x, 0u16)].fg,
                theme.spectrum_high,
                "sin pico no hay marca (x={x})"
            );
        }
        // ASCII: misma rampa que las barras + fondo de puntos.
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 0.3, 0.0, &theme, true);
        assert_eq!(buffer[(8u16, 0u16)].symbol(), "@", "llena ascii = @");
        assert_eq!(
            buffer[(9u16, 0u16)].symbol(),
            "+",
            "parcial 0.6 = ASCII_RAMP[6], como paint_bands"
        );
        let empty = &buffer[(20u16, 0u16)];
        assert_eq!(empty.symbol(), ".", "fondo de puntos en ascii");
        assert_eq!(empty.fg, theme.dim);
        let scale = &buffer[(0u16, 4u16)];
        assert_eq!(scale.symbol(), "|", "tics de escala ascii");
        assert_eq!(scale.fg, theme.dim);
    }

    #[test]
    fn paint_vu_peak_marks_above_bar_in_spectrum_high() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 5);
        let mut buffer = Buffer::empty(area);
        // Nivel 0.25 (8 cols) con pico en 0.5 (col 16): la marca flota sobre
        // el vacío, en todas las filas de barra.
        paint_vu(&mut buffer, area, 0.25, 0.5, &theme, false);
        for y in 0..4u16 {
            let marker = &buffer[(16u16, y)];
            assert_eq!(marker.symbol(), "█", "marca del pico (y={y})");
            assert_eq!(
                marker.fg, theme.spectrum_high,
                "el pico va en spectrum_high"
            );
        }
        assert_eq!(
            buffer[(7u16, 0u16)].symbol(),
            "█",
            "la barra sigue normal a la izquierda"
        );
        assert_eq!(
            buffer[(20u16, 0u16)].symbol(),
            " ",
            "a la derecha del pico no hay nada"
        );
    }

    #[test]
    fn paint_vu_peak_at_fractional_column_uses_full_glyph() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 5);
        // Pico en 0.26 → columna 8 con fracción > 0 (no cae en el borde
        // exacto de una celda): el medidor es horizontal, así que el glifo
        // vertical de 7/8 no aplica — queda el mismo glifo lleno que el
        // borde exacto (`band_full_glyph`).
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 0.0, 0.26, &theme, false);
        assert_eq!(
            buffer[(8u16, 0u16)].symbol(),
            "█",
            "pico a mitad de columna usa el glifo lleno (unicode)"
        );

        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 0.0, 0.26, &theme, true);
        assert_eq!(
            buffer[(8u16, 0u16)].symbol(),
            "@",
            "pico a mitad de columna usa el glifo lleno (ascii)"
        );
    }

    #[test]
    fn paint_vu_silence_shows_scale_only() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 32, 5);
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 0.0, 0.0, &theme, false);
        for y in 0..4u16 {
            for x in 0..32u16 {
                assert_eq!(
                    buffer[(x, y)].symbol(),
                    " ",
                    "silencio: barra vacía ({x},{y})"
                );
            }
        }
        assert_eq!(buffer[(0u16, 4u16)].symbol(), "┼", "tics unicode");
        assert_eq!(
            buffer[(1u16, 4u16)].symbol(),
            "─",
            "línea de escala unicode"
        );
        assert_eq!(buffer[(1u16, 4u16)].fg, theme.dim);
    }

    #[test]
    fn paint_vu_clamps_out_of_range_without_panic() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 8, 4);
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 1.5, 2.0, &theme, false);
        for y in 0..3u16 {
            assert_eq!(buffer[(7u16, y)].symbol(), "█", "clip al ancho (y={y})");
        }
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, -0.5, -1.0, &theme, true);
        for y in 0..3u16 {
            for x in 0..8u16 {
                assert_eq!(buffer[(x, y)].symbol(), ".", "negativo = vacío ({x},{y})");
            }
        }
    }

    #[test]
    fn paint_vu_tiny_heights_degrade_without_panic() {
        let theme = Theme::default();
        // 1 fila: una sola fila de barra, sin pico ni escala.
        let area = Rect::new(0, 0, 16, 1);
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 0.5, 1.0, &theme, false);
        for x in 0..8u16 {
            assert_eq!(buffer[(x, 0u16)].symbol(), "█", "media barra (x={x})");
        }
        for x in 8..16u16 {
            assert_ne!(
                buffer[(x, 0u16)].fg,
                theme.spectrum_high,
                "sin marca de pico en 1 fila (x={x})"
            );
        }
        // 2 filas: igual, sin pánico.
        let area = Rect::new(0, 0, 16, 2);
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 1.0, 1.0, &theme, false);
        for x in 0..16u16 {
            assert_eq!(buffer[(x, 0u16)].symbol(), "█", "barra arriba (x={x})");
        }
        // Área vacía: sin pánico.
        let area = Rect::new(0, 0, 0, 0);
        let mut buffer = Buffer::empty(area);
        paint_vu(&mut buffer, area, 1.0, 1.0, &theme, false);
    }

    // Anillo pulsante rasterizado celda a celda.
    // Área de referencia 40x20 (pareja): centro flotante (20.0, 10.0),
    // max_r = min(20, 20) - 0.5 = 19.5, base_r = 0.45·max_r ≈ 8.775,
    // span ≈ 10.725. Ángulo medido desde el norte, simétrico izq/derecha
    // (a = 0 arriba, a = 1 abajo); los graves (banda 0) quedan abajo.
    // ------------------------------------------------------------------

    #[test]
    fn paint_circular_silence_shows_a_closed_dim_ring_hole_as_background() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &[0.0f32; NUM_BANDS], &theme, false);

        // El hueco (celdas junto al centro) queda como fondo: sin punto
        // central, a diferencia del painter de radios anterior.
        for (x, y) in [(19u16, 9u16), (20, 9), (19, 10), (20, 10)] {
            let cell = &buffer[(x, y)];
            assert_eq!(cell.symbol(), " ", "hueco es fondo ({x},{y})");
            assert_eq!(cell.fg, Color::Reset, "hueco sin color ({x},{y})");
        }

        // Sin energía, ninguna celda lleva un color de gradiente: todo lo
        // pintado (anillo base) queda en `dim`, nunca en `spectrum_color`.
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "█" {
                    assert_eq!(cell.fg, theme.dim, "silencio: solo tenue ({x},{y})");
                }
            }
        }

        // El anillo base es continuo alrededor de todo el círculo (ángulo
        // real de cada celda tenue, 0°=norte, 90°=este, 180°=sur): cada
        // muestra cada 5° tiene una celda tenue cerca — los 32 radios de
        // antes dejaban huecos de ~11° SIN NADA, no una celda de por medio.
        let (cx, cy) = (20.0f32, 10.0f32);
        let mut dim_degrees: Vec<f32> = Vec::new();
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "█" && cell.fg == theme.dim {
                    let dx = f32::from(x) + 0.5 - cx;
                    let dy = f32::from(y) + 0.5 - cy;
                    let theta = dx.atan2(-(dy * 2.0)).to_degrees();
                    dim_degrees.push(if theta < 0.0 { theta + 360.0 } else { theta });
                }
            }
        }
        assert!(
            !dim_degrees.is_empty(),
            "el anillo base no pintó ninguna celda"
        );
        // Tolerancia 15°: a este radio, la grilla entera rasteriza el
        // círculo con algún hueco de 1 celda entre ángulos vecinos — muy
        // lejos de los ~11° COMPLETAMENTE vacíos que dejaban los 32 radios
        // discretos (ahí no había ni una celda tenue en medio).
        for deg in (0..360).step_by(5) {
            let target = deg as f32;
            let nearest = dim_degrees
                .iter()
                .map(|&d| {
                    let diff = (d - target).abs();
                    diff.min(360.0 - diff)
                })
                .fold(f32::INFINITY, f32::min);
            assert!(
                nearest <= 15.0,
                "hueco en el anillo base cerca de {deg}° (más cercana a {nearest}°)"
            );
        }
    }

    #[test]
    fn paint_circular_uniform_levels_are_approximately_circular_not_oval() {
        // Compensación del aspecto de celda (~1:2): con nivel uniforme el
        // alcance horizontal del relleno ≈ 2× el vertical (misma distancia
        // física), igual que antes pero ahora hasta `fill_r`, no `max_r`.
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &[0.6f32; NUM_BANDS], &theme, false);
        let filled = |x: u16, y: u16| {
            let cell = &buffer[(x, y)];
            cell.symbol() == "█" && cell.fg != theme.dim
        };
        let (cx, cy) = (20u16, 10u16);
        let dx_left = (0..cx).filter(|&x| filled(x, cy)).map(|x| cx - x).max();
        let dx_right = ((cx + 1)..area.width)
            .filter(|&x| filled(x, cy))
            .map(|x| x - cx)
            .max();
        let dy_up = (0..cy).filter(|&y| filled(cx, y)).map(|y| cy - y).max();
        let dy_down = ((cy + 1)..area.height)
            .filter(|&y| filled(cx, y))
            .map(|y| y - cy)
            .max();
        let (dx_left, dx_right, dy_up, dy_down) = (
            dx_left.unwrap_or(0),
            dx_right.unwrap_or(0),
            dy_up.unwrap_or(0),
            dy_down.unwrap_or(0),
        );
        assert!(
            dx_left > 0 && dx_right > 0 && dy_up > 0 && dy_down > 0,
            "hay relleno en las 4 direcciones (izq={dx_left}, der={dx_right}, \
             arr={dy_up}, aba={dy_down})"
        );
        assert!(
            dx_left.abs_diff(dx_right) <= 1 && dy_up.abs_diff(dy_down) <= 1,
            "simetría aproximada (izq={dx_left}, der={dx_right}, \
             arr={dy_up}, aba={dy_down})"
        );
        let dx = dx_left.max(dx_right);
        let dy = dy_up.max(dy_down);
        assert!(
            dx >= 2 * dy - 2 && dx <= 2 * dy + 2,
            "proporción física ~1:1 en celdas ~1:2 (dx={dx}, dy={dy})"
        );
    }

    #[test]
    fn paint_circular_uniform_level_has_no_gaps_between_bands() {
        // Anti-araña: con nivel uniforme, CADA ángulo (muestreado cada 5°)
        // tiene al menos una celda con energía entre `base_r` y `max_r` —
        // sin huecos, a diferencia de los 32 radios discretos de antes.
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &[0.6f32; NUM_BANDS], &theme, false);

        let (cx, cy) = (20.0f32, 10.0f32);
        let max_r = (f32::from(area.width) / 2.0).min(f32::from(area.height)) - 0.5;
        let base_r = 0.45 * max_r;

        for deg in (0..360).step_by(5) {
            let theta = (deg as f32).to_radians();
            let (sin, cos) = theta.sin_cos();
            let mut hit = false;
            let mut r = base_r;
            while r <= max_r {
                let fx = cx + r * sin - 0.5;
                let fy = cy - (r * cos) / 2.0 - 0.5;
                if fx >= 0.0 && fy >= 0.0 {
                    let (cell_x, cell_y) = (fx.round() as u16, fy.round() as u16);
                    if cell_x < area.width && cell_y < area.height {
                        let cell = &buffer[(cell_x, cell_y)];
                        if cell.symbol() == "█" && cell.fg != theme.dim {
                            hit = true;
                            break;
                        }
                    }
                }
                r += 0.25;
            }
            assert!(hit, "hueco en {deg}°: sin energía entre base_r y max_r");
        }
    }

    #[test]
    fn paint_circular_mirrors_left_and_right_columns() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut bands = [0.0f32; NUM_BANDS];
        for (i, level) in bands.iter_mut().enumerate() {
            *level = (i as f32 / (NUM_BANDS - 1) as f32).clamp(0.0, 1.0);
        }
        bands[3] = 0.9;
        bands[20] = 0.1; // valores bien asimétricos entre bandas vecinas.
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &bands, &theme, false);
        for x in 0..(area.width / 2) {
            let mirror = area.width - 1 - x;
            for y in 0..area.height {
                let cell = &buffer[(x, y)];
                let mirrored = &buffer[(mirror, y)];
                assert_eq!(
                    cell.symbol(),
                    mirrored.symbol(),
                    "columna {x} y su espejo {mirror} con mismo símbolo (y={y})"
                );
                assert_eq!(
                    cell.fg, mirrored.fg,
                    "columna {x} y su espejo {mirror} con mismo color (y={y})"
                );
            }
        }
    }

    #[test]
    fn paint_circular_bass_bands_extend_the_ring_at_the_bottom() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 1.0;
        bands[1] = 1.0; // graves a tope; agudos (resto) en cero.
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &bands, &theme, false);

        let is_energy = |x: u16, y: u16| {
            let cell = &buffer[(x, y)];
            cell.symbol() == "█" && cell.fg != theme.dim
        };
        let x = 19u16;
        let north = (0u16..10).filter(|&y| is_energy(x, y)).count();
        let south = (10u16..20).filter(|&y| is_energy(x, y)).count();
        assert_eq!(
            north, 0,
            "agudos en cero: nada relleno arriba, obtenido {north}"
        );
        assert!(south > 0, "graves a tope: relleno abajo, obtenido {south}");
    }

    #[test]
    fn paint_circular_radial_gradient_low_near_the_base_high_at_the_edge() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &[1.0f32; NUM_BANDS], &theme, false);

        let (cx, cy) = (20.0f32, 10.0f32);
        let max_r = (f32::from(area.width) / 2.0).min(f32::from(area.height)) - 0.5;
        let base_r = 0.45 * max_r;
        let span = max_r - base_r;
        let expected_t = |x: u16, y: u16| {
            let dx = f32::from(x) + 0.5 - cx;
            let dy = f32::from(y) + 0.5 - cy;
            let r = dx.hypot(dy * 2.0);
            ((r - base_r) / span).clamp(0.0, 1.0)
        };
        let color_at = |t: f32| {
            let (r, g, b) = spectrum_color(t, &theme);
            Color::Rgb(r, g, b)
        };

        let near = (20u16, 5u16); // r ≈ 9.01, t ≈ 0.02: junto a la base.
        let far = (20u16, 19u16); // r ≈ 19.01, t ≈ 0.95: junto al borde.
        let t_near = expected_t(near.0, near.1);
        let t_far = expected_t(far.0, far.1);
        assert!(t_near < 0.1, "celda elegida cerca de la base, t={t_near}");
        assert!(t_far > 0.9, "celda elegida cerca del borde, t={t_far}");
        assert_eq!(
            buffer[near].fg,
            color_at(t_near),
            "gradiente bajo junto a la base"
        );
        assert_eq!(
            buffer[far].fg,
            color_at(t_far),
            "gradiente alto junto al borde exterior"
        );
    }

    #[test]
    fn paint_circular_ascii_uses_full_glyph_and_dot_background() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 40, 20);
        let mut bands = [0.0f32; NUM_BANDS];
        bands[0] = 1.0;
        bands[1] = 1.0;
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &bands, &theme, true);
        let x = 19u16;
        let filled = (10u16..20).any(|y| {
            let cell = &buffer[(x, y)];
            cell.symbol() == "@" && cell.fg != theme.dim
        });
        assert!(filled, "ascii usa @ para las celdas con energía");
        let hole = &buffer[(20u16, 10u16)];
        assert_eq!(
            hole.symbol(),
            ".",
            "fondo de puntos en ascii dentro del hueco"
        );
        assert_eq!(hole.fg, theme.dim);
    }

    #[test]
    fn paint_circular_tiny_and_out_of_range_degrade_without_panic() {
        let theme = Theme::default();
        // Área vacía: sin pánico.
        let area = Rect::new(0, 0, 0, 0);
        let mut buffer = Buffer::empty(area);
        paint_circular(&mut buffer, area, &[1.0f32; NUM_BANDS], &theme, false);

        // 1x1 y 3x2: max_r < 2 degrada a solo fondo, sin pánico.
        for (w, h) in [(1u16, 1u16), (3u16, 2u16)] {
            let area = Rect::new(0, 0, w, h);
            let mut buffer = Buffer::empty(area);
            paint_circular(&mut buffer, area, &[1.0f32; NUM_BANDS], &theme, false);
            for y in 0..h {
                for x in 0..w {
                    assert_eq!(
                        buffer[(x, y)].symbol(),
                        " ",
                        "{w}x{h}: solo fondo, sin energía ({x},{y})"
                    );
                }
            }
        }

        // Nivel > 1.0 clampea igual que 1.0 exacto: mismo resultado.
        let area = Rect::new(0, 0, 40, 20);
        let mut over = [0.0f32; NUM_BANDS];
        over[0] = 1.5;
        over[1] = 1.5;
        let mut exact = [0.0f32; NUM_BANDS];
        exact[0] = 1.0;
        exact[1] = 1.0;
        let mut buffer_over = Buffer::empty(area);
        paint_circular(&mut buffer_over, area, &over, &theme, false);
        let mut buffer_exact = Buffer::empty(area);
        paint_circular(&mut buffer_exact, area, &exact, &theme, false);
        assert_eq!(
            buffer_over, buffer_exact,
            "1.5 clampeado a 1.0 pinta igual que un nivel de 1.0"
        );

        // Nivel negativo clampea a 0: no agrega relleno.
        let mut negative = [0.0f32; NUM_BANDS];
        negative[0] = -0.5;
        negative[1] = -0.5;
        let mut buffer_negative = Buffer::empty(area);
        paint_circular(&mut buffer_negative, area, &negative, &theme, false);
        let is_energy = |b: &Buffer, x: u16, y: u16| {
            let cell = &b[(x, y)];
            cell.symbol() == "█" && cell.fg != theme.dim
        };
        assert!(
            !(10u16..20).any(|y| is_energy(&buffer_negative, 19, y)),
            "nivel negativo clampea a 0: nada relleno abajo"
        );
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
        tx.send(crate::playlist::from_tracks_for_test(vec![track_named(
            "alpha",
        )]))
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
        assert_eq!(
            (panel.x, panel.width),
            (64, 36),
            "36% derecho para la lista"
        );
        assert_eq!(info.x + info.width, 64, "la info se achica antes del panel");
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
        assert_eq!(
            footer.height, 5,
            "pie con 3 líneas para los atajos del panel"
        );
        assert_eq!(
            info.y + info.height + visual.height,
            footer.y,
            "sin solapes entre zonas"
        );
    }

    #[test]
    fn go_back_closes_the_panel_and_only_quits_from_the_bare_explorer() {
        // Panel abierto: Esc cierra el panel, no sale de la app (Esc
        // es la única tecla de «back»; Tab vive en «explorer»).
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        go_back(&mut app);
        assert!(!app.explorer_open, "back cierra el panel");
        assert!(app.mode == Mode::Player, "el player de fondo queda");
        assert!(!app.should_quit, "no sale de la app");

        // Explorador a pantalla completa sin player: sale, pero con doble Esc
        // (el primero arma la guarda, el segundo sale).
        let mut bare = test_app();
        go_back(&mut bare);
        assert!(!bare.should_quit, "un Esc solo arma la salida");
        go_back(&mut bare);
        assert!(bare.should_quit, "sin player, Esc Esc sale");

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
        assert!(
            app.view.is_empty(),
            "sin coincidencias la lista queda vacia"
        );
        assert_eq!(app.selected, 0, "seleccion acotada al rango vacio");

        app.filter = None;
        refresh_view(&mut app);
        assert_eq!(
            app.view,
            vec![0, 1, 2],
            "sin filtro vuelve la lista completa"
        );
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

        handle_search_key(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            &mut app,
        );
        assert_eq!(app.search.as_deref(), Some("g"));
        assert_eq!(app.view, vec![2], "filtrado en vivo");

        handle_search_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut app,
        );
        assert_eq!(app.view, vec![0, 1, 2], "backspace amplía de vuelta");

        handle_search_key(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            &mut app,
        );
        handle_search_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut app,
        );
        handle_search_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            &mut app,
        );
        handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut app);
        assert!(app.search.is_none(), "Enter cierra el modo búsqueda");
        assert_eq!(
            app.filter.as_deref(),
            Some("bet"),
            "Enter confirma el filtro"
        );
        assert_eq!(app.view, vec![1]);

        open_search(&mut app);
        assert_eq!(
            app.search.as_deref(),
            Some("bet"),
            "reabre con el filtro actual"
        );
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
        assert_eq!(
            app.shuffle_order.len(),
            4,
            "orden mezclado de la lista visible"
        );

        app.filter = Some("ta".to_string()); // "ta" matchea beta y delta
        refresh_view(&mut app);
        assert_eq!(app.view, vec![1, 3]);
        assert_eq!(
            app.shuffle_order.len(),
            2,
            "orden re-armado al cambiar el filtro"
        );
        let mut sorted = app.shuffle_order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1], "sigue siendo una permutación");
    }

    ///: con shuffle activo el orden mezclado cubre el **pool mixto** que
    /// recorren `n`/`p` y el fin de pista (biblioteca + historial de URLs), no
    /// solo las pistas locales: si un refresco de la vista lo dejara corto, el
    /// orden quedaría a medio armar hasta el próximo salto.
    #[test]
    fn refresh_view_shuffle_order_covers_the_mixed_pool() {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_named("alpha"),
            track_named("beta"),
        ]));
        app.history.add(
            "Video de YouTube".to_string(),
            "https://www.youtube.com/watch?v=x".to_string(),
        );
        app.shuffle = true;
        refresh_view(&mut app);
        let after_refresh = app.shuffle_order.len();

        let pool = pool_positions_in_view(&mut app);
        assert_eq!(pool.len(), 3, "el pool mixto suma la URL del historial");
        assert_eq!(
            after_refresh,
            pool.len(),
            "el orden mezclado ya cubre el pool mixto al refrescar la vista"
        );
        let mut sorted = app.shuffle_order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2], "sigue siendo una permutación");
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
        // Pie del player reequilibrado en 2 líneas — restaura
        // `Tab: biblio`; `↑/↓: vol` abrevia para que entre en 78 cols.
        for hint in [
            "Tab: biblio",
            "b: buscar",
            "espacio: pausa",
            "↑/↓: vol",
            "←/→: ±5s",
            "n/p: pista",
            "s: shuffle",
            "r: repeat",
            "[/]: vel",
            "u: URL",
            "Ctrl+P: config",
            "Esc×2: salir",
        ] {
            assert!(
                player.contains(hint),
                "footer del player falta '{hint}': {player}"
            );
        }

        app.mode = Mode::Browser;
        let browser: String = footer_lines(&app, false)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        // `sig/ant` expandido (entra en 78 reagrupando), `Inicio/Fin`
        // documentado y fuera el `Esc: volver` inexacto (sin player Esc arma
        // la salida, cubierto por `Esc×2: salir`).
        for hint in [
            "PgUp/PgDn: mover",
            "Enter: reproducir",
            "n/p: pista siguiente/anterior",
            "espacio: pausa",
            "←/→: ±5s",
            "[/]: vel",
            "s: shuffle",
            "r: repeat",
            "b: buscar",
            "Inicio/Fin: primera/última",
            "u: URL",
            "Ctrl+P: config",
            "Esc×2: salir",
        ] {
            assert!(
                browser.contains(hint),
                "footer del explorador falta '{hint}': {browser}"
            );
        }
        assert!(
            !browser.contains("Esc: volver"),
            "sin `Esc: volver` inexacto: {browser}"
        );

        // Panel lateral abierto: lista + playback + aviso de volumen.
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
            "Esc/Tab: cerrar",
            "espacio: pausa",
            "←/→: ±5s",
            "[/]: vel",
            "n/p: pista siguiente/anterior",
            "u: URL",
            "Esc×2: salir",
            "volumen ↑/↓: con el panel cerrado",
        ] {
            assert!(
                panel.contains(hint),
                "footer del panel falta '{hint}': {panel}"
            );
        }
        assert_eq!(
            footer_lines(&app, false).len(),
            3,
            "el pie con panel usa sus 3 líneas"
        );
        app.explorer_open = false;

        // Pie del panel de configuración (`abrir/alternar`: en «Mouse»
        // Enter alterna, no abre sub-modal).
        open_settings_panel(&mut app);
        let settings: String = footer_lines(&app, false)
            .iter()
            .map(|line| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        for hint in ["↑/↓: mover", "Enter: abrir/alternar", "Esc: cerrar"] {
            assert!(
                settings.contains(hint),
                "pie del panel falta '{hint}': {settings}"
            );
        }
        app.settings_panel = None;

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

    #[test]
    fn library_rows_interleave_headers_and_keep_the_cursor_visible() {
        let mut app = library_two_folders();
        let lines = library_rows(&app, 40, 10);
        let texts: Vec<String> = lines.iter().map(|line| line_text(line)).collect();
        assert_eq!(texts.len(), 6, "2 encabezados + 4 pistas: {texts:?}");
        assert!(
            texts[0].contains("rock"),
            "encabezado de grupo: {:?}",
            texts[0]
        );
        assert!(
            texts[0].contains('\u{2500}'),
            "relleno de guiones: {:?}",
            texts[0]
        );
        assert_eq!(line_width(&lines[0]), 40, "el encabezado ocupa el ancho");
        assert!(
            !texts[0].contains('\u{258c}'),
            "el encabezado no selecciona"
        );
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
        // Ruta armada con componentes para que el separador sea el del sistema:
        // `folder_label` depende de `Path::file_name`, así que un literal con
        // backslash solo es una ruta con nombre propio en Windows.
        app.roots = vec![PathBuf::from("musica").join("rock")];
        assert_eq!(library_folder_label(&app), "rock");

        app.roots = vec![PathBuf::from("a"), PathBuf::from("b")];
        assert_eq!(library_folder_label(&app), "2 carpetas");
    }

    /// La forma con backslash solo es una ruta con carpeta propia en Windows;
    /// se conserva la expectativa original en la plataforma donde aplica.
    #[cfg(windows)]
    #[test]
    fn folder_label_shows_the_last_component_of_a_windows_path() {
        let mut app = test_app();
        app.roots = vec![PathBuf::from(r"C:\musica\rock")];
        assert_eq!(library_folder_label(&app), "rock");
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
        assert_eq!(url_display_label("http://localhost:8080"), "localhost:8080");
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
    // Menú de temas
    // -------------------------------------------------------------------------

    /// Serializa los tests que mutan el tema global y restaura el estado
    /// activo al salir (incluso si el test falla).

    #[test]
    fn bar_geometry_matches_build_bar_layout() {
        // "Progreso " (9) + " [" (2) => la barra empieza en la columna 11.
        let (offset, width) = bar_geometry("Progreso ", "1:00 / 3:00", 80).expect("entra");
        assert_eq!(offset, 11);
        assert_eq!(width as usize, 80 - (9 + 2 + 2 + "1:00 / 3:00".len()));
        assert!(
            bar_geometry("Progreso ", "1:00 / 3:00", 10).is_none(),
            "sin espacio"
        );
    }

    #[test]
    fn bar_hit_ratio_and_contains() {
        let bar = BarHit {
            x: 20,
            y: 6,
            width: 51,
        };
        assert!(bar.contains(20, 6) && bar.contains(70, 6));
        assert!(!bar.contains(19, 6) && !bar.contains(71, 6) && !bar.contains(30, 7));
        assert_eq!(bar.ratio(20), 0.0);
        assert_eq!(bar.ratio(70), 1.0);
        assert!((bar.ratio(45) - 0.5).abs() < 1e-9);
        assert_eq!(bar.ratio(5), 0.0, "a la izquierda se recorta a 0");
        assert_eq!(bar.ratio(200), 1.0, "a la derecha se recorta a 1");
    }

    #[test]
    fn click_on_progress_seeks_and_starts_drag() {
        let (action, drag) = mouse_action(
            &sample_hit(),
            None,
            mouse(MouseEventKind::Down(MouseButton::Left), 45, 6),
        );
        assert_eq!(drag, Some(Drag::Progress(None)));
        match action {
            MouseAction::SeekRatio(r) => assert!((r - 0.5).abs() < 1e-9),
            other => panic!("esperaba seek: {other:?}"),
        }
    }

    #[test]
    fn dragging_progress_only_seeks_on_release() {
        // Arrastrar no dispara un seek por cada movimiento (con URLs serían
        // decenas de pedidos): marca la posición y el seek llega al soltar.
        let (action, drag) = mouse_action(
            &sample_hit(),
            Some(Drag::Progress(None)),
            mouse(MouseEventKind::Drag(MouseButton::Left), 70, 12),
        );
        assert_eq!(action, MouseAction::None, "sin seek durante el arrastre");
        assert_eq!(
            drag,
            Some(Drag::Progress(Some(1.0))),
            "sigue la barra fuera de su fila"
        );

        let (action, drag) = mouse_action(
            &sample_hit(),
            drag,
            mouse(MouseEventKind::Up(MouseButton::Left), 70, 12),
        );
        assert_eq!(
            (action, drag),
            (MouseAction::SeekRatio(1.0), None),
            "soltar hace el seek"
        );

        let (action, _) = mouse_action(
            &sample_hit(),
            Some(Drag::Progress(None)),
            mouse(MouseEventKind::Up(MouseButton::Left), 70, 12),
        );
        assert_eq!(
            action,
            MouseAction::None,
            "clic sin arrastre no repite el seek"
        );
    }

    #[test]
    fn mouse_is_ignored_while_a_modal_is_open() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        *app.hit.borrow_mut() = sample_hit();
        app.url_input = Some(String::new());
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(
            engine.volume(),
            0.0,
            "el clic no atraviesa el prompt de URL"
        );
        assert_eq!(app.drag, None);
    }

    #[test]
    fn click_on_volume_sets_volume() {
        let (action, drag) = mouse_action(
            &sample_hit(),
            None,
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
        );
        assert_eq!(drag, Some(Drag::Volume));
        assert_eq!(action, MouseAction::SetVolume(1.0));
    }

    #[test]
    fn wheel_over_player_steps_volume_and_elsewhere_does_nothing() {
        let hit = sample_hit();
        let (up, _) = mouse_action(&hit, None, mouse(MouseEventKind::ScrollUp, 10, 10));
        let (down, _) = mouse_action(&hit, None, mouse(MouseEventKind::ScrollDown, 10, 10));
        assert_eq!(up, MouseAction::VolumeStep(1));
        assert_eq!(down, MouseAction::VolumeStep(-1));
        let (outside, _) = mouse_action(&hit, None, mouse(MouseEventKind::ScrollUp, 150, 40));
        assert_eq!(outside, MouseAction::None);
        let (miss, drag) = mouse_action(
            &hit,
            None,
            mouse(MouseEventKind::Down(MouseButton::Left), 2, 2),
        );
        assert_eq!(
            (miss, drag),
            (MouseAction::None, None),
            "clic fuera de las barras"
        );
    }

    /// Texto del frame de prueba: todas las filas del buffer concatenadas.

    /// La cola es visible mientras se reproduce: una línea «Siguiente: <label>»
    /// aparece solo cuando hay ítems encolados, con el título del próximo.
    #[test]
    fn player_info_shows_the_next_queued_item_while_playing() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.current_path = Some(PathBuf::from(r"C:\x\alpha.mp3"));
        app.track_label = "alpha.mp3".to_string();
        // Dos ítems encolados: el próximo es «beta» y queda 1 pendiente.
        app.queue.push(QueueItem::Local(track_named("beta")));
        app.queue.push(QueueItem::Local(track_named("gamma")));

        let buffer = render_player_frame(&app, &engine);
        let text = player_screen_text(&buffer);
        assert!(
            text.contains("Siguiente: beta (+1)"),
            "la línea con lo siguiente es visible: {text}"
        );
    }

    /// Con la cola vacía el panel se ve igual que hoy: sin fila «Siguiente» ni
    /// ningún placeholder de «0».
    #[test]
    fn player_info_hides_the_queue_line_when_queue_is_empty() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.current_path = Some(PathBuf::from(r"C:\x\alpha.mp3"));
        app.track_label = "alpha.mp3".to_string();

        let buffer = render_player_frame(&app, &engine);
        let text = player_screen_text(&buffer);
        assert!(
            !text.contains("Siguiente"),
            "sin cola no hay fila de próxima: {text}"
        );
    }

    /// con el cursor parado en el primer ítem, lo «siguiente» es el segundo
    /// de la cola (y no queda nada detrás, así que no hay «(+N)»).
    #[test]
    fn player_info_shows_the_item_after_the_queue_cursor() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.current_path = Some(PathBuf::from(r"C:\x\alpha.mp3"));
        app.track_label = "alpha.mp3".to_string();
        app.queue.push(QueueItem::Local(track_named("beta")));
        app.queue.push(QueueItem::Local(track_named("gamma")));
        app.queue_cursor = Some(0);

        let buffer = render_player_frame(&app, &engine);
        let text = player_screen_text(&buffer);
        assert!(
            text.contains("Siguiente: gamma"),
            "lo siguiente sale de la posición después del cursor: {text}"
        );
        assert!(
            !text.contains("(+1)"),
            "no queda nada detrás de gamma: {text}"
        );
    }

    /// con el cursor en el último ítem no hay nada siguiente, así que la
    /// fila desaparece igual que con la cola vacía.
    #[test]
    fn player_info_hides_the_queue_line_when_the_cursor_is_at_the_last_item() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.current_path = Some(PathBuf::from(r"C:\x\alpha.mp3"));
        app.track_label = "alpha.mp3".to_string();
        app.queue.push(QueueItem::Local(track_named("beta")));
        app.queue.push(QueueItem::Local(track_named("gamma")));
        app.queue_cursor = Some(1);

        let buffer = render_player_frame(&app, &engine);
        let text = player_screen_text(&buffer);
        assert!(
            !text.contains("Siguiente"),
            "sin nada después del cursor no hay fila de próxima: {text}"
        );
    }

    #[test]
    fn render_registers_the_volume_bar_where_it_is_drawn() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        let buffer = render_player_frame(&app, &engine);

        let bar = app
            .hit
            .borrow()
            .volume
            .expect("barra de volumen registrada");
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, bar.y)].symbol().to_string())
            .collect();
        assert!(
            row.contains("Volumen"),
            "la fila registrada es la del volumen: {row}"
        );
        assert_eq!(
            buffer[(bar.x - 1, bar.y)].symbol(),
            "[",
            "empieza justo después de «[»"
        );
        assert!(
            app.hit.borrow().player.is_some(),
            "área del reproductor registrada"
        );
    }

    #[test]
    fn clicking_the_end_of_the_volume_bar_sets_full_volume() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        render_player_frame(&app, &engine);
        let bar = app.hit.borrow().volume.expect("barra registrada");

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: bar.x + bar.width - 1,
            row: bar.y,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(click, &mut app, &mut engine);
        assert_eq!(engine.volume(), 1.0, "clic al final = 100%");
        engine.set_volume(0.0);
        assert_eq!(app.drag, Some(Drag::Volume));
    }

    #[test]
    fn transport_row_lists_buttons_in_order_with_play_state() {
        let (paused, buttons) = transport_row(false, false, RepeatMode::Off, 200);
        let kinds: Vec<Button> = buttons.iter().map(|(b, _, _)| *b).collect();
        assert_eq!(
            kinds,
            vec![
                Button::Prev,
                Button::PlayPause,
                Button::Next,
                Button::Shuffle,
                Button::Repeat
            ]
        );
        assert!(row_text(&paused).contains('▶'), "pausado muestra play");
        let (playing, _) = transport_row(true, false, RepeatMode::Off, 200);
        assert!(row_text(&playing).contains("▮▮"), "sonando muestra pausa");
    }

    #[test]
    fn transport_row_positions_match_the_drawn_text() {
        let (line, buttons) = transport_row(true, true, RepeatMode::All, 200);
        let text: Vec<char> = row_text(&line).chars().collect();
        let mut previous_end = 0;
        for (button, start, width) in &buttons {
            assert!(*width > 0, "{button:?} tiene ancho");
            assert!(*start >= previous_end, "{button:?} no se solapa");
            let label: String = text[*start as usize..(*start + *width) as usize]
                .iter()
                .collect();
            assert!(
                !label.trim().is_empty(),
                "{button:?} cubre texto visible: {label:?}"
            );
            previous_end = start + width;
        }
        let row = row_text(&line);
        assert!(
            row.contains("aleatorio") && row.contains("repetir: todo"),
            "{row}"
        );
    }

    #[test]
    fn active_toggles_are_highlighted() {
        let style_of = |line: &Line<'_>, needle: &str| {
            line.spans
                .iter()
                .find(|span| span.content.contains(needle))
                .and_then(|span| span.style.fg)
        };
        let (off, _) = transport_row(false, false, RepeatMode::Off, 200);
        let (on, _) = transport_row(false, true, RepeatMode::One, 200);
        assert_eq!(style_of(&on, "aleatorio"), Some(theme().accent));
        assert_ne!(style_of(&off, "aleatorio"), Some(theme().accent));
        assert_eq!(style_of(&on, "repetir"), Some(theme().accent));
    }

    #[test]
    fn click_on_a_button_presses_it() {
        let mut hit = sample_hit();
        hit.buttons = vec![(Rect::new(10, 8, 4, 1), Button::Next)];
        let (action, drag) = mouse_action(
            &hit,
            None,
            mouse(MouseEventKind::Down(MouseButton::Left), 12, 8),
        );
        assert_eq!((action, drag), (MouseAction::Press(Button::Next), None));
    }

    #[test]
    fn clicking_the_shuffle_button_toggles_shuffle() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        render_player_frame(&app, &engine);
        let shuffle = app
            .hit
            .borrow()
            .buttons
            .iter()
            .find(|(_, b)| *b == Button::Shuffle)
            .map(|(rect, _)| *rect)
            .expect("botón aleatorio registrado");
        handle_mouse(
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                shuffle.x,
                shuffle.y,
            ),
            &mut app,
            &mut engine,
        );
        assert!(app.shuffle, "el clic activa el aleatorio");
    }

    #[test]
    fn footer_chips_keep_the_text_and_highlight_keys() {
        let text = " s: shuffle   ←/→: ±5s   volumen ↑/↓: con el panel cerrado";
        let (spans, entries) = footer_chip_spans(text);
        let joined: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(joined, text, "el texto no cambia");
        let keycap = spans
            .iter()
            .find(|span| span.content == "s")
            .expect("la tecla es un span propio");
        assert_eq!(keycap.style.bg, Some(theme().select_bg), "keycap con fondo");
        let keys: Vec<&str> = entries.iter().map(|(_, _, k)| k.as_str()).collect();
        assert_eq!(keys, vec!["s", "←/→", "volumen ↑/↓"]);
        let (start, width, _) = &entries[0];
        assert_eq!((*start, *width), (1, "s: shuffle".chars().count() as u16));
    }

    #[test]
    fn chip_keys_resolve_to_configured_shortcuts() {
        assert_eq!(key_for_chip("s"), Some(KeyCode::Char('s')));
        assert_eq!(key_for_chip("←/→"), Some(KeyCode::Left));
        assert_eq!(key_for_chip("Esc×2"), Some(KeyCode::Esc));
        assert_eq!(key_for_chip("espacio"), Some(KeyCode::Char(' ')));
        assert_eq!(
            key_for_chip("volumen ↑/↓"),
            None,
            "texto suelto, no es un atajo"
        );
    }

    #[test]
    fn new_queue_chips_resolve_to_their_shortcuts() {
        assert_eq!(key_for_chip("a"), Some(KeyCode::Char('a')), "encolar");
        assert_eq!(
            key_for_chip("c"),
            Some(KeyCode::Char('c')),
            "Biblioteca/Cola"
        );
        assert_eq!(
            key_for_chip("d"),
            Some(KeyCode::Char('d')),
            "borrar de la cola"
        );
    }

    #[test]
    fn click_on_a_footer_chip_sends_its_key() {
        let mut hit = sample_hit();
        hit.chips = vec![(Rect::new(40, 25, 10, 1), KeyCode::Char('s'))];
        let (action, drag) = mouse_action(
            &hit,
            None,
            mouse(MouseEventKind::Down(MouseButton::Left), 42, 25),
        );
        assert_eq!((action, drag), (MouseAction::Key(KeyCode::Char('s')), None));
    }

    #[test]
    fn clicking_the_shuffle_chip_in_the_footer_toggles_shuffle() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        let buffer = render_player_frame(&app, &engine);
        let chip = app
            .hit
            .borrow()
            .chips
            .iter()
            .find(|(_, key)| *key == KeyCode::Char('s'))
            .map(|(rect, _)| *rect)
            .expect("chip «s: shuffle» registrado");
        let row: String = (chip.x..chip.x + chip.width)
            .map(|x| buffer[(x, chip.y)].symbol().to_string())
            .collect();
        assert!(
            row.starts_with("s: shuffle"),
            "el área es la del chip dibujado: {row:?}"
        );
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), chip.x, chip.y),
            &mut app,
            &mut engine,
        );
        assert!(app.shuffle);
    }

    #[test]
    fn registered_buttons_are_actually_drawn_on_small_terminals() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        for height in [8u16, 10, 12, 14] {
            let backend = ratatui::backend::TestBackend::new(100, height);
            let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
            terminal
                .draw(|frame| render(frame, &app, &engine))
                .expect("dibuja");
            let buffer = terminal.backend().buffer().clone();
            for (rect, button) in app.hit.borrow().buttons.iter() {
                assert!(rect.y < height, "{height}: {button:?} fuera del frame");
                let cell = buffer[(rect.x + 1, rect.y)].symbol().to_string();
                assert!(
                    !cell.trim().is_empty(),
                    "{height}: {button:?} registrado pero no dibujado"
                );
            }
        }
    }

    #[test]
    fn search_title_shows_a_fixed_width_box_with_theme_background() {
        let mut app = library_with_three();
        app.search = Some("ab".to_string());
        let line = library_title_line(&app, "Music", theme().warn);
        let text = line_text(&line);
        assert!(text.contains("Buscar:"), "{text}");
        assert!(text.contains("coincidencias"), "{text}");

        let boxed = find_search_box(&line).expect("la caja usa el fondo select_bg");
        assert_eq!(
            boxed.content.chars().count(),
            SEARCH_BOX_WIDTH,
            "ancho fijo"
        );
        assert!(
            boxed.content.starts_with(&format!("ab{SEARCH_CARET}")),
            "{:?}",
            boxed.content
        );
    }

    #[test]
    fn search_box_keeps_the_end_of_long_queries_visible() {
        let mut app = library_with_three();
        let long = "abcdefghijklmnopqrstuvwxyz0123456789";
        app.search = Some(long.to_string());
        let line = library_title_line(&app, "Music", theme().warn);
        let boxed = find_search_box(&line).expect("caja");
        assert_eq!(boxed.content.chars().count(), SEARCH_BOX_WIDTH);
        assert!(
            boxed.content.ends_with(&format!("789{SEARCH_CARET}")),
            "{:?}",
            boxed.content
        );
    }

    #[test]
    fn empty_search_box_shows_dim_placeholder() {
        let mut app = library_with_three();
        app.search = Some(String::new());
        let line = library_title_line(&app, "Music", theme().warn);
        let boxed = find_search_box(&line).expect("caja");
        assert!(
            boxed.content.contains(SEARCH_PLACEHOLDER),
            "{:?}",
            boxed.content
        );
        assert_eq!(boxed.style.fg, Some(theme().dim), "placeholder tenue");
        assert_eq!(boxed.content.chars().count(), SEARCH_BOX_WIDTH);
    }

    #[test]
    fn search_title_keeps_mode_indicators_and_plain_title_has_no_box() {
        let mut app = library_with_three();
        app.shuffle = true;
        app.search = Some("x".to_string());
        let searching = line_text(&library_title_line(&app, "Music", theme().warn));
        assert!(searching.contains("shuffle"), "{searching}");

        app.search = None;
        let plain = library_title_line(&app, "Music", theme().accent);
        assert!(
            find_search_box(&plain).is_none(),
            "sin búsqueda no hay caja"
        );
        assert_eq!(line_text(&plain), library_title(&app, "Music"));
    }

    #[test]
    fn quit_guard_needs_two_presses_within_the_window() {
        let t0 = Instant::now();
        let mut guard = QuitGuard::default();
        assert!(!guard.is_armed(t0), "arranca desarmado");

        assert_eq!(guard.on_quit_key(t0), QuitDecision::Arm, "primer Esc arma");
        assert!(guard.is_armed(t0 + Duration::from_millis(500)));

        assert_eq!(
            guard.on_quit_key(t0 + Duration::from_millis(800)),
            QuitDecision::Quit,
            "segundo Esc dentro de la ventana sale"
        );
    }

    #[test]
    fn quit_guard_expires_and_rearms() {
        let t0 = Instant::now();
        let mut guard = QuitGuard::default();
        guard.on_quit_key(t0);
        let late = t0 + QUIT_CONFIRM_WINDOW + Duration::from_millis(1);
        assert!(
            !guard.is_armed(late),
            "vencida la ventana ya no está armado"
        );
        assert_eq!(
            guard.on_quit_key(late),
            QuitDecision::Arm,
            "un Esc tarde vuelve a armar, no sale"
        );
    }

    #[test]
    fn quit_guard_other_key_disarms() {
        let t0 = Instant::now();
        let mut guard = QuitGuard::default();
        guard.on_quit_key(t0);
        guard.disarm();
        assert!(!guard.is_armed(t0));
        assert_eq!(guard.on_quit_key(t0), QuitDecision::Arm);
    }

    #[test]
    fn esc_in_player_arms_then_second_esc_quits() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.explorer_open = false;

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(!app.should_quit, "un solo Esc no sale");
        assert!(
            app.quit_guard.is_armed(Instant::now()),
            "queda armado con aviso"
        );
        assert!(
            line_text(&error_line(&app)).contains("Esc de nuevo"),
            "muestra el aviso: {}",
            line_text(&error_line(&app))
        );

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(app.should_quit, "el segundo Esc sale");
    }

    #[test]
    fn other_key_between_escs_cancels_the_exit() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.explorer_open = false;

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        handle_key(theme_menu_key(KeyCode::Char('s')), &mut app, &mut engine);
        assert!(
            !app.quit_guard.is_armed(Instant::now()),
            "otra tecla desarma"
        );
        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(
            !app.should_quit,
            "tras desarmar, un Esc solo vuelve a armar"
        );
    }

    #[test]
    fn q_no_longer_quits() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.explorer_open = false;
        handle_key(theme_menu_key(KeyCode::Char('q')), &mut app, &mut engine);
        handle_key(theme_menu_key(KeyCode::Char('q')), &mut app, &mut engine);
        assert!(!app.should_quit);
    }

    #[test]
    fn esc_with_explorer_panel_open_closes_it_without_arming() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(!app.explorer_open, "Esc cierra el panel");
        assert!(
            !app.quit_guard.is_armed(Instant::now()),
            "y no arma la salida"
        );
        assert!(!app.should_quit);
    }

    #[test]
    fn esc_in_fullscreen_browser_without_player_needs_two_presses() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Browser;
        app.has_player = false;
        app.explorer_open = false;

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(!app.should_quit, "un Esc no sale ni sin reproductor");
        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(app.should_quit);
    }

    #[test]
    fn b_opens_search_and_tab_toggles_explorer_from_player() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = false;

        handle_key(theme_menu_key(KeyCode::Tab), &mut app, &mut engine);
        assert!(app.explorer_open, "Tab abre el explorador");
        handle_key(theme_menu_key(KeyCode::Tab), &mut app, &mut engine);
        assert!(!app.explorer_open, "Tab lo cierra");

        handle_key(theme_menu_key(KeyCode::Char('b')), &mut app, &mut engine);
        assert!(app.search.is_some(), "b abre la búsqueda");
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
        let current = runtime::active_theme_name();
        let expected = config::builtin_themes()
            .iter()
            .position(|(id, _)| *id == current)
            .expect("el tema activo es un id builtin");
        assert_eq!(menu.selected, expected, "selección = tema actual");
        assert_eq!(menu.original_name, current, "guarda el id original");
        assert_eq!(
            menu.original_theme,
            runtime::active_theme(),
            "guarda los colores originales para Esc"
        );
        // Abrir no cambia el tema.
        assert_eq!(
            runtime::active_theme_name(),
            current,
            "solo previsualiza al mover"
        );
    }

    #[test]
    fn theme_menu_renders_inside_tiny_terminals() {
        // Más angosta que 24 columnas o más baja que 3 filas: el modal no puede
        // salirse del frame (ratatui entra en pánico al escribir fuera del buffer).
        let menu = ThemeMenuState {
            selected: 0,
            original_name: runtime::active_theme_name(),
            original_theme: runtime::active_theme(),
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
            original_name: runtime::active_theme_name(),
            original_theme: runtime::active_theme(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        let menu = app
            .theme_menu
            .as_ref()
            .expect("el menú sigue abierto al navegar");
        assert_eq!(menu.selected, 1, "↓ mueve la selección");
        let second_id = config::builtin_themes()[1].0;
        assert_eq!(
            runtime::active_theme(),
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
            runtime::active_theme(),
            config::resolve_theme(config::builtin_themes()[0].0),
            "↑ restaura la previsualización anterior"
        );
    }

    #[test]
    fn theme_menu_enter_applies_and_persists_injected_path() {
        let _setup = theme_menu_test_setup();
        let path =
            std::env::temp_dir().join(format!("mp3tui_theme_menu_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut app = test_app();
        let last = config::builtin_themes().len() - 1;
        app.theme_menu = Some(ThemeMenuState {
            selected: last,
            original_name: runtime::active_theme_name(),
            original_theme: runtime::active_theme(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |id| {
            config::persist_theme_name_at(&path, id)
        });

        let expected = config::builtin_themes()[last].0;
        assert!(app.theme_menu.is_none(), "Enter cierra el menú");
        assert_eq!(
            runtime::active_theme_name(),
            expected,
            "el id aplicado queda activo"
        );
        assert_eq!(
            runtime::active_theme(),
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

    /// aplicar el tema Miku desde el menú activa en vivo la bailarina
    /// (además del default de arranque que cubre `parse()`).
    #[test]
    fn applying_the_miku_theme_activates_the_dancer_mode_live() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        runtime::activate_visualizer_mode(config::VisualizerMode::Barras);
        let miku_index = config::builtin_themes()
            .iter()
            .position(|(id, _)| *id == "miku")
            .expect("el tema miku existe");
        app.theme_menu = Some(ThemeMenuState {
            selected: miku_index,
            original_name: runtime::active_theme_name(),
            original_theme: runtime::active_theme(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));

        assert_eq!(
            runtime::active_theme_name(),
            "miku",
            "el tema miku queda activo"
        );
        assert_eq!(
            runtime::active_visualizer_mode(),
            config::VisualizerMode::Miku,
            "aplicar miku activa la bailarina en vivo"
        );
    }

    #[test]
    fn theme_menu_enter_persist_failure_keeps_applied_theme() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        app.theme_menu = Some(ThemeMenuState {
            selected: 2,
            original_name: runtime::active_theme_name(),
            original_theme: runtime::active_theme(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |_| {
            Err("no se pudo guardar el tema: disco lleno".to_string())
        });

        let expected = config::builtin_themes()[2].0;
        assert!(app.theme_menu.is_none(), "Enter cierra igual");
        assert_eq!(
            runtime::active_theme_name(),
            expected,
            "el tema queda aplicado en la sesión pese al fallo"
        );
        assert_eq!(runtime::active_theme(), config::resolve_theme(expected));
        let err = app.error.as_deref().expect("error visible en la app");
        assert!(!err.is_empty(), "mensaje corto no vacío");
    }

    #[test]
    fn theme_menu_escape_restores_previous_theme() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        let before_name = runtime::active_theme_name();
        let before_theme = runtime::active_theme();
        app.theme_menu = Some(ThemeMenuState {
            selected: 0,
            original_name: before_name.clone(),
            original_theme: before_theme.clone(),
        });

        handle_theme_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_theme_menu_key(theme_menu_key(KeyCode::Esc), &mut app, |_| {
            panic!("Esc no debe persistir")
        });

        assert!(app.theme_menu.is_none(), "Esc cierra el menú");
        assert_eq!(
            runtime::active_theme(),
            before_theme,
            "Esc restaura los colores originales"
        );
        assert_eq!(
            runtime::active_theme_name(),
            before_name,
            "Esc restaura el id original"
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
            original_name: runtime::active_theme_name(),
            original_theme: runtime::active_theme(),
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
    fn theme_menu_lines_list_all_themes_and_mark_current() {
        let _setup = theme_menu_test_setup();
        let menu = ThemeMenuState {
            selected: 3,
            original_name: "nord".to_string(),
            original_theme: runtime::active_theme(),
        };

        let lines = theme_menu_lines(&menu);
        let texts: Vec<String> = lines.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(texts.len(), 8, "una fila por tema: {texts:?}");
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
            Some(runtime::active_theme().select_bg),
            "la fila seleccionada va resaltada"
        );
    }

    // -------------------------------------------------------------------------
    // Shell del panel de configuración (Ctrl+P)
    // -------------------------------------------------------------------------

    #[test]
    fn settings_opens_with_ctrl_p_from_player_and_list() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);

        let mut app = test_app();
        app.mode = Mode::Player;
        handle_player_key(ctrl_p, &mut app, &mut engine);
        assert!(
            app.settings_panel.is_some(),
            "Ctrl+P abre el panel desde el reproductor"
        );
        assert_eq!(
            app.settings_panel.as_ref().expect("abierto").selected,
            0,
            "arranca en la primera fila"
        );

        let mut app = test_app();
        handle_list_key(ctrl_p, &mut app, &mut engine);
        assert!(
            app.settings_panel.is_some(),
            "Ctrl+P abre el panel desde el explorador"
        );

        // La letra suelta sigue siendo pista anterior, no abre el panel.
        let mut app = test_app();
        app.mode = Mode::Player;
        handle_player_key(theme_menu_key(KeyCode::Char('p')), &mut app, &mut engine);
        assert!(
            app.settings_panel.is_none(),
            "la 'p' suelta no abre el panel"
        );
    }

    #[test]
    fn settings_panel_navigation_clamps_without_wrapping() {
        let mut app = test_app();
        open_settings_panel(&mut app);
        let selected = |app: &App| app.settings_panel.as_ref().expect("panel abierto").selected;

        // Como el menú de temas (move_selection con clamp): no envuelve.
        handle_settings_panel_key(theme_menu_key(KeyCode::Up), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 0, "↑ en la primera fila no se mueve");
        handle_settings_panel_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_settings_panel_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_settings_panel_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_settings_panel_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 3, "↓ frena en la última fila sin envolver");
        handle_settings_panel_key(theme_menu_key(KeyCode::Up), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 2, "↑ vuelve una fila");
    }

    // Enter en «Mouse» alterna el estado en vivo (el panel sigue abierto y
    // la selección intacta, como antes).
    #[test]
    fn settings_esc_closes_panel_without_arming_quit() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        open_settings_panel(&mut app);

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(app.settings_panel.is_none(), "un solo Esc cierra el panel");
        assert!(
            !app.quit_guard.is_armed(Instant::now()),
            "y no arma la salida"
        );
        assert!(!app.should_quit);

        // El doble-Esc de salida solo aplica sin modal: hacen falta dos más.
        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(!app.should_quit, "tras cerrar, un Esc solo arma");
        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(app.should_quit, "el segundo Esc sin modal sale");
    }

    #[test]
    fn settings_panel_lists_the_four_fixed_rows_with_highlight() {
        assert_eq!(
            SETTINGS_ROWS,
            ["Temas", "Mouse", "Bandas", "Modo"],
            "filas fijas (+ Modo al final, sin reordenar)"
        );
        let panel = SettingsPanelState { selected: 1 };
        let lines = settings_panel_lines(&panel, true);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts.len(), 4, "una fila por sección: {texts:?}");
        for row in SETTINGS_ROWS {
            assert!(
                texts.iter().any(|t| t.contains(row)),
                "falta la fila {row}: {texts:?}"
            );
        }
        assert_eq!(
            lines[1].spans[0].style.bg,
            Some(theme().select_bg),
            "la fila seleccionada va resaltada"
        );
        assert_ne!(
            lines[0].spans[0].style.bg,
            Some(theme().select_bg),
            "las demás filas no van resaltadas"
        );
    }

    #[test]
    fn settings_panel_renders_inside_tiny_terminals() {
        let panel = SettingsPanelState { selected: 0 };
        for (w, h) in [(10, 2), (23, 10), (40, 2), (1, 1), (0, 0)] {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
            terminal
                .draw(|frame| render_settings_panel(frame, &panel, true))
                .unwrap_or_else(|e| panic!("{w}x{h}: {e}"));
        }
    }

    #[test]
    fn footer_shows_settings_chip_in_player_and_browser() {
        let mut app = test_app();
        // Reproductor y explorador (panel + pantalla completa): chip del panel
        // sin `t` (la tecla queda libre).
        for (mode, open) in [
            (Mode::Player, false),
            (Mode::Player, true),
            (Mode::Browser, false),
        ] {
            app.mode = mode;
            app.explorer_open = open;
            let joined: String = footer_lines(&app, false)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                joined.contains("Ctrl+P: config"),
                "falta el chip en {mode:?}/{open}: {joined}"
            );
            assert!(
                !joined.contains(": temas"),
                "sin `t: temas` ({mode:?}/{open}): {joined}"
            );
            for line in footer_lines(&app, false) {
                assert!(
                    line_width(&line) <= 78,
                    "línea del pie demasiado ancha ({}): {}",
                    line_width(&line),
                    line_text(&line)
                );
            }
        }
    }

    #[test]
    fn mouse_is_inert_under_settings_panel() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        *app.hit.borrow_mut() = sample_hit();
        open_settings_panel(&mut app);
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(engine.volume(), 0.0, "el clic no atraviesa el panel");
        assert_eq!(app.drag, None);
    }

    #[test]
    fn settings_chip_click_opens_the_panel() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        assert_eq!(
            key_for_chip("Ctrl+P"),
            Some(KeyCode::Char('p')),
            "el chip resuelve a la tecla de settings"
        );
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        let buffer = render_player_frame(&app, &engine);
        let chip = app
            .hit
            .borrow()
            .chips
            .iter()
            .find(|(_, key)| *key == KeyCode::Char('p'))
            .map(|(rect, _)| *rect)
            .expect("chip «Ctrl+P: config» registrado");
        let row: String = (chip.x..chip.x + chip.width)
            .map(|x| buffer[(x, chip.y)].symbol().to_string())
            .collect();
        assert!(
            row.starts_with("Ctrl+P"),
            "el área es la del chip dibujado: {row:?}"
        );
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), chip.x, chip.y),
            &mut app,
            &mut engine,
        );
        assert!(
            app.settings_panel.is_some(),
            "clickear el chip abre el panel"
        );
    }

    #[test]
    fn handle_key_routes_to_settings_panel_when_open() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        open_settings_panel(&mut app);

        handle_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);

        assert_eq!(
            app.settings_panel.as_ref().expect("sigue abierto").selected,
            1,
            "con el panel abierto las teclas van al panel, no a la lista"
        );
        assert!(app.search.is_none());
    }

    // -------------------------------------------------------------------------
    // Temas dentro del panel + eliminar `t`
    // -------------------------------------------------------------------------

    #[test]
    fn settings_enter_on_themes_opens_theme_submodal_keeping_panel() {
        let _setup = theme_menu_test_setup();
        let mut app = test_app();
        open_settings_panel(&mut app);
        assert_eq!(
            app.settings_panel.as_ref().expect("panel abierto").selected,
            0,
            "la primera fila es Temas"
        );
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));
        assert!(
            app.settings_panel.is_some(),
            "el panel sigue abierto debajo del sub-modal"
        );
        assert!(
            app.theme_menu.is_some(),
            "Enter en Temas abre el ThemeMenuState existente"
        );
    }

    // -------------------------------------------------------------------------
    // Toggle de Mouse en vivo + persistencia de `[ui] mouse`
    // -------------------------------------------------------------------------
    // Enter en «Mouse» alterna el estado y persiste.

    #[test]
    fn settings_enter_on_mouse_toggles_state_and_persists() {
        // Enter en «Mouse» alterna el estado en el momento, deja el panel
        // abierto y persiste `[ui] mouse` (ruta temporal inyectada).
        let path = std::env::temp_dir().join(format!(
            "mp3tui_t4_mouse_toggle_{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "[ui]\nmouse = true\n").unwrap();

        let mut app = test_app();
        assert!(app.mouse_enabled, "el estado vivo arranca habilitado");
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 1;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |enabled| {
            config::persist_mouse_enabled_at(&path, enabled)
        });
        assert!(!app.mouse_enabled, "Enter apaga el mouse en el momento");
        assert!(app.settings_panel.is_some(), "el toggle no cierra el panel");
        let text = std::fs::read_to_string(&path).expect("mouse persistido");
        assert!(
            text.contains("mouse = false"),
            "persiste `[ui] mouse`: {text:?}"
        );

        // Segundo Enter lo vuelve a prender (toggle, no set).
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |enabled| {
            config::persist_mouse_enabled_at(&path, enabled)
        });
        assert!(app.mouse_enabled, "otro Enter lo vuelve a prender");
        let text = std::fs::read_to_string(&path).expect("mouse persistido");
        assert!(
            text.contains("mouse = true"),
            "persiste el re-encendido: {text:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn settings_enter_on_mouse_persist_failure_keeps_toggled_state() {
        // Como Temas/Bandas: se aplica primero y un fallo de guardado solo
        // deja el aviso, sin revertir el toggle.
        let mut app = test_app();
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 1;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| {
            Err("sin disco".to_string())
        });
        assert!(
            !app.mouse_enabled,
            "el toggle aplica aunque falle el guardado"
        );
        assert!(app.settings_panel.is_some(), "y el panel sigue abierto");
        assert_eq!(
            app.error.as_deref(),
            Some("sin disco"),
            "el fallo queda como aviso"
        );
    }

    #[test]
    fn settings_mouse_row_shows_current_state() {
        // La fila «Mouse» muestra el estado vivo: «Mouse: sí» / «Mouse: no».
        let panel = SettingsPanelState { selected: 1 };
        let on: Vec<String> = settings_panel_lines(&panel, true)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            on.iter().any(|t| t.contains("Mouse: sí")),
            "prendido se lee «Mouse: sí»: {on:?}"
        );
        let off: Vec<String> = settings_panel_lines(&panel, false)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            off.iter().any(|t| t.contains("Mouse: no")),
            "apagado se lee «Mouse: no»: {off:?}"
        );
        assert_eq!(on.len(), 4, "siguen siendo 4 filas: {on:?}");
    }

    #[test]
    fn mouse_toggle_off_stays_off_after_closing_panel() {
        // El toggle manda sobre el estado previo: apagar con el panel abierto
        // y cerrarlo no reactiva la captura.
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 1;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));
        assert!(!app.mouse_enabled, "el toggle apaga en el momento");

        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(app.settings_panel.is_none(), "Esc cierra el panel");
        assert!(!app.mouse_enabled, "cerrar el panel no reactiva la captura");

        // Y el handler lo respeta: el clic ya no actúa.
        *app.hit.borrow_mut() = sample_hit();
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(engine.volume(), 0.0, "apagado, el clic no actúa");
        assert_eq!(app.drag, None);
    }

    #[test]
    fn mouse_disabled_globally_ignores_events_even_without_modals() {
        // El estado vivo está ENCIMA del chequeo de modales: apagado, el
        // mouse se ignora aunque no haya ningún modal abierto.
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.mouse_enabled = false;
        assert!(
            app.url_input.is_none()
                && app.theme_menu.is_none()
                && app.bands_menu.is_none()
                && app.mode_menu.is_none()
                && app.settings_panel.is_none(),
            "sin modales abiertos"
        );
        *app.hit.borrow_mut() = sample_hit();
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(engine.volume(), 0.0, "apagado, el clic no fija volumen");
        assert_eq!(app.drag, None, "ni arranca arrastre");
    }

    #[test]
    fn t_key_is_free_does_nothing_in_player_and_list() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        handle_player_key(theme_menu_key(KeyCode::Char('t')), &mut app, &mut engine);
        assert!(
            app.theme_menu.is_none() && app.settings_panel.is_none(),
            "'t' queda libre en el reproductor: no abre nada"
        );
        let mut app = test_app();
        handle_list_key(theme_menu_key(KeyCode::Char('t')), &mut app, &mut engine);
        assert!(
            app.theme_menu.is_none() && app.settings_panel.is_none(),
            "'t' queda libre en la lista: no abre nada"
        );
    }

    #[test]
    fn theme_submodal_enter_saves_and_returns_to_panel() {
        let _setup = theme_menu_test_setup();
        let path = std::env::temp_dir().join(format!(
            "mp3tui_t2_submodal_enter_{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut app = test_app();
        open_settings_panel(&mut app);
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));
        let menu = app.theme_menu.as_ref().expect("sub-modal abierto");
        let expected = config::builtin_themes()[menu.selected].0;
        handle_theme_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |id| {
            config::persist_theme_name_at(&path, id)
        });
        assert!(app.theme_menu.is_none(), "Enter cierra el sub-modal");
        assert!(
            app.settings_panel.is_some(),
            "pero VUELVE al panel, no lo cierra"
        );
        assert_eq!(
            runtime::active_theme_name(),
            expected,
            "el tema queda aplicado"
        );
        let text = std::fs::read_to_string(&path).expect("tema persistido");
        assert!(
            text.contains(expected),
            "el tema se persiste como hoy: {text:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn theme_submodal_esc_restores_and_returns_to_panel() {
        let _setup = theme_menu_test_setup();
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        open_settings_panel(&mut app);
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));
        let before_name = runtime::active_theme_name();
        let before_theme = runtime::active_theme();
        // Previsualiza otro tema moviéndose dentro del sub-modal.
        handle_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);
        handle_key(theme_menu_key(KeyCode::Esc), &mut app, &mut engine);
        assert!(app.theme_menu.is_none(), "Esc cierra el sub-modal");
        assert!(
            app.settings_panel.is_some(),
            "y VUELVE al panel, no lo cierra"
        );
        assert_eq!(
            runtime::active_theme_name(),
            before_name,
            "Esc restaura el id"
        );
        assert_eq!(
            runtime::active_theme(),
            before_theme,
            "Esc restaura los colores"
        );
    }

    #[test]
    fn mouse_is_inert_under_theme_submodal() {
        // El guard inerte ya cubre el panel  y el sub-modal (existente):
        // se verifica, no se duplica.
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        *app.hit.borrow_mut() = sample_hit();
        open_settings_panel(&mut app);
        open_theme_menu(&mut app);
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(engine.volume(), 0.0, "el clic no atraviesa el sub-modal");
        assert_eq!(app.drag, None);
    }

    #[test]
    fn footer_no_longer_shows_t_temas_but_keeps_ctrl_p() {
        let mut app = test_app();
        for (mode, open) in [
            (Mode::Player, false),
            (Mode::Player, true),
            (Mode::Browser, false),
        ] {
            app.mode = mode;
            app.explorer_open = open;
            let joined: String = footer_lines(&app, false)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                !joined.contains(": temas"),
                "sin chip `t: temas` en {mode:?}/{open}: {joined}"
            );
            assert!(
                joined.contains("Ctrl+P: config"),
                "el chip del panel sigue en {mode:?}/{open}: {joined}"
            );
            for line in footer_lines(&app, false) {
                assert!(
                    line_width(&line) <= 78,
                    "línea del pie demasiado ancha ({}): {}",
                    line_width(&line),
                    line_text(&line)
                );
            }
        }
    }

    // -------------------------------------------------------------------------
    // Selector de Bandas dentro del panel + persistencia del estilo
    // -------------------------------------------------------------------------

    /// Serializa los tests que mutan el estilo activo global y lo restaura al
    /// salir (incluso si el test falla).

    #[test]
    fn settings_enter_on_bands_opens_bands_submodal_keeping_panel() {
        let _setup = bands_menu_test_setup();
        let mut app = test_app();
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 2;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));
        assert!(
            app.settings_panel.is_some(),
            "el panel sigue abierto debajo del sub-modal"
        );
        assert!(
            app.bands_menu.is_some(),
            "Enter en Bandas abre el selector de estilos"
        );
    }

    #[test]
    fn bands_submodal_navigation_clamps_and_previews_live() {
        let _setup = bands_menu_test_setup();
        runtime::activate_visualizer_style(config::VisualizerStyle::Ascii);
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_bands_menu(&mut app);
        let selected = |app: &App| app.bands_menu.as_ref().expect("sub-modal abierto").selected;
        assert_eq!(selected(&app), 1, "arranca en el estilo activo (ascii)");

        // Sin envoltura, como el menú de temas.
        handle_bands_menu_key(theme_menu_key(KeyCode::Up), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 0, "↑ va a auto");
        handle_bands_menu_key(theme_menu_key(KeyCode::Up), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 0, "↑ en la primera fila no se mueve");
        handle_bands_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_bands_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_bands_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 2, "↓ frena en unicode sin envolver");

        // Navegar previsualiza en vivo: el estilo efectivo sigue al cursor
        // (forzados ganan sin importar WT_SESSION, así no depende del env).
        assert_eq!(
            runtime::active_visualizer_style(),
            config::VisualizerStyle::Unicode
        );
        assert_eq!(
            runtime::effective_visualizer_style(),
            config::VisualizerStyle::Unicode,
            "el fondo previsualiza el estilo navegado"
        );
    }

    #[test]
    fn dispatch_routes_to_bands_submodal_when_open() {
        let _setup = bands_menu_test_setup();
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        runtime::activate_visualizer_style(config::VisualizerStyle::Auto);
        let mut app = test_app();
        app.mode = Mode::Player;
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 2;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));

        handle_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);

        assert_eq!(
            app.bands_menu.as_ref().expect("sigue abierto").selected,
            1,
            "con el sub-modal abierto las teclas van al sub-modal, no al panel"
        );
        assert_eq!(
            app.settings_panel.as_ref().expect("panel debajo").selected,
            2,
            "el panel de abajo no se mueve"
        );
    }

    #[test]
    fn bands_submodal_enter_saves_and_returns_to_panel() {
        let _setup = bands_menu_test_setup();
        let path =
            std::env::temp_dir().join(format!("mp3tui_t3_bands_enter_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        runtime::activate_visualizer_style(config::VisualizerStyle::Auto);
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_bands_menu(&mut app);
        handle_bands_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_bands_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |style| {
            config::persist_visualizer_style_at(&path, style)
        });
        assert!(app.bands_menu.is_none(), "Enter cierra el sub-modal");
        assert!(
            app.settings_panel.is_some(),
            "pero VUELVE al panel, no lo cierra"
        );
        assert_eq!(
            runtime::active_visualizer_style(),
            config::VisualizerStyle::Ascii,
            "el estilo queda aplicado"
        );
        let text = std::fs::read_to_string(&path).expect("estilo persistido");
        assert!(
            text.contains("style = \"ascii\""),
            "el estilo se persiste como hoy: {text:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bands_submodal_esc_restores_and_returns_to_panel() {
        let _setup = bands_menu_test_setup();
        runtime::activate_visualizer_style(config::VisualizerStyle::Ascii);
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_bands_menu(&mut app);
        // Previsualiza otro estilo moviéndose dentro del sub-modal.
        handle_bands_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        assert_eq!(
            runtime::active_visualizer_style(),
            config::VisualizerStyle::Unicode,
            "navegar previsualiza"
        );
        handle_bands_menu_key(theme_menu_key(KeyCode::Esc), &mut app, |_| Ok(()));
        assert!(app.bands_menu.is_none(), "Esc cierra el sub-modal");
        assert!(
            app.settings_panel.is_some(),
            "y VUELVE al panel, no lo cierra"
        );
        assert_eq!(
            runtime::active_visualizer_style(),
            config::VisualizerStyle::Ascii,
            "Esc restaura el estilo previo"
        );
    }

    #[test]
    fn mouse_is_inert_under_bands_submodal() {
        // El guard inerte cubre el panel y el sub-modal: se verifica.
        let _setup = bands_menu_test_setup();
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        *app.hit.borrow_mut() = sample_hit();
        open_settings_panel(&mut app);
        open_bands_menu(&mut app);
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(engine.volume(), 0.0, "el clic no atraviesa el sub-modal");
        assert_eq!(app.drag, None);
    }

    #[test]
    fn bands_footer_shows_preview_hints() {
        let _setup = bands_menu_test_setup();
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_bands_menu(&mut app);
        let joined: String = footer_lines(&app, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("previsualizar"),
            "el pie del sub-modal explica el preview: {joined}"
        );
        assert!(
            joined.contains("guardar"),
            "el pie del sub-modal explica Enter: {joined}"
        );
    }

    // -------------------------------------------------------------------------
    // Selector de Modo dentro del panel + fallback a
    // barras
    // -------------------------------------------------------------------------

    /// Serializa los tests que mutan el modo activo global y lo restaura al
    /// salir (incluso si el test falla).

    #[test]
    fn settings_enter_on_mode_opens_mode_submodal_keeping_panel() {
        let _setup = mode_menu_test_setup();
        let mut app = test_app();
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 3;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));
        assert!(
            app.settings_panel.is_some(),
            "el panel sigue abierto debajo del sub-modal"
        );
        assert!(
            app.mode_menu.is_some(),
            "Enter en Modo abre el selector de modos"
        );
    }

    #[test]
    fn mode_submodal_navigation_clamps_and_previews_live() {
        let _setup = mode_menu_test_setup();
        runtime::activate_visualizer_mode(config::VisualizerMode::Barras);
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_mode_menu(&mut app);
        let selected = |app: &App| app.mode_menu.as_ref().expect("sub-modal abierto").selected;
        assert_eq!(selected(&app), 0, "arranca en el modo activo (barras)");

        // Sin envoltura, como los otros selectores.
        handle_mode_menu_key(theme_menu_key(KeyCode::Up), &mut app, |_| Ok(()));
        assert_eq!(selected(&app), 0, "↑ en la primera fila no se mueve");
        for _ in 0..7 {
            handle_mode_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        }
        assert_eq!(selected(&app), 5, "↓ frena en miku sin envolver (6 modos)");

        // Navegar previsualiza en vivo: el modo activo sigue al cursor.
        assert_eq!(
            runtime::active_visualizer_mode(),
            config::VisualizerMode::Miku
        );
    }

    #[test]
    fn dispatch_routes_to_mode_submodal_when_open() {
        let _setup = mode_menu_test_setup();
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        runtime::activate_visualizer_mode(config::VisualizerMode::Barras);
        let mut app = test_app();
        app.mode = Mode::Player;
        open_settings_panel(&mut app);
        app.settings_panel.as_mut().expect("abierto").selected = 3;
        handle_settings_panel_key(theme_menu_key(KeyCode::Enter), &mut app, |_| Ok(()));

        handle_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);

        assert_eq!(
            app.mode_menu.as_ref().expect("sigue abierto").selected,
            1,
            "con el sub-modal abierto las teclas van al sub-modal, no al panel"
        );
        assert_eq!(
            app.settings_panel.as_ref().expect("panel debajo").selected,
            3,
            "el panel de abajo no se mueve"
        );
    }

    #[test]
    fn mode_submodal_enter_saves_and_returns_to_panel() {
        let _setup = mode_menu_test_setup();
        let path =
            std::env::temp_dir().join(format!("mp3tui_t1_mode_enter_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        runtime::activate_visualizer_mode(config::VisualizerMode::Barras);
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_mode_menu(&mut app);
        handle_mode_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        handle_mode_menu_key(theme_menu_key(KeyCode::Enter), &mut app, |mode| {
            config::persist_visualizer_mode_at(&path, mode)
        });
        assert!(app.mode_menu.is_none(), "Enter cierra el sub-modal");
        assert!(
            app.settings_panel.is_some(),
            "pero VUELVE al panel, no lo cierra"
        );
        assert_eq!(
            runtime::active_visualizer_mode(),
            config::VisualizerMode::Onda,
            "el modo queda aplicado"
        );
        let text = std::fs::read_to_string(&path).expect("modo persistido");
        assert!(
            text.contains("mode = \"onda\""),
            "el modo se persiste como hoy: {text:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mode_submodal_esc_restores_and_returns_to_panel() {
        let _setup = mode_menu_test_setup();
        runtime::activate_visualizer_mode(config::VisualizerMode::Barras);
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_mode_menu(&mut app);
        // Previsualiza otro modo moviéndose dentro del sub-modal.
        handle_mode_menu_key(theme_menu_key(KeyCode::Down), &mut app, |_| Ok(()));
        assert_eq!(
            runtime::active_visualizer_mode(),
            config::VisualizerMode::Onda,
            "navegar previsualiza"
        );
        handle_mode_menu_key(theme_menu_key(KeyCode::Esc), &mut app, |_| Ok(()));
        assert!(app.mode_menu.is_none(), "Esc cierra el sub-modal");
        assert!(
            app.settings_panel.is_some(),
            "y VUELVE al panel, no lo cierra"
        );
        assert_eq!(
            runtime::active_visualizer_mode(),
            config::VisualizerMode::Barras,
            "Esc restaura el modo previo"
        );
    }

    #[test]
    fn mouse_is_inert_under_mode_submodal() {
        // El guard inerte cubre el panel y el sub-modal: se verifica.
        let _setup = mode_menu_test_setup();
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        *app.hit.borrow_mut() = sample_hit();
        open_settings_panel(&mut app);
        open_mode_menu(&mut app);
        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
            &mut engine,
        );
        assert_eq!(engine.volume(), 0.0, "el clic no atraviesa el sub-modal");
        assert_eq!(app.drag, None);
    }

    #[test]
    fn mode_footer_shows_preview_hints() {
        let _setup = mode_menu_test_setup();
        let mut app = test_app();
        open_settings_panel(&mut app);
        open_mode_menu(&mut app);
        let joined: String = footer_lines(&app, false)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("previsualizar"),
            "el pie del sub-modal explica el preview: {joined}"
        );
        assert!(
            joined.contains("guardar"),
            "el pie del sub-modal explica Enter: {joined}"
        );
    }

    #[test]
    fn visualizer_paint_routes_all_modes_to_their_painters() {
        // Los 5 modos tienen pintor propio, sin fallback a barras.
        assert_eq!(
            resolve_visualizer_paint(config::VisualizerMode::Barras),
            VisualizerPaint::Bars,
            "barras recorre el path actual"
        );
        assert_eq!(
            resolve_visualizer_paint(config::VisualizerMode::Onda),
            VisualizerPaint::Wave,
            "onda va al osciloscopio, no a las barras"
        );
        assert_eq!(
            resolve_visualizer_paint(config::VisualizerMode::Espejo),
            VisualizerPaint::Mirror,
            "espejo va a su pintor simétrico, no a las barras"
        );
        assert_eq!(
            resolve_visualizer_paint(config::VisualizerMode::Vu),
            VisualizerPaint::Vu,
            "vu va a su medidor con RMS, no a las barras"
        );
        assert_eq!(
            resolve_visualizer_paint(config::VisualizerMode::Circular),
            VisualizerPaint::Circular,
            "circular va a su plot polar, no a las barras"
        );
        assert_eq!(
            resolve_visualizer_paint(config::VisualizerMode::Miku),
            VisualizerPaint::Miku,
            "miku va a su bailarina, no a las barras"
        );
        assert!(
            crate::visualizer::MIKU_POSES >= 4,
            "el arco de salto del analyzer tiene poses suficientes"
        );
    }

    /// El modo miku pinta la bailarina: avanza de pose por timer aunque haya
    /// silencio y el frame renderiza los glifos de la pose actual, también con
    /// el panel abierto a ancho chico.
    #[test]
    fn visualizer_miku_mode_paints_the_dancer_in_all_poses() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        runtime::preview_visualizer_mode(config::VisualizerMode::Miku);
        let mut analyzer = SpectrumAnalyzer::new();
        // Silencio: el timer de respiración mueve la pose sola (arco de salto).
        for _ in 0..200 {
            analyzer.tick_dance(true);
            if analyzer.dance_pose() != 0 {
                break;
            }
        }
        assert_ne!(analyzer.dance_pose(), 0, "el timer movió la pose");

        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.analyzer = analyzer;
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app, &engine))
            .expect("draw");
        let text = player_screen_text(&terminal.backend().buffer().clone());
        assert!(
            text.contains('@'),
            "la animación real de Miku está en pantalla en el estilo por defecto"
        );

        // Panel abierto a 64x24: la bailarina se recorta sin paniquear y
        // dejando el resto del frame intacto.
        app.explorer_open = true;
        let small = ratatui::backend::TestBackend::new(64, 24);
        let mut terminal = ratatui::Terminal::new(small).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app, &engine))
            .expect("draw sin panico con el panel abierto");
    }

    /// Modo `miku` en estilo unicode: dibuja la textura real de Miku
    /// (emojicombos) reducida a medios bloques, animada con el arco de salto,
    /// y se recorta sin paniquear con el panel abierto.
    #[test]
    fn visualizer_miku_unicode_paints_the_real_art_animated() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let previous = runtime::active_visualizer_style();
        runtime::preview_visualizer_style(config::VisualizerStyle::Unicode);
        runtime::preview_visualizer_mode(config::VisualizerMode::Miku);

        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        let mut analyzer = SpectrumAnalyzer::new();
        // Silencio: el timer de respiración saca la pose del reposo y activa
        // el arco de salto (el camino del beat está cubierto en los tests
        // puros de `next_dance_pose`).
        for _ in 0..25 {
            analyzer.tick_dance(true);
        }
        assert!(
            analyzer.dance_pose() != 0,
            "el timer movió la pose al arco de salto"
        );
        app.analyzer = analyzer;

        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app, &engine))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let text = player_screen_text(&buffer);
        assert!(
            text.contains('@'),
            "el fondo del lienzo (@ blancos) está en pantalla"
        );
        assert!(
            !text.contains("MIKU"),
            "la animación real, no las poses viejas"
        );
        // El fondo es @ blanco y la silueta de Miku se dibuja negra encima.
        let has_white = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                buffer[(x, y)].symbol() == "@"
                    && buffer[(x, y)].style().fg
                        == Some(ratatui::style::Color::Rgb(0xff, 0xff, 0xff))
            })
        });
        let has_black = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                buffer[(x, y)].symbol() != "@"
                    && buffer[(x, y)].style().fg
                        == Some(ratatui::style::Color::Rgb(0x00, 0x00, 0x00))
            })
        });
        assert!(has_white, "el cuerpo de Miku se dibuja sobre @ blancos");
        assert!(has_black, "la silueta oscura de Miku está sobre el fondo");

        app.explorer_open = true;
        let small = ratatui::backend::TestBackend::new(64, 24);
        let mut terminal = ratatui::Terminal::new(small).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app, &engine))
            .expect("draw sin panico en 64x24 con panel abierto");

        runtime::preview_visualizer_style(previous);
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
            Some((
                "https://example.com/fresh.m4a".to_string(),
                "Fresh Song".to_string()
            ))
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
        let playback_failed = engine.had_playback_error() || app.ytdlp_failed;
        let snapshot = EngineSnapshot::from_engine(&engine, playback_failed);
        let status = compute_status(app.ytdlp_rx.is_some(), snapshot);
        assert_eq!(
            status,
            Status::Ended,
            "non-playback app.error must not mask natural end into Stopped"
        );
    }

    /// Servidor HTTP local de un solo uso que sirve `wav` y devuelve la URL
    /// (`http://127.0.0.1:<puerto>/<filename>`). Permite que las pruebas de la
    /// ruta de URL dialen SOLO loopback, nunca un host externo. El thread
    /// acepta reconexiones del precargador y termina con el proceso.

    /// Ítem de cola con URL pendiente: se resuelve recién cuando le toca sonar.

    /// App con dos pistas locales REALES (WAV temporales) para los casos de
    /// fallback al pool; devuelve sus caminos para poder borrarlos al final.

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

        // Una pista local debe limpiar `is_url`.
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
        let t6 = std::env::temp_dir().join(format!("mp3tui_t11_urlok_{}.wav", std::process::id()));
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
        assert!(app.is_url, "la sesión sigue siendo de URL");
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

    /// La cola manda sobre el fin de pista: con un ítem encolado se reproduce
    /// ese y no se aplica `repeat`.
    #[test]
    fn test_natural_end_with_queued_item_takes_over_repeat() {
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        let dir = std::env::temp_dir();
        let actual = dir.join(format!("mp3tui_cola_actual_{}.wav", std::process::id()));
        let siguiente = dir.join(format!("mp3tui_cola_siguiente_{}.wav", std::process::id()));
        std::fs::write(&actual, make_test_wav(4410)).unwrap();
        std::fs::write(&siguiente, make_test_wav(4410)).unwrap();

        app.status = Status::Playing;
        app.repeat = RepeatMode::One;
        app.current_path = Some(actual.clone());
        let mut encolada = Track::from_path(siguiente.clone());
        encolada.title = "La que sigue".to_string();
        app.queue.push(QueueItem::Local(encolada));

        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        assert_eq!(
            app.current_path.as_ref(),
            Some(&siguiente),
            "con la cola cargada el fin de pista reproduce el ítem encolado"
        );
        assert_eq!(dispatched, None, "no se re-lanza la pista que terminó");
        assert_eq!(
            app.queue_cursor,
            Some(0),
            "el cursor de la cola queda en el ítem que sonó (S9)"
        );
        assert_eq!(
            app.queue.len(),
            1,
            "el ítem no sale de la cola: sigue visible para poder volver con p"
        );

        let _ = std::fs::remove_file(&actual);
        let _ = std::fs::remove_file(&siguiente);
    }

    /// La cola conserva el orden de encolado también con shuffle activo, y el
    /// cursor avanza en vez de consumir la lista.
    #[test]
    fn test_natural_end_with_queue_respects_enqueue_order_even_with_shuffle() {
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        let dir = std::env::temp_dir();
        let primera = dir.join(format!("mp3tui_cola_a_{}.wav", std::process::id()));
        let segunda = dir.join(format!("mp3tui_cola_b_{}.wav", std::process::id()));
        std::fs::write(&primera, make_test_wav(4410)).unwrap();
        std::fs::write(&segunda, make_test_wav(4410)).unwrap();

        app.status = Status::Playing;
        app.shuffle = true;
        let mut a = Track::from_path(primera.clone());
        a.title = "Primera".to_string();
        let mut b = Track::from_path(segunda.clone());
        b.title = "Segunda".to_string();
        app.queue.push(QueueItem::Local(a));
        app.queue.push(QueueItem::Local(b));

        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, _request| {});

        assert_eq!(
            app.current_path.as_ref(),
            Some(&primera),
            "con shuffle la cola igual respeta el orden de encolado"
        );
        assert_eq!(
            app.queue_cursor,
            Some(0),
            "el cursor queda en el primero: la cola no se consume"
        );
        assert_eq!(app.queue.len(), 2, "los dos ítems siguen en la cola");
        assert_eq!(
            app.queue.get(1).map(QueueItem::label),
            Some("Segunda"),
            "el segundo ítem espera su turno detrás del cursor"
        );

        let _ = std::fs::remove_file(&primera);
        let _ = std::fs::remove_file(&segunda);
    }

    /// Una URL encolada se resuelve recién al terminar la actual, sin salir de
    /// la cola: el cursor la marca como el ítem sonando.
    #[test]
    fn test_natural_end_with_queued_url_resolves_only_when_its_turn_comes() {
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        app.status = Status::Playing;
        app.is_url = true;
        app.queue.push(QueueItem::Url {
            url: "https://www.youtube.com/watch?v=encolada".to_string(),
            title: "Encolada".to_string(),
        });

        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        assert_eq!(
            dispatched.map(|request| request.original_url),
            Some("https://www.youtube.com/watch?v=encolada".to_string()),
            "la URL encolada se resuelve cuando se termina la actual"
        );
        assert_eq!(
            app.queue_cursor,
            Some(0),
            "el cursor queda en la URL encolada"
        );
        assert_eq!(app.queue.len(), 1, "la URL sigue en la cola");
    }

    /// con el shuffle activo: una pista local que termina encadena con
    /// la URL del historial que le sigue en el pool, pasando por el seam
    /// inyectado `replay` (no se lanza yt-dlp en el test). Con un pool de
    /// exactamente dos elementos el «siguiente» está forzado, así que no
    /// depende de la entropía del orden.
    #[test]
    fn test_shuffle_natural_end_local_chains_to_history_url_via_replay() {
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(vec![track_named(
            "alpha",
        )]));
        app.history.add(
            "Video de YouTube".to_string(),
            "https://www.youtube.com/watch?v=urlA".to_string(),
        );
        refresh_view(&mut app);
        app.shuffle = true;
        app.repeat = RepeatMode::All;
        app.is_url = false;
        app.current_path = Some(PathBuf::from(r"C:\x\alpha.mp3"));
        app.status = Status::Playing;

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        assert_eq!(
            dispatched.map(|r| r.original_url),
            Some("https://www.youtube.com/watch?v=urlA".to_string()),
            "con shuffle una pista local encadena a la URL del historial"
        );
    }

    /// con el shuffle activo: una URL que termina encadena a la OTRA
    /// URL del historial, nunca a la misma. Pool de dos URLs → el siguiente
    /// está forzado.
    #[test]
    fn test_shuffle_natural_end_url_chains_to_other_history_url() {
        let mut app = test_app();
        app.history.add(
            "A".to_string(),
            "https://www.youtube.com/watch?v=uA".to_string(),
        );
        app.history.add(
            "B".to_string(),
            "https://www.youtube.com/watch?v=uB".to_string(),
        );
        refresh_view(&mut app);
        app.shuffle = true;
        app.repeat = RepeatMode::All;
        app.is_url = true;
        app.current_url = Some((
            "https://www.youtube.com/watch?v=uA".to_string(),
            "A".to_string(),
        ));
        app.status = Status::Playing;

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        let url = dispatched.map(|r| r.original_url);
        assert_eq!(
            url.as_deref(),
            Some("https://www.youtube.com/watch?v=uB"),
            "una URL terminada encadena a la otra URL del historial"
        );
        assert_ne!(
            url.as_deref(),
            Some("https://www.youtube.com/watch?v=uA"),
            "nunca vuelve a la URL que acaba de terminar"
        );
    }

    ///: el shuffle no repite la pista que acaba de terminar. Con el
    /// orden forzado `[2, 0, 1]` y una pista local en el pool 0, el siguiente
    /// es determinista (el pool 1) y nunca vuelve a ser la pista que terminó.
    #[test]
    fn test_shuffle_natural_end_never_repeats_the_finished_track() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let pa = dir.join(format!("mp3tui_shuf_a_{}.wav", std::process::id()));
        let pb = dir.join(format!("mp3tui_shuf_b_{}.wav", std::process::id()));
        let pc = dir.join(format!("mp3tui_shuf_c_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        for p in [&pa, &pb, &pc] {
            std::fs::write(p, &wav).unwrap();
        }

        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(pa.clone(), "a"),
            track_with_path(pb.clone(), "b"),
            track_with_path(pc.clone(), "c"),
        ]));
        refresh_view(&mut app);
        app.shuffle = true;
        app.repeat = RepeatMode::Off;
        app.is_url = false;
        app.status = Status::Playing;
        // Orden explícito para que la prueba sea determinista (no entropía).
        app.shuffle_order = vec![2, 0, 1];
        app.current_path = Some(pa.clone());

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                for p in [&pa, &pb, &pc] {
                    let _ = std::fs::remove_file(p);
                }
                return;
            }
        };
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, _request| {});

        assert_ne!(
            app.current_path.as_ref(),
            Some(&pa),
            "el siguiente nunca es la pista que acaba de terminar"
        );
        assert_eq!(
            app.current_path.as_ref(),
            Some(&pb),
            "con el orden [2,0,1] y 'a' en el pool 0, el siguiente es 'b'"
        );

        for p in [&pa, &pb, &pc] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Preservación: con el shuffle APAGADO el fin natural avanza a la
    /// siguiente pista local en orden, nunca a una URL del historial, y
    /// `n`/`p` se comportan igual que antes.
    #[test]
    fn test_shuffle_off_natural_end_and_steps_stay_natural_never_url() {
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let p1 = dir.join(format!("mp3tui_off1_{}.wav", std::process::id()));
        let p2 = dir.join(format!("mp3tui_off2_{}.wav", std::process::id()));
        let p3 = dir.join(format!("mp3tui_off3_{}.wav", std::process::id()));
        let wav = make_test_wav(4410);
        for p in [&p1, &p2, &p3] {
            std::fs::write(p, &wav).unwrap();
        }

        app.library = Some(crate::playlist::from_tracks_for_test(vec![
            track_with_path(p1.clone(), "uno"),
            track_with_path(p2.clone(), "dos"),
            track_with_path(p3.clone(), "tres"),
        ]));
        // Una URL del historial adelante de la vista: no debe saltarse nunca.
        app.history.add(
            "Video".to_string(),
            "https://www.youtube.com/watch?v=vX".to_string(),
        );
        refresh_view(&mut app);
        app.repeat = RepeatMode::All;
        app.is_url = false;
        app.status = Status::Playing;
        app.current_path = Some(p1.clone());

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => {
                for p in [&p1, &p2, &p3] {
                    let _ = std::fs::remove_file(p);
                }
                return;
            }
        };
        let mut dispatched: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            dispatched = Some(request);
        });

        assert_eq!(
            dispatched, None,
            "sin shuffle el fin natural nunca dispara una URL"
        );
        assert_eq!(
            app.current_path.as_ref(),
            Some(&p2),
            "el fin natural avanza a la siguiente pista local en orden"
        );

        app.current_path = Some(p2.clone());
        play_step(&mut app, &mut engine, 1);
        assert_eq!(
            app.current_path.as_ref(),
            Some(&p3),
            "'n' avanza a la siguiente local"
        );

        app.current_path = Some(p3.clone());
        play_step(&mut app, &mut engine, -1);
        assert_eq!(
            app.current_path.as_ref(),
            Some(&p2),
            "'p' retrocede a la anterior local"
        );

        for p in [&p1, &p2, &p3] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// con shuffle activo: `n`/`p` deben recorrer el mismo pool mixto
    /// que el fin de pista. Con un pool de exactamente dos elementos (una
    /// pista local + una URL del historial) el «siguiente» está forzado; el
    /// paso tiene que despachar la URL por el seam inyectado `replay` en vez
    /// de quedar en silencio.
    #[test]
    fn test_shuffle_step_dispatches_history_url_through_replay() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        let dir = std::env::temp_dir();
        let wav_path = dir.join(format!("mp3tui_step_url_{}.wav", std::process::id()));
        std::fs::write(&wav_path, make_test_wav(4410)).unwrap();

        let local = track_with_path(wav_path.clone(), "local");
        // RED note: con play_step la URL no se despacha (quedaba en silencio);
        // el seam play_step_with la entrega al closure capturado.

        app.library = Some(crate::playlist::from_tracks_for_test(vec![local]));
        app.history.add(
            "Video de YouTube".to_string(),
            "https://www.youtube.com/watch?v=stepUrl".to_string(),
        );
        refresh_view(&mut app);
        app.shuffle = true;
        app.repeat = RepeatMode::All;
        app.current_path = Some(wav_path.clone());

        let mut captured: Option<UrlReplayRequest> = None;
        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured = Some(request);
        });

        assert_eq!(
            captured.map(|r| r.original_url),
            Some("https://www.youtube.com/watch?v=stepUrl".to_string()),
            "con shuffle 'n' encadena la URL del historial por el seam de replay"
        );

        let _ = std::fs::remove_file(wav_path);
    }

    /// «Encolar» (`a`) suma la pista seleccionada de la Biblioteca como
    /// `QueueItem::Local`, sin mover la selección ni la lista.
    #[test]
    fn enqueue_key_appends_the_selected_library_track_to_the_queue() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.selected = 1;

        handle_list_key(theme_menu_key(KeyCode::Char('a')), &mut app, &mut engine);

        assert_eq!(app.queue.len(), 1, "una pista encolada");
        assert_eq!(app.queue.first().map(QueueItem::label), Some("beta"));
        assert_eq!(app.selected, 1, "la selección de la biblioteca no se mueve");
        assert_eq!(app.view.len(), 3, "la lista no cambia");
    }

    /// Una fila del historial se encola como URL (se resuelve cuando le toca
    /// sonar), no como pista local.
    #[test]
    fn enqueue_key_adds_a_history_url_as_a_queue_url_item() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.history.add(
            "Video de YouTube".to_string(),
            "https://www.youtube.com/watch?v=x".to_string(),
        );
        refresh_view(&mut app);
        app.selected = 0;

        handle_list_key(theme_menu_key(KeyCode::Char('a')), &mut app, &mut engine);

        assert_eq!(
            app.queue.first(),
            Some(&QueueItem::Url {
                url: "https://www.youtube.com/watch?v=x".to_string(),
                title: "Video de YouTube".to_string(),
            })
        );
    }

    /// `c` alterna Biblioteca y Cola dentro del panel; cada lista conserva su
    /// propia selección.
    #[test]
    fn queue_panel_key_toggles_lists_and_keeps_two_selections() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        app.selected = 2;
        app.queue.push(QueueItem::Local(track_named("uno")));
        app.queue.push(QueueItem::Local(track_named("dos")));

        handle_list_key(theme_menu_key(KeyCode::Char('c')), &mut app, &mut engine);
        assert!(app.queue_panel, "c muestra la Cola en el panel");
        assert_eq!(app.selected, 2, "la selección de Biblioteca no se toca");

        handle_list_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);
        assert_eq!(app.queue_selected, 1, "↓ mueve la selección de la Cola");
        assert_eq!(app.selected, 2, "y no la de Biblioteca");

        handle_list_key(theme_menu_key(KeyCode::Char('c')), &mut app, &mut engine);
        assert!(!app.queue_panel, "c vuelve a Biblioteca");
        assert_eq!(app.queue_selected, 1, "la Cola conserva su selección");
    }

    /// Lista de la Cola: el resaltado (selección) es el mismo que el de la
    /// Biblioteca, el ítem parado en el cursor de reproducción se marca con
    /// `▶`, lo ya sonado queda atenuado y el estado vacío es explícito.
    #[test]
    fn queue_rows_mark_the_cursor_position_and_dim_the_played_items() {
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("primera")));
        app.queue.push(QueueItem::Url {
            url: "https://x/y".to_string(),
            title: "segunda".to_string(),
        });
        app.queue.push(QueueItem::Local(track_named("tercera")));
        app.queue_selected = 1;
        app.queue_cursor = Some(1);

        let lines = queue_rows(&app, 40, 10);
        let texts: Vec<String> = lines.iter().map(|line| line_text(line)).collect();
        assert_eq!(texts.len(), 3, "una fila por ítem: {texts:?}");
        assert!(
            texts[1].starts_with('\u{258c}'),
            "cursor de selección en el seleccionado"
        );
        assert!(texts[0].contains("primera"), "{:?}", texts[0]);
        assert!(texts[2].contains("tercera"), "{:?}", texts[2]);

        assert_eq!(
            texts[0].chars().nth(1),
            Some(' '),
            "lo ya sonado no lleva el marcador de reproducción: {:?}",
            texts[0]
        );
        assert_eq!(
            lines[0].spans[2].style.fg,
            Some(theme().dim),
            "lo ya sonado se muestra atenuado: {:?}",
            texts[0]
        );
        assert_eq!(
            texts[1].chars().nth(1),
            Some('\u{25b6}'),
            "el ítem parado en el cursor de la cola se marca: {:?}",
            texts[1]
        );
        assert_eq!(
            lines[2].spans[2].style.fg,
            Some(theme().text),
            "lo que falta suena normal: {:?}",
            texts[2]
        );

        let empty = queue_rows(&test_app(), 40, 10);
        let empty_text = line_text(&empty[0]);
        assert!(empty_text.contains("vacía"), "estado vacío: {empty_text}");
    }

    /// borrar un ítem que está antes del cursor lo corre para que siga
    /// apuntando al mismo ítem.
    #[test]
    fn queue_delete_before_the_cursor_keeps_it_on_the_same_item() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.explorer_open = true;
        app.queue_panel = true;
        app.queue.push(QueueItem::Local(track_named("uno")));
        app.queue.push(QueueItem::Local(track_named("dos")));
        app.queue.push(QueueItem::Local(track_named("tres")));
        app.queue_cursor = Some(2);
        app.queue_selected = 0;

        handle_list_key(theme_menu_key(KeyCode::Char('d')), &mut app, &mut engine);

        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(labels, vec!["dos", "tres"], "borra el seleccionado");
        assert_eq!(
            app.queue_cursor,
            Some(1),
            "el cursor sigue apuntando al mismo ítem (tres)"
        );
    }

    /// borrar el ítem que está sonando desde la cola apaga el cursor,
    /// porque ya no hay nada de la cola en reproducción.
    #[test]
    fn queue_delete_of_the_playing_item_clears_the_cursor() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.explorer_open = true;
        app.queue_panel = true;
        app.queue.push(QueueItem::Local(track_named("uno")));
        app.queue.push(QueueItem::Local(track_named("dos")));
        app.queue_cursor = Some(1);
        app.queue_selected = 1;

        handle_list_key(theme_menu_key(KeyCode::Char('d')), &mut app, &mut engine);

        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(labels, vec!["uno"], "borra el que estaba sonando");
        assert_eq!(app.queue_cursor, None, "nada de la cola está sonando");
    }

    /// el cursor de la cola nunca queda fuera de rango ni apuntando a un
    /// ítem inexistente, ni siquiera mezclando encolado, borrado, Enter, `n`,
    /// `p` y fin de pista en cualquier orden.
    #[test]
    fn queue_cursor_stays_in_range_through_mixed_operations() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let dir = std::env::temp_dir();
        let wav = make_test_wav(4410);
        let mut paths = Vec::new();
        for i in 0..5 {
            let path = dir.join(format!("mp3tui_cursor_{}_{}.wav", std::process::id(), i));
            std::fs::write(&path, &wav).unwrap();
            paths.push(path);
        }
        let mut app = test_app();
        app.library = Some(crate::playlist::from_tracks_for_test(
            paths
                .iter()
                .cloned()
                .map(|path| track_with_path(path, "t"))
                .collect(),
        ));
        refresh_view(&mut app);
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        app.queue_panel = true;
        app.status = Status::Playing;

        for i in 0..200usize {
            match i % 7 {
                0 => {
                    app.selected = i % app.view.len().max(1);
                    enqueue_selected(&mut app);
                }
                1 => play_step_with(&mut app, &mut engine, 1, |_app, _engine, _req| {}),
                2 => play_step_with(&mut app, &mut engine, -1, |_app, _engine, _req| {}),
                3 => handle_natural_end_with(&mut app, &mut engine, |_app, _engine, _req| {}),
                4 => {
                    app.queue_selected = (i * 3) % app.queue.len().max(1);
                    delete_selected_queue_item(&mut app);
                }
                5 => {
                    app.queue_selected = (i * 5) % app.queue.len().max(1);
                    play_selected_queue_item(&mut app, &mut engine);
                }
                _ => {
                    app.queue_cursor = match app.queue.len() {
                        0 => None,
                        len => Some((i * 7) % len),
                    };
                }
            }
            if let Some(cursor) = app.queue_cursor {
                assert!(
                    cursor < app.queue.len(),
                    "op {i}: cursor {cursor} fuera de rango (len {})",
                    app.queue.len()
                );
            }
            if app.queue.is_empty() {
                assert_eq!(app.queue_cursor, None, "cola vacía con cursor encendido");
            }
        }

        for path in &paths {
            let _ = std::fs::remove_file(path);
        }
    }

    /// `n` recorre la cola —avanza el cursor una posición y reproduce ese
    /// ítem— antes de caer al pool de la biblioteca.
    #[test]
    fn queue_step_next_advances_the_cursor_and_plays_that_item() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.queue.push(queue_url("uno"));
        app.queue.push(queue_url("dos"));

        let mut captured: Vec<String> = Vec::new();
        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured.push(request.original_url);
        });

        assert_eq!(
            captured,
            vec!["https://x/uno".to_string()],
            "n arranca por el primero de la cola"
        );
        assert_eq!(
            app.queue_cursor,
            Some(0),
            "el cursor queda en el ítem que sonó"
        );
        assert_eq!(app.queue.len(), 2, "la cola conserva los ítems");

        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured.push(request.original_url);
        });
        assert_eq!(
            captured,
            vec!["https://x/uno".to_string(), "https://x/dos".to_string()],
            "n vuelve a avanzar el cursor"
        );
        assert_eq!(app.queue_cursor, Some(1));
        assert_eq!(app.queue.len(), 2, "y la cola sigue entera");
    }

    /// `p` retrocede una posición del cursor y reproduce la anterior.
    #[test]
    fn queue_step_prev_goes_back_to_the_previous_queue_item() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.queue.push(queue_url("uno"));
        app.queue.push(queue_url("dos"));
        app.queue.push(queue_url("tres"));
        app.queue_cursor = Some(2);

        let mut captured: Option<UrlReplayRequest> = None;
        play_step_with(&mut app, &mut engine, -1, |_app, _engine, request| {
            captured = Some(request);
        });

        assert_eq!(
            captured.map(|request| request.original_url),
            Some("https://x/dos".to_string()),
            "p reproduce la anterior al cursor"
        );
        assert_eq!(app.queue_cursor, Some(1), "el cursor retrocede");
        assert_eq!(app.queue.len(), 3, "la cola no se consume");
    }

    /// sin nada por delante del cursor, `n` cae al pool de la biblioteca
    /// como hoy (repeat Off avanza a la siguiente) y no mueve el cursor.
    #[test]
    fn queue_step_next_at_the_end_of_the_cursor_falls_back_to_the_pool() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let (mut app, t1, t2) = queue_fallback_app();
        app.current_path = Some(t1);
        app.queue.push(queue_url("uno"));
        app.queue_cursor = Some(0);

        let mut captured: Option<UrlReplayRequest> = None;
        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured = Some(request);
        });

        assert_eq!(captured, None, "no hay nada después del cursor en la cola");
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "sin nada en la cola, n sigue avanzando la biblioteca como hoy"
        );
        assert_eq!(
            app.queue_cursor,
            Some(0),
            "el cursor de la cola no se mueve al caer al pool"
        );
        assert_eq!(app.queue.len(), 1, "la cola queda intacta");

        let _ = std::fs::remove_file(&t2);
    }

    /// el fin natural con el cursor ya en el final de la cola vuelve al
    /// fallback de (la biblioteca avanza como hoy).
    #[test]
    fn queue_natural_end_with_the_cursor_at_the_end_falls_back_to_the_pool() {
        let (mut app, t1, t2) = queue_fallback_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        app.current_path = Some(t1);
        app.status = Status::Playing;
        app.queue.push(queue_url("uno"));
        app.queue_cursor = Some(0);

        let mut captured: Option<UrlReplayRequest> = None;
        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, request| {
            captured = Some(request);
        });

        assert_eq!(captured, None, "la cola ya no tiene nada por delante");
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "el fin natural sigue avanzando la biblioteca"
        );
        assert_eq!(
            app.queue_cursor,
            Some(0),
            "el cursor de la cola no se mueve"
        );
        assert_eq!(app.queue.len(), 1, "la cola queda intacta");

        let _ = std::fs::remove_file(&t2);
    }

    /// extendido al fin natural: un ítem local de la cola que no abre en
    /// el fin de pista se saltea con aviso visible y se sigue con el próximo.
    #[test]
    fn queue_natural_end_skips_a_broken_local_with_a_visible_warning() {
        let (mut app, t1, t2) = queue_fallback_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        app.status = Status::Playing;
        app.current_path = Some(t1);
        let roto = if cfg!(windows) {
            PathBuf::from(r"C:\mp3tui_no_existe.wav")
        } else {
            PathBuf::from("/nonexistent/mp3tui_no_existe.wav")
        };
        // Un ítem roto (path inexistente) y detrás una pista real.
        app.queue
            .push(QueueItem::Local(track_with_path(roto, "roto")));
        app.queue
            .push(QueueItem::Local(track_with_path(t2.clone(), "buena")));

        handle_natural_end_with(&mut app, &mut engine, |_app, _engine, _request| {});

        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "el fin natural se saltea el ítem roto y sigue con la siguiente"
        );
        assert_eq!(app.queue_cursor, Some(1), "el cursor pasa el ítem roto");
        assert!(
            app.error
                .as_deref()
                .is_some_and(|msg| msg.contains("se saltea")),
            "el salto deja un aviso visible: {:?}",
            app.error
        );

        let _ = std::fs::remove_file(&t2);
    }

    /// sin cursor de cola todavía, `p` sigue retrocediendo el pool como
    /// hoy (la cola no tiene «anterior» desde donde está parado).
    #[test]
    fn queue_step_prev_without_a_cursor_falls_back_to_the_pool() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let (mut app, t1, t2) = queue_fallback_app();
        app.current_path = Some(t2.clone());
        app.queue.push(queue_url("uno"));

        let mut captured: Option<UrlReplayRequest> = None;
        play_step_with(&mut app, &mut engine, -1, |_app, _engine, request| {
            captured = Some(request);
        });

        assert_eq!(captured, None, "sin cursor no hay ítem de cola anterior");
        assert_eq!(app.queue_cursor, None, "el cursor sigue apagado");
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t1),
            "p sigue retrocediendo la biblioteca como hoy"
        );
        assert_eq!(app.queue.len(), 1, "la cola queda intacta");

        let _ = std::fs::remove_file(&t2);
    }

    /// Enter en la Cola salta y reproduce ya el ítem seleccionado, sin sacarlo
    /// de la cola: queda marcado como el actual para poder volver con `p`.
    #[test]
    fn queue_enter_plays_the_selected_item_and_moves_the_cursor() {
        let mut app = test_app();
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };
        let dir = std::env::temp_dir();
        let path = dir.join(format!("mp3tui_cola_enter_{}.wav", std::process::id()));
        std::fs::write(&path, make_test_wav(4410)).unwrap();

        app.explorer_open = true;
        app.queue_panel = true;
        app.queue
            .push(QueueItem::Local(track_with_path(path.clone(), "elegida")));
        app.queue.push(QueueItem::Local(track_named("queda")));

        handle_list_key(theme_menu_key(KeyCode::Enter), &mut app, &mut engine);

        assert_eq!(app.current_path.as_ref(), Some(&path), "Enter reproduce ya");
        assert_eq!(app.queue_cursor, Some(0), "el cursor queda en el elegido");
        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(
            labels,
            vec!["elegida", "queda"],
            "el ítem reproducido sigue en la cola"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// `d` borra el ítem seleccionado de la Cola y la selección cae en el
    /// siguiente.
    #[test]
    fn queue_delete_key_removes_the_selected_item() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.explorer_open = true;
        app.queue_panel = true;
        app.queue.push(QueueItem::Local(track_named("uno")));
        app.queue.push(QueueItem::Local(track_named("dos")));
        app.queue.push(QueueItem::Local(track_named("tres")));
        app.queue_selected = 1;

        handle_list_key(theme_menu_key(KeyCode::Char('d')), &mut app, &mut engine);

        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(labels, vec!["uno", "tres"], "borra el seleccionado");
        assert_eq!(app.queue_selected, 1, "la selección cae en el siguiente");
    }

    /// PRESERVE: con la lista Biblioteca activa, las flechas siguen moviendo
    /// `selected` (no la selección de la Cola).
    #[test]
    fn library_list_keys_keep_moving_the_library_selection() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = library_with_three();
        app.explorer_open = true;
        app.selected = 0;
        app.queue_selected = 0;

        handle_list_key(theme_menu_key(KeyCode::Down), &mut app, &mut engine);
        assert_eq!(app.selected, 1, "↓ mueve la Biblioteca");
        handle_list_key(theme_menu_key(KeyCode::End), &mut app, &mut engine);
        assert_eq!(app.selected, 2, "Fin va a la última de la Biblioteca");
        assert_eq!(app.queue_selected, 0, "la Cola no se toca");
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

        //...y un filtro la deja FUERA de la vista visible.
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

        // La resolución pendiente se descarta y el request se renueva.
        assert!(app.ytdlp_rx.is_none(), "pending resolution must be cleared");
        assert_ne!(
            app.ytdlp_request_id, initial_req_id,
            "request id must be bumped"
        );
        assert_ne!(
            app.status,
            Status::Resolving,
            "status must not stay Resolving"
        );

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
        app.history.add(
            "Historial Cancion 1".to_string(),
            "https://example.com/song1.mp3".to_string(),
        );
        refresh_view(&mut app);

        assert_eq!(app.view.len(), 4);
        assert_eq!(app.view[0], ViewItem::History(0));
        assert_eq!(app.view[1], ViewItem::Track(0));

        let lines = library_rows(&app, 60, 10);
        let has_history_header = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains("Agregadas por URL"))
        });
        assert!(
            has_history_header,
            "debe contener encabezado 'Agregadas por URL'"
        );

        let has_history_badge = lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("[URL] ")));
        assert!(
            has_history_badge,
            "fila de historial debe tener badge '[URL] '"
        );

        let has_history_title = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains("Historial Cancion 1"))
        });
        assert!(
            has_history_title,
            "fila de historial debe mostrar el título"
        );
    }

    #[test]
    fn test_navigation_crossing_boundary_between_history_and_local() {
        let mut app = library_with_three();
        app.history.add(
            "Historial Cancion 1".to_string(),
            "https://example.com/song1.mp3".to_string(),
        );
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
        app.history.add(
            "Bohemian Rhapsody".to_string(),
            "https://example.com/queen.mp3".to_string(),
        );
        app.history.add(
            "Yesterday".to_string(),
            "https://example.com/beatles.mp3".to_string(),
        );
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
        init_url_playback(
            &mut app,
            "https://youtube.com/watch?v=123".to_string(),
            "My Song".to_string(),
        );
        assert_eq!(app.status, Status::Connecting);

        let snapshot_error = EngineSnapshot {
            is_connecting: false,
            is_playing: false,
            is_paused: false,
            had_playback_error: true,
        };
        apply_engine_snapshot(&mut app, snapshot_error);
        assert_eq!(app.status, Status::Stopped);
        assert!(
            app.history.is_empty(),
            "history must stay empty on playback error"
        );
        assert!(app.pending_history.is_none());
    }

    #[test]
    fn test_url_playback_connecting_keeps_history_empty_until_playing() {
        let mut app = test_app();
        init_url_playback(
            &mut app,
            "https://youtube.com/watch?v=123".to_string(),
            "My Song".to_string(),
        );

        let snapshot_connecting = EngineSnapshot {
            is_connecting: true,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        apply_engine_snapshot(&mut app, snapshot_connecting);
        assert_eq!(app.status, Status::Connecting);
        assert!(
            app.history.is_empty(),
            "history must be empty while connecting"
        );

        let snapshot_playing = EngineSnapshot {
            is_connecting: false,
            is_playing: true,
            is_paused: false,
            had_playback_error: false,
        };
        apply_engine_snapshot(&mut app, snapshot_playing);
        assert_eq!(app.status, Status::Playing);
        assert_eq!(
            app.history.len(),
            1,
            "exactly one history entry recorded on playing"
        );
        assert_eq!(
            app.history.get(0).unwrap().url,
            "https://youtube.com/watch?v=123"
        );
        assert_eq!(app.history.get(0).unwrap().title, "My Song");
        assert!(app.pending_history.is_none());
    }

    #[test]
    fn test_superseded_url_playback_is_never_recorded() {
        let mut app = test_app();
        init_url_playback(
            &mut app,
            "https://example.com/A.mp3".to_string(),
            "Track A".to_string(),
        );
        init_url_playback(
            &mut app,
            "https://example.com/B.mp3".to_string(),
            "Track B".to_string(),
        );

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
            !app.history
                .entries()
                .iter()
                .any(|e| e.url == "https://example.com/A.mp3"),
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
        app.history.add(
            "Song 1".to_string(),
            "https://example.com/1.mp3".to_string(),
        );
        app.history.add(
            "Song 2".to_string(),
            "https://example.com/2.mp3".to_string(),
        );
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
        record_history_entry(
            &mut app,
            "http://user:pass@example.com/secret.mp3",
            "Secret",
        );
        assert_eq!(app.history.len(), 1);
        assert_eq!(
            app.history.get(0).unwrap().url,
            "http://example.com/secret.mp3"
        );
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

    // Seams de test para resolución y listado (no lanzan yt-dlp ni red).

    /// al listar una playlist, la primera canción se resuelve enseguida y
    /// el resto entra a la cola AL FINAL de la existente, sin tocar el cursor.
    #[test]
    fn playlist_submit_enqueues_rest_at_end_and_resolves_first() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("previa")));

        let list_calls = std::cell::RefCell::new(Vec::<String>::new());
        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        let playlist_url = "https://www.youtube.com/playlist?list=PLxyz".to_string();
        submit_url_prompt_with_seams(
            playlist_url.clone(),
            &mut app,
            &mut engine,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );

        assert_eq!(app.status, Status::Resolving);
        assert!(app.playlist_rx.is_some(), "arranca el listado de playlist");
        assert!(app.ytdlp_rx.is_none(), "todavía no resuelve ningún video");
        assert_eq!(
            app.current_url.as_ref().map(|(u, _)| u.as_str()),
            Some(playlist_url.as_str()),
            "la sesión guarda la URL de la playlist para la historia"
        );
        assert_eq!(list_calls.borrow().as_slice(), &[playlist_url.as_str()]);

        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        let req_id = app.ytdlp_request_id;
        handle_playlist_result(
            &mut app,
            req_id,
            Ok(vec![
                playlist_entry("Primera", "a"),
                playlist_entry("Segunda", "b"),
                playlist_entry("Tercera", "c"),
            ]),
            &mut resolve_seam,
        );

        // El resto va al final de la cola existente, sin tocar el cursor.
        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(labels, vec!["previa", "Segunda", "Tercera"]);
        assert_eq!(app.queue_cursor, None, "el cursor de la cola no cambia");
        // La primera se resuelve ahora, con su URL de página.
        assert!(app.ytdlp_rx.is_some());
        assert_eq!(
            resolve_calls.borrow().as_slice(),
            &["https://www.youtube.com/watch?v=a"]
        );
        assert_eq!(app.status, Status::Resolving);
    }

    /// una entrada del listado sin título (video privado/eliminado)
    /// entra a la cola mostrando su URL de página como etiqueta.
    #[test]
    fn playlist_submit_uses_the_page_url_as_label_for_untitled_entries() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        let list_calls = std::cell::RefCell::new(Vec::<String>::new());
        let playlist_url = "https://www.youtube.com/playlist?list=PLxyz".to_string();
        submit_url_prompt_with_seams(
            playlist_url.clone(),
            &mut app,
            &mut engine,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );
        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        let req_id = app.ytdlp_request_id;
        handle_playlist_result(
            &mut app,
            req_id,
            Ok(vec![
                playlist_entry("Primera", "a"),
                playlist_entry("", "b"),
                playlist_entry("Tercera", "c"),
            ]),
            &mut resolve_seam,
        );
        let labels: Vec<&str> = app.queue.iter().map(QueueItem::label).collect();
        assert_eq!(
            labels,
            vec!["https://www.youtube.com/watch?v=b", "Tercera"],
            "el título vacío se muestra como su URL de página"
        );
    }

    /// una lista vacía avisa visiblemente y NO toca la cola ni arranca la
    /// primera.
    #[test]
    fn playlist_submit_empty_list_warns_and_leaves_queue_untouched() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("previa")));

        let list_calls = std::cell::RefCell::new(Vec::<String>::new());
        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        submit_url_prompt_with_seams(
            "https://www.youtube.com/playlist?list=vacia".to_string(),
            &mut app,
            &mut engine,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );

        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        let req_id = app.ytdlp_request_id;
        handle_playlist_result(&mut app, req_id, Ok(vec![]), &mut resolve_seam);

        assert_eq!(
            app.queue.iter().map(QueueItem::label).collect::<Vec<_>>(),
            vec!["previa"],
            "la cola queda intacta"
        );
        assert_eq!(app.queue_cursor, None);
        assert_eq!(app.error.as_deref(), Some(PLAYLIST_EMPTY_MSG));
        assert!(app.ytdlp_rx.is_none(), "no arranca la primera");
        assert_eq!(resolve_calls.borrow().len(), 0);
    }

    /// un listado que falla avisa visiblemente y NO toca la cola.
    #[test]
    fn playlist_submit_listing_failure_warns_and_leaves_queue_untouched() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("previa")));

        let list_calls = std::cell::RefCell::new(Vec::<String>::new());
        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        submit_url_prompt_with_seams(
            "https://www.youtube.com/playlist?list=PLfail".to_string(),
            &mut app,
            &mut engine,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );

        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        let req_id = app.ytdlp_request_id;
        handle_playlist_result(
            &mut app,
            req_id,
            Err(crate::source::ytdlp::YtDlpError::Failed(
                "ERROR: boom".to_string(),
            )),
            &mut resolve_seam,
        );

        assert_eq!(
            app.queue.iter().map(QueueItem::label).collect::<Vec<_>>(),
            vec!["previa"],
            "la cola queda intacta"
        );
        assert_eq!(
            app.error.as_deref(),
            Some("error al ejecutar yt-dlp: ERROR: boom")
        );
        assert!(app.ytdlp_rx.is_none());
    }

    /// la resolución de la primera que falla es el error normal de URL
    /// (aviso visible) y la cola queda con lo encolado.
    #[test]
    fn playlist_first_entry_resolution_failure_keeps_queued_rest() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("previa")));

        let list_calls = std::cell::RefCell::new(Vec::<String>::new());
        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        submit_url_prompt_with_seams(
            "https://www.youtube.com/playlist?list=PLx".to_string(),
            &mut app,
            &mut engine,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );
        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        let req_id = app.ytdlp_request_id;
        handle_playlist_result(
            &mut app,
            req_id,
            Ok(vec![
                playlist_entry("Primera", "a"),
                playlist_entry("Segunda", "b"),
            ]),
            &mut resolve_seam,
        );

        // La primera no resuelve: error visible y la cola queda intacta.
        let outcome = handle_ytdlp_result(
            &mut app,
            req_id,
            Err(crate::source::ytdlp::YtDlpError::Failed(
                "ERROR: nope".to_string(),
            )),
        );
        assert_eq!(outcome, None);
        assert!(app.error.is_some());
        assert_eq!(
            app.queue.iter().map(QueueItem::label).collect::<Vec<_>>(),
            vec!["previa", "Segunda"],
            "la cola queda con lo encolado"
        );
        assert_eq!(
            app.queue_resolving, None,
            "no es una URL encolada: sin salto"
        );
    }

    /// el arranque por argumento de CLI (boot) pasa por el mismo flujo
    /// unificado y clasifica la playlist como tal.
    #[test]
    fn cli_boot_path_unifies_playlist_flow() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        let list_calls = std::cell::RefCell::new(Vec::<String>::new());
        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        let playlist_url = "https://www.youtube.com/playlist?list=PLboot".to_string();
        start_boot_url_with(
            &mut app,
            &mut engine,
            playlist_url.clone(),
            crate::source::UrlKind::Playlist,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );

        assert!(app.playlist_rx.is_some());
        assert_eq!(app.status, Status::Resolving);
        assert_eq!(
            app.current_url.as_ref().map(|(u, _)| u.as_str()),
            Some(playlist_url.as_str())
        );
        assert_eq!(list_calls.borrow().as_slice(), &[playlist_url.as_str()]);

        // Y un Page por argumento va por la resolución normal (seam unificado).
        let mut page_app = test_app();
        let page_resolve = std::cell::RefCell::new(Vec::<String>::new());
        let page_list = std::cell::RefCell::new(Vec::<String>::new());
        let page_url = "https://www.youtube.com/watch?v=xyz".to_string();
        start_boot_url_with(
            &mut page_app,
            &mut engine,
            page_url.clone(),
            crate::source::UrlKind::Page,
            empty_resolve_rx(&page_resolve),
            empty_list_rx(&page_list),
        );
        assert!(page_app.ytdlp_rx.is_some());
        assert!(page_app.playlist_rx.is_none());
        assert_eq!(page_resolve.borrow().as_slice(), &[page_url.as_str()]);
        assert!(page_list.borrow().is_empty(), "un Page no lista");
    }

    /// un listado que llega fuera de pedido (request id viejo) se
    /// descarta sin tocar la cola ni arrancar la primera.
    #[test]
    fn stale_playlist_listing_result_is_dropped() {
        let (mut app, t1, _t2) = queue_fallback_app();
        app.ytdlp_request_id = 99;
        app.queue.push(QueueItem::Local(track_named("previa")));

        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        let mut resolve_seam = empty_resolve_rx(&resolve_calls);
        handle_playlist_result(
            &mut app,
            98,
            Ok(vec![playlist_entry("Uno", "1"), playlist_entry("Dos", "2")]),
            &mut resolve_seam,
        );

        assert_eq!(
            app.queue.iter().map(QueueItem::label).collect::<Vec<_>>(),
            vec!["previa"],
            "el listado viejo no encola"
        );
        assert_eq!(app.queue_cursor, None);
        assert_eq!(resolve_calls.borrow().len(), 0, "no arranca la primera");

        let _ = std::fs::remove_file(&t1);
    }

    /// PRESERVE: la URL de página normal sigue resolviéndose en el mismo flujo
    /// de siempre, sin tocar la cola ni el listado.
    #[test]
    fn preserve_single_url_submit_resolves_without_queue_changes() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.queue.push(QueueItem::Local(track_named("unica")));
        let page_url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string();

        let resolve_calls = std::cell::RefCell::new(Vec::<String>::new());
        submit_url_prompt_with(
            page_url.clone(),
            &mut app,
            &mut engine,
            empty_resolve_rx(&resolve_calls),
        );

        assert!(app.ytdlp_rx.is_some(), "la página se resuelve");
        assert!(app.playlist_rx.is_none(), "no lista");
        assert_eq!(app.status, Status::Resolving);
        assert_eq!(app.queue_cursor, None);
        assert_eq!(
            app.queue.iter().map(QueueItem::label).collect::<Vec<_>>(),
            vec!["unica"],
            "la cola no cambia"
        );
        assert_eq!(resolve_calls.borrow().as_slice(), &[page_url.as_str()]);
    }

    /// DIAGNÓSTICO TEMPORAL: dump del frame con el panel abierto, meta + cola
    /// Regresión del reporte del usuario: con el panel lateral abierto (lista
    /// Biblioteca), `n`/`N` saltan a la siguiente pista y `p` a la anterior,
    /// igual que sin panel (los atajos de reproducción quedan activos sobre la
    /// lista).
    #[test]
    fn list_mode_step_keys_change_tracks_with_the_panel_open() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let (mut app, t1, t2) = queue_fallback_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        app.selected = 0;
        play_selected(&mut app, &mut engine);
        assert_eq!(app.current_path.as_ref(), Some(&t1));

        handle_list_key(theme_menu_key(KeyCode::Char('n')), &mut app, &mut engine);
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "n cambia a la siguiente con el panel abierto"
        );

        // Mayúsculas valen igual (el matcher ignora mayúsculas); con repeat
        // Off en la última pista n no envuelve.
        handle_list_key(theme_menu_key(KeyCode::Char('N')), &mut app, &mut engine);
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "N no envuelve con repeat Off en la última"
        );

        handle_list_key(theme_menu_key(KeyCode::Char('p')), &mut app, &mut engine);
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t1),
            "p retrocede con el panel abierto"
        );

        let _ = std::fs::remove_file(&t2);
    }

    /// Con la solapa Cola abierta, `n` avanza la cola (el cursor) desde donde
    /// está parado, igual que sin panel.
    #[test]
    fn queue_panel_step_key_advances_the_queue_cursor() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let (mut app, t1, t2) = queue_fallback_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        app.queue_panel = true;
        app.current_path = Some(t1);
        app.queue
            .push(QueueItem::Local(track_with_path(t2.clone(), "encolada")));

        handle_list_key(theme_menu_key(KeyCode::Char('n')), &mut app, &mut engine);

        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "n avanza al ítem encolado con la solapa Cola abierta"
        );
        assert_eq!(app.queue_cursor, Some(0), "el cursor avanza");

        let _ = std::fs::remove_file(&t2);
    }

    /// La fila de controles ajusta sus chips al ancho disponible: un chip que
    /// no entra completo se omite entero (nada de «rep» cortado a la mitad),
    /// y cada hit-zone queda dentro del ancho dado.
    #[test]
    fn transport_row_never_truncates_a_chip_or_overflows_the_width() {
        let (line, buttons) = transport_row(true, true, RepeatMode::All, 20);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("repito") && !text.contains("aleat"),
            "chip recortado a la mitad: {text:?}"
        );
        if let Some((_, start, width)) = buttons.last() {
            assert!(start + width <= 20, "hit-zone fuera del ancho: {buttons:?}");
        }

        let (wide, buttons_wide) = transport_row(true, true, RepeatMode::All, 60);
        let wide_text: String = wide.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            buttons_wide.len(),
            5,
            "con ancho 60 entran los cinco botones"
        );
        assert!(wide_text.contains("repetir: todo"), "{wide_text:?}");
    }

    ///: el marquee de un título que entra lo deja pegado a la izquierda
    /// con padding; el que no entra recorre una ventana en bucle, avanzando
    /// un carácter por frame.
    #[test]
    fn marquee_window_fits_short_titles_and_cycles_long_ones() {
        assert_eq!(
            marquee_window("corto", 10, 0),
            "corto     ",
            "entra y va a la izquierda"
        );
        assert_eq!(
            marquee_window("corto", 10, 42),
            "corto     ",
            "los frames no mueven un título que entra"
        );

        let title = "una canción super larga";
        let width = 8;
        let zero = marquee_window(title, width, 0);
        assert_eq!(zero.chars().count(), width, "ventana de ancho exacto");
        // Un carácter cada MARQUEE_FRAMES_PER_CHAR frames: la velocidad de
        // lectura es constante (~150 ms por carácter), no 60 chars/s.
        assert_eq!(
            marquee_window(title, width, 1),
            zero,
            "antes del paso el frame no mueve la ventana"
        );
        let one = marquee_window(title, width, super::render::MARQUEE_FRAMES_PER_CHAR);
        assert_ne!(
            one, zero,
            "al cumplirse el paso, la ventana avanza un carácter"
        );
        let cycle = title.chars().count() + 3; // título + separador
        assert_eq!(
            marquee_window(title, width, cycle * MARQUEE_FRAMES_PER_CHAR),
            zero,
            "el bucle envuelve a la posición inicial"
        );
        assert_eq!(
            marquee_window(title, 0, 7),
            marquee_window(title, 1, 7),
            "ancho 0 se trata como 1 (sin panico)"
        );
    }

    /// Con un próximo (cola o pool), el panel muestra la franja
    /// «próxima canción» con el título; sin próximo, no aparece.
    #[test]
    fn next_up_panel_section_shows_the_marquee_and_hides_without_a_next() {
        let Some(engine) = engine_or_skip() else {
            return;
        };

        // Con la cola cargada: la franja muestra el próximo de la cola.
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        app.queue.push(queue_url("próxima canción"));
        refresh_next_up(&mut app);
        let buffer = render_player_frame(&app, &engine);
        let text = player_screen_text(&buffer);
        assert!(
            text.contains("próxima canción"),
            "el título próximo aparece en el panel: {text}"
        );
        assert!(
            text.contains("Próxima canción"),
            "la etiqueta de la sección aparece en el panel"
        );

        // Con la cola vacía y biblioteca cargada: el próximo del pool.
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        refresh_next_up(&mut app);
        assert_eq!(
            app.next_up.as_deref(),
            Some("alpha"),
            "cola vacía → el primero del pool"
        );

        // Sin nada próximo (cola vacía y pool vacío): la franja no aparece.
        let mut app = test_app();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        refresh_next_up(&mut app);
        assert_eq!(app.next_up, None, "sin pool no hay próximo");
        let buffer = render_player_frame(&app, &engine);
        let text = player_screen_text(&buffer);
        assert!(
            !text.contains("próxima canción"),
            "sin próximo la franja no se dibuja: {text}"
        );
    }

    /// Fix de la franja NO desaparece mientras suena algo con pool cargado
    /// —en el borde del recorrido (repeat Off, última pista) muestra el
    /// primero del pool— y en el explorador a pantalla completa sin player no
    /// se dibuja: la sección vive sólo en el panel de Tab.
    #[test]
    fn next_up_wraps_at_the_list_end_and_is_absent_in_browser_mode() {
        let Some(engine) = engine_or_skip() else {
            return;
        };

        // Última pista sonando con repeat Off: el próximo del panel = el
        // primero del pool (el avance real de `n` sigue respetando repeat).
        let mut app = library_with_three();
        app.mode = Mode::Player;
        app.has_player = true;
        app.explorer_open = true;
        app.status = Status::Playing;
        app.current_path = Some(PathBuf::from(r"C:\x\gamma.mp3"));
        app.track_label = "gamma.mp3".to_string();
        refresh_next_up(&mut app);
        assert_eq!(
            app.next_up.as_deref(),
            Some("alpha"),
            "en el borde la franja muestra el primero del pool"
        );

        // Modo Browser (sin sesión de audio): el pool existe pero la franja no
        // se dibuja (solo vive en el panel lateral de Tab).
        let mut app = library_with_three();
        app.mode = Mode::Browser;
        refresh_next_up(&mut app);
        assert!(app.next_up.is_some(), "hay próximo en el pool");
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).expect("term");
        terminal
            .draw(|frame| render(frame, &app, &engine))
            .expect("draw");
        let text = player_screen_text(&terminal.backend().buffer().clone());
        assert!(
            !text.contains("Próxima canción"),
            "sin player la franja no aparece (solo en el panel de Tab)"
        );
    }

    /// una URL encolada que falla al resolver se saltea — el cursor
    /// avanza al próximo ítem de la cola y lo intenta (erst la URL siguiente).
    #[test]
    fn queue_url_failure_skips_to_next_queue_item() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.queue.push(queue_url("uno"));
        app.queue.push(queue_url("dos"));

        let mut captured: Vec<String> = Vec::new();
        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured.push(request.original_url);
        });
        assert_eq!(captured, vec!["https://x/uno".to_string()]);
        assert_eq!(app.queue_cursor, Some(0));
        assert_eq!(
            app.queue_resolving,
            Some(0),
            "la primera está en resolución"
        );

        // La resolución de "uno" falla: se saltea, se intenta "dos" y el
        // cursor avanza a "dos" (el ítem fallido queda detrás, atenuado).
        skip_failed_queue_item(&mut app, &mut engine, |_app, _engine, request| {
            captured.push(request.original_url);
        });
        assert_eq!(
            captured,
            vec!["https://x/uno".to_string(), "https://x/dos".to_string()]
        );
        assert_eq!(
            app.queue_cursor,
            Some(1),
            "el cursor avanza al próximo ítem"
        );
        assert_eq!(app.queue_resolving, Some(1), "'dos' queda en resolución");
        assert_eq!(app.queue.len(), 2, "el ítem fallido sigue en la cola");
        assert_eq!(
            app.error.as_deref(),
            Some(QUEUE_ITEM_FAILED_MSG),
            "aviso visible al saltear"
        );
    }

    /// cuando la cola se agota tras saltar un ítem fallido, vuelve al
    /// fallback de (el pool de la biblioteca avanza como hoy).
    #[test]
    fn queue_url_failure_exhausted_falls_back_to_pool() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let (mut app, t1, t2) = queue_fallback_app();
        app.current_path = Some(t1.clone());
        app.queue.push(queue_url("unica"));

        let mut captured: Vec<String> = Vec::new();
        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured.push(request.original_url);
        });
        assert_eq!(captured, vec!["https://x/unica".to_string()]);
        assert_eq!(app.queue_cursor, Some(0));
        assert_eq!(app.queue_resolving, Some(0));

        // La única URL encolada falla y la cola queda agotada: fallback al pool.
        skip_failed_queue_item(&mut app, &mut engine, |_app, _engine, request| {
            captured.push(request.original_url);
        });
        assert_eq!(captured.len(), 1, "no hay más ítems de cola que intentar");
        assert_eq!(
            app.current_path.as_ref(),
            Some(&t2),
            "la cola agotada cae al pool y avanza la biblioteca (S5/S6)"
        );
        assert_eq!(
            app.queue_cursor,
            Some(0),
            "el cursor queda en el ítem fallido"
        );
        assert_eq!(app.queue.len(), 1);

        let _ = std::fs::remove_file(&t1);
        let _ = std::fs::remove_file(&t2);
    }

    /// un ítem LOCAL de la cola que no arranca se saltea igual, con
    /// aviso, y se intenta el próximo.
    #[test]
    fn queue_local_failure_is_skipped_and_next_attempted() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        // Ruta local inexistente: start_local_playback falla y se saltea.
        let mut missing = track_named("no-existe");
        missing.path = PathBuf::from(r"C:\tmp\no_existe_esta_carpeta\x.mp3");
        app.queue.push(QueueItem::Local(missing));
        app.queue.push(queue_url("siguiente"));

        let mut captured: Vec<String> = Vec::new();
        play_step_with(&mut app, &mut engine, 1, |_app, _engine, request| {
            captured.push(request.original_url);
        });

        assert_eq!(
            captured,
            vec!["https://x/siguiente".to_string()],
            "el local fallido se saltea y se intenta la URL siguiente"
        );
        assert_eq!(app.queue_cursor, Some(1), "el cursor pasa el ítem fallido");
    }
}
