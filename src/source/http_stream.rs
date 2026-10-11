//! Fuente HTTP progresiva para symphonia (T3).
//!
//! Permite reproducir archivos de audio remotos mientras se descargan en segundo plano,
//! garantizando que `stop()` o un cambio de pista nunca bloqueen la UI.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use symphonia::core::io::MediaSource;

/// Metadatos básicos obtenidos al conectar con la URL remota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
}

/// Errores producidos al abrir o conectar el stream HTTP.
#[derive(Debug)]
pub enum StreamError {
    Connection(String),
    HttpStatus(u16, String),
    Io(String),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Connection(detail) => {
                write!(f, "error de conexión HTTP: {detail}")
            }
            StreamError::HttpStatus(code, detail) => {
                write!(f, "servidor HTTP respondió {code}: {detail}")
            }
            StreamError::Io(detail) => write!(f, "error de lectura HTTP: {detail}"),
        }
    }
}

impl std::error::Error for StreamError {}

/// Handle clonable para cancelar la descarga en segundo plano y despertar lectores bloqueados.
#[derive(Clone)]
pub struct CancelHandle {
    shared: Arc<Shared>,
}

impl CancelHandle {
    pub fn cancel(&self) {
        self.shared.canceled.store(true, Ordering::SeqCst);
        self.shared.condvar.notify_all();
    }

    pub fn is_canceled(&self) -> bool {
        self.shared.canceled.load(Ordering::SeqCst)
    }
}

struct BufferState {
    data: Vec<u8>,
    finished: bool,
    error: Option<String>,
}

struct Shared {
    state: Mutex<BufferState>,
    condvar: Condvar,
    canceled: AtomicBool,
}

/// Productor interno que llena el buffer compartido desde el hilo de red.
pub struct StreamProducer {
    shared: Arc<Shared>,
}

impl StreamProducer {
    pub fn new(content_length: Option<u64>) -> (Self, HttpStream) {
        let shared = Arc::new(Shared {
            state: Mutex::new(BufferState {
                data: Vec::new(),
                finished: false,
                error: None,
            }),
            condvar: Condvar::new(),
            canceled: AtomicBool::new(false),
        });
        let cancel = CancelHandle {
            shared: Arc::clone(&shared),
        };
        let producer = StreamProducer {
            shared: Arc::clone(&shared),
        };
        let consumer = HttpStream {
            shared,
            cancel,
            pos: 0,
            content_length,
        };
        (producer, consumer)
    }

    pub fn push(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Ok(mut state) = self.shared.state.lock() {
            state.data.extend_from_slice(bytes);
        }
        self.shared.condvar.notify_all();
    }

    pub fn finish(&self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.finished = true;
        }
        self.shared.condvar.notify_all();
    }

    pub fn fail(&self, msg: &str) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.error = Some(msg.to_string());
        }
        self.shared.condvar.notify_all();
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle {
            shared: Arc::clone(&self.shared),
        }
    }
}

/// Consumidor de audio progresivo que implementa Read, Seek y MediaSource de symphonia.
pub struct HttpStream {
    shared: Arc<Shared>,
    cancel: CancelHandle,
    pos: u64,
    content_length: Option<u64>,
}

impl fmt::Debug for HttpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpStream")
            .field("pos", &self.pos)
            .field("content_length", &self.content_length)
            .finish()
    }
}

impl Drop for HttpStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| io::Error::other("lock del buffer compartido envenenado"))?;

        loop {
            if self.shared.canceled.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "descarga cancelada",
                ));
            }

            if let Some(ref err) = state.error {
                return Err(io::Error::other(err.clone()));
            }

            let cur_len = state.data.len() as u64;
            if self.pos < cur_len {
                let avail = (cur_len - self.pos) as usize;
                let to_copy = avail.min(buf.len());
                let start = self.pos as usize;
                buf[..to_copy].copy_from_slice(&state.data[start..start + to_copy]);
                self.pos += to_copy as u64;
                return Ok(to_copy);
            }

            if state.finished {
                return Ok(0);
            }

            state = self
                .shared
                .condvar
                .wait(state)
                .map_err(|_| io::Error::other("condvar del buffer envenenado"))?;
        }
    }
}

impl Seek for HttpStream {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self.content_length {
            Some(len) => {
                let target = match pos {
                    SeekFrom::Start(n) => n as i128,
                    SeekFrom::Current(n) => (self.pos as i128) + (n as i128),
                    SeekFrom::End(n) => (len as i128) + (n as i128),
                };
                if !(0..=(len as i128)).contains(&target) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "posición de seek fuera del rango [0, content_length]",
                    ));
                }
                self.pos = target as u64;
                Ok(self.pos)
            }
            None => match pos {
                SeekFrom::Current(0) => Ok(self.pos),
                SeekFrom::Start(n) if n == self.pos => Ok(self.pos),
                _ => Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "seek no soportado en stream HTTP sin Content-Length",
                )),
            },
        }
    }
}

impl MediaSource for HttpStream {
    fn is_seekable(&self) -> bool {
        self.content_length.is_some()
    }

    fn byte_len(&self) -> Option<u64> {
        self.content_length
    }
}

/// Configuración de timeouts para peticiones HTTP de audio remoto.
///
/// ureq 3.4.2 solo ofrece `timeout_recv_body` (tiempo total máximo para descargar el body),
/// no un timeout por lectura inactiva (idle read). Por lo tanto, no se configura timeout de body
/// para evitar abortar descargas legítimas de pistas completas en conexiones lentas o con throttling.
///
/// En caso de conexión estancada (stall), el hilo productor desacoplado permanece bloqueado en la
/// lectura del socket hasta que la pila TCP del SO señale timeout/RST/FIN, o el proceso finalice.
/// Ante una cancelación explícita (`CancelHandle::cancel()` por stop o cambio de pista), el consumidor
/// se desbloquea inmediatamente; el productor detectará la cancelación al retornar de la lectura o al
/// terminar, saliendo limpiamente.
#[derive(Debug, Clone, Copy)]
pub struct HttpTimeouts {
    /// Timeout para establecer conexión (socket + TLS).
    pub connect: Duration,
    /// Timeout para recibir las cabeceras de respuesta HTTP.
    pub recv_response: Duration,
}

impl Default for HttpTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            recv_response: Duration::from_secs(15),
        }
    }
}

/// Abre una URL de audio remoto por HTTP usando los timeouts por defecto.
pub fn open(url: &str) -> Result<(HttpStream, StreamInfo), StreamError> {
    open_with(url, HttpTimeouts::default())
}

/// Abre una URL de audio remoto por HTTP con timeouts configurables.
pub(crate) fn open_with(
    url: &str,
    timeouts: HttpTimeouts,
) -> Result<(HttpStream, StreamInfo), StreamError> {
    let config = ureq::config::Config::builder()
        .timeout_connect(Some(timeouts.connect))
        .timeout_recv_response(Some(timeouts.recv_response))
        .build();
    let agent: ureq::Agent = config.new_agent();

    let user_agent = format!("mp3tui/{}", env!("CARGO_PKG_VERSION"));
    let response = agent
        .get(url)
        .header("User-Agent", &user_agent)
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(code) => StreamError::HttpStatus(code, format!("HTTP {code}")),
            other => StreamError::Connection(other.to_string()),
        })?;

    let status = response.status().as_u16();
    if status >= 400 {
        let reason = response.status().canonical_reason().unwrap_or("Error");
        return Err(StreamError::HttpStatus(status, reason.to_string()));
    }

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let content_length = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());

    let info = StreamInfo {
        content_type,
        content_length,
    };

    let (producer, stream) = StreamProducer::new(content_length);
    let mut body_reader = response.into_body().into_reader();
    let cancel = producer.cancel_handle();

    thread::spawn(move || {
        let mut chunk = [0u8; 32768];
        loop {
            if cancel.is_canceled() {
                break;
            }
            match body_reader.read(&mut chunk) {
                Ok(0) => {
                    producer.finish();
                    break;
                }
                Ok(n) => {
                    producer.push(&chunk[..n]);
                }
                Err(e) => {
                    if !cancel.is_canceled() {
                        producer.fail(&e.to_string());
                    }
                    break;
                }
            }
        }
    });

    Ok((stream, info))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_blocked_reader_receives_later_data() {
        let (producer, mut stream) = StreamProducer::new(Some(10));
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let mut buf = [0u8; 5];
            let n = stream.read(&mut buf).unwrap();
            tx.send((n, buf)).unwrap();
        });

        thread::sleep(Duration::from_millis(50));
        producer.push(b"hello");
        producer.finish();

        let (n, buf) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn test_eof_after_finish() {
        let (producer, mut stream) = StreamProducer::new(Some(4));
        producer.push(b"test");
        producer.finish();

        let mut buf = [0u8; 4];
        let n = stream.read(&mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf, b"test");

        let n_eof = stream.read(&mut buf).unwrap();
        assert_eq!(n_eof, 0);
    }

    #[test]
    fn test_error_propagation() {
        let (producer, mut stream) = StreamProducer::new(Some(10));
        producer.fail("error de conexión simulado");

        let mut buf = [0u8; 10];
        let err = stream.read(&mut buf).unwrap_err();
        assert!(
            err.to_string().contains("error de conexión simulado"),
            "Error mensaje incorrecto: {err}"
        );
    }

    #[test]
    fn test_cancel_unblocks_blocked_reader() {
        let (producer, mut stream) = StreamProducer::new(Some(100));
        let cancel = producer.cancel_handle();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let mut buf = [0u8; 10];
            let res = stream.read(&mut buf);
            tx.send(res).unwrap();
        });

        thread::sleep(Duration::from_millis(50));
        cancel.cancel();

        let res = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("El lector bloqueado no se desbloqueó a tiempo");
        assert!(res.is_err(), "Cancel debe retornar un error");
    }

    #[test]
    fn test_seek_finite_stream() {
        let (producer, mut stream) = StreamProducer::new(Some(20));
        assert!(stream.is_seekable());
        assert_eq!(stream.byte_len(), Some(20));

        producer.push(b"0123456789");

        // Seek dentro de lo descargado
        assert_eq!(stream.seek(SeekFrom::Start(5)).unwrap(), 5);
        let mut buf = [0u8; 5];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"56789");

        // Seek adelantado (permitido dentro de [0, len])
        assert_eq!(stream.seek(SeekFrom::Start(15)).unwrap(), 15);

        // Seek relativo y desde el final
        assert_eq!(stream.seek(SeekFrom::Current(-5)).unwrap(), 10);
        assert_eq!(stream.seek(SeekFrom::End(-2)).unwrap(), 18);

        // Seek fuera de rango
        assert!(stream.seek(SeekFrom::Start(21)).is_err());
        assert!(stream.seek(SeekFrom::Current(-30)).is_err());
    }

    #[test]
    #[allow(clippy::seek_from_current)]
    fn test_seek_no_length_stream() {
        let (producer, mut stream) = StreamProducer::new(None);
        assert!(!stream.is_seekable());
        assert_eq!(stream.byte_len(), None);

        producer.push(b"stream_bytes");

        // Seek a posición actual es Ok
        assert_eq!(stream.seek(SeekFrom::Current(0)).unwrap(), 0);

        // Cualquier otro seek es Unsupported
        assert!(stream.seek(SeekFrom::Start(5)).is_err());
        assert!(stream.seek(SeekFrom::End(0)).is_err());
    }

    #[test]
    fn test_drop_cancels_producer() {
        let (producer, stream) = StreamProducer::new(Some(10));
        let cancel = producer.cancel_handle();
        assert!(!cancel.is_canceled());

        drop(stream);
        assert!(cancel.is_canceled());
    }

    #[test]
    fn test_integration_open_with_content_length() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut req_buf = [0u8; 1024];
            let _ = socket.read(&mut req_buf);

            let body = b"hello mp3 file!";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).unwrap();
            socket.write_all(body).unwrap();
        });

        let url = format!("http://127.0.0.1:{port}/track.mp3");
        let (mut stream, info) = open(&url).unwrap();

        assert_eq!(info.content_length, Some(15));
        assert_eq!(info.content_type.as_deref(), Some("audio/mpeg"));
        assert_eq!(stream.byte_len(), Some(15));
        assert!(stream.is_seekable());

        let mut data = Vec::new();
        stream.read_to_end(&mut data).unwrap();
        assert_eq!(&data, b"hello mp3 file!");
    }

    #[test]
    fn test_integration_open_follows_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            // Primer request: 302 Redirect
            let (mut socket1, _) = listener.accept().unwrap();
            let mut req1 = [0u8; 1024];
            let _ = socket1.read(&mut req1);
            let redirect = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/final.mp3\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            socket1.write_all(redirect.as_bytes()).unwrap();

            // Segundo request: 200 OK
            let (mut socket2, _) = listener.accept().unwrap();
            let mut req2 = [0u8; 1024];
            let _ = socket2.read(&mut req2);
            let body = b"redirected audio";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket2.write_all(response.as_bytes()).unwrap();
            socket2.write_all(body).unwrap();
        });

        let url = format!("http://127.0.0.1:{port}/init.mp3");
        let (mut stream, info) = open(&url).unwrap();

        assert_eq!(info.content_length, Some(16));
        let mut data = Vec::new();
        stream.read_to_end(&mut data).unwrap();
        assert_eq!(&data, b"redirected audio");
    }

    #[test]
    fn test_integration_open_http_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut req = [0u8; 1024];
            let _ = socket.read(&mut req);
            let response =
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            socket.write_all(response.as_bytes()).unwrap();
        });

        let url = format!("http://127.0.0.1:{port}/missing.mp3");
        let res = open(&url);
        match res {
            Err(StreamError::HttpStatus(404, _)) => {}
            other => panic!("Esperaba HttpStatus(404), obtuve {other:?}"),
        }
    }

    #[test]
    fn test_integration_open_slow_body_does_not_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut req = [0u8; 1024];
            let _ = socket.read(&mut req);

            let part1 = b"hello ";
            let part2 = b"world!";
            let total_len = part1.len() + part2.len();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                total_len
            );
            socket.write_all(response.as_bytes()).unwrap();
            socket.write_all(part1).unwrap();
            socket.flush().unwrap();

            // Pausa mayor al timeout configurado (150 ms) para simular descarga lenta del body
            thread::sleep(Duration::from_millis(400));

            let _ = socket.write_all(part2);
            let _ = socket.flush();
        });

        let url = format!("http://127.0.0.1:{port}/slow.mp3");
        let timeouts = HttpTimeouts {
            connect: Duration::from_millis(150),
            recv_response: Duration::from_millis(150),
        };
        let (mut stream, _info) = open_with(&url, timeouts).unwrap();
        let mut data = Vec::new();
        stream
            .read_to_end(&mut data)
            .expect("la lectura del body no debe fallar por timeout total");
        assert_eq!(&data, b"hello world!");
    }
}
