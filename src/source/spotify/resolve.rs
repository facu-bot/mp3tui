//! Resolución de canciones de Spotify a una URL de YouTube reproducible.
//!
//! La Web API no entrega audio: se busca `"Artistas - Título"` con yt-dlp, se
//! valida el candidato por título y duración y se cachea `spotify:<id>`.

use std::fmt;

use super::client::SpotifyTrack;
use super::play::TrackSource;
use crate::source::ytdlp::{self, CommandRunner, ResolveCache, YtDlpError};

/// Errores de [`resolve_track`]. Nunca incluyen tokens: solo texto de la
/// canción y errores de yt-dlp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Falló la búsqueda en yt-dlp.
    Search(YtDlpError),
    /// Ningún resultado coincide con el título y la duración de la canción.
    NoMatch { query: String },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Search(err) => write!(f, "{err}"),
            Self::NoMatch { query } => write!(
                f,
                "no se encontró en YouTube una coincidencia para «{query}»"
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<YtDlpError> for ResolveError {
    fn from(err: YtDlpError) -> Self {
        Self::Search(err)
    }
}

/// Clave de caché de una canción de Spotify.
fn cache_key(id: &str) -> String {
    format!("spotify:{id}")
}

/// Consulta de búsqueda `"Artistas - Título"`.
fn search_query(track: &SpotifyTrack) -> String {
    let artists = track.artists.join(", ");
    if artists.is_empty() {
        track.title.clone()
    } else {
        format!("{artists} - {}", track.title)
    }
}

/// Devuelve la URL de YouTube de `track`, usando la caché si ya se resolvió.
///
/// `_client` no se usa todavía: queda en la firma para resolver por ISRC o
/// metadatos extra sin cambiar a los llamadores.
pub fn resolve_track(
    _client: &dyn TrackSource,
    cache: &mut ResolveCache,
    track: &SpotifyTrack,
    runner: &dyn CommandRunner,
) -> Result<String, ResolveError> {
    let key = (!track.id.is_empty()).then(|| cache_key(&track.id));
    if let Some(url) = key.as_deref().and_then(|key| cache.get(key)) {
        return Ok(url.to_string());
    }

    let query = search_query(track);
    let candidates = ytdlp::search(&query, runner)?;
    let duration_secs = Some(track.duration_ms as f64 / 1000.0);
    let Some(best) = ytdlp::best_match(&track.artists, &track.title, duration_secs, &candidates)
    else {
        return Err(ResolveError::NoMatch { query });
    };

    let url = best.webpage_url.clone();
    if let Some(key) = key {
        // La caché es una optimización: si no se puede escribir, la
        // reproducción sigue igual.
        let _ = cache.put(&key, &url);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::spotify::auth::TokenClient;
    use crate::source::spotify::client::SpotifyClient;
    use crate::source::ytdlp::{CommandOutput, RunError};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct FakeRunner {
        responses: Mutex<Vec<Result<CommandOutput, RunError>>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn new(responses: Vec<Result<CommandOutput, RunError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn stdout(text: &str) -> Result<CommandOutput, RunError> {
            Ok(CommandOutput {
                status_code: Some(0),
                stdout: text.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
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
            let mut responses = self.responses.lock().unwrap();
            assert!(!responses.is_empty(), "FakeRunner sin respuestas preparadas");
            responses.remove(0)
        }
    }

    fn temp_cache_path(name: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mp3tui_spotify_resolve_{name}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("cache.toml")
    }

    /// El cliente no se usa para resolver: apunta a un puerto cerrado para
    /// que cualquier uso accidental falle en vez de salir a la red.
    fn dead_client() -> SpotifyClient {
        SpotifyClient::with_base_url(
            "http://127.0.0.1:1",
            TokenClient::with_token_url("test-client", "http://127.0.0.1:1/api/token"),
            std::env::temp_dir().join("mp3tui_spotify_resolve_no_tokens.toml"),
        )
    }

    fn track() -> SpotifyTrack {
        SpotifyTrack {
            id: "t1".to_string(),
            title: "One More Time".to_string(),
            artists: vec!["Daft Punk".to_string()],
            album: Some("Discovery".to_string()),
            duration_ms: 320_000,
            isrc: None,
        }
    }

    const CANDIDATES: &str = "\
Daft Punk - One More Time (Official Video)\nhttps://www.youtube.com/watch?v=good\n320\n\
One More Time (1 hour loop)\nhttps://www.youtube.com/watch?v=loop\n3600\n";

    #[test]
    fn test_search_query_is_artists_dash_title() {
        let mut t = track();
        assert_eq!(search_query(&t), "Daft Punk - One More Time");
        t.artists = vec!["A".to_string(), "B".to_string()];
        assert_eq!(search_query(&t), "A, B - One More Time");
        t.artists.clear();
        assert_eq!(search_query(&t), "One More Time");
    }

    #[test]
    fn test_resolves_searches_matches_and_caches() {
        let path = temp_cache_path("miss");
        let mut cache = ResolveCache::at_path(path.clone());
        let runner = FakeRunner::new(vec![FakeRunner::stdout(CANDIDATES)]);

        let url = resolve_track(&dead_client(), &mut cache, &track(), &runner).unwrap();

        assert_eq!(url, "https://www.youtube.com/watch?v=good");
        assert_eq!(cache.get("spotify:t1"), Some(url.as_str()));
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].iter().any(|a| a == "ytsearch5:Daft Punk - One More Time"),
            "{:?}",
            calls[0]
        );
        // Persistida: otra instancia sobre el mismo archivo la ve.
        assert_eq!(
            ResolveCache::at_path(path).get("spotify:t1"),
            Some("https://www.youtube.com/watch?v=good")
        );
    }

    #[test]
    fn test_cache_hit_skips_ytdlp() {
        let mut cache = ResolveCache::at_path(temp_cache_path("hit"));
        cache.put("spotify:t1", "https://www.youtube.com/watch?v=cached").unwrap();
        let runner = FakeRunner::new(vec![]);

        let url = resolve_track(&dead_client(), &mut cache, &track(), &runner).unwrap();

        assert_eq!(url, "https://www.youtube.com/watch?v=cached");
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn test_no_match_is_a_clear_error_and_nothing_is_cached() {
        let mut cache = ResolveCache::at_path(temp_cache_path("nomatch"));
        let runner = FakeRunner::new(vec![FakeRunner::stdout(
            "Otra cancion\nhttps://www.youtube.com/watch?v=x\n100\n",
        )]);

        let err = resolve_track(&dead_client(), &mut cache, &track(), &runner).unwrap_err();

        assert_eq!(
            err,
            ResolveError::NoMatch {
                query: "Daft Punk - One More Time".to_string()
            }
        );
        let text = err.to_string();
        assert!(text.contains("Daft Punk - One More Time"), "{text}");
        assert!(cache.is_empty());
    }

    #[test]
    fn test_duration_mismatch_is_rejected() {
        let mut cache = ResolveCache::at_path(temp_cache_path("duration"));
        // Mismo título pero 1 hora: fuera de la tolerancia de duración.
        let runner = FakeRunner::new(vec![FakeRunner::stdout(
            "One More Time\nhttps://www.youtube.com/watch?v=loop\n3600\n",
        )]);
        let err = resolve_track(&dead_client(), &mut cache, &track(), &runner).unwrap_err();
        assert!(matches!(err, ResolveError::NoMatch { .. }), "{err:?}");
    }

    #[test]
    fn test_ytdlp_errors_are_propagated() {
        let mut cache = ResolveCache::at_path(temp_cache_path("notfound"));
        let runner = FakeRunner::new(vec![Err(RunError::NotFound)]);
        let err = resolve_track(&dead_client(), &mut cache, &track(), &runner).unwrap_err();
        assert_eq!(err, ResolveError::Search(YtDlpError::NotInstalled));
        assert!(err.to_string().contains("yt-dlp"), "{err}");
        assert!(cache.is_empty());
    }

    #[test]
    fn test_track_without_id_is_not_cached() {
        let path = temp_cache_path("noid");
        let mut cache = ResolveCache::at_path(path);
        let runner = FakeRunner::new(vec![FakeRunner::stdout(CANDIDATES)]);
        let mut t = track();
        t.id = String::new();

        let url = resolve_track(&dead_client(), &mut cache, &t, &runner).unwrap();

        assert_eq!(url, "https://www.youtube.com/watch?v=good");
        assert!(cache.is_empty());
    }

    #[test]
    fn test_errors_never_leak_tokens() {
        let err = ResolveError::Search(YtDlpError::Failed("boom".to_string()));
        let text = err.to_string();
        assert!(!text.contains("REFRESH"), "{text}");
        assert!(!text.contains("Bearer"), "{text}");
    }
}
