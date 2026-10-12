//! Fuente HTTP progresiva para symphonia.
//!
//! Permite reproducir archivos de audio remotos mientras se descargan en segundo plano,
//! garantizando que `stop()` o un cambio de pista nunca bloqueen la UI.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
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

/// Tope de datos sin consumir que el productor secuencial mantiene en memoria.
/// Al alcanzarlo, `StreamProducer::push` deja de leer del socket hasta que el
/// lector avance, evitando acumular el archivo completo en RAM.
const SEQUENTIAL_READ_AHEAD_CAP: usize = 4 * 1024 * 1024; // 4 MiB

/// Mensaje con el que se rechaza una lectura cuando el servidor manda las
/// cabeceras y deja de enviar cuerpo por más del presupuesto configurado en
/// [`HttpTimeouts::stall`]. Se hace explícito para que la UI lo muestre en vez
/// de colgarse esperando al timeout del kernel.
const STALL_ERROR_MESSAGE: &str = "el servidor dejó de enviar datos";

/// Cuánto prefijo ya consumido se acumula antes de recortar `data`. La poda se
/// hace en lotes para no recorrer el buffer entero en cada lectura.
const SEQUENTIAL_PRUNE_BATCH: usize = 512 * 1024; // 512 KiB

struct BufferState {
    data: Vec<u8>,
    /// Offset absoluto del primer byte conservado en `data` (lo anterior ya se podó).
    data_start: u64,
    /// Offset absoluto hasta donde llegó el lector; base para calcular lo no consumido.
    consumed: u64,
    finished: bool,
    error: Option<String>,
}

impl BufferState {
    /// Bytes de `data` que el lector todavía no consumió.
    fn unconsumed_len(&self) -> u64 {
        (self.data_start + self.data.len() as u64).saturating_sub(self.consumed)
    }

    /// Descarta el prefijo ya consumido en lotes: mantiene acotada la memoria
    /// sin recorrer todo el buffer en cada lectura.
    fn prune_consumed_prefix(&mut self) {
        let prunable = self.consumed.saturating_sub(self.data_start);
        if prunable < SEQUENTIAL_PRUNE_BATCH as u64 {
            return;
        }
        let prunable = prunable as usize;
        self.data.drain(..prunable);
        self.data_start += prunable as u64;
    }
}

struct SequentialShared {
    state: Mutex<BufferState>,
    condvar: Arc<Condvar>,
    canceled: Arc<AtomicBool>,
    /// Presupuesto de inactividad del lector en el modo secuencial.
    stall_deadline: Duration,
}

/// Productor interno que llena el buffer compartido desde el hilo de red (modo secuencial).
pub struct StreamProducer {
    shared: Arc<SequentialShared>,
}

impl StreamProducer {
    /// Solo para tests: la producción arma el productor con
    /// [`Self::new_sequential`].
    #[cfg(test)]
    pub fn new(content_length: Option<u64>) -> (Self, HttpStream) {
        let is_seekable = content_length.is_some();
        Self::new_sequential(content_length, is_seekable)
    }

    /// Solo para tests: usa el presupuesto de inactividad por defecto.
    #[cfg(test)]
    pub(crate) fn new_sequential(
        content_length: Option<u64>,
        is_seekable: bool,
    ) -> (Self, HttpStream) {
        Self::new_sequential_with_stall(
            content_length,
            is_seekable,
            HttpTimeouts::default().stall,
        )
    }

    /// Igual que [`Self::new_sequential`] pero con un presupuesto de
    /// inactividad configurable para el lector.
    pub(crate) fn new_sequential_with_stall(
        content_length: Option<u64>,
        is_seekable: bool,
        stall_deadline: Duration,
    ) -> (Self, HttpStream) {
        let canceled = Arc::new(AtomicBool::new(false));
        let condvar = Arc::new(Condvar::new());
        let shared = Arc::new(SequentialShared {
            state: Mutex::new(BufferState {
                data: Vec::new(),
                data_start: 0,
                consumed: 0,
                finished: false,
                error: None,
            }),
            condvar: Arc::clone(&condvar),
            canceled: Arc::clone(&canceled),
            stall_deadline,
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
        let mut state = match self.shared.state.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        loop {
            if self.shared.canceled.load(Ordering::SeqCst) {
                return;
            }
            if state.error.is_some() || state.finished {
                return;
            }
            if state.unconsumed_len() < SEQUENTIAL_READ_AHEAD_CAP as u64 {
                break;
            }
            // Se espera sin retener el `Mutex` (patrón del `Condvar`), y se
            // vuelve a evaluar el predicado al despertar.
            state = match self.shared.condvar.wait(state) {
                Ok(g) => g,
                Err(_) => return,
            };
        }
        state.data.extend_from_slice(bytes);
        drop(state);
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

#[derive(Clone, Debug)]
enum ChunkSlot {
    Missing,
    Downloading,
    Done(Vec<u8>),
}

struct RangedState {
    chunks: Vec<ChunkSlot>,
    wanted_offset: Option<u64>,
    error: Option<String>,
}

struct RangedShared {
    state: Mutex<RangedState>,
    condvar: Arc<Condvar>,
    canceled: Arc<AtomicBool>,
    total_len: u64,
    chunk_size: usize,
    /// Presupuesto de inactividad del lector en el modo con `Range`.
    stall_deadline: Duration,
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

/// Combina estados contiguos de fragmentos descargados en rangos de bytes [inicio, fin).
pub fn merge_chunk_ranges(chunks_done: &[bool], chunk_size: usize, total_len: u64) -> Vec<(u64, u64)> {
    if chunks_done.is_empty() || chunk_size == 0 || total_len == 0 {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut i = 0;
    while i < chunks_done.len() {
        if chunks_done[i] {
            let start_chunk = i;
            while i < chunks_done.len() && chunks_done[i] {
                i += 1;
            }
            let end_chunk = i;
            let start_byte = (start_chunk as u64 * chunk_size as u64).min(total_len);
            let end_byte = (end_chunk as u64 * chunk_size as u64).min(total_len);
            if end_byte > start_byte {
                ranges.push((start_byte, end_byte));
            }
        } else {
            i += 1;
        }
    }
    ranges
}

impl RangedShared {
    pub fn downloaded_ranges(&self) -> (Vec<(u64, u64)>, u64) {
        let is_done: Vec<bool> = match self.state.lock() {
            Ok(state) => state
                .chunks
                .iter()
                .map(|c| matches!(c, ChunkSlot::Done(_)))
                .collect(),
            Err(_) => Vec::new(),
        };
        let ranges = merge_chunk_ranges(&is_done, self.chunk_size, self.total_len);
        (ranges, self.total_len)
    }
}

/// Handle compartible para consultar las secciones descargadas de un stream con soporte Range.
#[derive(Clone)]
pub struct DownloadRangesHandle {
    shared: Arc<RangedShared>,
}

impl DownloadRangesHandle {
    pub fn downloaded_ranges(&self) -> (Vec<(u64, u64)>, u64) {
        self.shared.downloaded_ranges()
    }
}

impl HttpStream {
    /// Obtiene un handle clonable para cancelar la descarga asociada a este stream.
    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    /// Obtiene un handle compartible para consultar secciones descargadas si el stream soporta Range.
    pub fn download_ranges_handle(&self) -> Option<DownloadRangesHandle> {
        match &self.storage {
            StreamStorage::Ranged { shared } => Some(DownloadRangesHandle {
                shared: Arc::clone(shared),
            }),
            StreamStorage::Sequential { .. } => None,
        }
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

    // El presupuesto de inactividad se calcula al entrar a la lectura y se
    // reinicia en cada llamada: mientras haya datos que entregar, o el stream
    // termine, o falle el productor, o pidan cancelar, la espera retorna de
    // inmediato; solo una lectura en vuelo sin ningún progreso lo agota y
    // falla en vez de colgarse.
    let deadline = Instant::now() + shared.stall_deadline;

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

        let end = state.data_start + state.data.len() as u64;
        if *pos < state.data_start {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "lectura de bytes ya podados del buffer secuencial",
            ));
        }
        if *pos < end {
            let start = (*pos - state.data_start) as usize;
            let avail = (end - *pos) as usize;
            let to_copy = avail.min(buf.len());
            buf[..to_copy].copy_from_slice(&state.data[start..start + to_copy]);
            *pos += to_copy as u64;
            if *pos > state.consumed {
                state.consumed = *pos;
            }
            state.prune_consumed_prefix();
            // Despierta al productor que pueda estar esperando espacio.
            shared.condvar.notify_all();
            return Ok(to_copy);
        }

        if state.finished {
            return Ok(0);
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, STALL_ERROR_MESSAGE));
        }
        let remaining = deadline.saturating_duration_since(now);
        // Se espera sin retener el `Mutex` (patrón del `Condvar`) y solo hasta
        // el presupuesto restante; al despertar, la cabecera del bucle vuelve a
        // evaluar el predicado, así que si llegaron datos, cancelación, fin o
        // error no se pierde tiempo.
        (state, _) = shared
            .condvar
            .wait_timeout(state, remaining)
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

    // Presupuesto de inactividad idéntico al secuencial: se calcula al entrar
    // y se reinicia en cada llamada, de modo que solo una lectura en vuelo sin
    // ningún progreso lo agota.
    let deadline = Instant::now() + shared.stall_deadline;

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

        if chunk_idx < state.chunks.len() && let ChunkSlot::Done(chunk_data) = &state.chunks[chunk_idx] {
            let offset_in_chunk = (*pos - chunk_idx as u64 * shared.chunk_size as u64) as usize;
            if offset_in_chunk < chunk_data.len() {
                let avail = chunk_data.len() - offset_in_chunk;
                let to_copy = avail.min(buf.len());
                buf[..to_copy].copy_from_slice(&chunk_data[offset_in_chunk..offset_in_chunk + to_copy]);
                *pos += to_copy as u64;

                if *pos < shared.total_len {
                    state.wanted_offset = Some(*pos);
                    shared.condvar.notify_all();
                }

                return Ok(to_copy);
            }
        }

        state.wanted_offset = Some(*pos);
        shared.condvar.notify_all();

        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, STALL_ERROR_MESSAGE));
        }
        let remaining = deadline.saturating_duration_since(now);
        (state, _) = shared
            .condvar
            .wait_timeout(state, remaining)
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
    /// Presupuesto de inactividad del lector: si el servidor ya mandó las
    /// cabeceras y deja de enviar cuerpo por más de este tiempo, la lectura
    /// falla en vez de quedarse esperando para siempre. Se mide mirando el
    /// buffer compartido, no el socket, así que una pausa legítima de la
    /// reproducción (sin lectura en vuelo) no lo dispara.
    pub stall: Duration,
}

impl Default for HttpTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            recv_response: Duration::from_secs(15),
            stall: Duration::from_secs(30),
        }
    }
}

pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024; // 1 MiB
pub const MAX_RANGED_TOTAL: u64 = 512 * 1024 * 1024; // 512 MiB

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

/// Primer byte del rango en un `Content-Range: bytes START-END/TOTAL`.
fn parse_content_range_start(header: &str) -> Option<u64> {
    let s = header.trim();
    let after_bytes = if s.len() >= 6 && s[..6].eq_ignore_ascii_case("bytes ") {
        &s[6..]
    } else {
        return None;
    };
    let range = after_bytes.split('/').next()?.trim();
    range.split('-').next()?.trim().parse::<u64>().ok()
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
    range_start: u64,
    expected_len: usize,
    canceled: &AtomicBool,
) -> Result<Vec<u8>, RangeFetchError> {
    if canceled.load(Ordering::SeqCst) {
        return Err(RangeFetchError::Canceled);
    }

    let range_header = format!(
        "bytes={}-{}",
        range_start,
        range_start + expected_len as u64 - 1
    );

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

    // Un chunk pedido con `Range` solo sirve si el servidor responde 206 con el
    // rango exacto. Un 200 significa que ignoró el `Range` y mandó el archivo
    // entero: aceptarlo dejaría pasar sus primeros `expected_len` bytes como si
    // fueran el chunk pedido, o sea audio corrupto en silencio.
    if status != 206 {
        return Err(RangeFetchError::HttpStatus(
            status,
            "el servidor ignoró el rango pedido".to_string(),
        ));
    }

    // `Content-Range: bytes START-END/TOTAL` tiene que empezar en el offset
    // pedido: si no, los bytes recibidos no son los de ese chunk.
    match response
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_range_start)
    {
        Some(start) if start == range_start => {}
        _ => {
            return Err(RangeFetchError::Transient(
                "Content-Range no coincide con el rango pedido".to_string(),
            ));
        }
    }

    let mut body_reader = response.into_body().into_reader();
    let mut buf = Vec::new();
    if buf.try_reserve(expected_len).is_err() {
        return Err(RangeFetchError::Transient("fallo de asignación".to_string()));
    }
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
    buf.truncate(expected_len);
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

                let all_done = state.chunks.iter().all(|c| matches!(c, ChunkSlot::Done(_)));
                if all_done {
                    return;
                }

                let candidate = if let Some(wanted_pos) = state.wanted_offset {
                    let wanted_chunk = (wanted_pos / shared.chunk_size as u64) as usize;
                    if wanted_chunk < state.chunks.len()
                        && matches!(state.chunks[wanted_chunk], ChunkSlot::Missing)
                    {
                        Some(wanted_chunk)
                    } else {
                        None
                    }
                } else {
                    None
                };

                let next_chunk = candidate
                    .or_else(|| state.chunks.iter().position(|c| matches!(c, ChunkSlot::Missing)));

                if let Some(idx) = next_chunk {
                    state.chunks[idx] = ChunkSlot::Downloading;
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

        let expected_len = (range_end - range_start) as usize;
        let result = fetch_range(
            &agent,
            &url,
            &user_agent,
            range_start,
            expected_len,
            &shared.canceled,
        );

        match result {
            Ok(bytes) => {
                retries_for_chunk = 0;
                last_failed_chunk = None;
                if let Ok(mut state) = shared.state.lock() {
                    state.chunks[chunk_idx] = ChunkSlot::Done(bytes);
                    shared.condvar.notify_all();
                }
            }
            Err(RangeFetchError::Canceled) => {
                break;
            }
            Err(RangeFetchError::HttpStatus(code, msg)) => {
                if let Ok(mut state) = shared.state.lock() {
                    state.error = Some(format!("servidor HTTP respondió {code}: {msg}"));
                    state.chunks[chunk_idx] = ChunkSlot::Missing;
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
                        state.chunks[chunk_idx] = ChunkSlot::Missing;
                        shared.condvar.notify_all();
                    }
                    break;
                } else {
                    if let Ok(mut state) = shared.state.lock() {
                        state.chunks[chunk_idx] = ChunkSlot::Missing;
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

/// Abre una URL de audio remoto por HTTP con timeouts configurables. Solo
/// para tests: la producción entra por [`open`].
#[cfg(test)]
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

    if status == 206
        && let Some(total_len) = range_total
        && total_len > 0
        && total_len <= MAX_RANGED_TOTAL
        && usize::try_from(total_len).is_ok()
    {
        let info = StreamInfo {
            content_type,
            content_length: Some(total_len),
        };

        let expected_first_chunk = (chunk_size as u64).min(total_len) as usize;
        let mut initial_chunk = Vec::new();
        if initial_chunk.try_reserve(expected_first_chunk).is_err() {
            return Err(StreamError::Io("fallo de asignación para fragmento inicial".to_string()));
        }
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

        let num_chunks = if chunk_size > 0 {
            total_len.div_ceil(chunk_size as u64) as usize
        } else {
            0
        };

        let mut chunks = vec![ChunkSlot::Missing; num_chunks];
        if num_chunks > 0 {
            chunks[0] = ChunkSlot::Done(initial_chunk);
        }

        let canceled = Arc::new(AtomicBool::new(false));
        let condvar = Arc::new(Condvar::new());
        let shared = Arc::new(RangedShared {
            state: Mutex::new(RangedState {
                chunks,
                wanted_offset: None,
                error: None,
            }),
            condvar: Arc::clone(&condvar),
            canceled: Arc::clone(&canceled),
            total_len,
            chunk_size,
            stall_deadline: timeouts.stall,
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
        let (response, content_type, content_length) = if status == 206 {
            drop(response);
            let resp = agent
                .get(&effective_url)
                .header("User-Agent", &user_agent)
                .header("Accept-Encoding", "identity")
                .call()
                .map_err(|e| match e {
                    ureq::Error::StatusCode(code) => {
                        StreamError::HttpStatus(code, format!("HTTP {code}"))
                    }
                    other => StreamError::Connection(other.to_string()),
                })?;

            let s = resp.status().as_u16();
            if s >= 400 {
                let reason = resp.status().canonical_reason().unwrap_or("Error");
                return Err(StreamError::HttpStatus(s, reason.to_string()));
            }

            let ctype = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            let clen = resp
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok());

            (resp, ctype, clen)
        } else {
            let clen = response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok());
            (response, content_type, clen)
        };

        let info = StreamInfo {
            content_type,
            content_length,
        };

        let (producer, stream) =
            StreamProducer::new_sequential_with_stall(content_length, false, timeouts.stall);
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
            ..HttpTimeouts::default()
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

    /// El modo secuencial no puede bajarse el archivo entero a RAM: con el
    /// lector detenido, el productor tiene que dejar de leer del socket en vez
    /// de acumular. Se observa de punta a punta: si el cliente frena, el
    /// servidor no logra escribir mucho más que el tope de adelanto.
    #[test]
    fn test_sequential_stream_bounds_read_ahead_when_reader_stalls() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // Cuerpo enorme: si el cliente no frena, el servidor lo escribe todo.
        let total: usize = 64 * 1024 * 1024;
        let written = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let written_server = Arc::clone(&written);

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut req_buf = [0u8; 1024];
            let _ = socket.read(&mut req_buf);

            let response =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n");
            if socket.write_all(response.as_bytes()).is_err() {
                return;
            }
            let block = [0x5au8; 32 * 1024];
            let mut sent = 0usize;
            while sent < total {
                let n = (total - sent).min(block.len());
                if socket.write_all(&block[..n]).is_err() {
                    return;
                }
                sent += n;
                written_server.store(sent, Ordering::SeqCst);
            }
        });

        let url = format!("http://127.0.0.1:{port}/stream.mp3");
        let (mut stream, _) = open(&url).expect("open stream");

        // El lector consume apenas un poco y después se queda quieto.
        let mut buf = [0u8; 64 * 1024];
        stream.read_exact(&mut buf).unwrap();
        thread::sleep(Duration::from_millis(500));

        let sent = written.load(Ordering::SeqCst);
        assert!(
            sent < 12 * 1024 * 1024,
            "con el lector detenido el productor siguió bajando: {sent} bytes escritos"
        );
    }

    /// Un stream más grande que el tope de adelanto tiene que poder leerse
    /// completo y correcto: la poda del prefijo ya consumido no puede perder ni
    /// corromper bytes, y el productor no puede quedarse trabado esperando
    /// espacio que nunca llega.
    #[test]
    fn test_sequential_stream_reads_more_than_the_read_ahead_cap() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let total: usize = 10 * 1024 * 1024;

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut req_buf = [0u8; 1024];
            let _ = socket.read(&mut req_buf);

            let response =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n");
            if socket.write_all(response.as_bytes()).is_err() {
                return;
            }
            // Patrón dependiente de la posición, para detectar corrimientos.
            let mut block = vec![0u8; 32 * 1024];
            let mut offset = 0usize;
            while offset < total {
                let n = (total - offset).min(block.len());
                for (j, byte) in block[..n].iter_mut().enumerate() {
                    *byte = ((offset + j) % 251) as u8;
                }
                if socket.write_all(&block[..n]).is_err() {
                    return;
                }
                offset += n;
            }
        });

        let url = format!("http://127.0.0.1:{port}/stream.mp3");
        let (mut stream, _) = open(&url).expect("open stream");

        let mut data = Vec::new();
        stream
            .read_to_end(&mut data)
            .expect("el stream se cortó antes de terminar");

        assert_eq!(data.len(), total, "tiene que llegar el archivo completo");
        for (i, byte) in data.iter().enumerate() {
            assert_eq!(*byte, (i % 251) as u8, "byte {i} incorrecto tras la poda");
        }
    }

    /// El productor puede quedar esperando espacio cuando el lector se detiene:
    /// esa espera tiene que salir también al cancelar. Si no, el hilo de red
    /// quedaría tomado para siempre y el socket abierto.
    #[test]
    fn test_sequential_producer_waiting_for_space_releases_on_cancel() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let total: usize = 64 * 1024 * 1024;
        let saw_cut = Arc::new(AtomicBool::new(false));
        let saw_cut_server = Arc::clone(&saw_cut);

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut req_buf = [0u8; 1024];
            let _ = socket.read(&mut req_buf);

            let response =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n");
            if socket.write_all(response.as_bytes()).is_err() {
                saw_cut_server.store(true, Ordering::SeqCst);
                return;
            }
            let block = [0x5au8; 32 * 1024];
            let mut sent = 0usize;
            while sent < total {
                let n = (total - sent).min(block.len());
                if socket.write_all(&block[..n]).is_err() {
                    saw_cut_server.store(true, Ordering::SeqCst);
                    return;
                }
                sent += n;
            }
        });

        let url = format!("http://127.0.0.1:{port}/stream.mp3");
        let (mut stream, _) = open(&url).expect("open stream");
        let cancel = stream.cancel_handle();

        // Lee un poco: al productor le alcanza para llenar el tope y quedar
        // esperando espacio (100 ms es mucho menos que los 64 MiB que tardaría
        // el servidor en terminar de escribir).
        let mut buf = [0u8; 64 * 1024];
        stream.read_exact(&mut buf).unwrap();
        thread::sleep(Duration::from_millis(100));
        cancel.cancel();

        // Cancelar tiene que despertar al productor y cerrar el socket: el
        // corte de la conexión tiene que llegar en un plazo acotado.
        for _ in 0..40 {
            if saw_cut.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("el productor no se liberó al cancelar: la conexión siguió abierta");
    }

    /// Un servidor que acepta la petición, manda las cabeceras y después deja
    /// de enviar cuerpo deja al lector esperando para siempre. El deadline de
    /// inactividad tiene que hacer fallar la lectura con un error claro en vez
    /// de colgarse hasta que timeoutee el kernel.
    #[test]
    fn test_sequential_reader_times_out_when_server_stalls_after_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut req = [0u8; 1024];
            let _ = socket.read(&mut req);

            // Declara más cuerpo del que envía y después se queda callado.
            let response = "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.write_all(b"pocos bytes");
            let _ = socket.flush();
            thread::sleep(Duration::from_secs(5));
        });

        let url = format!("http://127.0.0.1:{port}/stall.mp3");
        let timeouts = HttpTimeouts {
            stall: Duration::from_millis(200),
            ..HttpTimeouts::default()
        };
        let (mut stream, _info) = open_with(&url, timeouts).unwrap();

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut data = Vec::new();
            let res = stream.read_to_end(&mut data);
            tx.send(res).unwrap();
        });

        let res = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("la lectura debe fallar por deadline, no quedarse colgada");
        let err = res.expect_err("esperaba error de stall");
        assert!(
            err.to_string().contains("dejó de enviar datos"),
            "mensaje inesperado: {err}"
        );
    }

    /// El mismo deadline tiene que cubrir el camino con `Range`: si el
    /// downloader pide un chunk y el servidor corta el cuerpo a mitad, el lector
    /// no puede esperar indefinidamente ese chunk.
    #[test]
    fn test_ranged_reader_times_out_when_server_stalls_mid_chunk() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let total_len = 8192usize;
        let chunk_size = 4096usize;
        let payload: Arc<Vec<u8>> =
            Arc::new((0..total_len).map(|i| (i % 251) as u8).collect());
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
                    let Some(rh) = range_header else { return };
                    let after = rh.split('=').nth(1).unwrap_or("0-0");
                    let parts: Vec<&str> = after.split('-').collect();
                    let start: usize = parts[0].trim().parse().unwrap_or(0);
                    let end: usize = parts[1].trim().parse().unwrap_or(total_len - 1);
                    let resp = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes {start}-{end}/{total_len}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload[start..=end].len()
                    );
                    let _ = socket.write_all(resp.as_bytes());
                    if start == 0 {
                        let _ = socket.write_all(&payload[0..=end]);
                        let _ = socket.flush();
                    } else {
                        // Manda un resto parcial y se queda callado.
                        let _ = socket.write_all(&payload[start..start + 16]);
                        let _ = socket.flush();
                        thread::sleep(Duration::from_secs(5));
                    }
                });
            }
        });

        let url = format!("http://127.0.0.1:{port}/audio.m4a");
        let timeouts = HttpTimeouts {
            stall: Duration::from_millis(200),
            ..HttpTimeouts::default()
        };
        let (mut stream, _) =
            open_with_params(&url, timeouts, chunk_size).expect("open stream");

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            stream.seek(SeekFrom::Start(chunk_size as u64)).unwrap();
            let mut data = Vec::new();
            let res = stream.read_to_end(&mut data);
            tx.send(res).unwrap();
        });

        let res = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("la lectura ranged debe fallar por deadline, no quedarse colgada");
        let err = res.expect_err("esperaba error de stall");
        assert!(
            err.to_string().contains("dejó de enviar datos"),
            "mensaje inesperado: {err}"
        );
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

    /// Un servidor que honra el `Range` del probe (206) y después lo ignora
    /// (200 con el cuerpo completo) no debe hacer pasar bytes del comienzo del
    /// archivo por el chunk pedido. Antes, `fetch_range` aceptaba cualquier
    /// status < 400, truncaba el cuerpo a `expected_len` y esa corrupción
    /// llegaba al decoder en silencio.
    #[test]
    fn test_range_server_ignoring_range_midstream_never_corrupts_data() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let total_len = 3 * 4096;
        let payload: Arc<Vec<u8>> =
            Arc::new((0..total_len).map(|i| (i % 251) as u8).collect());
        let payload_server = Arc::clone(&payload);
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        thread::spawn(move || {
            while let Ok((mut socket, _)) = listener.accept() {
                let payload = Arc::clone(&payload_server);
                let requests = Arc::clone(&requests);
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
                    let Some(rh) = range_header else { return };
                    let after = rh.split('=').nth(1).unwrap_or("0-0");
                    let parts: Vec<&str> = after.split('-').collect();
                    let start: usize = parts[0].trim().parse().unwrap_or(0);
                    let end: usize = parts[1].trim().parse().unwrap_or(total_len - 1);

                    // Solo el primer pedido (el probe) respeta el rango pedido.
                    let is_probe =
                        requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                    if is_probe {
                        let chunk_data = &payload[start..=end];
                        let resp = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes {start}-{end}/{total_len}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            chunk_data.len()
                        );
                        let _ = socket.write_all(resp.as_bytes());
                        let _ = socket.write_all(chunk_data);
                    } else {
                        // Ignora el `Range` y devuelve el archivo entero con 200.
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: audio/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            payload.len()
                        );
                        let _ = socket.write_all(resp.as_bytes());
                        let _ = socket.write_all(&payload);
                    }
                });
            }
        });

        let url = format!("http://127.0.0.1:{port}/audio.m4a");
        let (mut stream, _) =
            open_with_params(&url, HttpTimeouts::default(), 4096).expect("open stream");

        let mut delivered = Vec::new();
        let mut buf = [0u8; 4096];
        let mut failed = false;
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => delivered.extend_from_slice(&buf[..n]),
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }

        // Lo entregado tiene que ser un prefijo exacto del archivo: aceptar el
        // 200 hace repetir los primeros bytes y rompe esta comparación.
        assert!(
            payload.starts_with(&delivered),
            "el stream entregó bytes que no son del archivo ({} bytes entregados)",
            delivered.len()
        );
        // Y no puede cortar sin avisar: eso sería un truncado silencioso.
        assert!(
            failed || delivered.len() == payload.len(),
            "el stream cortó en {} de {} bytes sin reportar error",
            delivered.len(),
            payload.len()
        );
    }

    /// Un 206 que no nace en el offset pedido también es corrupción: si el
    /// `Content-Range` miente sobre dónde empieza el cuerpo, los bytes
    /// recibidos no son los del chunk. El `Content-Range` se valida contra el
    /// offset pedido, no solo su sintaxis.
    #[test]
    fn test_range_server_wrong_content_range_offset_never_corrupts_data() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let total_len = 3 * 4096;
        let payload: Arc<Vec<u8>> =
            Arc::new((0..total_len).map(|i| (i % 251) as u8).collect());
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
                    let has_range = req_str
                        .lines()
                        .any(|l| l.to_ascii_lowercase().starts_with("range: bytes="));
                    if !has_range {
                        return;
                    }

                    // Miente siempre: dice `bytes 0-4095` y manda el chunk 0,
                    // aunque el pedido haya sido otro offset.
                    let body = &payload[0..4096];
                    let resp = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 0-4095/{total_len}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes());
                    let _ = socket.write_all(body);
                });
            }
        });

        let url = format!("http://127.0.0.1:{port}/audio.m4a");
        let (mut stream, _) =
            open_with_params(&url, HttpTimeouts::default(), 4096).expect("open stream");

        let mut delivered = Vec::new();
        let mut buf = [0u8; 4096];
        let mut failed = false;
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => delivered.extend_from_slice(&buf[..n]),
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }

        assert!(
            payload.starts_with(&delivered),
            "el stream entregó bytes que no son del archivo ({} bytes entregados)",
            delivered.len()
        );
        assert!(
            failed || delivered.len() == payload.len(),
            "el stream cortó en {} de {} bytes sin reportar error",
            delivered.len(),
            payload.len()
        );
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

    #[test]
    fn test_huge_content_range_falls_back_to_sequential_unseekable() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let total_bytes = 200 * 1024;
        let mut expected_payload = vec![0u8; total_bytes];
        for (i, b) in expected_payload.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let expected_payload = Arc::new(expected_payload);
        let payload_server = Arc::clone(&expected_payload);

        thread::spawn(move || {
            if let Ok((mut socket1, _)) = listener.accept() {
                let mut req1 = [0u8; 1024];
                let n1 = socket1.read(&mut req1).unwrap_or(0);
                let req_str1 = String::from_utf8_lossy(&req1[..n1]);
                assert!(
                    req_str1.to_ascii_lowercase().contains("range: bytes="),
                    "first request must contain Range header"
                );
                let body1 = b"0123456789abcdef"; // 16 bytes
                let resp1 = "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 0-15/18446744073709551000\r\nContent-Length: 16\r\nConnection: close\r\n\r\n";
                let _ = socket1.write_all(resp1.as_bytes());
                let _ = socket1.write_all(body1);
            }

            if let Ok((mut socket2, _)) = listener.accept() {
                let mut req2 = [0u8; 1024];
                let n2 = socket2.read(&mut req2).unwrap_or(0);
                let req_str2 = String::from_utf8_lossy(&req2[..n2]);
                assert!(
                    !req_str2.to_ascii_lowercase().contains("range:"),
                    "second fallback request must NOT contain Range header"
                );
                let resp2 = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload_server.len()
                );
                let _ = socket2.write_all(resp2.as_bytes());
                let _ = socket2.write_all(&payload_server);
            }
        });

        let url = format!("http://127.0.0.1:{port}/huge.mp3");
        let (mut stream, _) = open_with_params(&url, HttpTimeouts::default(), 16)
            .expect("open stream must succeed in sequential fallback");

        assert!(
            !stream.is_seekable(),
            "stream with huge Content-Range must fall back to unseekable"
        );
        assert_eq!(
            stream.byte_len(),
            None,
            "stream with huge Content-Range must yield byte_len=None"
        );

        let mut data = Vec::new();
        stream.read_to_end(&mut data).expect("read_to_end");
        assert_eq!(data.len(), expected_payload.len());
        assert_eq!(data, *expected_payload);
    }

    #[test]
    fn test_content_range_40gib_falls_back_to_sequential_unseekable() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let total_bytes = 200 * 1024;
        let mut expected_payload = vec![0u8; total_bytes];
        for (i, b) in expected_payload.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let expected_payload = Arc::new(expected_payload);
        let payload_server = Arc::clone(&expected_payload);

        thread::spawn(move || {
            if let Ok((mut socket1, _)) = listener.accept() {
                let mut req1 = [0u8; 1024];
                let n1 = socket1.read(&mut req1).unwrap_or(0);
                let req_str1 = String::from_utf8_lossy(&req1[..n1]);
                assert!(
                    req_str1.to_ascii_lowercase().contains("range: bytes="),
                    "first request must contain Range header"
                );
                let body1 = b"0123456789abcdef"; // 16 bytes
                let resp1 = "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Range: bytes 0-15/42949672960\r\nContent-Length: 16\r\nConnection: close\r\n\r\n";
                let _ = socket1.write_all(resp1.as_bytes());
                let _ = socket1.write_all(body1);
            }

            if let Ok((mut socket2, _)) = listener.accept() {
                let mut req2 = [0u8; 1024];
                let n2 = socket2.read(&mut req2).unwrap_or(0);
                let req_str2 = String::from_utf8_lossy(&req2[..n2]);
                assert!(
                    !req_str2.to_ascii_lowercase().contains("range:"),
                    "second fallback request must NOT contain Range header"
                );
                let resp2 = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload_server.len()
                );
                let _ = socket2.write_all(resp2.as_bytes());
                let _ = socket2.write_all(&payload_server);
            }
        });

        let url = format!("http://127.0.0.1:{port}/large_40gib.mp3");
        let (mut stream, _) = open_with_params(&url, HttpTimeouts::default(), 16)
            .expect("open stream must succeed in sequential fallback for 40 GiB");

        assert!(
            !stream.is_seekable(),
            "stream with 40 GiB Content-Range must fall back to unseekable"
        );
        assert_eq!(
            stream.byte_len(),
            None,
            "stream with 40 GiB Content-Range must yield byte_len=None"
        );

        let mut data = Vec::new();
        stream.read_to_end(&mut data).expect("read_to_end");
        assert_eq!(data.len(), expected_payload.len());
        assert_eq!(data, *expected_payload);
    }

    #[test]
    fn test_merge_chunk_ranges_pure() {
        assert_eq!(merge_chunk_ranges(&[], 100, 200), vec![]);
        assert_eq!(merge_chunk_ranges(&[false, false], 100, 200), vec![]);
        assert_eq!(merge_chunk_ranges(&[true, true], 100, 200), vec![(0, 200)]);
        assert_eq!(
            merge_chunk_ranges(&[true, false, true], 100, 300),
            vec![(0, 100), (200, 300)]
        );
        // Último chunk parcial
        assert_eq!(merge_chunk_ranges(&[true, true], 100, 150), vec![(0, 150)]);
        assert_eq!(
            merge_chunk_ranges(&[false, true], 100, 150),
            vec![(100, 150)]
        );
        // Casos borde
        assert_eq!(merge_chunk_ranges(&[true], 100, 0), vec![]);
        assert_eq!(merge_chunk_ranges(&[true], 0, 100), vec![]);
    }

    #[test]
    fn test_download_ranges_handle_ranged_and_sequential() {
        let (_prod, seq_stream) = StreamProducer::new_sequential(Some(1000), false);
        assert!(seq_stream.download_ranges_handle().is_none());

        // Ranged handle
        let canceled = Arc::new(AtomicBool::new(false));
        let condvar = Arc::new(Condvar::new());
        let shared = Arc::new(RangedShared {
            state: Mutex::new(RangedState {
                chunks: vec![
                    ChunkSlot::Done(vec![0; 100]),
                    ChunkSlot::Missing,
                    ChunkSlot::Done(vec![0; 50]),
                ],
                wanted_offset: None,
                error: None,
            }),
            condvar,
            canceled,
            total_len: 250,
            chunk_size: 100,
            stall_deadline: Duration::from_secs(30),
        });

        let handle = DownloadRangesHandle {
            shared: Arc::clone(&shared),
        };
        let (ranges, total) = handle.downloaded_ranges();
        assert_eq!(total, 250);
        assert_eq!(ranges, vec![(0, 100), (200, 250)]);
    }
}

