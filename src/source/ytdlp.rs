//! Resolución de URLs de páginas (YouTube, etc.) a URLs de audio directo y título
//! delegando en la herramienta externa `yt-dlp`.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub const FORMAT_SELECTOR: &str = "bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]";

/// Salida de un comando ejecutado por un `CommandRunner`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Error al invocar o esperar un comando externo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    NotFound,
    Timeout,
    Io(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::NotFound => write!(f, "ejecutable no encontrado"),
            RunError::Timeout => write!(f, "tiempo de ejecución agotado"),
            RunError::Io(err) => write!(f, "error de E/S al ejecutar el comando: {err}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Interfaz para desacoplar la ejecución de comandos del sistema operativo (seam para tests).
pub trait CommandRunner: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<CommandOutput, RunError>;
}

/// Implementación real de `CommandRunner` usando `std::process::Command`.
pub struct ProcessCommandRunner;

#[cfg(windows)]
fn kill_process_tree(child: &mut std::process::Child) {
    let pid = child.id();
    let status = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if status.is_err() || !status.as_ref().map(|s| s.success()).unwrap_or(false) {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(not(windows))]
fn kill_process_tree(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_reader_thread<R: std::io::Read + Send + 'static>(
    reader: Option<R>,
) -> (thread::JoinHandle<()>, std::sync::mpsc::Receiver<Vec<u8>>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || {
        if let Some(mut r) = reader {
            let mut buf = [0u8; 4096];
            loop {
                match r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }
    });
    (handle, rx)
}

fn drain_reader(
    rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    deadline: std::time::Instant,
) -> (Vec<u8>, bool) {
    let mut collected = Vec::new();
    let mut completed = false;
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        let wait = deadline - now;
        match rx.recv_timeout(wait) {
            Ok(chunk) => collected.extend_from_slice(&chunk),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                completed = true;
                break;
            }
        }
    }
    (collected, completed)
}

fn is_valid_http_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.starts_with('-') {
        return false;
    }
    let prefix: String = trimmed
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    prefix.starts_with("http://") || prefix.starts_with("https://")
}

impl CommandRunner for ProcessCommandRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<CommandOutput, RunError> {
        let mut child = match Command::new(program)
            .args(args)
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUTF8", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(RunError::NotFound);
            }
            Err(err) => return Err(RunError::Io(err.to_string())),
        };

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let (stdout_handle, stdout_rx) = spawn_reader_thread(stdout_pipe);
        let (stderr_handle, stderr_rx) = spawn_reader_thread(stderr_pipe);

        let start = std::time::Instant::now();
        let exit_status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        kill_process_tree(&mut child);
                        // Los hilos lectores se desacoplan (detach) al descartar sus JoinHandles
                        drop(stdout_handle);
                        drop(stderr_handle);
                        return Err(RunError::Timeout);
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => return Err(RunError::Io(err.to_string())),
            }
        };

        // Post-exit bounded join: recolectamos a través de los canales hasta cierre de pipes
        // o hasta agotar la gracia (500 ms o el tiempo restante). Si un subproceso descendiente
        // retiene los descriptores abiertos, no bloqueamos indefinidamente: devolvemos lo leído
        // hasta el momento y limpiamos el árbol de procesos residual.
        let now = std::time::Instant::now();
        let remaining = timeout.saturating_sub(start.elapsed());
        let grace = Duration::from_millis(500).min(remaining);
        let deadline = now + grace;
        let (stdout, stdout_done) = drain_reader(&stdout_rx, deadline);
        let (stderr, stderr_done) = drain_reader(&stderr_rx, deadline);

        if !stdout_done || !stderr_done {
            kill_process_tree(&mut child);
        }

        drop(stdout_handle);
        drop(stderr_handle);

        Ok(CommandOutput {
            status_code: exit_status.code(),
            stdout,
            stderr,
        })
    }
}

/// Resultado de resolución exitosa de una URL con `yt-dlp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub title: String,
    pub audio_url: String,
}

/// Entrada de una playlist listada con `--flat-playlist`: solo el título y la
/// URL de página (sin resolver), que es lo que después se resuelve canción por
/// canción cuando le toca el turno (las URLs de audio vencen).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistEntry {
    pub title: String,
    pub page_url: String,
}

/// Errores posibles al resolver o invocar `yt-dlp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YtDlpError {
    NotInstalled,
    Failed(String),
    Timeout,
    NoCompatibleFormat(String),
    BadOutput(String),
    InvalidUrl(String),
    EmptyQuery,
}

impl std::fmt::Display for YtDlpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            YtDlpError::NotInstalled => write!(
                f,
                "yt-dlp no está instalado o no se encuentra en el PATH. Instálalo con: winget install yt-dlp o pip install yt-dlp"
            ),
            YtDlpError::Failed(msg) => write!(f, "error al ejecutar yt-dlp: {msg}"),
            YtDlpError::Timeout => write!(f, "tiempo de espera agotado al consultar yt-dlp"),
            YtDlpError::NoCompatibleFormat(msg) => {
                write!(
                    f,
                    "no se encontró un formato de audio compatible (AAC/M4A): {msg}"
                )
            }
            YtDlpError::BadOutput(msg) => write!(f, "salida inesperada de yt-dlp: {msg}"),
            YtDlpError::InvalidUrl(msg) => write!(f, "URL no válida para yt-dlp: {msg}"),
            YtDlpError::EmptyQuery => write!(f, "la consulta de búsqueda está vacía"),
        }
    }
}

impl std::error::Error for YtDlpError {}

/// Comprueba si `yt-dlp` está disponible en el entorno mediante `yt-dlp --version`.
/// Solo para tests: la ausencia del binario se detecta en la ejecución real por
/// [`YtDlpError::NotInstalled`].
#[cfg(test)]
pub fn is_available(runner: &dyn CommandRunner) -> bool {
    let args = vec!["--version".to_string()];
    match runner.run("yt-dlp", &args, Duration::from_secs(5)) {
        Ok(out) => out.status_code == Some(0),
        Err(_) => false,
    }
}

/// Resuelve una URL de página a URL de audio directa y título invocando `yt-dlp`.
pub fn resolve(page_url: &str, runner: &dyn CommandRunner) -> Result<Resolved, YtDlpError> {
    resolve_with_timeout(page_url, runner, DEFAULT_TIMEOUT)
}

/// Resuelve una URL de página con un timeout configurable.
pub fn resolve_with_timeout(
    page_url: &str,
    runner: &dyn CommandRunner,
    timeout: Duration,
) -> Result<Resolved, YtDlpError> {
    if !is_valid_http_url(page_url) {
        return Err(YtDlpError::InvalidUrl(format!(
            "la URL debe comenzar con http:// o https://: {page_url}"
        )));
    }

    let args = vec![
        "--no-playlist".to_string(),
        "--no-warnings".to_string(),
        "--encoding".to_string(),
        "utf-8".to_string(),
        "-f".to_string(),
        FORMAT_SELECTOR.to_string(),
        "--print".to_string(),
        "%(title)s".to_string(),
        "--print".to_string(),
        "%(url)s".to_string(),
        "--".to_string(),
        page_url.to_string(),
    ];

    let output = match runner.run("yt-dlp", &args, timeout) {
        Ok(out) => out,
        Err(RunError::NotFound) => return Err(YtDlpError::NotInstalled),
        Err(RunError::Timeout) => return Err(YtDlpError::Timeout),
        Err(RunError::Io(err)) => return Err(YtDlpError::Failed(err)),
    };

    let stderr_str = String::from_utf8_lossy(&output.stderr);
    if output.status_code != Some(0) {
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let first_err_line = stderr_str
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .or_else(|| stdout_str.lines().map(|l| l.trim()).find(|l| !l.is_empty()))
            .unwrap_or("falló la ejecución de yt-dlp")
            .to_string();

        if first_err_line.contains("Requested format is not available") {
            return Err(YtDlpError::NoCompatibleFormat(first_err_line));
        }
        return Err(YtDlpError::Failed(first_err_line));
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout_str
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    if lines.len() < 2 {
        return Err(YtDlpError::BadOutput(
            "se esperaban al menos el título y la URL directa en la salida de yt-dlp".to_string(),
        ));
    }

    let title = lines[0].to_string();
    let audio_url = lines[1].to_string();

    if !audio_url.starts_with("http://") && !audio_url.starts_with("https://") {
        return Err(YtDlpError::BadOutput(format!(
            "la URL extraída no comienza con http:// ni https://: {audio_url}"
        )));
    }

    Ok(Resolved { title, audio_url })
}

/// Lista una URL de playlist con `yt-dlp --flat-playlist`: devuelve títulos y
/// URL de página de cada entrada, sin resolver audio. Es la operación
/// barata que llena la cola; la resolución de audio se hace recién cuando a
/// cada canción le toca sonar.
pub fn list_flat(
    page_url: &str,
    runner: &dyn CommandRunner,
) -> Result<Vec<PlaylistEntry>, YtDlpError> {
    list_flat_with_timeout(page_url, runner, DEFAULT_TIMEOUT)
}

/// [`list_flat`] con timeout configurable.
pub fn list_flat_with_timeout(
    page_url: &str,
    runner: &dyn CommandRunner,
    timeout: Duration,
) -> Result<Vec<PlaylistEntry>, YtDlpError> {
    if !is_valid_http_url(page_url) {
        return Err(YtDlpError::InvalidUrl(format!(
            "la URL debe comenzar con http:// o https://: {page_url}"
        )));
    }

    let args = vec![
        "--flat-playlist".to_string(),
        "--no-warnings".to_string(),
        "--encoding".to_string(),
        "utf-8".to_string(),
        "--print".to_string(),
        "%(title)s".to_string(),
        "--print".to_string(),
        "%(url)s".to_string(),
        "--".to_string(),
        page_url.to_string(),
    ];

    let output = match runner.run("yt-dlp", &args, timeout) {
        Ok(out) => out,
        Err(RunError::NotFound) => return Err(YtDlpError::NotInstalled),
        Err(RunError::Timeout) => return Err(YtDlpError::Timeout),
        Err(RunError::Io(err)) => return Err(YtDlpError::Failed(err)),
    };

    let stderr_str = String::from_utf8_lossy(&output.stderr);
    if output.status_code != Some(0) {
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let first_err_line = stderr_str
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .or_else(|| stdout_str.lines().map(|l| l.trim()).find(|l| !l.is_empty()))
            .unwrap_or("falló el listado de la playlist")
            .to_string();
        return Err(YtDlpError::Failed(first_err_line));
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let mut lines: Vec<&str> = stdout_str.lines().map(|l| l.trim()).collect();
    // yt-dlp cierra cada entrada con un salto de línea: la última línea tras
    // el split queda vacía y se descarta. Las líneas vacías INTERMEDIAS son
    // títulos vacíos reales (video privado/eliminado en flat mode) y se
    // conservan para no desalinear el pareo título/URL de página.
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }

    // La salida viene en pares título/URL de página: `%(title)s` y `%(url)s`.
    if lines.len() % 2 != 0 {
        return Err(YtDlpError::BadOutput(
            "se esperaban pares de título y URL de página en la salida de yt-dlp".to_string(),
        ));
    }

    let mut entries = Vec::with_capacity(lines.len() / 2);
    for pair in lines.chunks(2) {
        let title = pair[0].to_string();
        let page_url = pair[1].to_string();
        if !is_valid_http_url(&page_url) {
            return Err(YtDlpError::BadOutput(format!(
                "la URL de página extraída no comienza con http:// ni https://: {page_url}"
            )));
        }
        entries.push(PlaylistEntry { title, page_url });
    }

    Ok(entries)
}

// --- Búsqueda y matching (T4) ------------------------------------------------
// `SearchCandidate`, `search`, `best_match` y `ResolveCache` los consume el
// cableado de Spotify (T5); hasta entonces son API sin uso.

/// Resultados que se le piden a yt-dlp en una búsqueda.
#[allow(dead_code)]
pub const SEARCH_RESULTS: usize = 5;

/// Tolerancia de duración (segundos) para aceptar un candidato.
const DURATION_TOLERANCE_SECS: f64 = 15.0;

/// Cobertura mínima de tokens del título para considerar un candidato.
const TITLE_COVERAGE_MIN: f64 = 0.75;

/// Un resultado de búsqueda de yt-dlp (`ytsearch`).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct SearchCandidate {
    pub title: String,
    pub webpage_url: String,
    pub duration_secs: Option<f64>,
}

/// Busca en YouTube una consulta de texto libre con `yt-dlp` (`ytsearchN:`).
///
/// La entrada es una pseudo-URL de búsqueda, no una URL real, así que a
/// propósito no pasa por `is_valid_http_url`.
#[allow(dead_code)]
pub fn search(query: &str, runner: &dyn CommandRunner) -> Result<Vec<SearchCandidate>, YtDlpError> {
    search_with_timeout(query, runner, DEFAULT_TIMEOUT)
}

/// [`search`] con timeout configurable.
#[allow(dead_code)]
pub fn search_with_timeout(
    query: &str,
    runner: &dyn CommandRunner,
    timeout: Duration,
) -> Result<Vec<SearchCandidate>, YtDlpError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(YtDlpError::EmptyQuery);
    }

    let args = vec![
        "--flat-playlist".to_string(),
        "--no-warnings".to_string(),
        "--encoding".to_string(),
        "utf-8".to_string(),
        "--print".to_string(),
        "%(title)s".to_string(),
        "--print".to_string(),
        "%(webpage_url)s".to_string(),
        "--print".to_string(),
        "%(duration)s".to_string(),
        "--".to_string(),
        format!("ytsearch{SEARCH_RESULTS}:{query}"),
    ];

    let output = match runner.run("yt-dlp", &args, timeout) {
        Ok(out) => out,
        Err(RunError::NotFound) => return Err(YtDlpError::NotInstalled),
        Err(RunError::Timeout) => return Err(YtDlpError::Timeout),
        Err(RunError::Io(err)) => return Err(YtDlpError::Failed(err)),
    };

    if output.status_code != Some(0) {
        let stderr_str = String::from_utf8_lossy(&output.stderr);
        let first_err_line = stderr_str
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("falló la búsqueda de yt-dlp")
            .to_string();
        return Err(YtDlpError::Failed(first_err_line));
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    Ok(parse_search_candidates(&stdout_str))
}

/// Parsea la salida de `search`: bloques de 3 líneas (título, `webpage_url`,
/// duración). Una duración `NA` o no numérica queda como `None`.
fn parse_search_candidates(stdout: &str) -> Vec<SearchCandidate> {
    let lines: Vec<&str> = stdout.lines().map(str::trim).collect();
    let mut candidates = Vec::new();
    for chunk in lines.chunks(3) {
        if chunk.len() < 3 {
            break;
        }
        let (title, webpage_url, duration) = (chunk[0], chunk[1], chunk[2]);
        if title.is_empty() || webpage_url.is_empty() {
            continue;
        }
        candidates.push(SearchCandidate {
            title: title.to_string(),
            webpage_url: webpage_url.to_string(),
            duration_secs: duration.parse::<f64>().ok(),
        });
    }
    candidates
}

/// Elige el mejor candidato para un track: exige cobertura de tokens del
/// título y, si se conoce la duración, que la del candidato caiga dentro de
/// [`DURATION_TOLERANCE_SECS`]. Los artistas suman puntaje.
#[allow(dead_code)]
pub fn best_match<'a>(
    artists: &[String],
    title: &str,
    duration_secs: Option<f64>,
    candidates: &'a [SearchCandidate],
) -> Option<&'a SearchCandidate> {
    let query_tokens = tokens(title);
    let artist_tokens = tokens(&artists.join(" "));
    let mut best: Option<(f64, &SearchCandidate)> = None;

    for candidate in candidates {
        let candidate_tokens = tokens(&candidate.title);
        let title_coverage = coverage(&query_tokens, &candidate_tokens);
        if title_coverage < TITLE_COVERAGE_MIN {
            continue;
        }
        if let (Some(want), Some(got)) = (duration_secs, candidate.duration_secs)
            && (want - got).abs() > DURATION_TOLERANCE_SECS
        {
            continue;
        }
        let score = title_coverage + 0.5 * coverage(&artist_tokens, &candidate_tokens);
        match best {
            Some((best_score, _)) if score <= best_score => {}
            _ => best = Some((score, candidate)),
        }
    }

    best.map(|(_, candidate)| candidate)
}

/// Tokens alfanuméricos en minúscula (descarta palabras de 1 carácter).
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() >= 2)
        .map(|word| word.to_lowercase())
        .collect()
}

/// Fracción de `query` presente en `candidate` (0.0 si `query` está vacía).
fn coverage(query: &[String], candidate: &[String]) -> f64 {
    if query.is_empty() {
        return 0.0;
    }
    let hits = query
        .iter()
        .filter(|token| candidate.contains(token))
        .count();
    hits as f64 / query.len() as f64
}

/// Nombre del archivo de la caché de resoluciones dentro del config dir.
#[allow(dead_code)]
pub const CACHE_FILE_NAME: &str = "ytdlp_cache.toml";

/// Formato persistido de la caché.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct CacheFile {
    #[serde(default)]
    entries: BTreeMap<String, String>,
}

/// Caché persistente `clave -> URL de YouTube` para no re-resolver.
///
/// Es perezosa: no escribe hasta el primer [`ResolveCache::put`]. Un archivo
/// corrupto se ignora (queda vacía) para no romper la reproducción.
#[allow(dead_code)]
#[derive(Debug)]
pub struct ResolveCache {
    path: PathBuf,
    entries: BTreeMap<String, String>,
}

#[allow(dead_code)]
impl ResolveCache {
    /// Caché en `path`. Lee el archivo si existe y es válido.
    pub fn at_path(path: PathBuf) -> Self {
        let entries = fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str::<CacheFile>(&text).ok())
            .map(|file| file.entries)
            .unwrap_or_default();
        Self { path, entries }
    }

    /// Caché en la ruta por defecto dentro del config dir, si existe.
    pub fn new() -> Option<Self> {
        crate::config::config_dir().map(|dir| Self::at_path(dir.join(CACHE_FILE_NAME)))
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Inserta y persiste (temporal + rename, para no corromper el archivo).
    pub fn put(&mut self, key: &str, url: &str) -> Result<(), String> {
        self.entries.insert(key.to_string(), url.to_string());
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = fs::create_dir_all(parent);
        }
        let file = CacheFile {
            entries: self.entries.clone(),
        };
        let text = toml::to_string(&file).map_err(|e| e.to_string())?;
        let temp = self.path.with_extension("toml.tmp");
        fs::write(&temp, text).map_err(|e| e.to_string())?;
        fs::rename(&temp, &self.path).map_err(|e| {
            let _ = fs::remove_file(&temp);
            e.to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeRunner {
        responses: Mutex<Vec<Result<CommandOutput, RunError>>>,
        recorded_calls: Mutex<Vec<(String, Vec<String>, Duration)>>,
    }

    impl FakeRunner {
        fn new(response: Result<CommandOutput, RunError>) -> Self {
            Self {
                responses: Mutex::new(vec![response]),
                recorded_calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>, Duration)> {
            self.recorded_calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            timeout: Duration,
        ) -> Result<CommandOutput, RunError> {
            self.recorded_calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec(), timeout));
            let mut resps = self.responses.lock().unwrap();
            if resps.is_empty() {
                panic!("FakeRunner llamado sin respuestas preparadas");
            }
            resps.remove(0)
        }
    }

    #[test]
    fn test_resolve_exact_args_passed() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Never Gonna Give You Up\nhttps://googlevideo.com/playback\n".to_vec(),
            stderr: Vec::new(),
        }));
        let res = resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ", &fake);
        assert!(res.is_ok(), "resolve debe tener éxito: {res:?}");

        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        let (prog, args, _timeout) = &calls[0];
        assert_eq!(prog, "yt-dlp");
        assert_eq!(
            args,
            &[
                "--no-playlist".to_string(),
                "--no-warnings".to_string(),
                "--encoding".to_string(),
                "utf-8".to_string(),
                "-f".to_string(),
                "bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]".to_string(),
                "--print".to_string(),
                "%(title)s".to_string(),
                "--print".to_string(),
                "%(url)s".to_string(),
                "--".to_string(),
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string(),
            ]
        );
    }

    #[test]
    fn test_resolve_rejects_option_injection_and_invalid_scheme() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Never Gonna Give You Up\nhttps://googlevideo.com/playback\n".to_vec(),
            stderr: Vec::new(),
        }));

        let bad_inputs = [
            "--exec calc",
            "-o evil.mp3",
            "--help",
            "ftp://example.com/audio.mp3",
            "file:///etc/passwd",
            "C:\\music\\song.mp3",
            "just-a-string",
            "",
        ];

        for bad in bad_inputs {
            let res = resolve(bad, &fake);
            assert!(
                matches!(res, Err(YtDlpError::InvalidUrl(_))),
                "debe retornar InvalidUrl para '{bad}', pero retornó {res:?}"
            );
        }

        assert_eq!(
            fake.calls().len(),
            0,
            "el runner de comandos nunca debe ser invocado si la URL es inválida"
        );
    }

    #[test]
    fn test_resolve_invalid_utf8_title_uses_lossy_decoding() {
        let mut stdout_bytes = Vec::new();
        stdout_bytes.extend_from_slice(b"Canci\xF3n de Prueba\n"); // 0xF3 es 'ó' en CP-1252 / ISO-8859-1
        stdout_bytes.extend_from_slice(b"https://example.com/audio.m4a\n");

        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: stdout_bytes,
            stderr: Vec::new(),
        }));

        let res = resolve("https://example.com/watch?v=test", &fake)
            .expect("debe resolver con pérdida en vez de fallar");
        assert!(
            res.title.contains("Canci"),
            "el título debe contener 'Canci': {}",
            res.title
        );
        assert_eq!(res.audio_url, "https://example.com/audio.m4a");
    }

    #[test]
    fn test_resolve_successful_parse_lf_and_crlf() {
        // LF
        let fake_lf = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Song Title\nhttps://audio.example.com/stream.m4a\n".to_vec(),
            stderr: Vec::new(),
        }));
        let res_lf = resolve("https://example.com/watch?v=1", &fake_lf).expect("parse LF");
        assert_eq!(res_lf.title, "Song Title");
        assert_eq!(res_lf.audio_url, "https://audio.example.com/stream.m4a");

        // CRLF con líneas vacías
        let fake_crlf = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"\r\nSong Title 2\r\nhttps://audio.example.com/stream2.m4a\r\n\r\n".to_vec(),
            stderr: Vec::new(),
        }));
        let res_crlf = resolve("https://example.com/watch?v=2", &fake_crlf).expect("parse CRLF");
        assert_eq!(res_crlf.title, "Song Title 2");
        assert_eq!(res_crlf.audio_url, "https://audio.example.com/stream2.m4a");
    }

    #[test]
    fn test_resolve_missing_binary_returns_not_installed() {
        let fake = FakeRunner::new(Err(RunError::NotFound));
        let err = resolve("https://example.com/watch?v=1", &fake).unwrap_err();
        assert_eq!(err, YtDlpError::NotInstalled);
        let msg = err.to_string();
        assert!(
            msg.contains("winget install yt-dlp") || msg.contains("pip install yt-dlp"),
            "mensaje: {msg}"
        );
    }

    #[test]
    fn test_resolve_non_zero_exit_with_stderr_returns_failed() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(1),
            stdout: Vec::new(),
            stderr: b"ERROR: Video unavailable\nTraceback (most recent call last):\n...".to_vec(),
        }));
        let err = resolve("https://example.com/watch?v=1", &fake).unwrap_err();
        assert_eq!(
            err,
            YtDlpError::Failed("ERROR: Video unavailable".to_string())
        );
    }

    #[test]
    fn test_resolve_timeout_returns_timeout() {
        let fake = FakeRunner::new(Err(RunError::Timeout));
        let err = resolve("https://example.com/watch?v=1", &fake).unwrap_err();
        assert_eq!(err, YtDlpError::Timeout);
        let msg = err.to_string();
        assert!(msg.contains("tiempo de espera agotado"), "mensaje: {msg}");
    }

    #[test]
    fn test_resolve_output_without_http_url_returns_bad_output() {
        // Salida con URL que no empieza con http
        let fake1 = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Song Title\nnot_a_valid_url\n".to_vec(),
            stderr: Vec::new(),
        }));
        let err1 = resolve("https://example.com/watch?v=1", &fake1).unwrap_err();
        assert!(
            matches!(err1, YtDlpError::BadOutput(_)),
            "recibido: {err1:?}"
        );

        // Salida con una sola línea
        let fake2 = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Only Title\n".to_vec(),
            stderr: Vec::new(),
        }));
        let err2 = resolve("https://example.com/watch?v=2", &fake2).unwrap_err();
        assert!(
            matches!(err2, YtDlpError::BadOutput(_)),
            "recibido: {err2:?}"
        );
    }

    #[test]
    fn test_is_available_true_and_false() {
        let fake_ok = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"2024.10.07\n".to_vec(),
            stderr: Vec::new(),
        }));
        assert!(is_available(&fake_ok), "status 0 debe dar true");

        let fake_missing = FakeRunner::new(Err(RunError::NotFound));
        assert!(!is_available(&fake_missing), "NotFound debe dar false");

        let fake_err = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(127),
            stdout: Vec::new(),
            stderr: b"error".to_vec(),
        }));
        assert!(!is_available(&fake_err), "status non-zero debe dar false");
    }

    #[test]
    fn test_real_runner_not_found() {
        let runner = ProcessCommandRunner;
        let res = runner.run(
            "__nonexistent_binary_xyz_12345__",
            &["--version".to_string()],
            Duration::from_millis(500),
        );
        assert_eq!(res, Err(RunError::NotFound));
    }

    #[test]
    #[cfg(windows)]
    fn test_real_runner_timeout_and_kill() {
        let runner = ProcessCommandRunner;
        let start = std::time::Instant::now();
        let res = runner.run(
            "ping",
            &["-n".to_string(), "5".to_string(), "127.0.0.1".to_string()],
            Duration::from_millis(150),
        );
        let elapsed = start.elapsed();
        assert_eq!(res, Err(RunError::Timeout));
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout debe cortar en < 2 s, tardó {elapsed:?}"
        );
    }

    #[test]
    #[cfg(windows)]
    fn test_real_runner_timeout_grandchild_pipe_does_not_hang() {
        let runner = ProcessCommandRunner;
        let start = std::time::Instant::now();
        let marker = std::env::temp_dir().join(format!(
            "mp3tui_grandchild_marker_{}.tmp",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);

        let res = runner.run(
            "cmd",
            &[
                "/C".to_string(),
                format!(
                    "start /b cmd /C \"ping -n 8 127.0.0.1 > {}\" & ping -n 8 127.0.0.1",
                    marker.display()
                ),
            ],
            Duration::from_millis(200),
        );
        let elapsed = start.elapsed();
        assert_eq!(res, Err(RunError::Timeout));
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout con subproceso en segundo plano debe cortar en < 2 s, tardó {elapsed:?}"
        );

        // Si el subproceso nieto siguiera vivo (como ocurre con child.kill() sin taskkill /T),
        // mantendría el archivo marker bloqueado para escritura en Windows.
        // Con taskkill /T /F, todo el árbol es eliminado y el archivo se puede abrir exclusivamente.
        thread::sleep(Duration::from_millis(150));
        let open_result = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&marker);
        assert!(
            open_result.is_ok(),
            "el subproceso nieto debe haber sido terminado por taskkill liberando el archivo marker: {open_result:?}"
        );
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    #[cfg(windows)]
    fn test_real_runner_post_exit_unbounded_join_is_bounded() {
        let runner = ProcessCommandRunner;
        let start = std::time::Instant::now();
        // cmd.exe imprime y sale de inmediato (status 0), pero start /b ping hereda el pipe por 5 s
        let res = runner.run(
            "cmd",
            &[
                "/C".to_string(),
                "echo Title& echo https://example.com/stream.m4a& start /b ping -n 5 127.0.0.1"
                    .to_string(),
            ],
            Duration::from_secs(10),
        );
        let elapsed = start.elapsed();
        assert!(res.is_ok(), "el comando debe retornar Ok: {res:?}");
        let out = res.unwrap();
        assert_eq!(out.status_code, Some(0));
        let stdout_str = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout_str.contains("Title"),
            "stdout debe contener lo impreso antes del exit: {stdout_str}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "el join post-exit debe estar acotado a < 2 s, tardó {elapsed:?}"
        );
    }

    #[test]
    fn test_list_flat_exact_args_passed() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Cancion Uno\nhttps://www.youtube.com/watch?v=uno\n".to_vec(),
            stderr: Vec::new(),
        }));
        let res = list_flat("https://www.youtube.com/playlist?list=PL123", &fake);
        assert!(res.is_ok(), "list_flat debe tener éxito: {res:?}");

        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        let (prog, args, _timeout) = &calls[0];
        assert_eq!(prog, "yt-dlp");
        assert_eq!(
            args,
            &[
                "--flat-playlist".to_string(),
                "--no-warnings".to_string(),
                "--encoding".to_string(),
                "utf-8".to_string(),
                "--print".to_string(),
                "%(title)s".to_string(),
                "--print".to_string(),
                "%(url)s".to_string(),
                "--".to_string(),
                "https://www.youtube.com/playlist?list=PL123".to_string(),
            ]
        );
    }

    #[test]
    fn test_list_flat_parses_title_and_page_url_pairs() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Uno\nhttps://www.youtube.com/watch?v=uno\nDos\nhttps://www.youtube.com/watch?v=dos\n".to_vec(),
            stderr: Vec::new(),
        }));
        let entries =
            list_flat("https://www.youtube.com/playlist?list=PL123", &fake).expect("lista válida");
        assert_eq!(
            entries,
            vec![
                PlaylistEntry {
                    title: "Uno".to_string(),
                    page_url: "https://www.youtube.com/watch?v=uno".to_string(),
                },
                PlaylistEntry {
                    title: "Dos".to_string(),
                    page_url: "https://www.youtube.com/watch?v=dos".to_string(),
                },
            ]
        );
    }

    /// Entrada sin título (video privado/eliminado en flat mode) no
    /// desalinea el pareo título/URL: la línea vacía del medio se conserva
    /// como título vacío y solo se descarta el salto de línea final.
    #[test]
    fn test_list_flat_keeps_empty_title_entries_aligned() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout:
                b"\nhttps://www.youtube.com/watch?v=uno\nDos\nhttps://www.youtube.com/watch?v=dos\n"
                    .to_vec(),
            stderr: Vec::new(),
        }));
        let entries = list_flat("https://www.youtube.com/playlist?list=PL123", &fake)
            .expect("la lista con título vacío sigue parseando");
        assert_eq!(
            entries,
            vec![
                PlaylistEntry {
                    title: String::new(),
                    page_url: "https://www.youtube.com/watch?v=uno".to_string(),
                },
                PlaylistEntry {
                    title: "Dos".to_string(),
                    page_url: "https://www.youtube.com/watch?v=dos".to_string(),
                },
            ]
        );
    }

    #[test]
    fn test_list_flat_empty_output_is_empty_vec() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }));
        let entries = list_flat("https://www.youtube.com/playlist?list=vacia", &fake)
            .expect("lista vacía es válida");
        assert!(
            entries.is_empty(),
            "una salida vacía no tiene entradas: {entries:?}"
        );
    }

    #[test]
    fn test_list_flat_rejects_invalid_url_without_running() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Uno\nhttps://www.youtube.com/watch?v=uno\n".to_vec(),
            stderr: Vec::new(),
        }));
        let res = list_flat("ftp://example.com/playlist", &fake);
        assert!(
            matches!(res, Err(YtDlpError::InvalidUrl(_))),
            "una URL no http debe fallar: {res:?}"
        );
        assert_eq!(
            fake.calls().len(),
            0,
            "no se invoca yt-dlp con URL inválida"
        );
    }

    #[test]
    fn test_list_flat_output_with_non_http_url_is_bad_output() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Uno\nnot-a-url\n".to_vec(),
            stderr: Vec::new(),
        }));
        let res = list_flat("https://www.youtube.com/playlist?list=PL123", &fake);
        assert!(
            matches!(res, Err(YtDlpError::BadOutput(_))),
            "una URL de página no http es salida inválida: {res:?}"
        );
    }

    #[test]
    fn test_list_flat_odd_line_count_is_bad_output() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Solo un titulo sin url\n".to_vec(),
            stderr: Vec::new(),
        }));
        let res = list_flat("https://www.youtube.com/playlist?list=PL123", &fake);
        assert!(
            matches!(res, Err(YtDlpError::BadOutput(_))),
            "un número impar de líneas es salida inválida: {res:?}"
        );
    }

    #[test]
    fn test_list_flat_non_zero_exit_returns_failed() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(1),
            stdout: Vec::new(),
            stderr: b"ERROR: Playlist unavailable\n".to_vec(),
        }));
        let err = list_flat("https://www.youtube.com/playlist?list=PL123", &fake).unwrap_err();
        assert_eq!(
            err,
            YtDlpError::Failed("ERROR: Playlist unavailable".to_string())
        );
    }

    #[test]
    fn test_list_flat_missing_binary_returns_not_installed() {
        let fake = FakeRunner::new(Err(RunError::NotFound));
        let err = list_flat("https://www.youtube.com/playlist?list=PL123", &fake).unwrap_err();
        assert_eq!(err, YtDlpError::NotInstalled);
    }

    // ------------------------------------------------------------------
    // T4: búsqueda y matcher (S2/S3)
    // ------------------------------------------------------------------

    fn search_candidate(
        title: &str,
        webpage_url: &str,
        duration_secs: Option<f64>,
    ) -> SearchCandidate {
        SearchCandidate {
            title: title.to_string(),
            webpage_url: webpage_url.to_string(),
            duration_secs,
        }
    }

    #[test]
    fn test_search_pseudo_url_bypasses_http_guard() {
        // El guard sigue siendo para URLs reales: la pseudo-URL de búsqueda no
        // debe pasar por él, pero una URL real sí.
        assert!(!is_valid_http_url("ytsearch5:never gonna give you up"));
        assert!(is_valid_http_url(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        ));

        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Never Gonna Give You Up\nhttps://www.youtube.com/watch?v=x\n213\n".to_vec(),
            stderr: Vec::new(),
        }));
        let res = search("never gonna give you up", &fake);
        assert!(
            res.is_ok(),
            "search debe aceptar la pseudo-URL de búsqueda: {res:?}"
        );
        assert_eq!(fake.calls().len(), 1, "search debe invocar yt-dlp una vez");
    }

    #[test]
    fn test_search_builds_exact_ytsearch_args() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Never Gonna Give You Up\nhttps://www.youtube.com/watch?v=x\n213\n".to_vec(),
            stderr: Vec::new(),
        }));
        let _ = search("never gonna give you up", &fake).expect("búsqueda válida");

        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        let (prog, args, _timeout) = &calls[0];
        assert_eq!(prog, "yt-dlp");
        assert_eq!(
            args,
            &[
                "--flat-playlist".to_string(),
                "--no-warnings".to_string(),
                "--encoding".to_string(),
                "utf-8".to_string(),
                "--print".to_string(),
                "%(title)s".to_string(),
                "--print".to_string(),
                "%(webpage_url)s".to_string(),
                "--print".to_string(),
                "%(duration)s".to_string(),
                "--".to_string(),
                format!("ytsearch{SEARCH_RESULTS}:never gonna give you up"),
            ]
        );
    }

    #[test]
    fn test_search_parses_candidates_and_optional_duration() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: b"Uno\nhttps://www.youtube.com/watch?v=uno\n213\nDos\nhttps://www.youtube.com/watch?v=dos\nNA\n".to_vec(),
            stderr: Vec::new(),
        }));
        let candidates = search("consulta", &fake).expect("salida válida");
        assert_eq!(
            candidates,
            vec![
                search_candidate("Uno", "https://www.youtube.com/watch?v=uno", Some(213.0)),
                search_candidate("Dos", "https://www.youtube.com/watch?v=dos", None),
            ]
        );
    }

    #[test]
    fn test_search_empty_query_is_rejected_without_running() {
        let fake = FakeRunner::new(Ok(CommandOutput {
            status_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }));
        let res = search("   ", &fake);
        assert!(
            matches!(res, Err(YtDlpError::EmptyQuery)),
            "consulta vacía debe fallar: {res:?}"
        );
        assert_eq!(fake.calls().len(), 0, "no se invoca yt-dlp sin consulta");
    }

    #[test]
    fn test_best_match_picks_candidate_by_tokens_and_duration() {
        let candidates = vec![
            search_candidate(
                "Rick Astley - Never Gonna Give You Up (Official Video)",
                "https://www.youtube.com/watch?v=a",
                Some(213.0),
            ),
            search_candidate(
                "Totally Different Song",
                "https://www.youtube.com/watch?v=b",
                Some(210.0),
            ),
            search_candidate(
                "Never Gonna Give You Up but it's a 10 hour loop",
                "https://www.youtube.com/watch?v=c",
                Some(36000.0),
            ),
        ];
        let got = best_match(
            &["Rick Astley".to_string()],
            "Never Gonna Give You Up",
            Some(213.0),
            &candidates,
        );
        assert_eq!(
            got.map(|c| c.webpage_url.as_str()),
            Some("https://www.youtube.com/watch?v=a")
        );
    }

    #[test]
    fn test_best_match_returns_none_when_nothing_close() {
        let candidates = vec![
            search_candidate(
                "Completely Unrelated Tune",
                "https://www.youtube.com/watch?v=x",
                Some(200.0),
            ),
            search_candidate(
                "Another Different Song",
                "https://www.youtube.com/watch?v=y",
                Some(180.0),
            ),
        ];
        let got = best_match(
            &["Rick Astley".to_string()],
            "Never Gonna Give You Up",
            Some(213.0),
            &candidates,
        );
        assert!(got.is_none(), "no debe elegir nada lejano: {got:?}");
    }

    #[test]
    fn test_best_match_rejects_duration_outside_tolerance() {
        let candidates = vec![search_candidate(
            "Rick Astley Never Gonna Give You Up",
            "https://www.youtube.com/watch?v=a",
            Some(999.0),
        )];
        let got = best_match(
            &["Rick Astley".to_string()],
            "Never Gonna Give You Up",
            Some(213.0),
            &candidates,
        );
        assert!(
            got.is_none(),
            "una duración fuera de tolerancia debe descartar el candidato: {got:?}"
        );
    }

    #[test]
    fn test_cache_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("mp3tui_ytdlp_cache_rt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(CACHE_FILE_NAME);

        let mut cache = ResolveCache::at_path(path.clone());
        assert_eq!(cache.get("spotify:track:abc"), None);
        assert!(
            !path.exists(),
            "la caché no debe tocar el disco antes del primer guardado"
        );

        cache
            .put("spotify:track:abc", "https://www.youtube.com/watch?v=abc")
            .expect("primer put");
        cache
            .put("spotify:track:def", "https://www.youtube.com/watch?v=def")
            .expect("segundo put");
        assert_eq!(cache.len(), 2);
        assert!(path.exists(), "put debe persistir la caché");

        let reloaded = ResolveCache::at_path(path.clone());
        assert_eq!(
            reloaded.get("spotify:track:abc"),
            Some("https://www.youtube.com/watch?v=abc")
        );
        assert_eq!(
            reloaded.get("spotify:track:def"),
            Some("https://www.youtube.com/watch?v=def")
        );
        assert_eq!(reloaded.get("spotify:track:zzz"), None);
        assert_eq!(reloaded.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cache_corrupt_file_is_ignored() {
        let dir =
            std::env::temp_dir().join(format!("mp3tui_ytdlp_cache_corrupt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(CACHE_FILE_NAME);
        std::fs::write(&path, "no es [[[ toml").unwrap();

        let cache = ResolveCache::at_path(path.clone());
        assert_eq!(cache.len(), 0, "una caché corrupta se ignora y queda vacía");
        assert_eq!(cache.get("spotify:track:abc"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "requiere yt-dlp instalado en el sistema y conexión a internet"]
    fn test_real_runner_youtube_manual() {
        let runner = ProcessCommandRunner;
        if !is_available(&runner) {
            eprintln!("yt-dlp no está disponible en este sistema; test manual ignorado");
            return;
        }
        let res = resolve("https://www.youtube.com/watch?v=dQw4w9WgXcQ", &runner)
            .expect("resolve youtube");
        assert!(!res.title.is_empty(), "título no debe estar vacío");
        assert!(
            res.audio_url.starts_with("https://"),
            "audio_url debe ser https"
        );
    }
}
