//! Cliente de metadatos de la Web API de Spotify.
//!
//! Solo lectura: búsqueda (canciones, playlists y artistas), canciones de una
//! playlist, playlists del usuario y canciones guardadas. El access token se
//! obtiene de [`TokenClient::fresh_tokens`] (que renueva solo, con margen); ante
//! un `401` se renueva una única vez y se reintenta la petición. Ningún valor de
//! token se imprime ni se incluye en un mensaje de error, y los cuerpos de
//! respuesta nunca se vuelcan crudos en los errores.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::auth::{self, AuthError, TokenClient};

/// Base por defecto de la Web API de Spotify.
pub const API_BASE_URL: &str = "https://api.spotify.com";

/// Canción de Spotify. La Web API no entrega audio: solo estos metadatos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyTrack {
    pub id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: u64,
    pub isrc: Option<String>,
}

/// Playlist de Spotify (metadatos; no incluye sus canciones).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyPlaylist {
    pub id: String,
    pub name: String,
    pub owner: Option<String>,
    pub tracks_total: Option<u64>,
}

/// Artista de Spotify, con lo mínimo para mostrar resultados de búsqueda.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyArtist {
    pub id: String,
    pub name: String,
}

/// Resultado de [`SpotifyClient::search`]: las tres categorías de S1.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResults {
    pub tracks: Vec<SpotifyTrack>,
    pub playlists: Vec<SpotifyPlaylist>,
    pub artists: Vec<SpotifyArtist>,
}

/// Errores del cliente de metadatos.
///
/// Nunca contienen valores de tokens ni el cuerpo crudo de la respuesta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpotifyError {
    /// Falla de autenticación (falta de sesión, refresh vencido, etc.).
    Auth(AuthError),
    /// Falla de red o de transporte HTTP.
    Http(String),
    /// Respuesta no-2xx con el `message` que reportó Spotify (si lo hubo).
    Status {
        status: u16,
        message: Option<String>,
    },
    /// La API limitó la tasa de peticiones; se conserva `Retry-After`.
    RateLimited { retry_after_secs: Option<u64> },
    /// La respuesta no tenía la forma JSON esperada.
    BadResponse,
}

impl fmt::Display for SpotifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auth(e) => write!(f, "{e}"),
            Self::Http(e) => write!(f, "error de red al hablar con la Web API de Spotify: {e}"),
            Self::Status { status, message } => match message {
                Some(m) => write!(f, "la Web API de Spotify respondió HTTP {status}: {m}"),
                None => write!(f, "la Web API de Spotify respondió HTTP {status}"),
            },
            Self::RateLimited { retry_after_secs } => match retry_after_secs {
                Some(s) => write!(
                    f,
                    "Spotify limitó la tasa de peticiones; reintentar en {s} s"
                ),
                None => write!(f, "Spotify limitó la tasa de peticiones"),
            },
            Self::BadResponse => write!(f, "respuesta inesperada de la Web API de Spotify"),
        }
    }
}

impl std::error::Error for SpotifyError {}

impl From<AuthError> for SpotifyError {
    fn from(err: AuthError) -> Self {
        Self::Auth(err)
    }
}

/// Cliente de metadatos de la Web API de Spotify.
///
/// La base de la API y el endpoint de tokens son inyectables para testear
/// contra un stub local.
pub struct SpotifyClient {
    base_url: String,
    tokens_path: PathBuf,
    token_client: TokenClient,
    agent: ureq::Agent,
}

impl SpotifyClient {
    /// Cliente contra la API real, reutilizando un [`TokenClient`] ya armado.
    pub fn new(token_client: TokenClient, tokens_path: impl Into<PathBuf>) -> Self {
        Self::with_base_url(API_BASE_URL, token_client, tokens_path)
    }

    /// Cliente con base URL inyectable (tests contra un stub).
    pub fn with_base_url(
        base_url: impl Into<String>,
        token_client: TokenClient,
        tokens_path: impl Into<PathBuf>,
    ) -> Self {
        let config = ureq::config::Config::builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(20)))
            .build();
        Self {
            base_url: base_url.into(),
            tokens_path: tokens_path.into(),
            token_client,
            agent: config.new_agent(),
        }
    }

    /// Ruta del archivo de tokens que usa este cliente.
    pub fn tokens_path(&self) -> &Path {
        &self.tokens_path
    }

    /// Busca canciones, playlists y artistas que coincidan con `query`.
    pub fn search(&self, query: &str) -> Result<SearchResults, SpotifyError> {
        let value = self.fetch(
            &self.resolve("/v1/search"),
            &[("q", query), ("type", "track,playlist,artist")],
        )?;
        let parsed: ApiSearch =
            serde_json::from_value(value).map_err(|_| SpotifyError::BadResponse)?;
        Ok(SearchResults {
            tracks: parsed
                .tracks
                .items
                .into_iter()
                .flatten()
                .map(SpotifyTrack::from_api)
                .collect(),
            playlists: parsed
                .playlists
                .items
                .into_iter()
                .flatten()
                .map(SpotifyPlaylist::from_api)
                .collect(),
            artists: parsed
                .artists
                .items
                .into_iter()
                .flatten()
                .map(SpotifyArtist::from_api)
                .collect(),
        })
    }

    /// Metadatos de una canción por id (`GET /v1/tracks/{id}`).
    pub fn track(&self, id: &str) -> Result<SpotifyTrack, SpotifyError> {
        let value = self.fetch(&self.resolve(&format!("/v1/tracks/{id}")), &[])?;
        let parsed: ApiTrack =
            serde_json::from_value(value).map_err(|_| SpotifyError::BadResponse)?;
        Ok(SpotifyTrack::from_api(parsed))
    }

    /// Canciones de una playlist, siguiendo la paginación de `next`.
    pub fn playlist_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
        let path = format!("/v1/playlists/{id}/tracks");
        let items: Vec<ApiPlaylistTrack> = self.collect_pages(&path, &[])?;
        Ok(items
            .into_iter()
            .filter_map(|item| item.track)
            .map(SpotifyTrack::from_api)
            .collect())
    }

    /// Canciones de un álbum, con paginación. A diferencia de las playlists, los
    /// ítems son canciones directas (sin objeto `track` anidado ni `album`).
    pub fn album_tracks(&self, id: &str) -> Result<Vec<SpotifyTrack>, SpotifyError> {
        let path = format!("/v1/albums/{id}/tracks");
        let items: Vec<ApiTrack> = self.collect_pages(&path, &[])?;
        Ok(items.into_iter().map(SpotifyTrack::from_api).collect())
    }

    /// Playlists del usuario autenticado (incluye las privadas), con paginación.
    pub fn my_playlists(&self) -> Result<Vec<SpotifyPlaylist>, SpotifyError> {
        let items: Vec<ApiPlaylist> = self.collect_pages("/v1/me/playlists", &[])?;
        Ok(items.into_iter().map(SpotifyPlaylist::from_api).collect())
    }

    /// Canciones guardadas del usuario, con paginación.
    pub fn liked_tracks(&self) -> Result<Vec<SpotifyTrack>, SpotifyError> {
        let items: Vec<ApiPlaylistTrack> = self.collect_pages("/v1/me/tracks", &[])?;
        Ok(items
            .into_iter()
            .filter_map(|item| item.track)
            .map(SpotifyTrack::from_api)
            .collect())
    }

    /// GET con `Bearer`, siguiendo `next` hasta agotar la paginación.
    fn collect_pages<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<T>, SpotifyError> {
        let mut collected = Vec::new();
        let mut url = self.resolve(path);
        let mut params: Vec<(&str, &str)> = query.to_vec();
        // Un `next` que se repite (o vuelve atrás) sería un bucle infinito en
        // un hilo de fondo: se corta al ver una URL ya visitada.
        let mut seen = std::collections::HashSet::new();
        loop {
            if !seen.insert(url.clone()) {
                break;
            }
            let value = self.fetch(&url, &params)?;
            let page: ApiPage<T> =
                serde_json::from_value(value).map_err(|_| SpotifyError::BadResponse)?;
            collected.extend(page.items.into_iter().flatten());
            match page.next.filter(|next| !next.is_empty()) {
                Some(next) => {
                    // La URL de `next` ya trae la query: no se repite.
                    url = self.resolve(&next);
                    params.clear();
                }
                None => break,
            }
        }
        Ok(collected)
    }

    /// Una petición autenticada. Ante `401` renueva el token una sola vez y
    /// reintenta; otros no-2xx se traducen a errores tipados.
    fn fetch(&self, url: &str, query: &[(&str, &str)]) -> Result<serde_json::Value, SpotifyError> {
        let mut tokens = self
            .token_client
            .fresh_tokens(&self.tokens_path)
            .map_err(SpotifyError::Auth)?;
        let mut refreshed = false;
        loop {
            let mut request = self
                .agent
                .get(url)
                .header("Authorization", format!("Bearer {}", tokens.access_token));
            for (key, value) in query {
                request = request.query(*key, *value);
            }
            let mut response = request
                .call()
                .map_err(|e| SpotifyError::Http(e.to_string()))?;
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok());
            let body = response
                .body_mut()
                .read_to_string()
                .map_err(|e| SpotifyError::Http(e.to_string()))?;

            match status {
                200..=299 => {
                    return serde_json::from_str(&body).map_err(|_| SpotifyError::BadResponse);
                }
                401 if !refreshed => {
                    refreshed = true;
                    tokens = self
                        .token_client
                        .refresh(&tokens)
                        .map_err(SpotifyError::Auth)?;
                    // Mejor esfuerzo: si no se puede persistir, el reintento usa
                    // igual el token renovado en memoria.
                    let _ = auth::save_tokens(&self.tokens_path, &tokens);
                }
                429 => {
                    return Err(SpotifyError::RateLimited {
                        retry_after_secs: retry_after,
                    });
                }
                _ => {
                    return Err(SpotifyError::Status {
                        status,
                        message: parse_error_message(&body),
                    });
                }
            }
        }
    }

    /// Resuelve una ruta relativa contra la base; deja intacta una URL absoluta
    /// (que es como Spotify devuelve el campo `next`).
    fn resolve(&self, path_or_url: &str) -> String {
        if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            path_or_url.to_string()
        } else {
            format!(
                "{}/{}",
                self.base_url.trim_end_matches('/'),
                path_or_url.trim_start_matches('/')
            )
        }
    }
}

impl SpotifyTrack {
    fn from_api(track: ApiTrack) -> Self {
        Self {
            id: track.id.unwrap_or_default(),
            title: track.name,
            artists: track
                .artists
                .into_iter()
                .map(|artist| artist.name)
                .filter(|name| !name.is_empty())
                .collect(),
            album: track
                .album
                .map(|album| album.name)
                .filter(|n| !n.is_empty()),
            duration_ms: track.duration_ms,
            isrc: track
                .external_ids
                .and_then(|ids| ids.isrc)
                .filter(|s| !s.is_empty()),
        }
    }
}

impl SpotifyPlaylist {
    fn from_api(playlist: ApiPlaylist) -> Self {
        Self {
            id: playlist.id.unwrap_or_default(),
            name: playlist.name,
            owner: playlist
                .owner
                .and_then(|owner| owner.display_name)
                .filter(|n| !n.is_empty()),
            tracks_total: playlist.tracks.and_then(|tracks| tracks.total),
        }
    }
}

impl SpotifyArtist {
    fn from_api(artist: ApiArtist) -> Self {
        Self {
            id: artist.id.unwrap_or_default(),
            name: artist.name,
        }
    }
}

/// `message` del error estructurado de Spotify, si el cuerpo lo trae. Nunca se
/// expone el cuerpo crudo.
fn parse_error_message(body: &str) -> Option<String> {
    serde_json::from_str::<ApiErrorBody>(body)
        .ok()
        .and_then(|parsed| parsed.error.message)
        .filter(|message| !message.is_empty())
}

// --- Formas JSON de la API (privadas) -------------------------------------

#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct ApiPage<T> {
    #[serde(default)]
    items: Vec<Option<T>>,
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct ApiItems<T> {
    #[serde(default)]
    items: Vec<Option<T>>,
}

impl<T> Default for ApiItems<T> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

#[derive(Deserialize, Default)]
struct ApiSearch {
    #[serde(default)]
    tracks: ApiItems<ApiTrack>,
    #[serde(default)]
    playlists: ApiItems<ApiPlaylist>,
    #[serde(default)]
    artists: ApiItems<ApiArtist>,
}

#[derive(Deserialize)]
struct ApiTrack {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    artists: Vec<ApiArtist>,
    #[serde(default)]
    album: Option<ApiNamed>,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    external_ids: Option<ApiExternalIds>,
}

#[derive(Deserialize)]
struct ApiArtist {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct ApiNamed {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct ApiExternalIds {
    #[serde(default)]
    isrc: Option<String>,
}

#[derive(Deserialize)]
struct ApiPlaylist {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    owner: Option<ApiOwner>,
    #[serde(default)]
    tracks: Option<ApiTracksRef>,
}

#[derive(Deserialize)]
struct ApiOwner {
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct ApiTracksRef {
    #[serde(default)]
    total: Option<u64>,
}

/// Elemento de `/playlists/{id}/tracks` y `/me/tracks`. Spotify renombró
/// `track` a `item` en algunos endpoints; se aceptan ambos.
#[derive(Deserialize)]
struct ApiPlaylistTrack {
    #[serde(default, alias = "item")]
    track: Option<ApiTrack>,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    error: ApiErrorMessage,
}

#[derive(Deserialize)]
struct ApiErrorMessage {
    #[serde(default)]
    message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::spotify::auth::Tokens;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};

    // --- helpers ---

    fn temp_path(name: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mp3tui_spotify_client_{name}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("spotify.toml")
    }

    fn write_tokens(path: &Path, access: &str) {
        let tokens = Tokens {
            access_token: access.to_string(),
            refresh_token: "REFRESH-TEST".to_string(),
            expires_at: auth::unix_now() + 3600,
        };
        auth::save_tokens(path, &tokens).unwrap();
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    #[derive(Clone, Default)]
    struct SeenRequest {
        target: String,
        bearer: Option<String>,
    }

    struct StubResponse {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    }

    fn json_response(status: u16, body: impl Into<String>) -> StubResponse {
        StubResponse {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// Servidor HTTP falso mínimo. Responde en orden las respuestas que arma la
    /// clausura (que recibe la URL base para poder armar páginas `next`).
    struct Stub {
        base_url: String,
        requests: Arc<Mutex<Vec<SeenRequest>>>,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl Stub {
        fn spawn<F>(build: F) -> Self
        where
            F: FnOnce(&str) -> Vec<StubResponse>,
        {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let responses = build(&base_url);
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&requests);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_flag = Arc::clone(&stop);
            let handle = thread::spawn(move || {
                let mut index = 0usize;
                while !stop_flag.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if index >= responses.len() {
                                break;
                            }
                            let head = read_head(&mut stream);
                            seen.lock().unwrap().push(parse_request(&head));
                            let response = &responses[index];
                            let mut reply = format!(
                                "HTTP/1.1 {} X\r\nContent-Type: application/json\r\n",
                                response.status
                            );
                            for (name, value) in &response.headers {
                                reply.push_str(&format!("{name}: {value}\r\n"));
                            }
                            reply.push_str(&format!(
                                "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                                response.body.len(),
                                response.body
                            ));
                            let _ = stream.write_all(reply.as_bytes());
                            index += 1;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                base_url,
                requests,
                stop,
                handle: Some(handle),
            }
        }

        fn finish(mut self) -> Vec<SeenRequest> {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            self.requests.lock().unwrap().clone()
        }
    }

    fn read_head(stream: &mut TcpStream) -> String {
        let mut data = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    data.extend_from_slice(&buf[..n]);
                    if find(&data, b"\r\n\r\n").is_some() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&data).into_owned()
    }

    fn parse_request(head: &str) -> SeenRequest {
        let mut lines = head.lines();
        let target = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_string();
        let bearer = head
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
            .and_then(|l| l.splitn(2, ':').nth(1))
            .map(|v| v.trim().to_string())
            .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string));
        SeenRequest { target, bearer }
    }

    fn client_with(api_base: &str, token_url: &str, tokens_file: &Path) -> SpotifyClient {
        SpotifyClient::with_base_url(
            api_base,
            TokenClient::with_token_url("test-client", token_url),
            tokens_file,
        )
    }

    const SEARCH_JSON: &str = r#"{
      "tracks": {"items": [
        {"id":"t1","name":"One More Time","artists":[{"id":"a1","name":"Daft Punk"}],
         "album":{"name":"Discovery"},"duration_ms":320000,
         "external_ids":{"isrc":"GBDUW0000059"}},
        null
      ]},
      "playlists": {"items": [
        {"id":"p1","name":"French Touch","owner":{"display_name":"Alice"},
         "tracks":{"total":42}}
      ]},
      "artists": {"items": [
        {"id":"a1","name":"Daft Punk"}
      ]}
    }"#;

    // --- search ---

    #[test]
    fn test_search_parses_tracks_playlists_and_artists() {
        let path = temp_path("search");
        write_tokens(&path, "TOKEN-1");
        let stub = Stub::spawn(|_| vec![json_response(200, SEARCH_JSON)]);
        let client = client_with(&stub.base_url, "http://127.0.0.1:1/api/token", &path);
        let results = client.search("daft punk").unwrap();
        let seen = stub.finish();

        assert_eq!(results.tracks.len(), 1);
        let track = &results.tracks[0];
        assert_eq!(track.id, "t1");
        assert_eq!(track.title, "One More Time");
        assert_eq!(track.artists, vec!["Daft Punk".to_string()]);
        assert_eq!(track.album.as_deref(), Some("Discovery"));
        assert_eq!(track.duration_ms, 320_000);
        assert_eq!(track.isrc.as_deref(), Some("GBDUW0000059"));

        assert_eq!(results.playlists.len(), 1);
        assert_eq!(results.playlists[0].id, "p1");
        assert_eq!(results.playlists[0].name, "French Touch");
        assert_eq!(results.playlists[0].owner.as_deref(), Some("Alice"));
        assert_eq!(results.playlists[0].tracks_total, Some(42));

        assert_eq!(results.artists.len(), 1);
        assert_eq!(results.artists[0].id, "a1");
        assert_eq!(results.artists[0].name, "Daft Punk");

        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].target.starts_with("/v1/search?"),
            "{}",
            seen[0].target
        );
        assert!(
            seen[0].target.contains("q=daft%20punk"),
            "{}",
            seen[0].target
        );
        assert!(
            seen[0].target.contains("type=track%2Cplaylist%2Cartist"),
            "{}",
            seen[0].target
        );
        assert_eq!(seen[0].bearer.as_deref(), Some("TOKEN-1"));
    }

    // --- paginación ---

    #[test]
    fn test_playlist_tracks_follows_next_pages() {
        let path = temp_path("playlist_pages");
        write_tokens(&path, "TOKEN-1");
        let stub = Stub::spawn(|base| {
            let next = format!("{base}/v1/playlists/PL/tracks?offset=1&limit=1");
            vec![
                json_response(
                    200,
                    r#"{"items":[{"track":{"id":"t1","name":"Uno","artists":[{"name":"A"}],"duration_ms":1000}}],"next":"__NEXT__"}"#
                        .replace("__NEXT__", &next),
                ),
                json_response(
                    200,
                    r#"{"items":[{"track":{"id":"t2","name":"Dos","artists":[{"name":"B"}],"duration_ms":2000}}],"next":null}"#,
                ),
            ]
        });
        let client = client_with(&stub.base_url, "http://127.0.0.1:1/api/token", &path);
        let tracks = client.playlist_tracks("PL").unwrap();
        let seen = stub.finish();

        assert_eq!(
            tracks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["t1", "t2"]
        );
        assert_eq!(seen.len(), 2);
        assert!(seen[0].target.starts_with("/v1/playlists/PL/tracks"));
        assert!(seen[1].target.contains("offset=1"), "{}", seen[1].target);
    }

    #[test]
    fn test_liked_tracks_paginates_and_accepts_item_alias() {
        let path = temp_path("liked");
        write_tokens(&path, "TOKEN-1");
        let stub = Stub::spawn(|base| {
            let next = format!("{base}/v1/me/tracks?offset=1");
            vec![
                json_response(
                    200,
                    r#"{"items":[{"track":{"id":"t1","name":"Fav","artists":[{"name":"A"}],"duration_ms":1}}],"next":"__NEXT__"}"#
                        .replace("__NEXT__", &next),
                ),
                json_response(
                    200,
                    r#"{"items":[{"item":{"id":"t2","name":"Fav 2","artists":[{"name":"B"}],"duration_ms":2}}],"next":null}"#,
                ),
            ]
        });
        let client = client_with(&stub.base_url, "http://127.0.0.1:1/api/token", &path);
        let tracks = client.liked_tracks().unwrap();
        let _ = stub.finish();

        assert_eq!(
            tracks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["t1", "t2"]
        );
    }

    // --- 401 / renovación ---

    #[test]
    fn test_401_refreshes_once_and_retries() {
        let path = temp_path("unauthorized");
        write_tokens(&path, "TOKEN-OLD");
        let token_stub = Stub::spawn(|_| {
            vec![json_response(
                200,
                r#"{"access_token":"TOKEN-NEW","refresh_token":"REFRESH-2","expires_in":3600}"#,
            )]
        });
        let api_stub = Stub::spawn(|_| {
            vec![
                json_response(
                    401,
                    r#"{"error":{"status":401,"message":"The access token expired"}}"#,
                ),
                json_response(
                    200,
                    r#"{"items":[{"id":"p1","name":"Mixtape","owner":{"display_name":"Bob"},"tracks":{"total":3}}],"next":null}"#,
                ),
            ]
        });
        let client = client_with(
            &api_stub.base_url,
            &format!("{}/api/token", token_stub.base_url),
            &path,
        );
        let playlists = client.my_playlists().unwrap();
        let api_seen = api_stub.finish();
        let token_seen = token_stub.finish();

        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].name, "Mixtape");
        assert_eq!(api_seen.len(), 2);
        assert_eq!(api_seen[0].bearer.as_deref(), Some("TOKEN-OLD"));
        assert_eq!(api_seen[1].bearer.as_deref(), Some("TOKEN-NEW"));
        assert_eq!(token_seen.len(), 1);
        assert_eq!(token_seen[0].target, "/api/token");
        // El token renovado quedó persistido para la próxima petición.
        let saved = auth::load_tokens(&path).unwrap().unwrap();
        assert_eq!(saved.access_token, "TOKEN-NEW");
    }

    #[test]
    fn test_repeated_401_returns_status_error_without_second_refresh() {
        let path = temp_path("unauthorized_twice");
        write_tokens(&path, "TOKEN-OLD");
        let token_stub = Stub::spawn(|_| {
            vec![json_response(
                200,
                r#"{"access_token":"TOKEN-NEW","refresh_token":"REFRESH-2","expires_in":3600}"#,
            )]
        });
        let api_stub = Stub::spawn(|_| {
            vec![
                json_response(401, r#"{"error":{"status":401,"message":"expired"}}"#),
                json_response(401, r#"{"error":{"status":401,"message":"still expired"}}"#),
            ]
        });
        let client = client_with(
            &api_stub.base_url,
            &format!("{}/api/token", token_stub.base_url),
            &path,
        );
        let err = client.my_playlists().unwrap_err();
        let api_seen = api_stub.finish();
        let token_seen = token_stub.finish();

        assert_eq!(
            err,
            SpotifyError::Status {
                status: 401,
                message: Some("still expired".to_string())
            }
        );
        assert_eq!(api_seen.len(), 2);
        assert_eq!(token_seen.len(), 1);
    }

    // --- 429 y otros no-2xx ---

    #[test]
    fn test_429_reports_retry_after() {
        let path = temp_path("rate_limited");
        write_tokens(&path, "TOKEN-1");
        let stub = Stub::spawn(|_| {
            vec![StubResponse {
                status: 429,
                headers: vec![("Retry-After", "7".to_string())],
                body: r#"{"error":{"status":429,"message":"rate limited"}}"#.to_string(),
            }]
        });
        let client = client_with(&stub.base_url, "http://127.0.0.1:1/api/token", &path);
        let err = client.liked_tracks().unwrap_err();
        let _ = stub.finish();

        assert_eq!(
            err,
            SpotifyError::RateLimited {
                retry_after_secs: Some(7)
            }
        );
    }

    #[test]
    fn test_non_2xx_maps_to_typed_status_error() {
        let path = temp_path("forbidden");
        write_tokens(&path, "TOKEN-1");
        let stub = Stub::spawn(|_| {
            vec![json_response(
                403,
                r#"{"error":{"status":403,"message":"Forbidden"}}"#,
            )]
        });
        let client = client_with(&stub.base_url, "http://127.0.0.1:1/api/token", &path);
        let err = client.search("x").unwrap_err();
        let _ = stub.finish();

        assert_eq!(
            err,
            SpotifyError::Status {
                status: 403,
                message: Some("Forbidden".to_string())
            }
        );
    }

    #[test]
    fn test_error_display_does_not_leak_raw_body() {
        let path = temp_path("no_leak");
        write_tokens(&path, "TOKEN-1");
        let stub = Stub::spawn(|_| {
            vec![json_response(
                500,
                r#"{"access_token":"SUPER-SECRET-TOKEN","detail":"boom"}"#,
            )]
        });
        let client = client_with(&stub.base_url, "http://127.0.0.1:1/api/token", &path);
        let err = client.my_playlists().unwrap_err();
        let _ = stub.finish();

        let text = err.to_string();
        assert!(!text.contains("SUPER-SECRET-TOKEN"), "{text}");
        assert!(!text.contains("boom"), "{text}");
    }

    // --- track ---

    #[test]
    fn test_track_fetches_a_single_track() {
        let path = temp_path("track");
        write_tokens(&path, "TOKEN-T");
        let stub = Stub::spawn(|_| {
            vec![json_response(
                200,
                r#"{"id":"t9","name":"Around the World","artists":[{"id":"a1","name":"Daft Punk"}],
                    "album":{"name":"Homework"},"duration_ms":429000,
                    "external_ids":{"isrc":"GBDUW9700001"}}"#,
            )]
        });
        let client = client_with(&stub.base_url, "http://127.0.0.1:1/api/token", &path);
        let track = client.track("t9").unwrap();
        let seen = stub.finish();

        assert_eq!(track.id, "t9");
        assert_eq!(track.title, "Around the World");
        assert_eq!(track.artists, vec!["Daft Punk".to_string()]);
        assert_eq!(track.album.as_deref(), Some("Homework"));
        assert_eq!(track.duration_ms, 429_000);
        assert_eq!(track.isrc.as_deref(), Some("GBDUW9700001"));
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].target, "/v1/tracks/t9");
        assert_eq!(seen[0].bearer.as_deref(), Some("TOKEN-T"));
    }

    #[test]
    fn test_track_not_found_is_a_status_error_without_token() {
        let path = temp_path("track_404");
        write_tokens(&path, "SUPER-SECRET-TOKEN");
        let stub = Stub::spawn(|_| {
            vec![json_response(
                404,
                r#"{"error":{"status":404,"message":"Non existing id"}}"#,
            )]
        });
        let client = client_with(&stub.base_url, "http://127.0.0.1:1/api/token", &path);
        let err = client.track("nope").unwrap_err();
        stub.finish();

        assert_eq!(
            err,
            SpotifyError::Status {
                status: 404,
                message: Some("Non existing id".to_string())
            }
        );
        assert!(!err.to_string().contains("SUPER-SECRET-TOKEN"));
    }

    #[test]
    fn test_track_with_unexpected_body_is_bad_response() {
        let path = temp_path("track_bad");
        write_tokens(&path, "TOKEN-B");
        let stub = Stub::spawn(|_| vec![json_response(200, "[1,2,3]")]);
        let client = client_with(&stub.base_url, "http://127.0.0.1:1/api/token", &path);
        assert_eq!(client.track("t1").unwrap_err(), SpotifyError::BadResponse);
        stub.finish();
    }

    // --- album_tracks ---

    #[test]
    fn test_album_tracks_maps_plain_items_and_follows_next() {
        let path = temp_path("album");
        write_tokens(&path, "TOKEN-AL");
        let stub = Stub::spawn(|base| {
            vec![
                json_response(
                    200,
                    format!(
                        r#"{{"items":[
                          {{"id":"a1","name":"Intro","artists":[{{"id":"x","name":"Band"}}],
                            "duration_ms":61000}},
                          null
                        ],"next":"{base}/v1/albums/alb1/tracks?offset=1&limit=1"}}"#
                    ),
                ),
                json_response(
                    200,
                    r#"{"items":[
                      {"id":"a2","name":"Song","artists":[{"id":"x","name":"Band"},{"id":"y","name":"Guest"}],
                       "duration_ms":200000,"external_ids":{"isrc":"ISRC2"}}
                    ],"next":null}"#,
                ),
            ]
        });
        let client = client_with(&stub.base_url, "http://127.0.0.1:1/api/token", &path);
        let tracks = client.album_tracks("alb1").unwrap();
        let seen = stub.finish();

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, "a1");
        assert_eq!(tracks[0].title, "Intro");
        assert_eq!(tracks[0].artists, vec!["Band".to_string()]);
        assert_eq!(tracks[0].duration_ms, 61_000);
        assert_eq!(tracks[0].isrc, None);
        assert_eq!(tracks[0].album, None);
        assert_eq!(
            tracks[1].artists,
            vec!["Band".to_string(), "Guest".to_string()]
        );
        assert_eq!(tracks[1].isrc.as_deref(), Some("ISRC2"));
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].target, "/v1/albums/alb1/tracks");
        assert!(
            seen[1]
                .target
                .starts_with("/v1/albums/alb1/tracks?offset=1"),
            "{}",
            seen[1].target
        );
        assert_eq!(seen[0].bearer.as_deref(), Some("TOKEN-AL"));
    }

    #[test]
    fn test_album_tracks_not_found_is_a_status_error() {
        let path = temp_path("album_404");
        write_tokens(&path, "TOKEN-AL");
        let stub = Stub::spawn(|_| {
            vec![json_response(
                404,
                r#"{"error":{"status":404,"message":"Non existing id"}}"#,
            )]
        });
        let client = client_with(&stub.base_url, "http://127.0.0.1:1/api/token", &path);
        let err = client.album_tracks("nope").unwrap_err();
        stub.finish();
        assert!(
            matches!(err, SpotifyError::Status { status: 404, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn test_collect_pages_stops_on_a_repeating_next() {
        let path = temp_path("loop");
        write_tokens(&path, "TOKEN");
        let stub = Stub::spawn(|base| {
            vec![json_response(
                200,
                format!(
                    r#"{{"items":[
                      {{"id":"a1","name":"Uno","artists":[{{"id":"x","name":"Band"}}],
                       "duration_ms":1000}}
                    ],"next":"{base}/v1/albums/loop/tracks"}}"#
                ),
            )]
        });
        let client = client_with(&stub.base_url, "http://127.0.0.1:1/api/token", &path);
        let tracks = client.album_tracks("loop").unwrap();
        stub.finish();
        assert_eq!(tracks.len(), 1, "un next repetido no debe dar un bucle");
    }
}
