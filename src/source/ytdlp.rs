//! Resolución de URLs de páginas (YouTube, etc.) a URLs de audio directo y título
//! delegando en la herramienta externa `yt-dlp`.

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

        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();

        let stdout_handle = thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(pipe) = stdout_pipe.as_mut() {
                use std::io::Read;
                let _ = pipe.read_to_end(&mut out);
            }
            out
        });

        let stderr_handle = thread::spawn(move || {
            let mut err = Vec::new();
            if let Some(pipe) = stderr_pipe.as_mut() {
                use std::io::Read;
                let _ = pipe.read_to_end(&mut err);
            }
            err
        });

        let start = std::time::Instant::now();
        let exit_status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        // En Windows, empaquetados PyInstaller (como yt-dlp.exe) pueden engendrar
                        // subprocesos que heredan los descriptores de stdout/stderr. Si se hace join
                        // de los hilos lectores, estos quedarían bloqueados indefinidamente esperando EOF.
                        // Al descartar los JoinHandles sin join(), los hilos se desacoplan (detach)
                        // y retornamos Timeout inmediatamente.
                        return Err(RunError::Timeout);
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => return Err(RunError::Io(err.to_string())),
            }
        };

        let stdout = stdout_handle.join().unwrap_or_default();
        let stderr = stderr_handle.join().unwrap_or_default();

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

/// Errores posibles al resolver o invocar `yt-dlp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YtDlpError {
    NotInstalled,
    Failed(String),
    Timeout,
    NoCompatibleFormat(String),
    BadOutput(String),
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
        }
    }
}

impl std::error::Error for YtDlpError {}

/// Comprueba si `yt-dlp` está disponible en el entorno mediante `yt-dlp --version`.
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
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string(),
            ]
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
        let res = runner.run(
            "cmd",
            &[
                "/C".to_string(),
                "start /b ping -n 5 127.0.0.1 & ping -n 5 127.0.0.1".to_string(),
            ],
            Duration::from_millis(200),
        );
        let elapsed = start.elapsed();
        assert_eq!(res, Err(RunError::Timeout));
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout con subproceso en segundo plano debe cortar en < 2 s, tardó {elapsed:?}"
        );
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
