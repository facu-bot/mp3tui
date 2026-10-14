//! Integración de Spotify con la reproducción: conexión del cliente, despacho
//! del resolvedor por página, listado de playlists y login por CLI.
//!
//! Todo lo que toca red o procesos recibe sus dependencias por parámetro para
//! que la UI lo corra en un hilo y los tests lo inyecten.

use std::fmt;
use std::path::{Path, PathBuf};

use super::auth::{self, AuthError, TokenClient, Tokens};
use super::client::{SpotifyClient, SpotifyError, SpotifyTrack};
use super::resolve::{ResolveError, resolve_track};
use super::{MissingClientId, client_id};
use crate::config::Config;
use crate::source::{self, UrlKind, ytdlp};
use ytdlp::{CommandRunner, PlaylistEntry, Resolved, ResolveCache, YtDlpError};

/// Lo mínimo del cliente que necesita la reproducción (seam para tests).
pub trait TrackSource {
    fn track(&self, id: &str) -> Result<SpotifyTrack, SpotifyError>;
    fn playlist_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError>;
}

impl TrackSource for SpotifyClient {
    fn track(&self, id: &str) -> Result<SpotifyTrack, SpotifyError> {
        SpotifyClient::track(self, id)
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

/// Id de la playlist o álbum si `url` lo es.
pub fn playlist_id(url: &str) -> Option<String> {
    source::spotify_ref(url)
        .filter(|r| r.kind == UrlKind::SpotifyPlaylist)
        .map(|r| r.id)
}

fn failure(err: SpotifyError) -> YtDlpError {
    match err {
        SpotifyError::Auth(AuthError::NotAuthorized | AuthError::ReauthorizationRequired) => {
            YtDlpError::Failed(LOGIN_HINT.to_string())
        }
        other => YtDlpError::Failed(other.to_string()),
    }
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

/// Lista una playlist o álbum de Spotify como entradas de cola: título
/// `"Artistas - Título"` y marcador `spotify:track:<id>`. Las canciones sin id
/// (archivos locales) se omiten porque no se pueden buscar.
pub fn list_playlist(
    url: &str,
    connect: &dyn Fn() -> Result<Box<dyn TrackSource>, ConnectError>,
) -> Result<Vec<PlaylistEntry>, YtDlpError> {
    let id = playlist_id(url)
        .ok_or_else(|| YtDlpError::InvalidUrl("no es una playlist de Spotify".to_string()))?;
    let client = connect().map_err(|e| YtDlpError::Failed(e.to_string()))?;
    let tracks = client.playlist_tracks(&id).map_err(failure)?;
    Ok(tracks
        .iter()
        .filter(|track| !track.id.is_empty())
        .map(|track| PlaylistEntry {
            title: track_label(track),
            page_url: track_marker(&track.id),
        })
        .collect())
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
    }

    fn connect_to(tracks: Vec<SpotifyTrack>) -> impl Fn() -> Result<Box<dyn TrackSource>, ConnectError> {
        move || {
            Ok(Box::new(FakeSource {
                tracks: tracks.clone(),
                seen: Mutex::new(Vec::new()),
            }) as Box<dyn TrackSource>)
        }
    }

    fn connect_fails() -> Result<Box<dyn TrackSource>, ConnectError> {
        Err(ConnectError::NotLoggedIn)
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
        assert_eq!(playlist_id("spotify:album:zz"), Some("zz".to_string()));
        assert_eq!(playlist_id("spotify:track:zz"), None);
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
        }
        let connect = || Ok(Box::new(Expired) as Box<dyn TrackSource>);
        let err = resolve_page("spotify:track:t1", &FakeRunner::new(), &mut cache("exp"), &connect)
            .unwrap_err();
        assert!(err.to_string().contains("--spotify-login"), "{err}");
        let err = list_playlist("spotify:playlist:p1", &connect).unwrap_err();
        assert!(err.to_string().contains("--spotify-login"), "{err}");
    }

    #[test]
    fn test_list_playlist_builds_titles_and_markers_and_skips_local_tracks() {
        let connect = connect_to(vec![
            track("t1", "One", &["A"]),
            track("", "Local file", &["B"]),
            track("t2", "Two", &["C", "D"]),
        ]);

        let entries = list_playlist("https://open.spotify.com/album/p1?si=x", &connect).unwrap();

        assert_eq!(
            entries,
            vec![
                PlaylistEntry { title: "A - One".into(), page_url: "spotify:track:t1".into() },
                PlaylistEntry { title: "C, D - Two".into(), page_url: "spotify:track:t2".into() },
            ]
        );
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
