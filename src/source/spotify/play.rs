//! Integración de Spotify con la reproducción: conexión del cliente, despacho
//! del resolvedor por página, listado de playlists y login por CLI.
//!
//! Todo lo que toca red o procesos recibe sus dependencias por parámetro para
//! que la UI lo corra en un hilo y los tests lo inyecten.

use std::fmt;
use std::path::{Path, PathBuf};

use super::auth::{self, AuthError, TokenClient, Tokens};
use super::client::{SearchResults, SpotifyClient, SpotifyError, SpotifyPlaylist, SpotifyTrack};
use super::resolve::{ResolveError, resolve_track};
use super::{MissingClientId, client_id};
use crate::config::Config;
use crate::source::{self, UrlKind, ytdlp};
use ytdlp::{Collection, CommandRunner, PlaylistEntry, Resolved, ResolveCache, YtDlpError};

/// Lo mínimo del cliente que necesita la reproducción (seam para tests).
pub trait TrackSource {
    fn track(&self, id: &str) -> Result<SpotifyTrack, SpotifyError>;
    fn playlist_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError>;
    fn album_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError>;
    fn playlist_name(&self, id: &str) -> Result<String, SpotifyError>;
    fn album_name(&self, id: &str) -> Result<String, SpotifyError>;
}

impl TrackSource for SpotifyClient {
    fn track(&self, id: &str) -> Result<SpotifyTrack, SpotifyError> {
        SpotifyClient::track(self, id)
    }

    fn playlist_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
        SpotifyClient::playlist_tracks(self, id)
    }

    fn album_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
        SpotifyClient::album_tracks(self, id)
    }

    fn playlist_name(&self, id: &str) -> Result<String, SpotifyError> {
        SpotifyClient::playlist_name(self, id)
    }

    fn album_name(&self, id: &str) -> Result<String, SpotifyError> {
        SpotifyClient::album_name(self, id)
    }
}

/// Lo mínimo del cliente que necesita la búsqueda de catálogo (seam para
/// tests).
pub trait SearchSource {
    fn search(&self, query: &str) -> Result<SearchResults, SpotifyError>;
}

impl SearchSource for SpotifyClient {
    fn search(&self, query: &str) -> Result<SearchResults, SpotifyError> {
        SpotifyClient::search(self, query)
    }
}

/// Lo mínimo del cliente que necesita el panel de "mis playlists" (seam para
/// tests).
pub trait LibrarySource {
    fn my_playlists(&self) -> Result<Vec<SpotifyPlaylist>, SpotifyError>;
    fn playlist_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError>;
}

impl LibrarySource for SpotifyClient {
    fn my_playlists(&self) -> Result<Vec<SpotifyPlaylist>, SpotifyError> {
        SpotifyClient::my_playlists(self)
    }

    fn playlist_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
        SpotifyClient::playlist_tracks(self, id)
    }
}

/// No se pudo armar el cliente de Spotify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    ClientId(MissingClientId),
    NoConfigDir,
    /// Nunca se corrió el login (no hay archivo de tokens).
    NotLoggedIn,
    Auth(AuthError),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientId(err) => write!(f, "{err}"),
            Self::NoConfigDir => write!(f, "{}", AuthError::NoConfigDir),
            Self::NotLoggedIn => write!(f, "{LOGIN_HINT}"),
            Self::Auth(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ConnectError {}

const LOGIN_HINT: &str =
    "no hay sesión de Spotify: corré `mp3tui --spotify-login` para autorizar la app";

/// Arma el cliente con el `client_id` y la ruta de tokens dados. Es la parte
/// pura de [`connect`].
pub fn connect_with(
    client_id: Result<String, MissingClientId>,
    tokens_path: Option<PathBuf>,
) -> Result<SpotifyClient, ConnectError> {
    let client_id = client_id.map_err(ConnectError::ClientId)?;
    let path = tokens_path.ok_or(ConnectError::NoConfigDir)?;
    match auth::load_tokens(&path) {
        Ok(Some(_)) => {}
        Ok(None) => return Err(ConnectError::NotLoggedIn),
        Err(err) => return Err(ConnectError::Auth(err)),
    }
    Ok(SpotifyClient::new(TokenClient::new(client_id), path))
}

/// Cliente real: `client_id` de env/config y tokens del config dir.
pub fn connect(config: &Config) -> Result<SpotifyClient, ConnectError> {
    connect_with(client_id(config), auth::tokens_path())
}

/// Marcador de página que se encola para una canción: lo reconoce
/// [`resolve_page`] sin necesidad de metadatos.
pub fn track_marker(id: &str) -> String {
    format!("spotify:track:{id}")
}

/// Título que se muestra en la cola: `"Artistas - Título"`.
pub fn track_label(track: &SpotifyTrack) -> String {
    let artists = track.artists.join(", ");
    if artists.is_empty() {
        track.title.clone()
    } else {
        format!("{artists} - {}", track.title)
    }
}

/// Id de la canción si `page` es una canción de Spotify (URI o URL web).
pub fn track_id(page: &str) -> Option<String> {
    source::spotify_ref(page)
        .filter(|r| r.kind == UrlKind::SpotifyTrack)
        .map(|r| r.id)
}

/// Id de la playlist si `url` lo es.
pub fn playlist_id(url: &str) -> Option<String> {
    source::spotify_ref(url)
        .filter(|r| r.kind == UrlKind::SpotifyPlaylist)
        .map(|r| r.id)
}

/// Id del álbum si `url` lo es.
pub fn album_id(url: &str) -> Option<String> {
    source::spotify_ref(url)
        .filter(|r| r.kind == UrlKind::SpotifyAlbum)
        .map(|r| r.id)
}

/// `true` si `url` es una playlist o un álbum de Spotify (se lista, no se
/// resuelve como una sola canción).
pub fn is_collection(url: &str) -> bool {
    playlist_id(url).is_some() || album_id(url).is_some()
}

/// Texto para mostrar de un error del cliente: la falta de sesión (o su
/// vencimiento) se traduce al aviso de login; el resto conserva su mensaje
/// (incluido el límite de tasa, con el `Retry-After` si vino).
fn failure_text(err: SpotifyError) -> String {
    match err {
        SpotifyError::Auth(AuthError::NotAuthorized | AuthError::ReauthorizationRequired) => {
            LOGIN_HINT.to_string()
        }
        other => other.to_string(),
    }
}

fn failure(err: SpotifyError) -> YtDlpError {
    YtDlpError::Failed(failure_text(err))
}

/// Busca canciones y playlists en el catálogo de Spotify. El error ya viene
/// como mensaje listo para mostrar en pantalla: sin `client_id`, sin sesión
/// (corré `mp3tui --spotify-login`), sesión vencida o límite de tasa (429).
///
/// `connect` se inyecta para que la UI lo corra en un hilo y los tests no
/// toquen la red.
pub fn search_catalog(
    query: &str,
    connect: &dyn Fn() -> Result<Box<dyn SearchSource>, ConnectError>,
) -> Result<SearchResults, String> {
    let client = connect().map_err(|e| e.to_string())?;
    client.search(query).map_err(failure_text)
}

/// Playlists del usuario (incluye las privadas). El error ya viene como
/// mensaje para pantalla: sin `client_id`, sin sesión, sesión vencida o 429.
pub fn user_playlists(
    connect: &dyn Fn() -> Result<Box<dyn LibrarySource>, ConnectError>,
) -> Result<Vec<SpotifyPlaylist>, String> {
    let client = connect().map_err(|e| e.to_string())?;
    client.my_playlists().map_err(failure_text)
}

/// Canciones de una playlist del usuario, con el mismo manejo de errores que
/// [`user_playlists`].
pub fn user_playlist_tracks(
    id: &str,
    connect: &dyn Fn() -> Result<Box<dyn LibrarySource>, ConnectError>,
) -> Result<Vec<SpotifyTrack>, String> {
    let client = connect().map_err(|e| e.to_string())?;
    client.playlist_tracks(id).map_err(failure_text)
}

/// Resuelve una página a audio: una canción de Spotify pasa por
/// [`resolve_track`] (búsqueda en YouTube + caché) y se reproduce desde la URL
/// elegida; cualquier otra página va directo a `ytdlp::resolve`.
///
/// `connect` se llama solo para canciones de Spotify.
pub fn resolve_page(
    page: &str,
    runner: &dyn CommandRunner,
    cache: &mut ResolveCache,
    connect: &dyn Fn() -> Result<Box<dyn TrackSource>, ConnectError>,
) -> Result<Resolved, YtDlpError> {
    let Some(id) = track_id(page) else {
        return ytdlp::resolve(page, runner);
    };
    let client = connect().map_err(|e| YtDlpError::Failed(e.to_string()))?;
    let track = client.track(&id).map_err(failure)?;
    let youtube_url = resolve_track(client.as_ref(), cache, &track, runner).map_err(|e| match e {
        ResolveError::Search(err) => err,
        other => YtDlpError::Failed(other.to_string()),
    })?;
    let mut resolved = ytdlp::resolve(&youtube_url, runner)?;
    resolved.title = track_label(&track);
    Ok(resolved)
}

/// Lista una playlist o álbum de Spotify: las entradas de cola (título
/// `"Artistas - Título"` y marcador `spotify:track:<id>`) y su nombre real. Las
/// canciones sin id (archivos locales) se omiten porque no se pueden buscar.
///
/// Es una sola pasada: primero los temas (si fallan, el listado falla) y
/// después el nombre, que es un extra: si no se puede pedir queda `None` y la
/// UI usa su nombre de respaldo.
pub fn list_playlist(
    url: &str,
    connect: &dyn Fn() -> Result<Box<dyn TrackSource>, ConnectError>,
) -> Result<Collection, YtDlpError> {
    let target = match (playlist_id(url), album_id(url)) {
        (Some(id), _) => (id, false),
        (None, Some(id)) => (id, true),
        (None, None) => {
            return Err(YtDlpError::InvalidUrl(
                "no es una playlist ni un álbum de Spotify".to_string(),
            ));
        }
    };
    let client = connect().map_err(|e| YtDlpError::Failed(e.to_string()))?;
    let (id, is_album) = target;
    let tracks = if is_album {
        client.album_tracks(&id)
    } else {
        client.playlist_tracks(&id)
    }
    .map_err(failure)?;
    let name = if is_album {
        client.album_name(&id)
    } else {
        client.playlist_name(&id)
    }
    .ok()
    .map(|name| name.trim().to_string())
    .filter(|name| !name.is_empty());
    let entries = tracks
        .iter()
        .filter(|track| !track.id.is_empty())
        .map(|track| PlaylistEntry {
            title: track_label(track),
            page_url: track_marker(&track.id),
        })
        .collect();
    Ok(Collection { name, entries })
}

/// `mp3tui --spotify-login`: autoriza la app y guarda los tokens. El login
/// real se inyecta (`login`) para no abrir navegador ni red en los tests.
pub fn run_login(
    client_id: Result<String, MissingClientId>,
    tokens_path: Option<PathBuf>,
    login: impl FnOnce(&TokenClient, &Path) -> Result<Tokens, AuthError>,
) -> Result<(), String> {
    let client_id = client_id.map_err(|e| e.to_string())?;
    let path = tokens_path.ok_or_else(|| AuthError::NoConfigDir.to_string())?;
    login(&TokenClient::new(client_id), &path).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::ytdlp::{CommandOutput, RunError};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

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

    struct FakeSource {
        tracks: Vec<SpotifyTrack>,
        name: Result<String, SpotifyError>,
        seen: Mutex<Vec<String>>,
    }

    impl TrackSource for FakeSource {
        fn track(&self, id: &str) -> Result<SpotifyTrack, SpotifyError> {
            self.seen.lock().unwrap().push(format!("track:{id}"));
            self.tracks
                .iter()
                .find(|t| t.id == id)
                .cloned()
                .ok_or(SpotifyError::Status { status: 404, message: None })
        }

        fn playlist_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
            self.seen.lock().unwrap().push(format!("playlist:{id}"));
            Ok(self.tracks.clone())
        }

        fn album_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
            self.seen.lock().unwrap().push(format!("album:{id}"));
            Ok(self.tracks.clone())
        }

        fn playlist_name(&self, id: &str) -> Result<String, SpotifyError> {
            self.seen.lock().unwrap().push(format!("playlist_name:{id}"));
            self.name.clone()
        }

        fn album_name(&self, id: &str) -> Result<String, SpotifyError> {
            self.seen.lock().unwrap().push(format!("album_name:{id}"));
            self.name.clone()
        }
    }

    fn connect_to(tracks: Vec<SpotifyTrack>) -> impl Fn() -> Result<Box<dyn TrackSource>, ConnectError> {
        connect_named(tracks, Ok("Nombre real".to_string()))
    }

    fn connect_named(
        tracks: Vec<SpotifyTrack>,
        name: Result<String, SpotifyError>,
    ) -> impl Fn() -> Result<Box<dyn TrackSource>, ConnectError> {
        move || {
            Ok(Box::new(FakeSource {
                tracks: tracks.clone(),
                name: name.clone(),
                seen: Mutex::new(Vec::new()),
            }) as Box<dyn TrackSource>)
        }
    }

    fn connect_fails() -> Result<Box<dyn TrackSource>, ConnectError> {
        Err(ConnectError::NotLoggedIn)
    }

    struct FakeSearch {
        result: Result<SearchResults, SpotifyError>,
        seen: std::sync::Arc<Mutex<Vec<String>>>,
    }

    impl SearchSource for FakeSearch {
        fn search(&self, query: &str) -> Result<SearchResults, SpotifyError> {
            self.seen.lock().unwrap().push(query.to_string());
            self.result.clone()
        }
    }

    fn search_connect(
        result: Result<SearchResults, SpotifyError>,
        seen: std::sync::Arc<Mutex<Vec<String>>>,
    ) -> impl Fn() -> Result<Box<dyn SearchSource>, ConnectError> {
        move || {
            Ok(Box::new(FakeSearch {
                result: result.clone(),
                seen: std::sync::Arc::clone(&seen),
            }) as Box<dyn SearchSource>)
        }
    }

    fn one_track_results() -> SearchResults {
        SearchResults {
            tracks: vec![track("t1", "Song", &["Artist"])],
            playlists: Vec::new(),
            artists: Vec::new(),
        }
    }

    #[test]
    fn test_search_catalog_returns_the_client_results() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let connect = search_connect(Ok(one_track_results()), std::sync::Arc::clone(&seen));
        let found = search_catalog("daft punk", &connect).unwrap();
        assert_eq!(found.tracks.len(), 1);
        assert_eq!(found.tracks[0].id, "t1");
        assert_eq!(seen.lock().unwrap().as_slice(), &["daft punk"]);
    }

    #[test]
    fn test_search_catalog_without_session_says_to_run_spotify_login() {
        let connect = || Err(ConnectError::NotLoggedIn);
        let msg = search_catalog("x", &connect).unwrap_err();
        assert!(msg.contains("--spotify-login"), "{msg}");
    }

    #[test]
    fn test_search_catalog_without_client_id_explains_how_to_set_it() {
        let connect = || Err(ConnectError::ClientId(MissingClientId));
        let msg = search_catalog("x", &connect).unwrap_err();
        assert!(msg.contains(super::super::CLIENT_ID_ENV), "{msg}");
    }

    #[test]
    fn test_search_catalog_expired_session_says_to_run_spotify_login() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        for auth in [AuthError::NotAuthorized, AuthError::ReauthorizationRequired] {
            let connect =
                search_connect(Err(SpotifyError::Auth(auth)), std::sync::Arc::clone(&seen));
            let msg = search_catalog("x", &connect).unwrap_err();
            assert!(msg.contains("--spotify-login"), "{msg}");
        }
    }

    #[test]
    fn test_search_catalog_rate_limit_message_is_clear() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let connect = search_connect(
            Err(SpotifyError::RateLimited {
                retry_after_secs: Some(30),
            }),
            seen,
        );
        let msg = search_catalog("x", &connect).unwrap_err();
        assert!(msg.contains("limitó la tasa"), "{msg}");
        assert!(msg.contains("30"), "{msg}");
    }

    struct FakeLibrary {
        playlists: Result<Vec<SpotifyPlaylist>, SpotifyError>,
        tracks: Result<Vec<SpotifyTrack>, SpotifyError>,
        seen: std::sync::Arc<Mutex<Vec<String>>>,
    }

    impl LibrarySource for FakeLibrary {
        fn my_playlists(&self) -> Result<Vec<SpotifyPlaylist>, SpotifyError> {
            self.seen.lock().unwrap().push("my_playlists".to_string());
            self.playlists.clone()
        }

        fn playlist_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
            self.seen.lock().unwrap().push(format!("tracks:{id}"));
            self.tracks.clone()
        }
    }

    fn library_connect(
        playlists: Result<Vec<SpotifyPlaylist>, SpotifyError>,
        tracks: Result<Vec<SpotifyTrack>, SpotifyError>,
        seen: std::sync::Arc<Mutex<Vec<String>>>,
    ) -> impl Fn() -> Result<Box<dyn LibrarySource>, ConnectError> {
        move || {
            Ok(Box::new(FakeLibrary {
                playlists: playlists.clone(),
                tracks: tracks.clone(),
                seen: std::sync::Arc::clone(&seen),
            }) as Box<dyn LibrarySource>)
        }
    }

    #[test]
    fn test_user_playlists_and_tracks_use_the_client() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let playlist = SpotifyPlaylist {
            id: "p1".into(),
            name: "Mix".into(),
            owner: Some("yo".into()),
            tracks_total: Some(2),
            public: None,
            collaborative: false,
        };
        let connect = library_connect(
            Ok(vec![playlist.clone()]),
            Ok(vec![track("t1", "One", &["A"])]),
            std::sync::Arc::clone(&seen),
        );
        assert_eq!(user_playlists(&connect).unwrap(), vec![playlist]);
        assert_eq!(user_playlist_tracks("p1", &connect).unwrap()[0].id, "t1");
        assert_eq!(seen.lock().unwrap().as_slice(), &["my_playlists", "tracks:p1"]);
    }

    #[test]
    fn test_user_library_errors_are_clear_for_screen() {
        let no_session = || Err(ConnectError::NotLoggedIn);
        assert!(user_playlists(&no_session).unwrap_err().contains("--spotify-login"));
        assert!(user_playlist_tracks("p", &no_session).unwrap_err().contains("--spotify-login"));

        let no_id = || Err(ConnectError::ClientId(MissingClientId));
        assert!(user_playlists(&no_id).unwrap_err().contains(super::super::CLIENT_ID_ENV));

        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let limited = library_connect(
            Err(SpotifyError::RateLimited { retry_after_secs: Some(7) }),
            Err(SpotifyError::RateLimited { retry_after_secs: None }),
            std::sync::Arc::clone(&seen),
        );
        let msg = user_playlists(&limited).unwrap_err();
        assert!(msg.contains("limitó la tasa") && msg.contains('7'), "{msg}");
        assert!(user_playlist_tracks("p", &limited).unwrap_err().contains("limitó la tasa"));

        let expired = library_connect(
            Err(SpotifyError::Auth(AuthError::ReauthorizationRequired)),
            Ok(vec![]),
            seen,
        );
        assert!(user_playlists(&expired).unwrap_err().contains("--spotify-login"));
    }

    /// Responde según el primer argumento: `ytsearch` → candidatos, `--print`
    /// de resolución → audio. Anota cada llamada.
    struct FakeRunner {
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn new() -> Self {
            Self { calls: Mutex::new(Vec::new()) }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            _program: &str,
            args: &[String],
            _timeout: Duration,
        ) -> Result<CommandOutput, RunError> {
            self.calls.lock().unwrap().push(args.to_vec());
            let last = args.last().cloned().unwrap_or_default();
            let stdout = if last.starts_with("ytsearch") {
                "Artist - Song\nhttps://www.youtube.com/watch?v=yt1\n200\n".to_string()
            } else {
                // Salida de `resolve`: título + URL de audio.
                "Titulo de YouTube\nhttps://audio.example/yt.m4a\n".to_string()
            };
            Ok(CommandOutput {
                status_code: Some(0),
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
            })
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mp3tui_spotify_play_{name}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cache(name: &str) -> ResolveCache {
        ResolveCache::at_path(temp_dir(name).join("cache.toml"))
    }

    #[test]
    fn test_marker_label_and_ids() {
        assert_eq!(track_marker("abc"), "spotify:track:abc");
        assert_eq!(track_label(&track("1", "Song", &["A", "B"])), "A, B - Song");
        assert_eq!(track_label(&track("1", "Song", &[])), "Song");
        assert_eq!(track_id("spotify:track:abc"), Some("abc".to_string()));
        assert_eq!(
            track_id("https://open.spotify.com/track/abc?si=1"),
            Some("abc".to_string())
        );
        assert_eq!(track_id("spotify:playlist:abc"), None);
        assert_eq!(track_id("https://www.youtube.com/watch?v=abc"), None);
        assert_eq!(playlist_id("spotify:playlist:zz"), Some("zz".to_string()));
        assert_eq!(playlist_id("spotify:album:zz"), None);
        assert_eq!(playlist_id("spotify:track:zz"), None);
        assert_eq!(album_id("spotify:album:zz"), Some("zz".to_string()));
        assert_eq!(
            album_id("https://open.spotify.com/intl-es/album/zz?si=1"),
            Some("zz".to_string())
        );
        assert_eq!(album_id("spotify:playlist:zz"), None);
    }

    #[test]
    fn test_resolve_page_spotify_marker_goes_through_the_spotify_resolver() {
        let runner = FakeRunner::new();
        let mut cache = cache("spotify");
        let connect = connect_to(vec![track("t1", "Song", &["Artist"])]);

        let resolved =
            resolve_page("spotify:track:t1", &runner, &mut cache, &connect).unwrap();

        assert_eq!(resolved.title, "Artist - Song");
        assert_eq!(resolved.audio_url, "https://audio.example/yt.m4a");
        let calls = runner.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[0].last().unwrap().starts_with("ytsearch"), "{calls:?}");
        assert!(
            calls[1].iter().any(|a| a == "https://www.youtube.com/watch?v=yt1"),
            "{calls:?}"
        );
        assert_eq!(cache.get("spotify:t1"), Some("https://www.youtube.com/watch?v=yt1"));
    }

    #[test]
    fn test_resolve_page_non_spotify_uses_ytdlp_without_connecting() {
        let runner = FakeRunner::new();
        let mut cache = cache("yt");

        let resolved = resolve_page(
            "https://www.youtube.com/watch?v=abc",
            &runner,
            &mut cache,
            &connect_fails,
        )
        .unwrap();

        assert_eq!(resolved.title, "Titulo de YouTube");
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].iter().any(|a| a == "https://www.youtube.com/watch?v=abc"));
        assert!(cache.is_empty());
    }

    #[test]
    fn test_resolve_page_without_session_reports_login_hint() {
        let runner = FakeRunner::new();
        let mut cache = cache("nologin");
        let err = resolve_page("spotify:track:t1", &runner, &mut cache, &connect_fails)
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("--spotify-login"), "{text}");
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn test_resolve_page_expired_session_reports_login_hint() {
        struct Expired;
        impl TrackSource for Expired {
            fn track(&self, _: &str) -> Result<SpotifyTrack, SpotifyError> {
                Err(SpotifyError::Auth(AuthError::ReauthorizationRequired))
            }
            fn playlist_tracks(&self, _: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
                Err(SpotifyError::Auth(AuthError::NotAuthorized))
            }
            fn album_tracks(&self, _: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
                Err(SpotifyError::Auth(AuthError::NotAuthorized))
            }
            fn playlist_name(&self, _: &str) -> Result<String, SpotifyError> {
                unreachable!("el nombre se pide después de los temas")
            }
            fn album_name(&self, _: &str) -> Result<String, SpotifyError> {
                unreachable!("el nombre se pide después de los temas")
            }
        }
        let connect = || Ok(Box::new(Expired) as Box<dyn TrackSource>);
        let err = resolve_page("spotify:track:t1", &FakeRunner::new(), &mut cache("exp"), &connect)
            .unwrap_err();
        assert!(err.to_string().contains("--spotify-login"), "{err}");
        let err = list_playlist("spotify:playlist:p1", &connect).unwrap_err();
        assert!(err.to_string().contains("--spotify-login"), "{err}");
        let err = list_playlist("spotify:album:a1", &connect).unwrap_err();
        assert!(err.to_string().contains("--spotify-login"), "{err}");
    }

    /// Álbum y playlist usan endpoints distintos: cada URL llega al suyo.
    #[test]
    fn test_list_playlist_dispatches_albums_and_playlists_to_their_own_endpoint() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        struct Recording(std::sync::Arc<Mutex<Vec<String>>>);
        impl TrackSource for Recording {
            fn track(&self, _: &str) -> Result<SpotifyTrack, SpotifyError> {
                unreachable!()
            }
            fn playlist_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
                self.0.lock().unwrap().push(format!("playlist:{id}"));
                Ok(vec![track("t1", "One", &["A"])])
            }
            fn album_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
                self.0.lock().unwrap().push(format!("album:{id}"));
                Ok(vec![track("t2", "Two", &["B"])])
            }
            fn playlist_name(&self, id: &str) -> Result<String, SpotifyError> {
                self.0.lock().unwrap().push(format!("playlist_name:{id}"));
                Ok("Lista".to_string())
            }
            fn album_name(&self, id: &str) -> Result<String, SpotifyError> {
                self.0.lock().unwrap().push(format!("album_name:{id}"));
                Ok("Disco".to_string())
            }
        }
        let shared = std::sync::Arc::clone(&seen);
        let connect = move || Ok(Box::new(Recording(std::sync::Arc::clone(&shared))) as Box<dyn TrackSource>);

        let album = list_playlist("https://open.spotify.com/album/alb1", &connect).unwrap();
        let playlist = list_playlist("spotify:playlist:pl1", &connect).unwrap();

        // Una sola pasada por colección: temas y después su nombre real.
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["album:alb1", "album_name:alb1", "playlist:pl1", "playlist_name:pl1"]
        );
        assert_eq!(album.entries[0].page_url, "spotify:track:t2");
        assert_eq!(album.name.as_deref(), Some("Disco"));
        assert_eq!(playlist.entries[0].page_url, "spotify:track:t1");
        assert_eq!(playlist.name.as_deref(), Some("Lista"));
    }

    #[test]
    fn test_list_playlist_builds_titles_and_markers_and_skips_local_tracks() {
        let connect = connect_to(vec![
            track("t1", "One", &["A"]),
            track("", "Local file", &["B"]),
            track("t2", "Two", &["C", "D"]),
        ]);

        let collection =
            list_playlist("https://open.spotify.com/album/p1?si=x", &connect).unwrap();

        assert_eq!(collection.name.as_deref(), Some("Nombre real"));
        assert_eq!(
            collection.entries,
            vec![
                PlaylistEntry { title: "A - One".into(), page_url: "spotify:track:t1".into() },
                PlaylistEntry { title: "C, D - Two".into(), page_url: "spotify:track:t2".into() },
            ]
        );
    }

    /// El nombre es un extra: si falla (429, red) el listado igual sale, sin
    /// nombre, y la UI usa su respaldo.
    #[test]
    fn test_list_playlist_survives_a_failing_name_lookup() {
        for failure in [
            SpotifyError::RateLimited { retry_after_secs: Some(3) },
            SpotifyError::BadResponse,
            SpotifyError::Auth(AuthError::NotAuthorized),
        ] {
            let connect = connect_named(vec![track("t1", "One", &["A"])], Err(failure));
            let collection = list_playlist("spotify:playlist:p1", &connect).unwrap();
            assert_eq!(collection.name, None);
            assert_eq!(collection.entries.len(), 1);
        }
    }

    #[test]
    fn test_list_playlist_treats_a_blank_name_as_missing() {
        let connect = connect_named(vec![track("t1", "One", &["A"])], Ok("   ".to_string()));
        assert_eq!(list_playlist("spotify:playlist:p1", &connect).unwrap().name, None);
    }

    #[test]
    fn test_list_playlist_rejects_non_playlist_and_propagates_connect_errors() {
        let connect = connect_to(vec![]);
        assert!(list_playlist("spotify:track:t1", &connect).is_err());
        let err = list_playlist("spotify:playlist:p1", &connect_fails).unwrap_err();
        assert!(err.to_string().contains("--spotify-login"), "{err}");
    }

    #[test]
    fn test_connect_without_client_id_explains_how_to_set_it() {
        let err = connect_with(Err(MissingClientId), Some(temp_dir("cid").join("t.toml")))
            .err()
            .unwrap();
        let text = err.to_string();
        assert!(text.contains("client_id"), "{text}");
        assert!(text.contains("MP3TUI_SPOTIFY_CLIENT_ID"), "{text}");
    }

    #[test]
    fn test_connect_without_tokens_says_to_run_spotify_login() {
        let err = connect_with(Ok("cid".into()), Some(temp_dir("notok").join("t.toml")))
            .err()
            .unwrap();
        assert_eq!(err, ConnectError::NotLoggedIn);
        assert!(err.to_string().contains("mp3tui --spotify-login"), "{err}");
    }

    #[test]
    fn test_connect_without_config_dir_is_an_error() {
        let err = connect_with(Ok("cid".into()), None).err().unwrap();
        assert_eq!(err, ConnectError::NoConfigDir);
    }

    #[test]
    fn test_connect_with_tokens_builds_the_client() {
        let path = temp_dir("ok").join("t.toml");
        auth::save_tokens(
            &path,
            &Tokens {
                access_token: "A".into(),
                refresh_token: "R".into(),
                expires_at: auth::unix_now() + 3600,
            },
        )
        .unwrap();
        let client = connect_with(Ok("cid".into()), Some(path.clone())).unwrap();
        assert_eq!(client.tokens_path(), path.as_path());
    }

    #[test]
    fn test_run_login_requires_client_id_and_passes_it_to_the_login() {
        let path = temp_dir("login").join("t.toml");
        let err = run_login(Err(MissingClientId), Some(path.clone()), |_, _| unreachable!())
            .unwrap_err();
        assert!(err.contains("MP3TUI_SPOTIFY_CLIENT_ID"), "{err}");

        let mut seen = None;
        run_login(Ok("my-id".into()), Some(path.clone()), |client, store| {
            seen = Some((client.client_id().to_string(), store.to_path_buf()));
            Ok(Tokens { access_token: "A".into(), refresh_token: "R".into(), expires_at: 0 })
        })
        .unwrap();
        assert_eq!(seen, Some(("my-id".to_string(), path.clone())));

        let err = run_login(Ok("id".into()), Some(path), |_, _| Err(AuthError::Timeout))
            .unwrap_err();
        assert!(err.contains("tiempo"), "{err}");
        assert!(!err.contains("REFRESH"));
    }
}
