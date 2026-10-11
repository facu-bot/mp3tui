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
use ureq::ResponseExt;

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
///
/// Tras cancelar, las lecturas en `HttpStream` retornan inmediatamente `io::ErrorKind::ConnectionAborted`,
/// evitando bucles infinitos de reintento en métodos como `read_exact`, `read_to_end` o adaptadores de symphonia.
#[derive(Clone)]
pub struct CancelHandle {
    canceled: Arc<AtomicBool>,
    condvar: Arc<Condvar>,
}

impl CancelHandle {
    pub fn cancel(&self) {
        self.canceled.store(true, Ordering::SeqCst);
        self.condvar.notify_all();
    }

    pub fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::SeqCst)
    }
}

struct BufferState {
    data: Vec<u8>,
    finished: bool,
    error: Option<String>,
}

struct SequentialShared {
    state: Mutex<BufferState>,
    condvar: Arc<Condvar>,
    canceled: Arc<AtomicBool>,
}

/// Productor interno que llena el buffer compartido desde el hilo de red (modo secuencial).
pub struct StreamProducer {
    shared: Arc<SequentialShared>,
}

impl StreamProducer {
    pub fn new(content_length: Option<u64>) -> (Self, HttpStream) {
        let is_seekable = content_length.is_some();
        Self::new_sequential(content_length, is_seekable)
    }

    pub(crate) fn new_sequential(
        content_length: Option<u64>,
        is_seekable: bool,
    ) -> (Self, HttpStream) {
        let canceled = Arc::new(AtomicBool::new(false));
        let condvar = Arc::new(Condvar::new());
        let shared = Arc::new(SequentialShared {
            state: Mutex::new(BufferState {
                data: Vec::new(),
                finished: false,
                error: None,
            }),
            condvar: Arc::clone(&condvar),
            canceled: Arc::clone(&canceled),
        });
        let cancel = CancelHandle {
            canceled: Arc::clone(&canceled),
            condvar: Arc::clone(&condvar),
        };
        let producer = StreamProducer {
            shared: Arc::clone(&shared),
        };
        let consumer = HttpStream {
            storage: StreamStorage::Sequential {
                shared,
                content_length,
                is_seekable,
            },
            cancel,
            pos: 0,
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
            canceled: Arc::clone(&self.shared.canceled),
            condvar: Arc::clone(&self.shared.condvar),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChunkStatus {
    Missing,
    Downloading,
    Done,
}

struct RangedState {
    data: Vec<u8>,
    chunks: Vec<ChunkStatus>,
    wanted_offset: Option<u64>,
    error: Option<String>,
}

struct RangedShared {
    state: Mutex<RangedState>,
    condvar: Arc<Condvar>,
    canceled: Arc<AtomicBool>,
    total_len: u64,
    chunk_size: usize,
}

enum StreamStorage {
    Sequential {
        shared: Arc<SequentialShared>,
        content_length: Option<u64>,
        is_seekable: bool,
    },
    Ranged {
        shared: Arc<RangedShared>,
    },
}

/// Consumidor de audio progresivo que implementa Read, Seek y MediaSource de symphonia.
pub struct HttpStream {
    storage: StreamStorage,
    cancel: CancelHandle,
    pos: u64,
}

impl fmt::Debug for HttpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpStream")
            .field("pos", &self.pos)
            .field("is_seekable", &self.is_seekable())
            .field("byte_len", &self.byte_len())
            .finish()
    }
}

impl HttpStream {
    /// Obtiene un handle clonable para cancelar la descarga asociada a este stream.
    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }
}

impl Drop for HttpStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn read_sequential(
    shared: &SequentialShared,
    pos: &mut u64,
    buf: &mut [u8],
) -> io::Result<usize> {
    let mut state = shared
        .state
        .lock()
        .map_err(|_| io::Error::other("lock del buffer compartido envenenado"))?;

    loop {
        if shared.canceled.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "descarga cancelada",
            ));
        }

        if let Some(ref err) = state.error {
            return Err(io::Error::other(err.clone()));
        }

        let cur_len = state.data.len() as u64;
        if *pos < cur_len {
            let avail = (cur_len - *pos) as usize;
            let to_copy = avail.min(buf.len());
            let start = *pos as usize;
            buf[..to_copy].copy_from_slice(&state.data[start..start + to_copy]);
            *pos += to_copy as u64;
            return Ok(to_copy);
        }

        if state.finished {
            return Ok(0);
        }

        state = shared
            .condvar
            .wait(state)
            .map_err(|_| io::Error::other("condvar del buffer envenenado"))?;
    }
}

fn read_ranged(shared: &RangedShared, pos: &mut u64, buf: &mut [u8]) -> io::Result<usize> {
    if *pos >= shared.total_len {
        return Ok(0);
    }

    let chunk_idx = (*pos / shared.chunk_size as u64) as usize;
    let mut state = shared
        .state
        .lock()
        .map_err(|_| io::Error::other("lock del buffer compartido envenenado"))?;

    loop {
        if shared.canceled.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "descarga cancelada",
            ));
        }

        if let Some(ref err) = state.error {
            return Err(io::Error::other(err.clone()));
        }

        if chunk_idx < state.chunks.len() && state.chunks[chunk_idx] == ChunkStatus::Done {
            let chunk_end =
                ((chunk_idx as u64 + 1) * shared.chunk_size as u64).min(shared.total_len);
            let avail = (chunk_end - *pos) as usize;
            let to_copy = avail.min(buf.len());
            let start = *pos as usize;
            buf[..to_copy].copy_from_slice(&state.data[start..start + to_copy]);
            *pos += to_copy as u64;

            if *pos < shared.total_len {
                state.wanted_offset = Some(*pos);
                shared.condvar.notify_all();
            }

            return Ok(to_copy);
        }

        state.wanted_offset = Some(*pos);
        shared.condvar.notify_all();

        state = shared
            .condvar
            .wait(state)
            .map_err(|_| io::Error::other("condvar del buffer envenenado"))?;
    }
}

impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match &self.storage {
            StreamStorage::Ranged { shared } => read_ranged(shared, &mut self.pos, buf),
            StreamStorage::Sequential { shared, .. } => {
                read_sequential(shared, &mut self.pos, buf)
            }
        }
    }
}

impl Seek for HttpStream {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match &self.storage {
            StreamStorage::Ranged { shared } => {
                let target = match pos {
                    SeekFrom::Start(n) => n as i128,
                    SeekFrom::Current(n) => (self.pos as i128) + (n as i128),
                    SeekFrom::End(n) => (shared.total_len as i128) + (n as i128),
                };
                if !(0..=(shared.total_len as i128)).contains(&target) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "posición de seek fuera del rango [0, total_len]",
                    ));
                }
                self.pos = target as u64;
                Ok(self.pos)
            }
            StreamStorage::Sequential {
                content_length,
                is_seekable,
                ..
            } => {
                if *is_seekable {
                    match content_length {
                        Some(len) => {
                            let target = match pos {
                                SeekFrom::Start(n) => n as i128,
                                SeekFrom::Current(n) => (self.pos as i128) + (n as i128),
                                SeekFrom::End(n) => (*len as i128) + (n as i128),
                            };
                            if !(0..=(*len as i128)).contains(&target) {
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
                } else {
                    match pos {
                        SeekFrom::Current(0) => Ok(self.pos),
                        SeekFrom::Start(n) if n == self.pos => Ok(self.pos),
                        _ => Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "seek no soportado en stream HTTP sin soporte Range",
                        )),
                    }
                }
            }
        }
    }
}

impl MediaSource for HttpStream {
    fn is_seekable(&self) -> bool {
        match &self.storage {
            StreamStorage::Ranged { .. } => true,
            StreamStorage::Sequential { is_seekable, .. } => *is_seekable,
        }
    }

    fn byte_len(&self) -> Option<u64> {
        match &self.storage {
            StreamStorage::Ranged { shared } => Some(shared.total_len),
            StreamStorage::Sequential {
                content_length,
                is_seekable,
                ..
            } => {
                if *is_seekable {
                    *content_length
                } else {
                    None
                }
            }
        }
    }
}

/// Configuración de timeouts para peticiones HTTP de audio remoto.
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

pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024; // 1 MiB

pub(crate) fn parse_content_range_total(header: &str) -> Option<u64> {
    let s = header.trim();
    let after_bytes = if s.len() >= 6 && s[..6].eq_ignore_ascii_case("bytes ") {
        &s[6..]
    } else {
        return None;
    };
    let slash_idx = after_bytes.rfind('/')?;
    let total_str = after_bytes[slash_idx + 1..].trim();
    if total_str == "*" {
        return None;
    }
    total_str.parse::<u64>().ok()
}

enum RangeFetchError {
    Canceled,
    HttpStatus(u16, String),
    Transient(String),
}

fn fetch_range(
    agent: &ureq::Agent,
    url: &str,
    user_agent: &str,
    range_header: &str,
    expected_len: usize,
    canceled: &AtomicBool,
) -> Result<Vec<u8>, RangeFetchError> {
    if canceled.load(Ordering::SeqCst) {
        return Err(RangeFetchError::Canceled);
    }

    let resp = agent
        .get(url)
        .header("User-Agent", user_agent)
        .header("Accept-Encoding", "identity")
        .header("Range", range_header)
        .call();

    let response = match resp {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(code)) => {
            return Err(RangeFetchError::HttpStatus(code, format!("HTTP {code}")));
        }
        Err(other) => {
            if canceled.load(Ordering::SeqCst) {
                return Err(RangeFetchError::Canceled);
            }
            return Err(RangeFetchError::Transient(other.to_string()));
        }
    };

    let status = response.status().as_u16();
    if status >= 400 {
        let reason = response.status().canonical_reason().unwrap_or("Error");
        return Err(RangeFetchError::HttpStatus(status, reason.to_string()));
    }

    let mut body_reader = response.into_body().into_reader();
    let mut buf = Vec::with_capacity(expected_len);
    let mut chunk = [0u8; 16384];

    while buf.len() < expected_len {
        if canceled.load(Ordering::SeqCst) {
            return Err(RangeFetchError::Canceled);
        }
        match body_reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let to_take = n.min(expected_len - buf.len());
                buf.extend_from_slice(&chunk[..to_take]);
            }
            Err(e) => {
                if canceled.load(Ordering::SeqCst) {
                    return Err(RangeFetchError::Canceled);
                }
                return Err(RangeFetchError::Transient(e.to_string()));
            }
        }
    }

    if buf.len() < expected_len {
        if canceled.load(Ordering::SeqCst) {
            return Err(RangeFetchError::Canceled);
        }
        return Err(RangeFetchError::Transient(
            "rango HTTP incompleto (EOF prematuro)".to_string(),
        ));
    }

    Ok(buf)
}

fn run_ranged_downloader(
    agent: ureq::Agent,
    url: String,
    user_agent: String,
    shared: Arc<RangedShared>,
) {
    let mut retries_for_chunk = 0;
    let mut last_failed_chunk: Option<usize> = None;

    loop {
        if shared.canceled.load(Ordering::SeqCst) {
            break;
        }

        let (chunk_idx, range_start, range_end) = {
            let mut state = match shared.state.lock() {
                Ok(g) => g,
                Err(_) => break,
            };

            loop {
                if shared.canceled.load(Ordering::SeqCst) || state.error.is_some() {
                    return;
                }

                let all_done = state.chunks.iter().all(|c| *c == ChunkStatus::Done);
                if all_done {
                    return;
                }

                let candidate = if let Some(wanted_pos) = state.wanted_offset {
                    let wanted_chunk = (wanted_pos / shared.chunk_size as u64) as usize;
                    if wanted_chunk < state.chunks.len()
                        && state.chunks[wanted_chunk] == ChunkStatus::Missing
                    {
                        Some(wanted_chunk)
                    } else {
                        None
                    }
                } else {
                    None
                };

                let next_chunk = candidate
                    .or_else(|| state.chunks.iter().position(|c| *c == ChunkStatus::Missing));

                if let Some(idx) = next_chunk {
                    state.chunks[idx] = ChunkStatus::Downloading;
                    let start = idx as u64 * shared.chunk_size as u64;
                    let end =
                        ((idx as u64 + 1) * shared.chunk_size as u64).min(shared.total_len);
                    break (idx, start, end);
                }

                match shared.condvar.wait(state) {
                    Ok(g) => state = g,
                    Err(_) => return,
                }
            }
        };

        let range_header = format!("bytes={}-{}", range_start, range_end - 1);
        let expected_len = (range_end - range_start) as usize;
        let result = fetch_range(
            &agent,
            &url,
            &user_agent,
            &range_header,
            expected_len,
            &shared.canceled,
        );

        match result {
            Ok(bytes) => {
                retries_for_chunk = 0;
                last_failed_chunk = None;
                if let Ok(mut state) = shared.state.lock() {
                    let start = range_start as usize;
                    let end = start + bytes.len();
                    if end <= state.data.len() {
                        state.data[start..end].copy_from_slice(&bytes);
                    }
                    state.chunks[chunk_idx] = ChunkStatus::Done;
                    shared.condvar.notify_all();
                }
            }
            Err(RangeFetchError::Canceled) => {
                break;
            }
            Err(RangeFetchError::HttpStatus(code, msg)) => {
                if let Ok(mut state) = shared.state.lock() {
                    state.error = Some(format!("servidor HTTP respondió {code}: {msg}"));
                    state.chunks[chunk_idx] = ChunkStatus::Missing;
                    shared.condvar.notify_all();
                }
                break;
            }
            Err(RangeFetchError::Transient(msg)) => {
                let retries = if last_failed_chunk == Some(chunk_idx) {
                    retries_for_chunk + 1
                } else {
                    last_failed_chunk = Some(chunk_idx);
                    1
                };
                retries_for_chunk = retries;

                if retries > 2 {
                    if let Ok(mut state) = shared.state.lock() {
                        state.error = Some(format!(
                            "error de conexión HTTP tras {retries} intentos: {msg}"
                        ));
                        state.chunks[chunk_idx] = ChunkStatus::Missing;
                        shared.condvar.notify_all();
                    }
                    break;
                } else {
                    if let Ok(mut state) = shared.state.lock() {
                        state.chunks[chunk_idx] = ChunkStatus::Missing;
                        shared.condvar.notify_all();
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

/// Abre una URL de audio remoto por HTTP usando los timeouts y tamaño de chunk por defecto.
pub fn open(url: &str) -> Result<(HttpStream, StreamInfo), StreamError> {
    open_with_params(url, HttpTimeouts::default(), DEFAULT_CHUNK_SIZE)
}

/// Abre una URL de audio remoto por HTTP con timeouts configurables.
pub(crate) fn open_with(
    url: &str,
    timeouts: HttpTimeouts,
) -> Result<(HttpStream, StreamInfo), StreamError> {
    open_with_params(url, timeouts, DEFAULT_CHUNK_SIZE)
}

/// Abre una URL de audio remoto por HTTP con timeouts y tamaño de chunk configurables.
pub(crate) fn open_with_params(
    url: &str,
    timeouts: HttpTimeouts,
    chunk_size: usize,
) -> Result<(HttpStream, StreamInfo), StreamError> {
    let config = ureq::config::Config::builder()
        .timeout_connect(Some(timeouts.connect))
        .timeout_recv_response(Some(timeouts.recv_response))
        .build();
    let agent: ureq::Agent = config.new_agent();

    let user_agent = format!("mp3tui/{}", env!("CARGO_PKG_VERSION"));
    let range_header = format!("bytes=0-{}", chunk_size.saturating_sub(1));

    let response = agent
        .get(url)
        .header("User-Agent", &user_agent)
        .header("Accept-Encoding", "identity")
        .header("Range", &range_header)
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

    let content_range = response
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok());

    let range_total = content_range.and_then(parse_content_range_total);

    let effective_url = {
        let u = response.get_uri().to_string();
        if u.starts_with("http://") || u.starts_with("https://") {
            u
        } else {
            url.to_string()
        }
    };

    if status == 206 && let Some(total_len) = range_total && total_len > 0 {
        let info = StreamInfo {
            content_type,
            content_length: Some(total_len),
        };

        let expected_first_chunk = (chunk_size as u64).min(total_len) as usize;
        let mut initial_chunk = Vec::with_capacity(expected_first_chunk);
        let mut body_reader = response.into_body().into_reader();
        while initial_chunk.len() < expected_first_chunk {
            let mut buf = [0u8; 16384];
            let to_read = (expected_first_chunk - initial_chunk.len()).min(buf.len());
            match body_reader.read(&mut buf[..to_read]) {
                Ok(0) => break,
                Ok(n) => initial_chunk.extend_from_slice(&buf[..n]),
                Err(e) => return Err(StreamError::Io(e.to_string())),
            }
        }

        if initial_chunk.len() < expected_first_chunk {
            return Err(StreamError::Io("primer fragmento HTTP incompleto".to_string()));
        }

        let mut data = vec![0u8; total_len as usize];
        data[..expected_first_chunk].copy_from_slice(&initial_chunk[..expected_first_chunk]);

        let num_chunks = if chunk_size > 0 {
            total_len.div_ceil(chunk_size as u64) as usize
        } else {
            0
        };

        let mut chunks = vec![ChunkStatus::Missing; num_chunks];
        if num_chunks > 0 {
            chunks[0] = ChunkStatus::Done;
        }

        let canceled = Arc::new(AtomicBool::new(false));
        let condvar = Arc::new(Condvar::new());
        let shared = Arc::new(RangedShared {
            state: Mutex::new(RangedState {
                data,
                chunks,
                wanted_offset: None,
                error: None,
            }),
            condvar: Arc::clone(&condvar),
            canceled: Arc::clone(&canceled),
            total_len,
            chunk_size,
        });

        let cancel = CancelHandle {
            canceled: Arc::clone(&canceled),
            condvar: Arc::clone(&condvar),
        };

        let dl_shared = Arc::clone(&shared);
        let dl_agent = agent.clone();
        let dl_url = effective_url;
        let dl_ua = user_agent;
        thread::Builder::new()
            .name("http-range-downloader".into())
            .spawn(move || {
                run_ranged_downloader(dl_agent, dl_url, dl_ua, dl_shared);
            })
            .map_err(|e| StreamError::Io(e.to_string()))?;

        let stream = HttpStream {
            storage: StreamStorage::Ranged { shared },
            cancel,
            pos: 0,
        };

        Ok((stream, info))
    } else {
        let content_length = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());

        let info = StreamInfo {
            content_type,
            content_length,
        };

        let (producer, stream) = StreamProducer::new_sequential(content_length, false);
        let mut body_reader = response.into_body().into_reader();
        let cancel = producer.cancel_handle();

        thread::Builder::new()
            .name("http-sequential-downloader".into())
            .spawn(move || {
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
            })
            .map_err(|e| StreamError::Io(e.to_string()))?;

        Ok((stream, info))
    }
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
        assert_eq!(
            res.unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted,
            "Cancel debe retornar ConnectionAborted"
        );
    }

    #[test]
    fn test_cancel_unblocks_read_exact() {
        let (producer, mut stream) = StreamProducer::new(Some(100));
        let cancel = producer.cancel_handle();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let mut buf = [0u8; 10];
            let res = stream.read_exact(&mut buf);
            tx.send(res).unwrap();
        });

        thread::sleep(Duration::from_millis(50));
        cancel.cancel();

        let res = rx
            .recv_timeout(Duration::from_millis(300))
            .expect("read_exact no retornó tras cancel (livelock por Interrupted)");
        assert_eq!(res.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
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
                "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mpeg\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len() - 1,
                body.len(),
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

    #[test]
    fn test_integration_open_sends_accept_encoding_identity() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut req_buf = [0u8; 2048];
            let n = socket.read(&mut req_buf).unwrap();
            let req_str = String::from_utf8_lossy(&req_buf[..n]);

            let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response.as_bytes());
            req_str.to_string()
        });

        let url = format!("http://127.0.0.1:{port}/track.mp3");
        let _ = open(&url);
        let request_str = handle.join().unwrap();
        assert!(
            request_str
                .to_ascii_lowercase()
                .contains("accept-encoding: identity"),
            "El request debe incluir 'Accept-Encoding: identity', recibido:\n{request_str}"
        );
    }

    #[test]
    fn test_server_ignores_range_returns_200_unseekable() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut req_buf = [0u8; 1024];
            let _ = socket.read(&mut req_buf);

            let body = b"plain sequential stream";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.write_all(body);
        });

        let url = format!("http://127.0.0.1:{port}/stream.mp3");
        let (mut stream, info) = open(&url).expect("open stream");

        assert_eq!(info.content_length, Some(23));
        assert!(
            !stream.is_seekable(),
            "server answering 200 must yield is_seekable=false"
        );
        assert_eq!(
            stream.byte_len(),
            None,
            "server answering 200 must yield byte_len=None"
        );

        let mut data = Vec::new();
        stream.read_to_end(&mut data).unwrap();
        assert_eq!(&data, b"plain sequential stream");
    }

    #[test]
    fn test_range_server_tail_read_returns_fast_while_sequential_is_throttled() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let total_len = 2 * 1024 * 1024; // 2 MiB
        let mut payload = vec![0u8; total_len];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let payload = Arc::new(payload);
        let payload_server = Arc::clone(&payload);

        thread::spawn(move || {
            while let Ok((mut socket, _)) = listener.accept() {
                let payload = Arc::clone(&payload_server);
                thread::spawn(move || {
                    let mut req_buf = [0u8; 2048];
                    let n = match socket.read(&mut req_buf) {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let req_str = String::from_utf8_lossy(&req_buf[..n]);
                    let range_header = req_str
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range: bytes="))
                        .map(|l| l.trim().to_string());

                    if let Some(rh) = range_header {
                        let after = rh.split('=').nth(1).unwrap_or("0-0");
                        let parts: Vec<&str> = after.split('-').collect();
                        let start: usize = parts[0].trim().parse().unwrap_or(0);
                        let end: usize = parts[1].trim().parse().unwrap_or(payload.len() - 1);
                        let chunk_data = &payload[start..=end];

                        let resp = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            payload.len(),
                            chunk_data.len()
                        );
                        let _ = socket.write_all(resp.as_bytes());
                        let _ = socket.write_all(chunk_data);
                    } else {
                        // Non-range GET: simulate throttling (sleep)
                        thread::sleep(Duration::from_secs(3));
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            payload.len()
                        );
                        let _ = socket.write_all(resp.as_bytes());
                        let _ = socket.write_all(&payload);
                    }
                });
            }
        });

        let url = format!("http://127.0.0.1:{port}/track.m4a");
        let chunk_size = 64 * 1024;
        let (mut stream, info) = open_with_params(&url, HttpTimeouts::default(), chunk_size)
            .expect("open stream");

        assert_eq!(info.content_length, Some(total_len as u64));
        assert!(stream.is_seekable());

        let t0 = std::time::Instant::now();
        // Seek to trailing 160 bytes (probe tag read)
        stream.seek(SeekFrom::Start((total_len - 160) as u64)).unwrap();
        let mut buf = [0u8; 160];
        stream.read_exact(&mut buf).unwrap();
        let elapsed = t0.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "tail read must complete in < 1 s without waiting for sequential download, took: {:?}",
            elapsed
        );
        assert_eq!(&buf, &payload[total_len - 160..]);
    }

    #[test]
    fn test_range_server_full_sequential_read_multi_chunk() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let total_len = 256 * 1024; // 256 KiB
        let mut payload = vec![0u8; total_len];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = ((i * 7 + 13) % 256) as u8;
        }
        let payload = Arc::new(payload);
        let payload_server = Arc::clone(&payload);

        thread::spawn(move || {
            while let Ok((mut socket, _)) = listener.accept() {
                let payload = Arc::clone(&payload_server);
                thread::spawn(move || {
                    let mut req_buf = [0u8; 2048];
                    let n = match socket.read(&mut req_buf) {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let req_str = String::from_utf8_lossy(&req_buf[..n]);
                    let range_header = req_str
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range: bytes="))
                        .map(|l| l.trim().to_string());

                    if let Some(rh) = range_header {
                        let after = rh.split('=').nth(1).unwrap_or("0-0");
                        let parts: Vec<&str> = after.split('-').collect();
                        let start: usize = parts[0].trim().parse().unwrap_or(0);
                        let end: usize = parts[1].trim().parse().unwrap_or(payload.len() - 1);
                        let chunk_data = &payload[start..=end];

                        let resp = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            payload.len(),
                            chunk_data.len()
                        );
                        let _ = socket.write_all(resp.as_bytes());
                        let _ = socket.write_all(chunk_data);
                    }
                });
            }
        });

        let url = format!("http://127.0.0.1:{port}/audio.m4a");
        let chunk_size = 64 * 1024;
        let (mut stream, info) = open_with_params(&url, HttpTimeouts::default(), chunk_size)
            .expect("open stream");

        assert_eq!(info.content_length, Some(total_len as u64));
        let mut read_data = Vec::new();
        stream.read_to_end(&mut read_data).expect("read_to_end");
        assert_eq!(read_data.len(), total_len);
        assert_eq!(read_data, *payload);
    }

    #[test]
    fn test_range_server_forward_seek_and_read() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let total_len = 300 * 1024; // 300 KiB
        let mut payload = vec![0u8; total_len];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i % 255) as u8;
        }
        let payload = Arc::new(payload);
        let payload_server = Arc::clone(&payload);

        thread::spawn(move || {
            while let Ok((mut socket, _)) = listener.accept() {
                let payload = Arc::clone(&payload_server);
                thread::spawn(move || {
                    let mut req_buf = [0u8; 2048];
                    let n = match socket.read(&mut req_buf) {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let req_str = String::from_utf8_lossy(&req_buf[..n]);
                    let range_header = req_str
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range: bytes="))
                        .map(|l| l.trim().to_string());

                    if let Some(rh) = range_header {
                        let after = rh.split('=').nth(1).unwrap_or("0-0");
                        let parts: Vec<&str> = after.split('-').collect();
                        let start: usize = parts[0].trim().parse().unwrap_or(0);
                        let end: usize = parts[1].trim().parse().unwrap_or(payload.len() - 1);
                        let chunk_data = &payload[start..=end];

                        let resp = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            payload.len(),
                            chunk_data.len()
                        );
                        let _ = socket.write_all(resp.as_bytes());
                        let _ = socket.write_all(chunk_data);
                    }
                });
            }
        });

        let url = format!("http://127.0.0.1:{port}/audio.m4a");
        let chunk_size = 64 * 1024;
        let (mut stream, _) = open_with_params(&url, HttpTimeouts::default(), chunk_size)
            .expect("open stream");

        // Forward seek to the middle (offset 150_000)
        stream.seek(SeekFrom::Start(150_000)).unwrap();
        let mut buf = [0u8; 200];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &payload[150_000..150_200]);
    }

    #[test]
    fn test_range_server_chunk_403_surfaces_error_without_hang() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let total_len = 128 * 1024; // 128 KiB
        let payload = vec![42u8; total_len];

        thread::spawn(move || {
            while let Ok((mut socket, _)) = listener.accept() {
                let mut req_buf = [0u8; 2048];
                let n = match socket.read(&mut req_buf) {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                let req_str = String::from_utf8_lossy(&req_buf[..n]);
                let range_header = req_str
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("range: bytes="))
                    .map(|l| l.trim().to_string());

                if let Some(rh) = range_header {
                    let after = rh.split('=').nth(1).unwrap_or("0-0");
                    let start: usize = after.split('-').next().unwrap_or("0").trim().parse().unwrap_or(0);
                    if start == 0 {
                        let resp = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 0-65535/{total_len}\r\nContent-Length: 65536\r\nConnection: close\r\n\r\n"
                        );
                        let _ = socket.write_all(resp.as_bytes());
                        let _ = socket.write_all(&payload[0..65536]);
                    } else {
                        // Chunk 1 fails with 403 Forbidden
                        let resp = "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = socket.write_all(resp.as_bytes());
                    }
                }
            }
        });

        let url = format!("http://127.0.0.1:{port}/audio.m4a");
        let (mut stream, _) = open_with_params(&url, HttpTimeouts::default(), 64 * 1024)
            .expect("open stream");

        stream.seek(SeekFrom::Start(70_000)).unwrap();
        let mut buf = [0u8; 100];
        let err = stream.read_exact(&mut buf).unwrap_err();
        assert!(
            err.to_string().contains("403"),
            "error must mention HTTP 403, got: {err}"
        );
    }

    #[test]
    fn test_range_server_cancel_unblocks_promptly_and_thread_exits() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let total_len = 128 * 1024;
        let payload = vec![1u8; total_len];

        thread::spawn(move || {
            while let Ok((mut socket, _)) = listener.accept() {
                let mut req_buf = [0u8; 2048];
                let n = match socket.read(&mut req_buf) {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                let req_str = String::from_utf8_lossy(&req_buf[..n]);
                let range_header = req_str
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("range: bytes="))
                    .map(|l| l.trim().to_string());

                if let Some(rh) = range_header {
                    let after = rh.split('=').nth(1).unwrap_or("0-0");
                    let start: usize = after.split('-').next().unwrap_or("0").trim().parse().unwrap_or(0);
                    if start == 0 {
                        let resp = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 0-65535/{total_len}\r\nContent-Length: 65536\r\nConnection: close\r\n\r\n"
                        );
                        let _ = socket.write_all(resp.as_bytes());
                        let _ = socket.write_all(&payload[0..65536]);
                    } else {
                        // Sleep to simulate hanging request
                        thread::sleep(Duration::from_secs(5));
                    }
                }
            }
        });

        let url = format!("http://127.0.0.1:{port}/audio.m4a");
        let (mut stream, _) = open_with_params(&url, HttpTimeouts::default(), 64 * 1024)
            .expect("open stream");

        let cancel = stream.cancel_handle();
        let (tx, rx) = std::sync::mpsc::channel();

        thread::spawn(move || {
            stream.seek(SeekFrom::Start(70_000)).unwrap();
            let mut buf = [0u8; 100];
            let res = stream.read_exact(&mut buf);
            tx.send(res).unwrap();
        });

        thread::sleep(Duration::from_millis(50));
        let t0 = std::time::Instant::now();
        cancel.cancel();

        let res = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader must unblock promptly upon cancel");
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "cancel unblock took: {:?}",
            elapsed
        );
        assert_eq!(res.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
    }
}
