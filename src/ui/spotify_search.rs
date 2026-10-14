//! Búsqueda de Spotify integrada a la UI: un modal con campo de texto y lista
//! de resultados (canciones y playlists). La red corre en un hilo aparte
//! (mismo patrón que `spawn_ytdlp_*`): el hilo de la UI solo manda el pedido y
//! recoge el resultado por un channel con `request_id`.

use super::*;

use crate::source::spotify::client::SearchResults;
use crate::source::spotify::play::{self, ConnectError, SearchSource};

/// Aviso cuando el hilo de búsqueda muere sin contestar.
const SEARCH_ABORTED_MSG: &str = "La búsqueda terminó de forma inesperada; probá de nuevo";

/// Un resultado elegible de la búsqueda.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SearchHit {
    /// Canción: `label` = «Artistas - Título».
    Track { id: String, label: String },
    /// Playlist: `label` = nombre, `detail` = dueño y cantidad de canciones.
    Playlist {
        id: String,
        label: String,
        detail: Option<String>,
    },
}

impl SearchHit {
    /// Marcador que entiende el flujo de URLs (`classify` lo reconoce).
    pub(super) fn marker(&self) -> String {
        match self {
            Self::Track { id, .. } => play::track_marker(id),
            Self::Playlist { id, .. } => format!("spotify:playlist:{id}"),
        }
    }

    pub(super) fn label(&self) -> &str {
        match self {
            Self::Track { label, .. } | Self::Playlist { label, .. } => label,
        }
    }
}

/// Etapa del modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SearchPhase {
    /// Escribiendo la consulta.
    Input,
    /// Pedido en vuelo.
    Searching,
    /// Resultados navegables.
    Results {
        hits: Vec<SearchHit>,
        selected: usize,
    },
    /// La búsqueda falló: mensaje listo para mostrar.
    Failed(String),
}

/// Estado del modal de búsqueda de Spotify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SpotifySearch {
    pub(super) query: String,
    pub(super) phase: SearchPhase,
}

/// Mensaje del hilo de búsqueda: id de pedido, consulta y resultado (el error
/// ya es un texto para pantalla).
pub(super) type SpotifySearchMessage = (u64, String, Result<Vec<SearchHit>, String>);

/// Qué hacer con el texto enviado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchRoute {
    /// Vacío: no hace nada.
    Nothing,
    /// URL o ruta local existente: flujo de siempre del prompt de URL.
    Open,
    /// Texto libre: búsqueda en Spotify.
    Query,
}

/// Clasifica el texto del campo: una URL (de cualquier tipo) o una ruta que
/// existe va al flujo de siempre; todo lo demás es una consulta.
pub(super) fn route_search_input(input: &str) -> SearchRoute {
    match classify_submit_input(input) {
        SubmitAction::Empty => SearchRoute::Nothing,
        SubmitAction::Local(path) if !path.exists() => SearchRoute::Query,
        _ => SearchRoute::Open,
    }
}

/// Ordena los resultados de la API para la lista: canciones primero y
/// playlists después; descarta los que no tienen id.
pub(super) fn hits_from(results: SearchResults) -> Vec<SearchHit> {
    let tracks = results
        .tracks
        .iter()
        .filter(|track| !track.id.is_empty())
        .map(|track| SearchHit::Track {
            id: track.id.clone(),
            label: play::track_label(track),
        });
    let playlists = results
        .playlists
        .into_iter()
        .filter(|playlist| !playlist.id.is_empty())
        .map(|playlist| SearchHit::Playlist {
            detail: playlist_detail(playlist.owner.as_deref(), playlist.tracks_total),
            id: playlist.id,
            label: playlist.name,
        });
    tracks.chain(playlists).collect()
}

/// «Dueño · N canciones», con lo que haya.
fn playlist_detail(owner: Option<&str>, total: Option<u64>) -> Option<String> {
    let count = total.map(|n| match n {
        1 => "1 canción".to_string(),
        n => format!("{n} canciones"),
    });
    match (owner, count) {
        (Some(owner), Some(count)) => Some(format!("{owner} · {count}")),
        (Some(owner), None) => Some(owner.to_string()),
        (None, count) => count,
    }
}

/// Cuerpo del hilo de búsqueda con el cliente inyectado.
pub(super) fn run_spotify_search(
    query: &str,
    connect: &dyn Fn() -> Result<Box<dyn SearchSource>, ConnectError>,
) -> Result<Vec<SearchHit>, String> {
    play::search_catalog(query, connect).map(hits_from)
}

fn connect_search() -> Result<Box<dyn SearchSource>, ConnectError> {
    play::connect(crate::config::config()).map(|client| Box::new(client) as Box<dyn SearchSource>)
}

/// Único punto que lanza la búsqueda de Spotify: devuelve el receiver del
/// resultado. Los tests inyectan un doble y nunca tocan la red.
pub(super) fn spawn_spotify_search(query: String, req_id: u64) -> Receiver<SpotifySearchMessage> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = run_spotify_search(&query, &connect_search);
        let _ = tx.send((req_id, query, result));
    });
    rx
}

pub(super) fn open_spotify_search(app: &mut App) {
    app.spotify_search = Some(SpotifySearch {
        query: String::new(),
        phase: SearchPhase::Input,
    });
}

/// Descarta la búsqueda en vuelo: su resultado, si llega, ya no coincide con
/// el id vigente.
pub(super) fn cancel_spotify_search(app: &mut App) {
    app.spotify_search_id = app.spotify_search_id.wrapping_add(1);
    app.spotify_search_rx = None;
}

fn close_spotify_search(app: &mut App) {
    cancel_spotify_search(app);
    app.spotify_search = None;
}

/// Aplica el resultado de un hilo de búsqueda al modal.
pub(super) fn handle_spotify_search_result(
    app: &mut App,
    request_id: u64,
    result: Result<Vec<SearchHit>, String>,
) {
    if request_id != app.spotify_search_id {
        return;
    }
    let Some(search) = app.spotify_search.as_mut() else {
        return;
    };
    if search.phase != SearchPhase::Searching {
        return;
    }
    search.phase = match result {
        Ok(hits) => SearchPhase::Results { hits, selected: 0 },
        Err(message) => SearchPhase::Failed(message),
    };
}

/// Recoge el resultado del hilo de búsqueda sin bloquear (lo llama el event
/// loop en cada frame).
pub(super) fn poll_spotify_search(app: &mut App) {
    let Some(rx) = &app.spotify_search_rx else {
        return;
    };
    match rx.try_recv() {
        Ok((request_id, _query, result)) => {
            app.spotify_search_rx = None;
            handle_spotify_search_result(app, request_id, result);
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            app.spotify_search_rx = None;
            if let Some(search) = app.spotify_search.as_mut()
                && search.phase == SearchPhase::Searching
            {
                search.phase = SearchPhase::Failed(SEARCH_ABORTED_MSG.to_string());
            }
        }
    }
}

/// Pegado de texto en el campo (solo mientras se escribe la consulta).
pub(super) fn paste_into_spotify_search(app: &mut App, pasted: &str) {
    if let Some(search) = app.spotify_search.as_mut()
        && search.phase == SearchPhase::Input
    {
        handle_prompt_paste(&mut search.query, pasted);
    }
}

/// Teclas del modal con los efectos externos inyectados: `spawn_search`
/// lanza la búsqueda y `resolve_page` / `list_playlist` arrancan la
/// reproducción de lo elegido. Devuelve `Some(texto)` cuando el texto enviado
/// es una URL o ruta: lo resuelve el flujo de siempre (el llamador).
pub(super) fn handle_spotify_search_key_with(
    key: KeyEvent,
    app: &mut App,
    spawn_search: impl FnOnce(String, u64) -> Receiver<SpotifySearchMessage>,
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
    list_playlist: impl FnOnce(String, u64) -> Receiver<YtDlpListMessage>,
) -> Option<String> {
    let phase = app.spotify_search.as_ref()?.phase.clone();
    let code = key.code;
    let keys = &config::config().keys;
    let wants_close = code == KeyCode::Esc || config::pressed(&keys.back, code);
    match phase {
        SearchPhase::Input => match code {
            KeyCode::Esc => close_spotify_search(app),
            KeyCode::Enter => return submit_spotify_query(app, spawn_search),
            KeyCode::Backspace => {
                if let Some(search) = app.spotify_search.as_mut() {
                    handle_prompt_backspace(&mut search.query);
                }
            }
            // El pegado llega por `Event::Paste` o como secuencia de teclas.
            KeyCode::Char('v' | 'V') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(search) = app.spotify_search.as_mut() {
                    handle_prompt_char(&mut search.query, c);
                }
            }
            _ => {}
        },
        SearchPhase::Searching => {
            if wants_close {
                close_spotify_search(app);
            }
        }
        SearchPhase::Results { hits, selected } => {
            let len = hits.len();
            let step = |delta: isize| move_selection(selected, delta, len);
            let moved = if wants_close {
                close_spotify_search(app);
                return None;
            } else if config::pressed(&keys.spotify_search, code) {
                edit_spotify_query(app);
                return None;
            } else if config::pressed(&keys.select, code) {
                if let Some(hit) = hits.get(selected) {
                    play_search_hit(app, hit, resolve_page, list_playlist);
                }
                return None;
            } else if config::pressed(&keys.move_up, code) {
                step(-1)
            } else if config::pressed(&keys.move_down, code) {
                step(1)
            } else if config::pressed(&keys.page_up, code) {
                step(-PAGE_STEP)
            } else if config::pressed(&keys.page_down, code) {
                step(PAGE_STEP)
            } else if config::pressed(&keys.home, code) {
                step(isize::MIN)
            } else if config::pressed(&keys.end, code) {
                step(isize::MAX)
            } else {
                return None;
            };
            if let Some(SpotifySearch {
                phase: SearchPhase::Results { selected, .. },
                ..
            }) = app.spotify_search.as_mut()
            {
                *selected = moved;
            }
        }
        SearchPhase::Failed(_) => {
            if wants_close {
                close_spotify_search(app);
            } else if code == KeyCode::Enter || config::pressed(&keys.spotify_search, code) {
                edit_spotify_query(app);
            }
        }
    }
    None
}

/// Vuelve al campo de texto conservando la consulta.
fn edit_spotify_query(app: &mut App) {
    if let Some(search) = app.spotify_search.as_mut() {
        search.phase = SearchPhase::Input;
    }
}

/// Enter en el campo: URL o ruta → se devuelve al flujo de siempre; texto
/// libre → búsqueda en segundo plano.
fn submit_spotify_query(
    app: &mut App,
    spawn_search: impl FnOnce(String, u64) -> Receiver<SpotifySearchMessage>,
) -> Option<String> {
    let text = app.spotify_search.as_ref()?.query.trim().to_string();
    match route_search_input(&text) {
        SearchRoute::Nothing => None,
        SearchRoute::Open => {
            close_spotify_search(app);
            Some(text)
        }
        SearchRoute::Query => {
            // Una búsqueda nueva deja obsoleta a la que estuviera en vuelo.
            cancel_spotify_search(app);
            app.spotify_search_rx = Some(spawn_search(text.clone(), app.spotify_search_id));
            if let Some(search) = app.spotify_search.as_mut() {
                search.query = text;
                search.phase = SearchPhase::Searching;
            }
            None
        }
    }
}

/// Reproduce la canción o lista la playlist elegida por el flujo de URLs: el
/// marcador `spotify:track:<id>` / `spotify:playlist:<id>` ya lo entiende.
fn play_search_hit(
    app: &mut App,
    hit: &SearchHit,
    resolve_page: impl FnOnce(String, u64) -> Receiver<YtDlpMessage>,
    list_playlist: impl FnOnce(String, u64) -> Receiver<YtDlpListMessage>,
) {
    close_spotify_search(app);
    let marker = hit.marker();
    begin_url_flow(app, marker.clone(), resolve_page, list_playlist);
    // Mientras resuelve se muestra el nombre, no el marcador.
    app.track_label = hit.label().to_string();
    app.current_url = Some((marker, hit.label().to_string()));
}

pub(super) fn handle_spotify_search_key(key: KeyEvent, app: &mut App, engine: &mut AudioEngine) {
    if let Some(text) = handle_spotify_search_key_with(
        key,
        app,
        spawn_spotify_search,
        spawn_ytdlp_resolution,
        spawn_ytdlp_listing,
    ) {
        submit_url_prompt(text, app, engine);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::spotify::client::{SpotifyError, SpotifyPlaylist, SpotifyTrack};
    use crate::ui::tests_common::*;
    use std::cell::RefCell;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn track(id: &str, title: &str, artists: &[&str]) -> SpotifyTrack {
        SpotifyTrack {
            id: id.to_string(),
            title: title.to_string(),
            artists: artists.iter().map(|a| a.to_string()).collect(),
            album: None,
            duration_ms: 200_000,
            isrc: None,
        }
    }

    fn playlist(id: &str, name: &str, owner: Option<&str>, total: Option<u64>) -> SpotifyPlaylist {
        SpotifyPlaylist {
            id: id.to_string(),
            name: name.to_string(),
            owner: owner.map(str::to_string),
            tracks_total: total,
        }
    }

    fn sample_hits() -> Vec<SearchHit> {
        hits_from(SearchResults {
            tracks: vec![track("t1", "Around the World", &["Daft Punk"])],
            playlists: vec![playlist(
                "p1",
                "This Is Daft Punk",
                Some("Spotify"),
                Some(50),
            )],
            artists: Vec::new(),
        })
    }

    /// Búsqueda que anota (consulta, id) y nunca contesta.
    fn recording_search(
        log: &RefCell<Vec<(String, u64)>>,
    ) -> impl FnOnce(String, u64) -> Receiver<SpotifySearchMessage> + '_ {
        move |query, req_id| {
            log.borrow_mut().push((query, req_id));
            let (_tx, rx) = mpsc::channel();
            rx
        }
    }

    fn unused_search(query: String, _req_id: u64) -> Receiver<SpotifySearchMessage> {
        panic!("no debía buscar: {query}");
    }

    fn unused_resolve(url: String, _req_id: u64) -> Receiver<YtDlpMessage> {
        panic!("no debía resolver: {url}");
    }

    fn unused_list(url: String, _req_id: u64) -> Receiver<YtDlpListMessage> {
        panic!("no debía listar: {url}");
    }

    /// Tecla al modal sin ningún efecto externo permitido.
    fn press(app: &mut App, code: KeyCode) -> Option<String> {
        handle_spotify_search_key_with(key(code), app, unused_search, unused_resolve, unused_list)
    }

    fn results_app() -> App {
        let mut app = test_app();
        app.spotify_search = Some(SpotifySearch {
            query: "daft punk".to_string(),
            phase: SearchPhase::Results {
                hits: sample_hits(),
                selected: 0,
            },
        });
        app
    }

    fn selected_of(app: &App) -> usize {
        match &app.spotify_search.as_ref().unwrap().phase {
            SearchPhase::Results { selected, .. } => *selected,
            other => panic!("se esperaban resultados: {other:?}"),
        }
    }

    // --- entrada ---

    #[test]
    fn u_is_the_default_key_and_opens_an_empty_search_field() {
        let keys = &config::config().keys;
        assert!(config::pressed(&keys.spotify_search, KeyCode::Char('u')));
        assert!(!config::pressed(&keys.spotify_search, KeyCode::Char('/')));

        let mut app = test_app();
        open_spotify_search(&mut app);
        assert_eq!(
            app.spotify_search,
            Some(SpotifySearch {
                query: String::new(),
                phase: SearchPhase::Input
            })
        );
    }

    #[test]
    fn u_key_opens_the_search_from_the_list_and_the_player() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        for (mode, explorer_open) in [
            (Mode::Browser, false),
            (Mode::Player, false),
            (Mode::Player, true),
        ] {
            let mut app = test_app();
            app.mode = mode;
            app.explorer_open = explorer_open;
            app.has_player = mode == Mode::Player;
            dispatch_key(key(KeyCode::Char('u')), &mut app, &mut engine);
            assert!(
                app.spotify_search.is_some(),

                "{mode:?} explorer={explorer_open}"
            );
        }
    }

    #[test]
    fn modal_swallows_keys_so_letters_do_not_trigger_player_shortcuts() {
        let Some(mut engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        open_spotify_search(&mut app);
        dispatch_key(key(KeyCode::Char('q')), &mut app, &mut engine);
        dispatch_key(key(KeyCode::Char('u')), &mut app, &mut engine);
        assert_eq!(app.spotify_search.as_ref().unwrap().query, "qu");
        assert!(app.url_input.is_none());
        assert!(!app.should_quit);
    }

    #[test]
    fn typing_backspace_and_paste_edit_the_query() {
        let mut app = test_app();
        open_spotify_search(&mut app);
        type_text(&mut app, "daft pux");
        press(&mut app, KeyCode::Backspace);
        type_text(&mut app, "nk");
        paste_into_spotify_search(&mut app, " ft.\r\nAlive");
        assert_eq!(
            app.spotify_search.as_ref().unwrap().query,
            "daft punk ft.Alive"
        );
    }

    #[test]
    fn route_distinguishes_query_url_local_path_and_empty() {
        assert_eq!(route_search_input("   "), SearchRoute::Nothing);
        assert_eq!(route_search_input("daft punk"), SearchRoute::Query);
        assert_eq!(route_search_input("no/existe/esto.mp3"), SearchRoute::Query);
        for url in [
            "https://www.youtube.com/watch?v=abc",
            "https://example.com/a.mp3",
            "spotify:track:abc123",
            "https://open.spotify.com/playlist/abc123",
        ] {
            assert_eq!(route_search_input(url), SearchRoute::Open, "{url}");
        }
        let existing = std::env::current_exe().unwrap();
        assert_eq!(
            route_search_input(&existing.to_string_lossy()),
            SearchRoute::Open,
            "una ruta local que existe va al flujo de siempre"
        );
    }

    // --- enviar ---

    #[test]
    fn enter_with_plain_text_starts_a_background_search() {
        let mut app = test_app();
        open_spotify_search(&mut app);
        type_text(&mut app, "  daft punk ");
        let log = RefCell::new(Vec::new());
        let delegated = handle_spotify_search_key_with(
            key(KeyCode::Enter),
            &mut app,
            recording_search(&log),
            unused_resolve,
            unused_list,
        );
        assert_eq!(delegated, None);
        assert_eq!(
            log.borrow().as_slice(),
            &[("daft punk".to_string(), app.spotify_search_id)]
        );
        assert_eq!(
            app.spotify_search.as_ref().unwrap().phase,
            SearchPhase::Searching
        );
        assert!(app.spotify_search_rx.is_some(), "la red corre en otro hilo");
    }

    #[test]
    fn enter_with_url_or_path_goes_to_the_existing_flow_without_searching() {
        let existing = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for text in [
            "https://www.youtube.com/watch?v=abc",
            "spotify:track:abc",
            existing.as_str(),
        ] {
            let mut app = test_app();
            open_spotify_search(&mut app);
            type_text(&mut app, text);
            let delegated = press(&mut app, KeyCode::Enter);
            assert_eq!(delegated.as_deref(), Some(text));
            assert!(app.spotify_search.is_none(), "el modal se cierra: {text}");
            assert!(app.spotify_search_rx.is_none());
        }
    }

    #[test]
    fn enter_on_empty_text_keeps_the_field_open_and_does_nothing() {
        let mut app = test_app();
        open_spotify_search(&mut app);
        type_text(&mut app, "   ");
        assert_eq!(press(&mut app, KeyCode::Enter), None);
        assert_eq!(
            app.spotify_search.as_ref().unwrap().phase,
            SearchPhase::Input
        );
    }

    #[test]
    fn a_new_search_supersedes_the_one_in_flight() {
        let mut app = test_app();
        open_spotify_search(&mut app);
        type_text(&mut app, "uno");
        let log = RefCell::new(Vec::new());
        handle_spotify_search_key_with(
            key(KeyCode::Enter),
            &mut app,
            recording_search(&log),
            unused_resolve,
            unused_list,
        );
        let first_id = app.spotify_search_id;

        // Vuelve a editar (Esc no: cierra) y manda otra.
        app.spotify_search.as_mut().unwrap().phase = SearchPhase::Input;
        type_text(&mut app, "dos");
        handle_spotify_search_key_with(
            key(KeyCode::Enter),
            &mut app,
            recording_search(&log),
            unused_resolve,
            unused_list,
        );
        assert_ne!(app.spotify_search_id, first_id);

        handle_spotify_search_result(&mut app, first_id, Ok(sample_hits()));
        assert_eq!(
            app.spotify_search.as_ref().unwrap().phase,
            SearchPhase::Searching,
            "el resultado viejo no pisa a la búsqueda vigente"
        );
    }

    // --- resultados ---

    #[test]
    fn hits_list_tracks_then_playlists_with_readable_labels() {
        let hits = sample_hits();
        assert_eq!(
            hits,
            vec![
                SearchHit::Track {
                    id: "t1".into(),
                    label: "Daft Punk - Around the World".into()
                },
                SearchHit::Playlist {
                    id: "p1".into(),
                    label: "This Is Daft Punk".into(),
                    detail: Some("Spotify · 50 canciones".into())
                },
            ]
        );
        assert_eq!(hits[0].marker(), "spotify:track:t1");
        assert_eq!(hits[1].marker(), "spotify:playlist:p1");
    }

    #[test]
    fn hits_skip_entries_without_id_and_tolerate_missing_playlist_detail() {
        let hits = hits_from(SearchResults {
            tracks: vec![track("", "Local", &["X"]), track("t2", "Dos", &[])],
            playlists: vec![
                playlist("", "Sin id", None, None),
                playlist("p2", "Mix", None, None),
            ],
            artists: Vec::new(),
        });
        assert_eq!(
            hits,
            vec![
                SearchHit::Track {
                    id: "t2".into(),
                    label: "Dos".into()
                },
                SearchHit::Playlist {
                    id: "p2".into(),
                    label: "Mix".into(),
                    detail: None
                },
            ]
        );
    }

    #[test]
    fn matching_result_fills_the_list_and_stale_one_is_dropped() {
        let mut app = test_app();
        open_spotify_search(&mut app);
        app.spotify_search.as_mut().unwrap().phase = SearchPhase::Searching;
        app.spotify_search_id = 7;

        handle_spotify_search_result(&mut app, 6, Ok(sample_hits()));
        assert_eq!(
            app.spotify_search.as_ref().unwrap().phase,
            SearchPhase::Searching
        );

        handle_spotify_search_result(&mut app, 7, Ok(sample_hits()));
        assert_eq!(
            app.spotify_search.as_ref().unwrap().phase,
            SearchPhase::Results {
                hits: sample_hits(),
                selected: 0
            }
        );
    }

    #[test]
    fn result_arriving_after_the_modal_closed_is_ignored() {
        let mut app = test_app();
        handle_spotify_search_result(&mut app, 0, Ok(sample_hits()));
        assert!(app.spotify_search.is_none());
    }

    #[test]
    fn failed_search_shows_the_message_on_screen() {
        let mut app = test_app();
        open_spotify_search(&mut app);
        app.spotify_search.as_mut().unwrap().phase = SearchPhase::Searching;
        handle_spotify_search_result(
            &mut app,
            0,
            Err("no hay sesión: corré `mp3tui --spotify-login`".into()),
        );
        assert!(matches!(
            &app.spotify_search.as_ref().unwrap().phase,
            SearchPhase::Failed(msg) if msg.contains("--spotify-login")
        ));
    }

    #[test]
    fn worker_thread_dying_reports_a_failure_instead_of_hanging() {
        let mut app = test_app();
        open_spotify_search(&mut app);
        app.spotify_search.as_mut().unwrap().phase = SearchPhase::Searching;
        let (tx, rx) = mpsc::channel::<SpotifySearchMessage>();
        app.spotify_search_rx = Some(rx);
        drop(tx);
        poll_spotify_search(&mut app);
        assert!(app.spotify_search_rx.is_none());
        assert!(matches!(
            app.spotify_search.as_ref().unwrap().phase,
            SearchPhase::Failed(_)
        ));
    }

    #[test]
    fn poll_applies_a_ready_result() {
        let mut app = test_app();
        open_spotify_search(&mut app);
        app.spotify_search.as_mut().unwrap().phase = SearchPhase::Searching;
        let (tx, rx) = mpsc::channel();
        app.spotify_search_rx = Some(rx);
        tx.send((app.spotify_search_id, "q".to_string(), Ok(sample_hits())))
            .unwrap();
        poll_spotify_search(&mut app);
        assert!(app.spotify_search_rx.is_none());
        assert!(matches!(
            app.spotify_search.as_ref().unwrap().phase,
            SearchPhase::Results { .. }
        ));
    }

    // --- errores de la búsqueda (hilo) ---

    #[test]
    fn run_search_without_session_or_client_id_gives_actionable_messages() {
        let no_session = || Err(ConnectError::NotLoggedIn);
        assert!(
            run_spotify_search("x", &no_session)
                .unwrap_err()
                .contains("--spotify-login")
        );

        let no_id = || {
            Err(ConnectError::ClientId(
                crate::source::spotify::MissingClientId,
            ))
        };
        assert!(
            run_spotify_search("x", &no_id)
                .unwrap_err()
                .contains(crate::source::spotify::CLIENT_ID_ENV)
        );
    }

    #[test]
    fn run_search_rate_limit_is_reported_with_the_wait_time() {
        struct Limited;
        impl SearchSource for Limited {
            fn search(&self, _query: &str) -> Result<SearchResults, SpotifyError> {
                Err(SpotifyError::RateLimited {
                    retry_after_secs: Some(12),
                })
            }
        }
        let connect = || Ok(Box::new(Limited) as Box<dyn SearchSource>);
        let msg = run_spotify_search("x", &connect).unwrap_err();
        assert!(
            msg.contains("limitó la tasa") && msg.contains("12"),
            "{msg}"
        );
    }

    #[test]
    fn run_search_maps_results_to_hits() {
        struct Found;
        impl SearchSource for Found {
            fn search(&self, query: &str) -> Result<SearchResults, SpotifyError> {
                assert_eq!(query, "daft punk");
                Ok(SearchResults {
                    tracks: vec![track("t1", "Around the World", &["Daft Punk"])],
                    playlists: Vec::new(),
                    artists: Vec::new(),
                })
            }
        }
        let connect = || Ok(Box::new(Found) as Box<dyn SearchSource>);
        let hits = run_spotify_search("daft punk", &connect).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].label(), "Daft Punk - Around the World");
    }

    // --- navegación y elección ---

    #[test]
    fn arrows_move_the_selection_and_clamp_at_the_ends() {
        let mut app = results_app();
        press(&mut app, KeyCode::Down);
        assert_eq!(selected_of(&app), 1);
        press(&mut app, KeyCode::Down);
        assert_eq!(selected_of(&app), 1, "no pasa del último");
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Up);
        assert_eq!(selected_of(&app), 0);
        press(&mut app, KeyCode::End);
        assert_eq!(selected_of(&app), 1);
        press(&mut app, KeyCode::Home);
        assert_eq!(selected_of(&app), 0);
    }

    #[test]
    fn choosing_a_track_starts_resolving_its_marker() {
        let mut app = results_app();
        let resolve_calls = RefCell::new(Vec::new());
        let list_calls = RefCell::new(Vec::new());
        handle_spotify_search_key_with(
            key(KeyCode::Enter),
            &mut app,
            unused_search,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );
        assert_eq!(resolve_calls.borrow().as_slice(), &["spotify:track:t1"]);
        assert!(list_calls.borrow().is_empty());
        assert_eq!(app.status, Status::Resolving);
        assert!(app.ytdlp_rx.is_some());
        assert!(app.playlist_rx.is_none());
        assert!(app.spotify_search.is_none(), "el modal se cierra al elegir");
        assert_eq!(app.track_label, "Daft Punk - Around the World");
    }

    #[test]
    fn choosing_a_playlist_starts_listing_it() {
        let mut app = results_app();
        press(&mut app, KeyCode::Down);
        let resolve_calls = RefCell::new(Vec::new());
        let list_calls = RefCell::new(Vec::new());
        handle_spotify_search_key_with(
            key(KeyCode::Enter),
            &mut app,
            unused_search,
            empty_resolve_rx(&resolve_calls),
            empty_list_rx(&list_calls),
        );
        assert_eq!(list_calls.borrow().as_slice(), &["spotify:playlist:p1"]);
        assert!(resolve_calls.borrow().is_empty());
        assert_eq!(app.status, Status::Resolving);
        assert!(app.playlist_rx.is_some());
        assert!(app.ytdlp_rx.is_none());
        assert!(app.spotify_search.is_none());
    }

    #[test]
    fn enter_with_no_results_does_nothing() {
        let mut app = test_app();
        app.spotify_search = Some(SpotifySearch {
            query: "zzz".into(),
            phase: SearchPhase::Results {
                hits: Vec::new(),
                selected: 0,
            },
        });
        assert_eq!(press(&mut app, KeyCode::Enter), None);
        assert!(app.spotify_search.is_some());
    }

    // --- cerrar y reintentar ---

    #[test]
    fn esc_closes_the_modal_and_cancels_the_search_in_flight() {
        let mut app = test_app();
        open_spotify_search(&mut app);
        type_text(&mut app, "daft");
        let log = RefCell::new(Vec::new());
        handle_spotify_search_key_with(
            key(KeyCode::Enter),
            &mut app,
            recording_search(&log),
            unused_resolve,
            unused_list,
        );
        let in_flight = app.spotify_search_id;

        press(&mut app, KeyCode::Esc);
        assert!(app.spotify_search.is_none());
        assert!(app.spotify_search_rx.is_none());
        assert_ne!(
            app.spotify_search_id, in_flight,
            "el id viejo queda obsoleto"
        );
        assert!(!app.should_quit, "Esc en el modal no arma la salida");
    }

    #[test]
    fn u_in_results_or_failure_returns_to_editing_the_same_query() {
        let mut app = results_app();
        press(&mut app, KeyCode::Char('u'));
        let state = app.spotify_search.as_ref().unwrap();
        assert_eq!(state.phase, SearchPhase::Input);
        assert_eq!(state.query, "daft punk");

        let mut app = test_app();
        app.spotify_search = Some(SpotifySearch {
            query: "x".into(),
            phase: SearchPhase::Failed("boom".into()),
        });
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.spotify_search.as_ref().unwrap().phase,
            SearchPhase::Input
        );
    }

    // --- render ---

    /// Pantalla completa (100x30) como texto plano.
    fn screen_text(app: &App, engine: &AudioEngine) -> String {
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
        terminal.draw(|frame| render(frame, app, engine)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect()
    }

    fn text_of(lines: &[Line<'_>]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    fn search_in(phase: SearchPhase) -> SpotifySearch {
        SpotifySearch {
            query: "daft".to_string(),
            phase,
        }
    }

    #[test]
    fn modal_shows_the_query_with_a_caret_while_typing() {
        let lines = spotify_search_lines(&search_in(SearchPhase::Input), 40, 8);
        let text = text_of(&lines);
        assert!(text.contains(" daft▌"), "{text}");
    }

    #[test]
    fn modal_results_show_kind_label_and_selection_and_fit_the_width() {
        let phase = SearchPhase::Results {
            hits: sample_hits(),
            selected: 1,
        };
        let lines = spotify_search_lines(&search_in(phase), 44, 8);
        let text = text_of(&lines);
        assert!(text.contains("Canción"), "{text}");
        assert!(text.contains("Daft Punk - Around the World"), "{text}");
        assert!(text.contains("Playlist"), "{text}");
        assert!(text.contains("This Is Daft Punk"), "{text}");
        assert!(text.contains("2 resultados"), "{text}");
        let rows: Vec<&Line> = lines
            .iter()
            .filter(|l| line_text(l).contains("Playlist"))
            .collect();
        assert!(
            line_text(rows[0]).starts_with('▌'),
            "la fila elegida lleva el cursor"
        );
        for line in &lines {
            assert!(line_width(line) <= 44, "{}", line_text(line));
        }
    }

    #[test]
    fn modal_states_searching_empty_and_failed_are_explicit() {
        let searching = text_of(&spotify_search_lines(
            &search_in(SearchPhase::Searching),
            40,
            8,
        ));
        assert!(searching.contains("Buscando en Spotify"), "{searching}");

        let empty = text_of(&spotify_search_lines(
            &search_in(SearchPhase::Results {
                hits: Vec::new(),
                selected: 0,
            }),
            40,
            8,
        ));
        assert!(empty.contains("Sin resultados"), "{empty}");

        let failed = text_of(&spotify_search_lines(
            &search_in(SearchPhase::Failed("corré `mp3tui --spotify-login`".into())),
            40,
            8,
        ));
        assert!(failed.contains("--spotify-login"), "{failed}");
    }

    #[test]
    fn modal_scrolls_so_the_selection_stays_visible() {
        let hits: Vec<SearchHit> = (0..30)
            .map(|i| SearchHit::Track {
                id: format!("t{i}"),
                label: format!("Tema numero {i:02}"),
            })
            .collect();
        let phase = SearchPhase::Results { hits, selected: 25 };
        let text = text_of(&spotify_search_lines(&search_in(phase), 40, 8));
        assert!(text.contains("Tema numero 25"), "{text}");
        assert!(!text.contains("Tema numero 00"), "{text}");
    }

    #[test]
    fn footer_explains_the_keys_of_each_phase_within_the_width() {
        let cases = [
            (SearchPhase::Input, vec!["Enter: buscar", "Esc: cancelar"]),
            (SearchPhase::Searching, vec!["Esc: cancelar"]),
            (
                SearchPhase::Results {
                    hits: sample_hits(),
                    selected: 0,
                },
                vec!["Enter: reproducir", "u: nueva búsqueda", "Esc: cerrar"],
            ),
            (
                SearchPhase::Failed("x".into()),
                vec!["Enter: reintentar", "Esc: cerrar"],
            ),
        ];
        for (phase, hints) in cases {
            let mut app = test_app();
            app.spotify_search = Some(search_in(phase.clone()));
            let lines = footer_lines(&app, false);
            let text = text_of(&lines);
            for hint in hints {
                assert!(text.contains(hint), "{phase:?} falta '{hint}': {text}");
            }
            for line in &lines {
                assert!(line_width(line) <= 78, "{}", line_text(line));
            }
        }
    }

    #[test]
    fn regular_screens_advertise_the_spotify_key_on_the_footer_border() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        for (mode, has_player, explorer_open) in [
            (Mode::Browser, false, false),
            (Mode::Player, true, false),
            (Mode::Player, true, true),
        ] {
            let mut app = test_app();
            app.mode = mode;
            app.has_player = has_player;
            app.explorer_open = explorer_open;
            assert!(
                screen_text(&app, &engine).contains("u: Spotify"),
                "{mode:?} explorer={explorer_open}"
            );
            // El pie de cada modo no cambió: la pista vive en el borde.
            for line in footer_lines(&app, false) {
                assert!(line_width(&line) <= 78, "{}", line_text(&line));
            }
        }
    }

    #[test]
    fn the_border_hint_is_hidden_while_a_modal_explains_its_own_keys() {
        let Some(engine) = engine_or_skip() else {
            return;
        };
        let mut app = test_app();
        app.spotify_search = Some(search_in(SearchPhase::Input));
        assert!(!screen_text(&app, &engine).contains("u: Spotify"));
    }

    #[test]
    fn modal_renders_in_every_phase_even_on_tiny_terminals() {
        let phases = [
            SearchPhase::Input,
            SearchPhase::Searching,
            SearchPhase::Results {
                hits: sample_hits(),
                selected: 0,
            },
            SearchPhase::Failed("un error bastante largo ".repeat(20)),
        ];
        for phase in phases {
            for (w, h) in [(100, 30), (40, 8), (23, 10), (10, 2), (1, 1), (0, 0)] {
                let backend = ratatui::backend::TestBackend::new(w, h);
                let mut terminal = ratatui::Terminal::new(backend).expect("terminal de prueba");
                terminal
                    .draw(|frame| render_spotify_search(frame, &search_in(phase.clone())))
                    .unwrap_or_else(|e| panic!("{phase:?} {w}x{h}: {e}"));
            }
        }
    }

    #[test]
    fn full_render_draws_the_modal_title_over_the_screen() {
        let mut app = test_app();
        app.spotify_search = Some(search_in(SearchPhase::Input));
        let Some(engine) = engine_or_skip() else {
            return;
        };
        assert!(
            screen_text(&app, &engine).contains("Buscar en Spotify"),
            "falta el título del modal"
        );
    }
}
