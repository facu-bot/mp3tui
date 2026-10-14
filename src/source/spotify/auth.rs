//! Autenticación con Spotify por Authorization Code + PKCE (RFC 7636).
//!
//! Flujo: se genera un `code_verifier`, se abre la URL de autorización, un
//! listener loopback recibe el redirect (o el usuario pega la URL a mano en un
//! entorno sin navegador), se canjea el `code` por tokens y se guardan en el
//! config dir con permisos `0600`. Ningún valor de token se imprime ni se
//! incluye en un mensaje de error.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Endpoint de autorización (el usuario lo abre en el navegador).
pub const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
/// Endpoint de tokens por defecto; se puede reemplazar para tests.
pub const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
/// Permisos que pide mp3tui: playlists privadas y colaborativas, biblioteca y
/// datos básicos del perfil.
pub const SCOPES: &[&str] = &[
    "playlist-read-private",
    "playlist-read-collaborative",
    "user-library-read",
    "user-read-private",
];
/// Se renueva el access token cuando le quedan menos de estos segundos.
pub const REFRESH_MARGIN_SECS: u64 = 60;
/// Archivo de tokens dentro del config dir de mp3tui.
const TOKENS_FILE: &str = "spotify.toml";
/// Ruta que atiende el listener loopback.
/// Ruta del redirect. Es la raíz (`http://127.0.0.1:<puerto>`) para que
/// coincida con la Redirect URI que el usuario registra: Spotify exige
/// coincidencia exacta y en loopback solo permite que varíe el puerto.
const CALLBACK_PATH: &str = "";
/// Espera por defecto del redirect antes de rendirse.
pub const DEFAULT_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

const SUCCESS_HTML: &str = "<!doctype html><html lang=\"es\"><meta charset=\"utf-8\">\
<title>mp3tui</title><body><h1>Listo</h1><p>Ya puedes volver a la terminal.</p></body></html>";
const FAILURE_HTML: &str = "<!doctype html><html lang=\"es\"><meta charset=\"utf-8\">\
<title>mp3tui</title><body><h1>No se pudo autorizar</h1>\
<p>Vuelve a la terminal para ver el detalle.</p></body></html>";
const NOT_FOUND_HTML: &str = "<!doctype html><title>mp3tui</title><p>Not found</p>";

/// Caracteres que se dejan sin escapar en la query (unreserved de RFC 3986).
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Errores de autenticación. Los mensajes nunca contienen valores de tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    Random(String),
    Io(String),
    Http(String),
    /// El redirect trae un `state` distinto (o ausente): posible CSRF.
    StateMismatch,
    /// El usuario rechazó el acceso o Spotify devolvió `error=...`.
    Denied(String),
    /// El redirect no trae `code`.
    MissingCode,
    /// El texto pegado no parece una URL de redirect.
    InvalidRedirect,
    /// El refresh token venció o fue revocado: hay que autorizar de nuevo.
    ReauthorizationRequired,
    /// Todavía no hay sesión guardada.
    NotAuthorized,
    /// Spotify rechazó la petición al endpoint de tokens.
    TokenEndpoint {
        error: String,
        description: Option<String>,
    },
    BadResponse,
    Storage(String),
    NoConfigDir,
    Timeout,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Random(e) => write!(f, "no se pudo generar un valor aleatorio: {e}"),
            Self::Io(e) => write!(f, "error de E/S: {e}"),
            Self::Http(e) => write!(f, "error de red al hablar con Spotify: {e}"),
            Self::StateMismatch => write!(
                f,
                "el parámetro state del redirect no coincide; se descartó por seguridad"
            ),
            Self::Denied(reason) => write!(f, "Spotify no concedió el acceso: {reason}"),
            Self::MissingCode => write!(f, "el redirect no trae el parámetro code"),
            Self::InvalidRedirect => write!(f, "la URL pegada no es un redirect de Spotify válido"),
            Self::ReauthorizationRequired => write!(
                f,
                "la sesión de Spotify venció o fue revocada (invalid_grant): hay que volver a autorizar mp3tui"
            ),
            Self::NotAuthorized => write!(
                f,
                "todavía no hay una sesión de Spotify: hay que autorizar mp3tui primero"
            ),
            Self::TokenEndpoint { error, description } => match description {
                Some(d) => write!(f, "Spotify rechazó la petición de tokens: {error} ({d})"),
                None => write!(f, "Spotify rechazó la petición de tokens: {error}"),
            },
            Self::BadResponse => {
                write!(f, "respuesta inesperada del endpoint de tokens de Spotify")
            }
            Self::Storage(e) => write!(f, "no se pudo acceder al archivo de tokens: {e}"),
            Self::NoConfigDir => write!(f, "no se pudo determinar el directorio de configuración"),
            Self::Timeout => write!(
                f,
                "se agotó el tiempo esperando la autorización en el navegador"
            ),
        }
    }
}

impl std::error::Error for AuthError {}

fn io_err(e: io::Error) -> AuthError {
    AuthError::Io(e.to_string())
}

/// Segundos desde la época Unix.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// --- PKCE -----------------------------------------------------------------

/// Par `code_verifier` / `code_challenge`. El verifier es secreto del flujo.
#[derive(Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// `n` bytes aleatorios del sistema, codificados en base64url sin padding
/// (solo caracteres no reservados de RFC 3986).
fn random_url_safe(n: usize) -> Result<String, AuthError> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| AuthError::Random(e.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

/// `code_challenge` S256: `base64url_sin_padding(sha256(verifier))`.
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Genera un par PKCE nuevo (verifier de 64 caracteres: 48 bytes en base64url,
/// sin sesgo de módulo y dentro del rango 43-128 que exige RFC 7636).
pub fn new_pkce() -> Result<Pkce, AuthError> {
    let verifier = random_url_safe(48)?;
    let challenge = challenge_for(&verifier);
    Ok(Pkce {
        verifier,
        challenge,
    })
}

/// URL de autorización de Spotify con todos los parámetros PKCE.
pub fn build_authorize_url(
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> String {
    let scope = SCOPES.join(" ");
    let params = [
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_challenge_method", "S256"),
        ("code_challenge", challenge),
        ("state", state),
        ("scope", scope.as_str()),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", utf8_percent_encode(v, UNRESERVED)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZE_URL}?{query}")
}

// --- Redirect -------------------------------------------------------------

fn decode_component(raw: &str) -> String {
    percent_decode_str(&raw.replace('+', " "))
        .decode_utf8_lossy()
        .into_owned()
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (decode_component(k), decode_component(v)))
        .collect()
}

/// Extrae `code` de un redirect validando `state`. Acepta la URL completa
/// pegada a mano, el request-target (`/?code=...`) o solo la query.
pub fn parse_redirect(input: &str, expected_state: &str) -> Result<String, AuthError> {
    let input = input.trim();
    let without_fragment = input.split('#').next().unwrap_or("");
    let query = without_fragment
        .split_once('?')
        .map_or(without_fragment, |(_, q)| q);
    let params = parse_query(query);
    let get = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };

    if let Some(reason) = get("error") {
        return Err(AuthError::Denied(reason.to_string()));
    }
    if get("code").is_none() && get("state").is_none() {
        return Err(AuthError::InvalidRedirect);
    }
    if get("state") != Some(expected_state) {
        return Err(AuthError::StateMismatch);
    }
    match get("code") {
        Some(code) if !code.is_empty() => Ok(code.to_string()),
        _ => Err(AuthError::MissingCode),
    }
}

// --- Listener loopback ----------------------------------------------------

/// Abre un listener en `127.0.0.1` con puerto efímero y devuelve también el
/// `redirect_uri` correspondiente.
pub fn bind_loopback() -> Result<(TcpListener, String), AuthError> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(io_err)?;
    let port = listener.local_addr().map_err(io_err)?.port();
    Ok((listener, format!("http://127.0.0.1:{port}{CALLBACK_PATH}")))
}

/// Lee la cabecera del request (hasta `\r\n\r\n`) y devuelve `(método, target)`.
/// `None` si la conexión se cerró o expiró sin un request completo.
fn read_request_target(stream: &mut TcpStream) -> io::Result<Option<(String, String)>> {
    const MAX_HEADER: usize = 8 * 1024;
    let mut data = Vec::new();
    let mut buf = [0u8; 1024];
    // Se lee hasta el final de la cabecera: cerrar con bytes sin leer en el
    // buffer de recepción puede mandar un RST y el navegador perdería la página.
    while !data.windows(4).any(|w| w == b"\r\n\r\n") {
        if data.len() >= MAX_HEADER {
            return Ok(None);
        }
        match stream.read(&mut buf) {
            Ok(0) => return Ok(None),
            Ok(n) => data.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
    }
    let head = String::from_utf8_lossy(&data);
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    Ok(match (parts.next(), parts.next()) {
        (Some(method), Some(target)) => Some((method.to_string(), target.to_string())),
        _ => None,
    })
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(msg.as_bytes());
    let _ = stream.flush();
}

/// Espera el redirect en `listener`: ignora requests a otras rutas, valida
/// `state` en la raíz, contesta un HTML mínimo y devuelve el `code`.
pub fn wait_for_callback(
    listener: &TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, AuthError> {
    listener.set_nonblocking(true).map_err(io_err)?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Algunas plataformas heredan el modo no bloqueante del listener.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                // Un preconnect del navegador sin request no cuenta: se sigue.
                let Ok(Some((method, target))) = read_request_target(&mut stream) else {
                    continue;
                };
                let path = target.split('?').next().unwrap_or("");
                // La Redirect URI es la raíz del loopback: el navegador pide `/`.
                let is_root = path.is_empty() || path == "/";
                if method != "GET" || !is_root {
                    respond(&mut stream, "404 Not Found", NOT_FOUND_HTML);
                    continue;
                }
                return match parse_redirect(&target, expected_state) {
                    Ok(code) => {
                        respond(&mut stream, "200 OK", SUCCESS_HTML);
                        Ok(code)
                    }
                    Err(e) => {
                        respond(&mut stream, "400 Bad Request", FAILURE_HTML);
                        Err(e)
                    }
                };
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(AuthError::Timeout);
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(io_err(e)),
        }
    }
}

/// Intenta abrir `url` con el navegador del sistema. `false` si no se pudo.
pub fn open_browser(url: &str) -> bool {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

// --- Tokens ---------------------------------------------------------------

/// Tokens persistidos. `expires_at` es un instante Unix en segundos.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
    error_description: Option<String>,
}

impl Tokens {
    /// `true` si el access token venció o le quedan menos de
    /// [`REFRESH_MARGIN_SECS`] segundos.
    pub fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at.saturating_sub(now) < REFRESH_MARGIN_SECS
    }

    /// Arma los tokens desde la respuesta del endpoint. Spotify puede omitir el
    /// refresh token en un refresh: en ese caso se conserva el anterior.
    fn from_response(
        resp: TokenResponse,
        previous_refresh: Option<&str>,
        now: u64,
    ) -> Result<Self, AuthError> {
        let refresh_token = resp
            .refresh_token
            .or_else(|| previous_refresh.map(str::to_string))
            .filter(|t| !t.is_empty())
            .ok_or(AuthError::BadResponse)?;
        Ok(Self {
            access_token: resp.access_token,
            refresh_token,
            expires_at: now.saturating_add(resp.expires_in),
        })
    }
}

// --- Cliente del endpoint de tokens ---------------------------------------

/// Cliente del endpoint de tokens. La URL es inyectable para testear contra un
/// stub local sin pegarle a Spotify.
pub struct TokenClient {
    client_id: String,
    token_url: String,
    agent: ureq::Agent,
}

impl TokenClient {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self::with_token_url(client_id, TOKEN_URL)
    }

    pub fn with_token_url(client_id: impl Into<String>, token_url: impl Into<String>) -> Self {
        let config = ureq::config::Config::builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(20)))
            .build();
        Self {
            client_id: client_id.into(),
            token_url: token_url.into(),
            agent: config.new_agent(),
        }
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Canjea el `code` de autorización por tokens.
    pub fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<Tokens, AuthError> {
        self.post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", &self.client_id),
                ("code_verifier", verifier),
            ],
            None,
        )
    }

    /// Renueva el access token con el refresh token. `invalid_grant` significa
    /// que la sesión (6 meses) venció o fue revocada.
    pub fn refresh(&self, tokens: &Tokens) -> Result<Tokens, AuthError> {
        self.post_token(
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &tokens.refresh_token),
                ("client_id", &self.client_id),
            ],
            Some(&tokens.refresh_token),
        )
    }

    /// Tokens vigentes desde `path`: renueva y persiste si faltan menos de
    /// [`REFRESH_MARGIN_SECS`] segundos para que venza el access token.
    pub fn fresh_tokens(&self, path: &Path) -> Result<Tokens, AuthError> {
        let tokens = load_tokens(path)?.ok_or(AuthError::NotAuthorized)?;
        if !tokens.needs_refresh(unix_now()) {
            return Ok(tokens);
        }
        let renewed = self.refresh(&tokens)?;
        save_tokens(path, &renewed)?;
        Ok(renewed)
    }

    /// POST al endpoint de tokens. `previous_refresh` solo viene en un refresh:
    /// ahí `invalid_grant` se traduce a [`AuthError::ReauthorizationRequired`].
    fn post_token(
        &self,
        form: &[(&str, &str)],
        previous_refresh: Option<&str>,
    ) -> Result<Tokens, AuthError> {
        let mut response = self
            .agent
            .post(&self.token_url)
            .send_form(form.iter().copied())
            .map_err(|e| AuthError::Http(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| AuthError::Http(e.to_string()))?;

        if (200..300).contains(&status) {
            // Los errores de serde pueden citar valores: no se propagan.
            let parsed: TokenResponse =
                serde_json::from_str(&body).map_err(|_| AuthError::BadResponse)?;
            return Tokens::from_response(parsed, previous_refresh, unix_now());
        }

        match serde_json::from_str::<ErrorResponse>(&body) {
            Ok(err) if previous_refresh.is_some() && err.error == "invalid_grant" => {
                Err(AuthError::ReauthorizationRequired)
            }
            Ok(err) => Err(AuthError::TokenEndpoint {
                error: err.error,
                description: err.error_description,
            }),
            Err(_) => Err(AuthError::Http(format!("HTTP {status}"))),
        }
    }
}

// --- Persistencia ---------------------------------------------------------

/// Ruta del archivo de tokens dentro del config dir de mp3tui.
pub fn tokens_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|dir| dir.join(TOKENS_FILE))
}

/// Guarda los tokens como TOML con modo `0600` (Unix), escribiendo a un
/// temporal privado y renombrando para que nunca exista legible por otros.
pub fn save_tokens(path: &Path, tokens: &Tokens) -> Result<(), AuthError> {
    let storage = |e: io::Error| AuthError::Storage(e.to_string());
    let text = toml::to_string(tokens)
        .map_err(|_| AuthError::Storage("no se pudieron serializar los tokens".to_string()))?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(storage)?;
    }
    let tmp = path.with_extension("toml.tmp");
    let _ = fs::remove_file(&tmp);
    write_private(&tmp, text.as_bytes()).map_err(storage)?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        storage(e)
    })
}

/// Crea `path` (que no debe existir) legible solo por el dueño y lo escribe.
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        // Defensa extra por si el umask o el sistema de archivos alteran el modo.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(data)?;
    file.sync_all()
}

/// Lee los tokens guardados. `None` si el archivo no existe.
pub fn load_tokens(path: &Path) -> Result<Option<Tokens>, AuthError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AuthError::Storage(e.to_string())),
    };
    // Los errores de toml citan la línea que falló (con el valor del token).
    toml::from_str(&text).map(Some).map_err(|_| {
        AuthError::Storage(
            "el archivo de tokens es inválido; hay que volver a autorizar".to_string(),
        )
    })
}

// --- Flujo completo -------------------------------------------------------

/// Sesión de autorización en curso: PKCE, `state` y listener loopback.
pub struct AuthSession {
    pub auth_url: String,
    pub redirect_uri: String,
    verifier: String,
    state: String,
    listener: TcpListener,
}

impl fmt::Debug for AuthSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthSession")
            .field("redirect_uri", &self.redirect_uri)
            .field("verifier", &"<redacted>")
            .finish()
    }
}

impl AuthSession {
    pub fn start(client: &TokenClient) -> Result<Self, AuthError> {
        let pkce = new_pkce()?;
        let state = random_url_safe(16)?;
        let (listener, redirect_uri) = bind_loopback()?;
        let auth_url =
            build_authorize_url(client.client_id(), &redirect_uri, &pkce.challenge, &state);
        Ok(Self {
            auth_url,
            redirect_uri,
            verifier: pkce.verifier,
            state,
            listener,
        })
    }

    /// Espera el redirect en el listener loopback y devuelve el `code`.
    pub fn wait_for_code(&self, timeout: Duration) -> Result<String, AuthError> {
        wait_for_callback(&self.listener, &self.state, timeout)
    }

    /// Caso headless: saca el `code` de una URL de redirect pegada a mano.
    pub fn code_from_pasted(&self, pasted: &str) -> Result<String, AuthError> {
        parse_redirect(pasted, &self.state)
    }

    /// Canjea el `code` por tokens con el verifier de esta sesión.
    pub fn finish(&self, client: &TokenClient, code: &str) -> Result<Tokens, AuthError> {
        client.exchange_code(code, &self.redirect_uri, &self.verifier)
    }
}

/// Autoriza con el listener loopback: imprime la URL, intenta abrir el
/// navegador, espera el redirect, canjea el code y guarda los tokens.
pub fn login(client: &TokenClient, store: &Path, timeout: Duration) -> Result<Tokens, AuthError> {
    let session = AuthSession::start(client)?;
    println!(
        "Autoriza mp3tui en Spotify abriendo esta URL:\n{}",
        session.auth_url
    );
    if !open_browser(&session.auth_url) {
        println!("(No se pudo abrir el navegador automáticamente.)");
    }
    let code = session.wait_for_code(timeout)?;
    let tokens = session.finish(client, &code)?;
    save_tokens(store, &tokens)?;
    Ok(tokens)
}

/// Autoriza sin navegador local: imprime la URL y lee la URL de redirect que
/// el usuario pega (`read_pasted`), sin abrir nada.
pub fn login_headless(
    client: &TokenClient,
    store: &Path,
    read_pasted: impl FnOnce() -> io::Result<String>,
) -> Result<Tokens, AuthError> {
    let session = AuthSession::start(client)?;
    println!(
        "Abre esta URL en un navegador, autoriza y pega aquí la URL a la que te redirige \
         (aunque la página no cargue):\n{}",
        session.auth_url
    );
    let pasted = read_pasted().map_err(io_err)?;
    let code = session.code_from_pasted(&pasted)?;
    let tokens = session.finish(client, &code)?;
    save_tokens(store, &tokens)?;
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    // --- helpers ---

    fn temp_dir(name: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mp3tui_spotify_{name}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn sample_tokens(expires_at: u64) -> Tokens {
        Tokens {
            access_token: "ACCESS-SECRET-1".to_string(),
            refresh_token: "REFRESH-SECRET-1".to_string(),
            expires_at,
        }
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// Lee un request HTTP completo y devuelve su body.
    fn read_http_body(s: &mut TcpStream) -> String {
        let mut data = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = s.read(&mut buf).unwrap();
            data.extend_from_slice(&buf[..n]);
            if let Some(pos) = find(&data, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..pos]).to_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map(|v| v.trim().parse::<usize>().unwrap())
                    .unwrap_or(0);
                if data.len() >= pos + 4 + len {
                    return String::from_utf8_lossy(&data[pos + 4..pos + 4 + len]).into_owned();
                }
            }
            if n == 0 {
                return String::new();
            }
        }
    }

    /// Endpoint de tokens falso: contesta cada conexión con la respuesta
    /// indicada, en orden, y guarda el body recibido.
    struct Stub {
        url: String,
        bodies: Arc<Mutex<Vec<String>>>,
        handle: Option<JoinHandle<()>>,
    }

    impl Stub {
        fn spawn(responses: Vec<(u16, &'static str)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/api/token", listener.local_addr().unwrap());
            let bodies = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&bodies);
            let handle = thread::spawn(move || {
                for (status, json) in responses {
                    let (mut s, _) = listener.accept().unwrap();
                    let body = read_http_body(&mut s);
                    seen.lock().unwrap().push(body);
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{json}",
                        json.len()
                    );
                    s.write_all(reply.as_bytes()).unwrap();
                }
            });
            Self {
                url,
                bodies,
                handle: Some(handle),
            }
        }

        fn finish(mut self) -> Vec<String> {
            self.handle.take().unwrap().join().unwrap();
            self.bodies.lock().unwrap().clone()
        }
    }

    fn form_value(body: &str, key: &str) -> Option<String> {
        parse_query(body)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    /// Manda un request crudo al listener y devuelve la respuesta completa.
    fn http_get(addr: std::net::SocketAddr, target: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    // --- PKCE ---

    #[test]
    fn test_pkce_challenge_matches_rfc7636_vector() {
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn test_new_pkce_verifier_is_valid_and_random() {
        let a = new_pkce().unwrap();
        let b = new_pkce().unwrap();
        assert!(
            (43..=128).contains(&a.verifier.len()),
            "len {}",
            a.verifier.len()
        );
        assert!(
            a.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)),
            "solo caracteres no reservados: {}",
            a.verifier
        );
        assert_ne!(a.verifier, b.verifier);
        assert_eq!(a.challenge, challenge_for(&a.verifier));
        assert!(
            !format!("{a:?}").contains(&a.verifier),
            "Debug no debe exponer el verifier"
        );
    }

    // --- URL de autorización ---

    #[test]
    fn test_authorize_url_has_pkce_params_and_scopes() {
        let url = build_authorize_url(
            "cid123",
            "http://127.0.0.1:4567/callback",
            "CHALLENGE",
            "STATE",
        );
        let (base, query) = url.split_once('?').expect("tiene query");
        assert_eq!(base, "https://accounts.spotify.com/authorize");
        let params = parse_query(query);
        let get = |k: &str| {
            params
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("response_type"), Some("code"));
        assert_eq!(get("client_id"), Some("cid123"));
        assert_eq!(get("redirect_uri"), Some("http://127.0.0.1:4567/callback"));
        assert_eq!(get("code_challenge"), Some("CHALLENGE"));
        assert_eq!(get("code_challenge_method"), Some("S256"));
        assert_eq!(get("state"), Some("STATE"));
        assert_eq!(
            get("scope"),
            Some(
                "playlist-read-private playlist-read-collaborative user-library-read user-read-private"
            )
        );
        assert!(
            !query.contains("http://"),
            "el redirect_uri va percent-encoded: {query}"
        );
    }

    // --- Parseo del redirect ---

    #[test]
    fn test_parse_redirect_full_url_ok() {
        let code = parse_redirect(
            "http://127.0.0.1:4567/callback?code=AQD%2Fabc&state=xyz",
            "xyz",
        );
        assert_eq!(code, Ok("AQD/abc".to_string()));
    }

    #[test]
    fn test_parse_redirect_accepts_target_query_and_whitespace() {
        assert_eq!(
            parse_redirect("/callback?code=c1&state=s", "s"),
            Ok("c1".into())
        );
        assert_eq!(parse_redirect("code=c2&state=s", "s"), Ok("c2".into()));
        assert_eq!(
            parse_redirect("  http://127.0.0.1:1/callback?code=c3&state=s#frag \n", "s"),
            Ok("c3".into())
        );
    }

    #[test]
    fn test_parse_redirect_state_mismatch_is_error() {
        assert_eq!(
            parse_redirect("http://127.0.0.1:1/callback?code=c&state=otro", "esperado"),
            Err(AuthError::StateMismatch)
        );
        assert_eq!(
            parse_redirect("http://127.0.0.1:1/callback?code=c", "esperado"),
            Err(AuthError::StateMismatch),
            "sin state también se rechaza"
        );
    }

    #[test]
    fn test_parse_redirect_access_denied_is_error() {
        assert_eq!(
            parse_redirect(
                "http://127.0.0.1:1/callback?error=access_denied&state=s",
                "s"
            ),
            Err(AuthError::Denied("access_denied".to_string()))
        );
    }

    #[test]
    fn test_parse_redirect_missing_code_and_garbage() {
        assert_eq!(
            parse_redirect("http://127.0.0.1:1/callback?state=s", "s"),
            Err(AuthError::MissingCode)
        );
        assert_eq!(
            parse_redirect("http://127.0.0.1:1/callback", "s"),
            Err(AuthError::InvalidRedirect)
        );
    }

    // --- Tokens: serialización y expiración ---

    #[test]
    fn test_tokens_toml_roundtrip() {
        let t = sample_tokens(1_900_000_000);
        let text = toml::to_string(&t).unwrap();
        assert!(text.contains("expires_at = 1900000000"), "{text}");
        let back: Tokens = toml::from_str(&text).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn test_tokens_debug_never_prints_values() {
        let t = sample_tokens(10);
        let dbg = format!("{t:?}");
        assert!(
            !dbg.contains("ACCESS-SECRET") && !dbg.contains("REFRESH-SECRET"),
            "{dbg}"
        );
    }

    #[test]
    fn test_needs_refresh_boundaries() {
        let t = sample_tokens(1_000);
        assert!(!t.needs_refresh(900), "quedan 100s");
        assert!(!t.needs_refresh(940), "quedan exactamente 60s: no renueva");
        assert!(t.needs_refresh(941), "quedan 59s: renueva");
        assert!(t.needs_refresh(1_000), "vence ahora");
        assert!(t.needs_refresh(5_000), "ya venció");
    }

    #[test]
    fn test_from_response_keeps_previous_refresh_token() {
        let resp = TokenResponse {
            access_token: "nuevo".into(),
            refresh_token: None,
            expires_in: 3600,
        };
        let t = Tokens::from_response(resp, Some("viejo-refresh"), 1_000).unwrap();
        assert_eq!(t.refresh_token, "viejo-refresh");
        assert_eq!(t.expires_at, 4_600);

        let resp = TokenResponse {
            access_token: "a".into(),
            refresh_token: None,
            expires_in: 3600,
        };
        assert_eq!(
            Tokens::from_response(resp, None, 0),
            Err(AuthError::BadResponse),
            "sin refresh token previo ni nuevo es una respuesta inválida"
        );
    }

    // --- Endpoint de tokens (stub local) ---

    #[test]
    fn test_exchange_code_posts_pkce_form_and_parses_tokens() {
        let stub = Stub::spawn(vec![(
            200,
            r#"{"access_token":"AT","token_type":"Bearer","expires_in":3600,"refresh_token":"RT","scope":"x"}"#,
        )]);
        let client = TokenClient::with_token_url("cid", stub.url.clone());
        let before = unix_now();
        let tokens = client
            .exchange_code("the-code", "http://127.0.0.1:9/callback", "the-verifier")
            .unwrap();
        let after = unix_now();

        assert_eq!(tokens.access_token, "AT");
        assert_eq!(tokens.refresh_token, "RT");
        assert!((before + 3600..=after + 3600).contains(&tokens.expires_at));

        let bodies = stub.finish();
        let body = &bodies[0];
        assert_eq!(
            form_value(body, "grant_type").as_deref(),
            Some("authorization_code")
        );
        assert_eq!(form_value(body, "code").as_deref(), Some("the-code"));
        assert_eq!(
            form_value(body, "redirect_uri").as_deref(),
            Some("http://127.0.0.1:9/callback")
        );
        assert_eq!(form_value(body, "client_id").as_deref(), Some("cid"));
        assert_eq!(
            form_value(body, "code_verifier").as_deref(),
            Some("the-verifier")
        );
    }

    #[test]
    fn test_exchange_code_rejection_is_token_endpoint_error() {
        let stub = Stub::spawn(vec![(
            400,
            r#"{"error":"invalid_grant","error_description":"Invalid authorization code"}"#,
        )]);
        let client = TokenClient::with_token_url("cid", stub.url.clone());
        let err = client
            .exchange_code("c", "http://127.0.0.1:9/callback", "v")
            .unwrap_err();
        assert!(
            matches!(&err, AuthError::TokenEndpoint { error, .. } if error == "invalid_grant"),
            "un code inválido no es 'reautorizar sesión': {err:?}"
        );
        stub.finish();
    }

    #[test]
    fn test_refresh_posts_refresh_grant_and_keeps_refresh_token() {
        let stub = Stub::spawn(vec![(
            200,
            r#"{"access_token":"AT2","token_type":"Bearer","expires_in":3600}"#,
        )]);
        let client = TokenClient::with_token_url("cid", stub.url.clone());
        let old = sample_tokens(0);
        let new = client.refresh(&old).unwrap();
        assert_eq!(new.access_token, "AT2");
        assert_eq!(new.refresh_token, old.refresh_token);
        assert!(new.expires_at > unix_now());

        let bodies = stub.finish();
        assert_eq!(
            form_value(&bodies[0], "grant_type").as_deref(),
            Some("refresh_token")
        );
        assert_eq!(
            form_value(&bodies[0], "refresh_token").as_deref(),
            Some("REFRESH-SECRET-1")
        );
        assert_eq!(form_value(&bodies[0], "client_id").as_deref(), Some("cid"));
    }

    #[test]
    fn test_refresh_invalid_grant_requires_reauthorization() {
        let stub = Stub::spawn(vec![(
            400,
            r#"{"error":"invalid_grant","error_description":"Refresh token revoked"}"#,
        )]);
        let client = TokenClient::with_token_url("cid", stub.url.clone());
        let err = client.refresh(&sample_tokens(0)).unwrap_err();
        assert_eq!(err, AuthError::ReauthorizationRequired);
        let msg = err.to_string();
        assert!(msg.contains("volver a autorizar"), "{msg}");
        assert!(!msg.contains("REFRESH-SECRET"), "{msg}");
        stub.finish();
    }

    #[test]
    fn test_refresh_other_errors_are_not_reauthorization() {
        let stub = Stub::spawn(vec![(400, r#"{"error":"invalid_client"}"#)]);
        let client = TokenClient::with_token_url("cid", stub.url.clone());
        let err = client.refresh(&sample_tokens(0)).unwrap_err();
        assert!(matches!(err, AuthError::TokenEndpoint { .. }), "{err:?}");
        stub.finish();
    }

    #[test]
    fn test_malformed_token_response_does_not_leak_content() {
        let stub = Stub::spawn(vec![(
            200,
            r#"{"access_token":"LEAKY-TOKEN","expires_in":"x"}"#,
        )]);
        let client = TokenClient::with_token_url("cid", stub.url.clone());
        let err = client
            .exchange_code("c", "http://127.0.0.1:9/callback", "v")
            .unwrap_err();
        assert_eq!(err, AuthError::BadResponse);
        assert!(!err.to_string().contains("LEAKY-TOKEN"));
        stub.finish();
    }

    // --- Persistencia ---

    #[test]
    fn test_save_and_load_tokens_roundtrip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("nested").join("spotify.toml");
        let t = sample_tokens(1_900_000_000);
        save_tokens(&path, &t).unwrap();
        assert_eq!(load_tokens(&path).unwrap(), Some(t));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_save_tokens_sets_mode_0600_even_over_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("spotify.toml");
        fs::write(&path, "viejo").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        save_tokens(&path, &sample_tokens(1)).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "modo {mode:o}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_tokens_missing_is_none_and_corrupt_does_not_leak() {
        let dir = temp_dir("load");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("spotify.toml");
        assert_eq!(load_tokens(&path).unwrap(), None);

        fs::write(&path, "access_token = \"LEAKY-TOKEN\"\nrefresh_token = 5\n").unwrap();
        let err = load_tokens(&path).unwrap_err();
        assert!(matches!(err, AuthError::Storage(_)), "{err:?}");
        assert!(!err.to_string().contains("LEAKY-TOKEN"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_fresh_tokens_skips_network_when_valid() {
        let dir = temp_dir("fresh_valid");
        let path = dir.join("spotify.toml");
        let valid = sample_tokens(unix_now() + 3_600);
        save_tokens(&path, &valid).unwrap();
        // Puerto cerrado: cualquier request fallaría.
        let client = TokenClient::with_token_url("cid", "http://127.0.0.1:1/api/token");
        assert_eq!(client.fresh_tokens(&path).unwrap(), valid);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_fresh_tokens_refreshes_and_persists_when_expiring() {
        let dir = temp_dir("fresh_expired");
        let path = dir.join("spotify.toml");
        save_tokens(&path, &sample_tokens(unix_now() + 10)).unwrap();
        let stub = Stub::spawn(vec![(
            200,
            r#"{"access_token":"AT-NEW","token_type":"Bearer","expires_in":3600}"#,
        )]);
        let client = TokenClient::with_token_url("cid", stub.url.clone());

        let fresh = client.fresh_tokens(&path).unwrap();
        assert_eq!(fresh.access_token, "AT-NEW");
        assert_eq!(load_tokens(&path).unwrap(), Some(fresh));
        stub.finish();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_fresh_tokens_without_session_is_not_authorized() {
        let dir = temp_dir("fresh_none");
        let client = TokenClient::with_token_url("cid", "http://127.0.0.1:1/api/token");
        assert_eq!(
            client.fresh_tokens(&dir.join("spotify.toml")),
            Err(AuthError::NotAuthorized)
        );
    }

    // --- Listener loopback ---

    #[test]
    fn test_bind_loopback_uses_ephemeral_port_on_127_0_0_1() {
        let (listener, redirect) = bind_loopback().unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(redirect, format!("http://127.0.0.1:{port}"));
    }

    #[test]
    fn test_wait_for_callback_returns_code_and_serves_html() {
        let (listener, _) = bind_loopback().unwrap();
        let addr = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            // Una ruta ajena (favicon) se ignora y el flujo sigue esperando.
            let other = http_get(addr, "/favicon.ico");
            let ok = http_get(addr, "/?code=THE-CODE&state=S1");
            (other, ok)
        });

        let code = wait_for_callback(&listener, "S1", Duration::from_secs(5));
        assert_eq!(code, Ok("THE-CODE".to_string()));

        let (other, ok) = client.join().unwrap();
        assert!(other.starts_with("HTTP/1.1 404"), "{other}");
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.contains("volver a la terminal"), "{ok}");
        assert!(
            !ok.contains("THE-CODE"),
            "la página no debe repetir el code"
        );
    }

    #[test]
    fn test_wait_for_callback_rejects_wrong_state() {
        let (listener, _) = bind_loopback().unwrap();
        let addr = listener.local_addr().unwrap();
        let client = thread::spawn(move || http_get(addr, "/?code=c&state=ATACANTE"));

        let res = wait_for_callback(&listener, "S1", Duration::from_secs(5));
        assert_eq!(res, Err(AuthError::StateMismatch));
        assert!(client.join().unwrap().starts_with("HTTP/1.1 400"));
    }

    #[test]
    fn test_wait_for_callback_times_out() {
        let (listener, _) = bind_loopback().unwrap();
        let res = wait_for_callback(&listener, "S1", Duration::from_millis(150));
        assert_eq!(res, Err(AuthError::Timeout));
    }

    // --- Sesión ---

    #[test]
    fn test_session_url_points_at_its_own_listener_and_headless_paste_works() {
        let stub = Stub::spawn(vec![(
            200,
            r#"{"access_token":"AT","token_type":"Bearer","expires_in":3600,"refresh_token":"RT"}"#,
        )]);
        let client = TokenClient::with_token_url("cid", stub.url.clone());
        let session = AuthSession::start(&client).unwrap();

        let query = session.auth_url.split_once('?').unwrap().1;
        let params = parse_query(query);
        let redirect = params
            .iter()
            .find(|(k, _)| k == "redirect_uri")
            .unwrap()
            .1
            .clone();
        let state = params.iter().find(|(k, _)| k == "state").unwrap().1.clone();
        assert_eq!(redirect, session.redirect_uri);
        assert!(redirect.starts_with("http://127.0.0.1:"));

        let pasted = format!("{redirect}?code=PASTED&state={state}");
        let code = session.code_from_pasted(&pasted).unwrap();
        assert_eq!(code, "PASTED");
        let tokens = session.finish(&client, &code).unwrap();
        assert_eq!(tokens.access_token, "AT");

        let bodies = stub.finish();
        let sent_verifier = form_value(&bodies[0], "code_verifier").unwrap();
        let challenge = params
            .iter()
            .find(|(k, _)| k == "code_challenge")
            .unwrap()
            .1
            .clone();
        assert_eq!(
            challenge_for(&sent_verifier),
            challenge,
            "el verifier canjeado corresponde al challenge"
        );
        assert_eq!(
            form_value(&bodies[0], "redirect_uri").as_deref(),
            Some(redirect.as_str())
        );
    }
}
