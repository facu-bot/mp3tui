#![allow(dead_code)]

//! Motor de audio (T2): decodificación con symphonia, salida con cpal y un ring
//! buffer compartido con muestras PCM para el visualizador FFT (T4).

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase};

const STATE_STOPPED: u8 = 0;
const STATE_PLAYING: u8 = 1;
const STATE_PAUSED: u8 = 2;

const PLAYBACK_BUF_SAMPLES: usize = 1 << 16;
const TAP_BUF_SAMPLES: usize = 1 << 13;
/// Espera máxima, al aplicar un seek, a que el callback vacíe el ring viejo.
const SEEK_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

/// Velocidad de reproducción (T8): multiplicador sobre el paso del resampler,
/// en pasos de 0.25 entre 0.5x y 2.0x.
pub const SPEED_MIN: f32 = 0.5;
pub const SPEED_MAX: f32 = 2.0;
pub const SPEED_STEP: f32 = 0.25;

/// Velocidad válida. Un valor no finito (NaN) volvería el paso del resampler
/// a NaN y el loop de `process` dejaría de avanzar: se sanea a 1.0.
fn clamp_speed(speed: f32) -> f32 {
    if !speed.is_finite() {
        return 1.0;
    }
    speed.clamp(SPEED_MIN, SPEED_MAX)
}

#[derive(Debug)]
pub enum AudioError {
    NoOutputDevice,
    FileNotFound(PathBuf),
    NotAFile(PathBuf),
    UnsupportedFormat(String),
    Decode(String),
    Io(String),
    Device(String),
    UnsupportedSampleFormat(String),
    Http(String),
}

/// Entrada de audio para el motor: archivo local o URL remota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaInput {
    File(PathBuf),
    Url(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::NoOutputDevice => {
                write!(f, "no se encontró un device de salida de audio")
            }
            AudioError::FileNotFound(path) => {
                write!(f, "el archivo no existe: {}", path.display())
            }
            AudioError::NotAFile(path) => write!(f, "no es un archivo: {}", path.display()),
            AudioError::UnsupportedFormat(detail) => {
                write!(f, "formato no soportado o desconocido: {detail}")
            }
            AudioError::Decode(detail) => write!(f, "error al decodificar: {detail}"),
            AudioError::Io(detail) => write!(f, "error de lectura: {detail}"),
            AudioError::Device(detail) => write!(f, "error del device de audio: {detail}"),
            AudioError::UnsupportedSampleFormat(detail) => {
                write!(f, "formato de muestra del device no soportado: {detail}")
            }
            AudioError::Http(detail) => write!(f, "error HTTP: {detail}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// Deduce una extensión recomendada a partir del `Content-Type` de la respuesta
/// o de la extensión en la ruta de la URL para asistir al probe de symphonia.
pub fn extension_hint(url: &str, content_type: Option<&str>) -> Option<&'static str> {
    if let Some(ct) = content_type {
        let mime = ct.split(';').next().unwrap_or("").trim();
        let mime_lower = mime.to_ascii_lowercase();
        match mime_lower.as_str() {
            "audio/mpeg" => return Some("mp3"),
            "audio/mp4" | "audio/m4a" | "audio/x-m4a" | "video/mp4" => return Some("m4a"),
            "audio/aac" => return Some("aac"),
            "audio/ogg" | "application/ogg" => return Some("ogg"),
            "audio/flac" | "audio/x-flac" => return Some("flac"),
            "audio/wav" | "audio/x-wav" | "audio/wave" => return Some("wav"),
            _ => {}
        }
    }

    let path = match url.find(['?', '#']) {
        Some(idx) => &url[..idx],
        None => url,
    };
    let last_segment = path.rsplit('/').next().unwrap_or("");
    if let Some((stem, ext)) = last_segment.rsplit_once('.')
        && !stem.is_empty()
        && !ext.is_empty()
    {
        let ext_lower = ext.to_ascii_lowercase();
        return match ext_lower.as_str() {
            "mp3" => Some("mp3"),
            "m4a" | "mp4" => Some("m4a"),
            "aac" => Some("aac"),
            "ogg" | "oga" | "opus" => Some("ogg"),
            "flac" => Some("flac"),
            "wav" | "wave" => Some("wav"),
            _ => None,
        };
    }

    None
}

/// Construye un `Hint` de symphonia a partir de la URL y el Content-Type reportado.
pub fn hint_from_url_and_content_type(url: &str, content_type: Option<&str>) -> Hint {
    let mut hint = Hint::new();
    if let Some(ext) = extension_hint(url, content_type) {
        hint.with_extension(ext);
    }
    hint
}

/// Consumidor de muestras mono para el visualizador FFT (T4).
pub struct VisualizerTap {
    consumer: Option<HeapCons<f32>>,
    sample_rate: u32,
}

impl VisualizerTap {
    fn new(consumer: HeapCons<f32>, sample_rate: u32) -> Self {
        Self {
            consumer: Some(consumer),
            sample_rate,
        }
    }

    /// Sample rate del device (Hz), válido para mapear las bandas de frecuencia.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn is_empty(&self) -> bool {
        match &self.consumer {
            Some(consumer) => consumer.is_empty(),
            None => true,
        }
    }

    /// Lee hasta `out.len()` muestras mono. Devuelve la cantidad leída.
    pub fn read_mono(&mut self, out: &mut [f32]) -> usize {
        match self.consumer.as_mut() {
            Some(consumer) => consumer.pop_slice(out),
            None => 0,
        }
    }

    /// Descarta todo lo acumulado (se usa tras un seek para no dibujar
    /// espectro de la posición vieja).
    pub fn clear(&mut self) {
        if let Some(consumer) = self.consumer.as_mut() {
            let mut discard = [0f32; 512];
            while consumer.pop_slice(&mut discard) == discard.len() {}
        }
    }
}

struct SessionState {
    /// Identificador de generación/sesión autoritativo. Protegido por el mutex
    /// de sesión; toda mutación de estado dependiente de la sesión (cancel_handle,
    /// is_seekable, error) valida `session.id == session_id` bajo este lock.
    id: u64,
    cancel_handle: Option<crate::source::http_stream::CancelHandle>,
    is_seekable: bool,
    error: Option<AudioError>,
    is_connecting: bool,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHooks {
    pub(crate) pre_publish: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    pub(crate) pre_error: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}

struct Shared {
    state: AtomicU8,
    decoder_done: AtomicBool,
    finished: AtomicBool,
    volume: AtomicU32,
    /// Velocidad (f32 en bits): estado idempotente que lee el hilo
    /// decodificador en cada ciclo, igual que `volume`.
    speed: AtomicU32,
    /// Estado mutable por sesión protegido por mutex. `session.id` es la
    /// fuente autoritativa de verdad para resolver carreras entre sesiones.
    session: Mutex<SessionState>,
    /// Copia atómica de `session.id` utilizada como camino rápido sin bloqueo
    /// (lock-free fast path) para chequeos frecuentes en el hilo URL (`is_current`)
    /// y en el loop de decodificación.
    ///
    /// Seguridad:
    /// 1. Los IDs de sesión crecen de forma estrictamente monótona (con wrapping tras u64::MAX).
    /// 2. Un chequeo atómico con ordenamiento `Acquire` es conservador: si observa
    ///    un ID distinto al de la sesión del hilo, el hilo es definitivamente obsoleto
    ///    y puede abortar de inmediato sin adquirir el lock.
    /// 3. Toda mutación sensible (publicar cancel_handle, cambiar is_seekable o escribir
    ///    errores) re-valida `session.id == session_id` bajo el mutex `session`
    ///    (fuente autoritativa).
    session_id: AtomicU64,
    /// Sticky: la última reproducción falló (apertura, decode o device).
    /// A diferencia de `error` (que se consume con `take_error`), este flag
    /// queda en true hasta el próximo `play()` exitoso, para que la UI
    /// distinga «Detenido por fallo» de «Fin de la pista».
    had_playback_error: AtomicBool,
    /// Seek pedido desde la UI, tomado por el hilo decodificador en su loop.
    seek: Mutex<Option<Duration>>,
    /// Error del último seek (si `format.seek` falló), tomado por la UI.
    seek_error: Mutex<Option<AudioError>>,
    /// El callback (único consumer del ring de reproducción) descarta todo lo
    /// que quedó de la posición vieja; el productor no puede vaciar.
    playback_clear: AtomicBool,
    /// Frames (a la tasa del device) ya sonados: base de `position()`.
    played_frames: AtomicU64,
    /// Duración total de la pista en nanosegundos; 0 = desconocida.
    duration_ns: AtomicU64,
    #[cfg(test)]
    test_hooks: Mutex<TestHooks>,
}

pub struct AudioEngine {
    device: cpal::Device,
    config: cpal::StreamConfig,
    sample_format: SampleFormat,
    shared: Arc<Shared>,
    tap: Arc<Mutex<VisualizerTap>>,
    stream: Option<Stream>,
    decoder: Option<JoinHandle<()>>,
}

impl AudioEngine {
    pub fn new() -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioError::NoOutputDevice)?;
        let supported = device
            .default_output_config()
            .map_err(|err| AudioError::Device(err.to_string()))?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        Ok(Self {
            device,
            config,
            sample_format,
            shared: Arc::new(Shared {
                state: AtomicU8::new(STATE_STOPPED),
                decoder_done: AtomicBool::new(true),
                finished: AtomicBool::new(true),
                volume: AtomicU32::new(1.0f32.to_bits()),
                speed: AtomicU32::new(1.0f32.to_bits()),
                session: Mutex::new(SessionState {
                    id: 0,
                    cancel_handle: None,
                    is_seekable: true,
                    error: None,
                    is_connecting: false,
                }),
                session_id: AtomicU64::new(0),
                had_playback_error: AtomicBool::new(false),
                seek: Mutex::new(None),
                seek_error: Mutex::new(None),
                playback_clear: AtomicBool::new(false),
                played_frames: AtomicU64::new(0),
                duration_ns: AtomicU64::new(0),
                #[cfg(test)]
                test_hooks: Mutex::new(TestHooks::default()),
            }),
            tap: Arc::new(Mutex::new(VisualizerTap {
                consumer: None,
                sample_rate: 0,
            })),
            stream: None,
            decoder: None,
        })
    }

    /// Inicia la reproducción desde una URL remota de audio sin bloquear el hilo actual.
    pub fn play_url(&mut self, url: &str) -> Result<(), AudioError> {
        self.play_input(MediaInput::Url(url.to_string()))
    }

    /// Inicia la reproducción desde una ruta de archivo local.
    pub fn play(&mut self, path: &Path) -> Result<(), AudioError> {
        self.play_input(MediaInput::File(path.to_path_buf()))
    }

    /// Inicia la reproducción desde una entrada de audio (archivo local o URL remota).
    pub fn play_input(&mut self, input: MediaInput) -> Result<(), AudioError> {
        self.stop();

        let is_seekable = match input {
            MediaInput::File(_) => true,
            MediaInput::Url(_) => false,
        };

        let session_id = if let Ok(mut session) = self.shared.session.lock() {
            session.id = session.id.wrapping_add(1);
            self.shared.session_id.store(session.id, Ordering::Release);
            session.error = None;
            session.cancel_handle = None;
            session.is_seekable = is_seekable;
            session.is_connecting = matches!(input, MediaInput::Url(_));
            session.id
        } else {
            0
        };

        // Si el arranque falla en cualquier punto (archivo, device, hilo),
        // el flag queda en true y la UI muestra «Detenido» en vez de decir
        // «Fin de la pista» para algo que nunca arrancó. Se limpia recién
        // cuando todo está listo para sonar.
        self.shared
            .had_playback_error
            .store(true, Ordering::Release);

        let dev_channels = self.config.channels as usize;
        if dev_channels == 0 {
            return Err(AudioError::Device(
                "el device reportó 0 canales".to_string(),
            ));
        }
        let dev_rate = self.config.sample_rate;

        match input {
            MediaInput::File(path) => {
                if !path.exists() {
                    return Err(AudioError::FileNotFound(path));
                }
                if !path.is_file() {
                    return Err(AudioError::NotAFile(path));
                }

                let media = open_media(&path)?;
                let OpenedMedia {
                    format,
                    decoder,
                    track_id,
                    duration,
                    ..
                } = media;

                let (prod, cons) = HeapRb::<f32>::new(PLAYBACK_BUF_SAMPLES).split();
                let (tap_prod, tap_cons) = HeapRb::<f32>::new(TAP_BUF_SAMPLES).split();
                if let Ok(mut tap) = self.tap.lock() {
                    *tap = VisualizerTap::new(tap_cons, dev_rate);
                }

                // Pistas nuevas no heredan seeks ni errores de la anterior.
                if let Ok(mut seek) = self.shared.seek.lock() {
                    *seek = None;
                }
                if let Ok(mut seek_err) = self.shared.seek_error.lock() {
                    *seek_err = None;
                }
                self.shared.playback_clear.store(false, Ordering::Release);
                self.shared.played_frames.store(0, Ordering::Release);
                self.shared
                    .duration_ns
                    .store(duration.map(ns_u64).unwrap_or(0), Ordering::Release);
                self.shared.decoder_done.store(false, Ordering::Release);
                self.shared.finished.store(false, Ordering::Release);
                self.shared.state.store(STATE_PLAYING, Ordering::Release);

                let stream = match self.build_stream(cons, tap_prod) {
                    Ok(stream) => stream,
                    Err(err) => {
                        self.reset_state();
                        return Err(err);
                    }
                };

                let thread_shared = Arc::clone(&self.shared);
                // Último punto sin carrera: el hilo de decode recién va a existir.
                self.shared
                    .had_playback_error
                    .store(false, Ordering::Release);
                let spawned = thread::Builder::new()
                    .name("mp3tui-decoder".to_string())
                    .spawn(move || {
                        decode_loop(
                            format,
                            decoder,
                            track_id,
                            dev_rate,
                            dev_channels,
                            prod,
                            thread_shared,
                            session_id,
                        )
                    });
                let decoder_thread = match spawned {
                    Ok(handle) => handle,
                    Err(err) => {
                        drop(stream);
                        self.reset_state();
                        self.shared
                            .had_playback_error
                            .store(true, Ordering::Release);
                        return Err(AudioError::Device(format!(
                            "no se pudo crear el hilo de decodificación: {err}"
                        )));
                    }
                };

                if let Err(err) = stream.play() {
                    self.shared.state.store(STATE_STOPPED, Ordering::Release);
                    let _ = decoder_thread.join();
                    drop(stream);
                    self.reset_state();
                    self.shared
                        .had_playback_error
                        .store(true, Ordering::Release);
                    return Err(AudioError::Device(err.to_string()));
                }

                self.stream = Some(stream);
                self.decoder = Some(decoder_thread);
                Ok(())
            }
            MediaInput::Url(url) => {
                let (prod, cons) = HeapRb::<f32>::new(PLAYBACK_BUF_SAMPLES).split();
                let (tap_prod, tap_cons) = HeapRb::<f32>::new(TAP_BUF_SAMPLES).split();
                if let Ok(mut tap) = self.tap.lock() {
                    *tap = VisualizerTap::new(tap_cons, dev_rate);
                }

                if let Ok(mut seek) = self.shared.seek.lock() {
                    *seek = None;
                }
                if let Ok(mut seek_err) = self.shared.seek_error.lock() {
                    *seek_err = None;
                }
                self.shared.playback_clear.store(false, Ordering::Release);
                self.shared.played_frames.store(0, Ordering::Release);
                self.shared.duration_ns.store(0, Ordering::Release);
                self.shared.decoder_done.store(false, Ordering::Release);
                self.shared.finished.store(false, Ordering::Release);
                self.shared.state.store(STATE_PLAYING, Ordering::Release);

                let stream = match self.build_stream(cons, tap_prod) {
                    Ok(stream) => stream,
                    Err(err) => {
                        self.reset_state();
                        return Err(err);
                    }
                };

                let thread_shared = Arc::clone(&self.shared);
                self.shared
                    .had_playback_error
                    .store(false, Ordering::Release);
                let spawned = thread::Builder::new()
                    .name("mp3tui-url-decoder".to_string())
                    .spawn(move || {
                        decode_url_pipeline(
                            url,
                            dev_rate,
                            dev_channels,
                            prod,
                            thread_shared,
                            session_id,
                        );
                    });
                if let Err(err) = spawned {
                    drop(stream);
                    self.reset_state();
                    self.shared
                        .had_playback_error
                        .store(true, Ordering::Release);
                    return Err(AudioError::Device(format!(
                        "no se pudo crear el hilo de decodificación: {err}"
                    )));
                }

                if let Err(err) = stream.play() {
                    self.shared.state.store(STATE_STOPPED, Ordering::Release);
                    let cancel = self
                        .shared
                        .session
                        .lock()
                        .ok()
                        .and_then(|mut s| s.cancel_handle.take());
                    if let Some(cancel) = cancel {
                        cancel.cancel();
                    }
                    drop(stream);
                    self.reset_state();
                    self.shared
                        .had_playback_error
                        .store(true, Ordering::Release);
                    return Err(AudioError::Device(err.to_string()));
                }

                self.stream = Some(stream);
                self.decoder = None;
                Ok(())
            }
        }
    }

    pub fn pause(&self) {
        if self.shared.state.load(Ordering::Acquire) == STATE_PLAYING {
            self.shared.state.store(STATE_PAUSED, Ordering::Release);
        }
    }

    pub fn resume(&self) {
        if self.shared.state.load(Ordering::Acquire) == STATE_PAUSED {
            self.shared.state.store(STATE_PLAYING, Ordering::Release);
        }
    }

    pub fn stop(&mut self) {
        let cancel = if let Ok(mut session) = self.shared.session.lock() {
            session.id = session.id.wrapping_add(1);
            self.shared.session_id.store(session.id, Ordering::Release);
            session.is_seekable = false;
            session.is_connecting = false;
            // No se borra `session.error`: el error de una pista que falló debe
            // seguir disponible para que la UI lo drene vía `take_error()`, incluso si
            // se invoca `stop()` antes del drenaje. Se limpia recién al iniciar un nuevo play.
            session.cancel_handle.take()
        } else {
            None
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        self.shared.state.store(STATE_STOPPED, Ordering::Release);
        // Un seek pedido pero no aplicado no debe colarse en la próxima pista.
        if let Ok(mut seek) = self.shared.seek.lock() {
            *seek = None;
        }
        self.stream.take();
        if let Some(handle) = self.decoder.take() {
            let _ = handle.join();
        }
        self.shared.decoder_done.store(true, Ordering::Release);
        self.shared.finished.store(true, Ordering::Release);
    }

    pub fn is_playing(&self) -> bool {
        self.shared.state.load(Ordering::Acquire) == STATE_PLAYING
    }

    /// Indica si el motor está actualmente conectando / abriendo un stream remoto (URL).
    pub fn is_connecting(&self) -> bool {
        self.shared
            .session
            .lock()
            .ok()
            .map(|s| s.is_connecting)
            .unwrap_or(false)
            && self.shared.state.load(Ordering::Acquire) == STATE_PLAYING
    }

    pub fn is_paused(&self) -> bool {
        self.shared.state.load(Ordering::Acquire) == STATE_PAUSED
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.shared.volume.load(Ordering::Relaxed))
    }

    pub fn set_volume(&self, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        self.shared
            .volume
            .store(volume.to_bits(), Ordering::Relaxed);
    }

    /// Velocidad de reproducción vigente (1.0 = normal), en `[SPEED_MIN, SPEED_MAX]`.
    pub fn speed(&self) -> f32 {
        f32::from_bits(self.shared.speed.load(Ordering::Acquire))
    }

    /// Pide cambiar la velocidad: el hilo decodificador la aplica sobre su
    /// resampler en el próximo ciclo (misma arquitectura de pedido que el
    /// seek). A diferencia del seek, la velocidad es estado idempotente y no
    /// un evento único, así que no necesita lock: cambiarla al mismo tiempo
    /// que un seek no puede perderse ni dejar el pipeline inconsistente —
    /// `apply_seek` resetea fracción/buffer del resampler y el próximo ciclo
    /// vuelve a setear la velocidad vigente.
    pub fn set_speed(&mut self, speed: f32) {
        self.shared
            .speed
            .store(clamp_speed(speed).to_bits(), Ordering::Release);
    }

    /// Tiempo transcurrido de la pista actual, contado sobre los frames que ya
    /// salieron al device (se recalibra al instante en cada seek).
    pub fn position(&self) -> Duration {
        if self.is_connecting() {
            return Duration::ZERO;
        }
        let frames = self.shared.played_frames.load(Ordering::Acquire);
        let rate = u64::from(self.config.sample_rate).max(1);
        let nanos = u128::from(frames) * 1_000_000_000 / u128::from(rate);
        Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
    }

    /// Duración total de la pista según los metadatos del formato; `None` si
    /// el contenedor no la reporta.
    pub fn duration(&self) -> Option<Duration> {
        match self.shared.duration_ns.load(Ordering::Acquire) {
            0 => None,
            nanos => Some(Duration::from_nanos(nanos)),
        }
    }

    /// Pide reposicionar la decodificación a `position`. El hilo decodificador
    /// aplica el seek en su próximo ciclo (el `FormatReader` no es seguro
    /// compartirlo desde la UI): aquí solo se deja el pedido y se ajusta la
    /// posición reportada para que la barra salte al instante.
    pub fn seek(&mut self, position: Duration) -> Result<(), AudioError> {
        // El lock serializa con el cierre del hilo decodificador: o el pedido
        // queda visible para que lo consuma el loop, o `decoder_done` ya está
        // en true y el seek se rechaza acá mismo (carrera del fin de pista:
        // sin esto, un seek podía escribirse justo cuando el hilo salía y se
        // perdía sin error, con position() mintiendo por un instante).
        let mut pending = match self.shared.seek.lock() {
            Ok(guard) => guard,
            Err(_) => {
                return Err(AudioError::Decode(
                    "error bloqueando estado de seek".to_string(),
                ));
            }
        };
        if self.stream.is_none() || self.shared.decoder_done.load(Ordering::Acquire) {
            return Err(AudioError::Decode(
                "no hay una pista en reproducción".to_string(),
            ));
        }
        let is_seekable = self
            .shared
            .session
            .lock()
            .ok()
            .map(|s| s.is_seekable)
            .unwrap_or(false);
        if !is_seekable {
            return Err(AudioError::Io(
                "seek no soportado en stream HTTP sin Content-Length".to_string(),
            ));
        }
        let position = self.duration().map_or(position, |max| position.min(max));

        let rate = u64::from(self.config.sample_rate).max(1);
        let nanos = position.as_nanos();
        let frames = (nanos * u128::from(rate) / 1_000_000_000) as u64;
        self.shared.played_frames.store(frames, Ordering::Release);
        // El visualizador no debe seguir dibujando espectro de la posición
        // vieja: se vacía el tap acá (es su único lector, el callback solo
        // empuja). El ring de reproducción lo drena el callback con su flag.
        if let Ok(mut tap) = self.tap.lock() {
            tap.clear();
        }
        *pending = Some(position);
        Ok(())
    }

    /// Indica si la sesión actual soporta seek.
    pub fn is_seekable(&self) -> bool {
        self.shared
            .session
            .lock()
            .ok()
            .map(|s| s.is_seekable)
            .unwrap_or(false)
    }

    /// Error del último seek fallido (si lo hubo), para mostrarlo en la UI.
    pub fn take_seek_error(&self) -> Option<AudioError> {
        self.shared
            .seek_error
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
    }

    /// Handle compartido para leer las muestras mono que suena el device (T4).
    pub fn visualizer(&self) -> Arc<Mutex<VisualizerTap>> {
        Arc::clone(&self.tap)
    }

    /// Bloquea hasta que la pista termine de sonar (o falle). Limpia el playback.
    pub fn wait_until_done(&mut self) -> Result<(), AudioError> {
        loop {
            if let Some(err) = self.take_error() {
                self.stop();
                return Err(err);
            }
            if self.shared.finished.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        let pending = self.take_error();
        self.stop();
        match pending {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Error del último decode/device fallido (si lo hubo), para mostrarlo
    /// en la UI. Se consume una sola vez.
    pub fn take_error(&self) -> Option<AudioError> {
        self.shared
            .session
            .lock()
            .ok()
            .and_then(|mut session| session.error.take())
    }

    /// ¿La última reproducción falló (o falló el arranque)? A diferencia de
    /// `take_error`, no se consume: queda hasta el próximo `play()` exitoso.
    pub fn had_playback_error(&self) -> bool {
        self.shared.had_playback_error.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn set_test_pre_publish(
        &self,
        pair: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    ) {
        if let Ok(mut hooks) = self.shared.test_hooks.lock() {
            hooks.pre_publish = pair;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_test_pre_error(
        &self,
        pair: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    ) {
        if let Ok(mut hooks) = self.shared.test_hooks.lock() {
            hooks.pre_error = pair;
        }
    }

    fn reset_state(&self) {
        self.shared.state.store(STATE_STOPPED, Ordering::Release);
        self.shared.decoder_done.store(true, Ordering::Release);
        self.shared.finished.store(true, Ordering::Release);
    }

    fn build_stream(
        &self,
        cons: HeapCons<f32>,
        tap_prod: HeapProd<f32>,
    ) -> Result<Stream, AudioError> {
        let device = &self.device;
        let config = self.config;
        let shared = Arc::clone(&self.shared);
        match self.sample_format {
            SampleFormat::F32 => build_stream_impl::<f32>(device, config, shared, cons, tap_prod),
            SampleFormat::F64 => build_stream_impl::<f64>(device, config, shared, cons, tap_prod),
            SampleFormat::I8 => build_stream_impl::<i8>(device, config, shared, cons, tap_prod),
            SampleFormat::I16 => build_stream_impl::<i16>(device, config, shared, cons, tap_prod),
            SampleFormat::I24 => {
                build_stream_impl::<cpal::I24>(device, config, shared, cons, tap_prod)
            }
            SampleFormat::I32 => build_stream_impl::<i32>(device, config, shared, cons, tap_prod),
            SampleFormat::I64 => build_stream_impl::<i64>(device, config, shared, cons, tap_prod),
            SampleFormat::U8 => build_stream_impl::<u8>(device, config, shared, cons, tap_prod),
            SampleFormat::U16 => build_stream_impl::<u16>(device, config, shared, cons, tap_prod),
            SampleFormat::U24 => {
                build_stream_impl::<cpal::U24>(device, config, shared, cons, tap_prod)
            }
            SampleFormat::U32 => build_stream_impl::<u32>(device, config, shared, cons, tap_prod),
            SampleFormat::U64 => build_stream_impl::<u64>(device, config, shared, cons, tap_prod),
            other => Err(AudioError::UnsupportedSampleFormat(other.to_string())),
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn build_stream_impl<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: Arc<Shared>,
    mut playback: HeapCons<f32>,
    mut tap: HeapProd<f32>,
) -> Result<Stream, AudioError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    if channels == 0 {
        return Err(AudioError::Device(
            "el device reportó 0 canales".to_string(),
        ));
    }

    let mut scratch: Vec<f32> = Vec::new();
    let mut mono: Vec<f32> = Vec::new();
    let error_shared = Arc::clone(&shared);

    let data_callback = move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
        // Tras un seek: descartar el audio viejo del ring de reproducción.
        // Va antes del check de estado para aplicarse también con la pista
        // pausada, cuando nadie más drena el buffer.
        if shared.playback_clear.swap(false, Ordering::AcqRel) {
            let mut discard = [0f32; 512];
            while playback.pop_slice(&mut discard) == discard.len() {}
        }

        let state = shared.state.load(Ordering::Acquire);
        if state != STATE_PLAYING {
            data.fill(T::EQUILIBRIUM);
            return;
        }

        scratch.resize(data.len(), 0.0);
        let popped = playback.pop_slice(&mut scratch);
        if popped < data.len() {
            scratch[popped..].fill(0.0);
        }

        let frames = data.len() / channels;
        // Lo que sale al device marca el tiempo transcurrido (no lo que el
        // decoder ya encoló): así position() es fiel a lo que se escucha.
        // Con velocidad != 1 se cuenta en frames de CONTENIDO de la pista
        // (frames * speed): a 2x, 10 s reales consumen 20 s de pista, y tanto
        // la barra como los seeks trabajan en esa misma unidad (duration()
        // también es contenido).
        let speed = f64::from(f32::from_bits(shared.speed.load(Ordering::Relaxed)));
        let counted = (frames as f64 * speed).round() as u64;
        shared.played_frames.fetch_add(counted, Ordering::Relaxed);
        mono.resize(frames, 0.0);
        for (frame_index, frame) in scratch.chunks_exact(channels).enumerate() {
            mono[frame_index] = frame.iter().sum::<f32>() / channels as f32;
        }
        tap.push_slice(&mono);

        let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
        for (dst, &sample) in data.iter_mut().zip(scratch.iter()) {
            *dst = T::from_sample((sample * volume).clamp(-1.0, 1.0));
        }

        if shared.decoder_done.load(Ordering::Acquire) && playback.is_empty() {
            shared.state.store(STATE_STOPPED, Ordering::Release);
            shared.finished.store(true, Ordering::Release);
        }
    };

    let error_callback = move |err: cpal::Error| {
        if let Ok(mut session) = error_shared.session.lock() {
            session.error = Some(AudioError::Device(err.to_string()));
        }
        // El device murió en caliente (cambio de salida, sleep): mismo caso
        // que un decode fallido — estado STOPPED y error visible en la TUI,
        // no «Reproduciendo» eterno. El error se escribe primero que el
        // estado por la misma razón que en decode_loop.
        error_shared.state.store(STATE_STOPPED, Ordering::Release);
        error_shared.finished.store(true, Ordering::Release);
        error_shared
            .had_playback_error
            .store(true, Ordering::Release);
    };

    device
        .build_output_stream(config, data_callback, error_callback, None)
        .map_err(|err| AudioError::Device(err.to_string()))
}

struct OpenedMedia {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    /// Duración reportada por el contenedor; `None` si no la conoce.
    duration: Option<Duration>,
    /// Timebase de la pista, para convertir timestamps en segundos (tests).
    time_base: Option<TimeBase>,
}

fn open_media(path: &Path) -> Result<OpenedMedia, AudioError> {
    let file =
        File::open(path).map_err(|err| AudioError::Io(format!("{}: {err}", path.display())))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(&ext.to_ascii_lowercase());
    }

    let format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(map_symphonia_error)?;

    let info = format.media_info();
    // `MediaInfo::duration` viene en ticks del timebase del contenedor.
    let duration = match (info.time_base, info.duration) {
        (Some(time_base), Some(ticks)) => time_base.calc_duration(ticks).and_then(|time| {
            let nanos = time.as_nanos();
            u64::try_from(nanos).ok().map(Duration::from_nanos)
        }),
        _ => None,
    };
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| AudioError::UnsupportedFormat("sin pistas de audio".to_string()))?;
    let track_id = track.id;
    let time_base = track.time_base;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .ok_or_else(|| {
            AudioError::UnsupportedFormat("sin parámetros de codec de audio".to_string())
        })?;

    let decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(map_symphonia_error)?;

    Ok(OpenedMedia {
        format,
        decoder,
        track_id,
        duration,
        time_base,
    })
}

fn map_symphonia_error(err: SymphoniaError) -> AudioError {
    match err {
        SymphoniaError::IoError(io) => AudioError::Io(io.to_string()),
        other => AudioError::UnsupportedFormat(other.to_string()),
    }
}

/// Duración a nanosegundos para los atomics (saturando en u64).
fn ns_u64(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn decode_url_pipeline(
    url: String,
    dev_rate: u32,
    dev_channels: usize,
    prod: HeapProd<f32>,
    shared: Arc<Shared>,
    session_id: u64,
) {
    let is_current = || {
        shared.session_id.load(Ordering::Acquire) == session_id
            && shared.state.load(Ordering::Acquire) != STATE_STOPPED
    };

    if !is_current() {
        return;
    }

    let report_error = |err: AudioError| {
        if let Ok(mut session) = shared.session.lock()
            && session.id == session_id
            && shared.state.load(Ordering::Acquire) != STATE_STOPPED
        {
            session.error = Some(err);
            session.is_connecting = false;
            shared.state.store(STATE_STOPPED, Ordering::Release);
            shared.had_playback_error.store(true, Ordering::Release);
            shared.decoder_done.store(true, Ordering::Release);
            shared.finished.store(true, Ordering::Release);
        }
    };

    let (http_stream, info) = match crate::source::http_stream::open(&url) {
        Ok(res) => res,
        Err(err) => {
            #[cfg(test)]
            {
                let hook = shared
                    .test_hooks
                    .lock()
                    .ok()
                    .and_then(|mut hooks| hooks.pre_error.take());
                if let Some((tx, rx)) = hook {
                    let _ = tx.send(());
                    let _ = rx.recv();
                }
            }

            report_error(AudioError::Http(err.to_string()));
            return;
        }
    };

    let cancel_handle = http_stream.cancel_handle();
    #[cfg(test)]
    {
        let hook = shared
            .test_hooks
            .lock()
            .ok()
            .and_then(|mut hooks| hooks.pre_publish.take());
        if let Some((tx, rx)) = hook {
            let _ = tx.send(());
            let _ = rx.recv();
        }
    }

    {
        let mut session = match shared.session.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if session.id != session_id || shared.state.load(Ordering::Acquire) == STATE_STOPPED {
            cancel_handle.cancel();
            return;
        }
        session.cancel_handle = Some(cancel_handle.clone());
        session.is_seekable = info.content_length.is_some();
    }

    if !is_current() {
        cancel_handle.cancel();
        return;
    }

    let hint = hint_from_url_and_content_type(&url, info.content_type.as_deref());
    let mss = MediaSourceStream::new(Box::new(http_stream), Default::default());

    let format = match symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(map_symphonia_error)
    {
        Ok(fmt) => fmt,
        Err(err) => {
            cancel_handle.cancel();
            report_error(err);
            return;
        }
    };

    if !is_current() {
        cancel_handle.cancel();
        return;
    }

    let media_info = format.media_info();
    let duration = match (media_info.time_base, media_info.duration) {
        (Some(time_base), Some(ticks)) => time_base.calc_duration(ticks).and_then(|time| {
            let nanos = time.as_nanos();
            u64::try_from(nanos).ok().map(Duration::from_nanos)
        }),
        _ => None,
    };
    shared
        .duration_ns
        .store(duration.map(ns_u64).unwrap_or(0), Ordering::Release);

    let track = match format.default_track(TrackType::Audio) {
        Some(t) => t,
        None => {
            cancel_handle.cancel();
            report_error(AudioError::UnsupportedFormat(
                "sin pistas de audio".to_string(),
            ));
            return;
        }
    };
    let track_id = track.id;
    let params = match track.codec_params.as_ref().and_then(|p| p.audio()) {
        Some(p) => p,
        None => {
            cancel_handle.cancel();
            report_error(AudioError::UnsupportedFormat(
                "sin parámetros de codec de audio".to_string(),
            ));
            return;
        }
    };

    let decoder = match symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(map_symphonia_error)
    {
        Ok(dec) => dec,
        Err(err) => {
            cancel_handle.cancel();
            report_error(err);
            return;
        }
    };

    if !is_current() {
        cancel_handle.cancel();
        return;
    }

    if let Ok(mut session) = shared.session.lock()
        && session.id == session_id
    {
        session.is_connecting = false;
        shared.played_frames.store(0, Ordering::Release);
    }

    decode_loop(
        format,
        decoder,
        track_id,
        dev_rate,
        dev_channels,
        prod,
        shared,
        session_id,
    );
}

#[allow(clippy::too_many_arguments)]
fn decode_loop(
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    dev_rate: u32,
    dev_channels: usize,
    prod: HeapProd<f32>,
    shared: Arc<Shared>,
    session_id: u64,
) {
    if let Err(err) = run_decode(
        format,
        decoder,
        track_id,
        dev_rate,
        dev_channels,
        prod,
        &shared,
        session_id,
    ) && let Ok(mut session) = shared.session.lock()
        && session.id == session_id
        && shared.state.load(Ordering::Acquire) != STATE_STOPPED
    {
        session.error = Some(err);
        // Estado honesto: sin esto el state queda PLAYING y la TUI mostraría
        // «Reproduciendo» para siempre mientras el device suena silencio (el
        // error nadie lo leía). El orden importa: el error se escribe ANTES
        // que el estado, así quien lea state = STOPPED ya ve el error listo.
        shared.state.store(STATE_STOPPED, Ordering::Release);
        shared.had_playback_error.store(true, Ordering::Release);
    }
    // Cierre atómico con AudioEngine::seek() (mismo lock): o el último pedido
    // lo tomó el chequeo de fin de pista del loop, o se descarta acá. Si se
    // descarta, la pista terminó antes de poder aplicarlo: se fija la posición
    // en el fin para no reportar un seek que nunca se ejecutó (si el
    // contenedor no reporta duración, se conserva el último valor).
    if shared.session_id.load(Ordering::Acquire) == session_id {
        if let Ok(mut pending) = shared.seek.lock()
            && pending.take().is_some()
        {
            let nanos = shared.duration_ns.load(Ordering::Acquire);
            if nanos > 0 {
                let rate = u64::from(dev_rate).max(1);
                let frames = (u128::from(nanos) * u128::from(rate) / 1_000_000_000) as u64;
                shared.played_frames.store(frames, Ordering::Release);
            }
        }
        shared.decoder_done.store(true, Ordering::Release);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_decode(
    mut format: Box<dyn FormatReader>,
    mut decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    dev_rate: u32,
    dev_channels: usize,
    mut prod: HeapProd<f32>,
    shared: &Shared,
    session_id: u64,
) -> Result<(), AudioError> {
    let mut scratch: Vec<f32> = Vec::new();
    let mut mapped: Vec<f32> = Vec::new();
    let mut resampled: Vec<f32> = Vec::new();
    let mut pipeline: Option<PipelineState> = None;

    loop {
        if shared.session_id.load(Ordering::Acquire) != session_id
            || shared.state.load(Ordering::Acquire) == STATE_STOPPED
        {
            return Ok(());
        }

        // Un seek pedido por la UI se aplica aquí (el FormatReader vive en
        // este hilo) y luego se reanuda el decode desde la nueva posición.
        let pending_seek = shared.seek.lock().ok().and_then(|mut guard| guard.take());
        if let Some(target) = pending_seek {
            apply_seek(
                target,
                &mut format,
                &mut decoder,
                track_id,
                &mut pipeline,
                &mut prod,
                dev_rate,
                shared,
            );
            continue;
        }

        if shared.state.load(Ordering::Acquire) == STATE_PAUSED {
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => {
                // Fin natural: un seek pedido en la ventana entre el chequeo
                // del loop y este punto se aplica acá (la pista reanuda desde
                // la posición pedida). Si ese seek también cae al final, el
                // próximo chequeo lo toma y sale limpio.
                if let Some(target) = shared.seek.lock().ok().and_then(|mut guard| guard.take()) {
                    apply_seek(
                        target,
                        &mut format,
                        &mut decoder,
                        track_id,
                        &mut pipeline,
                        &mut prod,
                        dev_rate,
                        shared,
                    );
                    continue;
                }
                return Ok(());
            }
            Err(SymphoniaError::ResetRequired) => return Ok(()),
            Err(err) => return Err(AudioError::Decode(err.to_string())),
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::IoError(_) | SymphoniaError::DecodeError(_)) => continue,
            Err(err) => return Err(AudioError::Decode(err.to_string())),
        };

        let src_rate = decoded.spec().rate();
        let src_channels = decoded.spec().channels().count();
        if src_rate == 0 || src_channels == 0 {
            continue;
        }

        decoded.copy_to_vec_interleaved(&mut scratch);

        if !matches!(
            &pipeline,
            Some(p) if p.src_rate == src_rate && p.src_channels == src_channels
        ) {
            pipeline = Some(PipelineState::new(
                src_rate,
                src_channels,
                dev_rate,
                dev_channels,
            ));
        }

        // Velocidad variable (T8): la UI escribe `shared.speed` y se aplica
        // acá, en el hilo dueño del resampler. Si la velocidad es != 1 pero
        // src == device no había resampler (rate matching era identidad), se
        // crea con paso base 1.0 para poder aplicar el multiplicador igual.
        let speed = f32::from_bits(shared.speed.load(Ordering::Acquire));
        if let Some(state) = pipeline.as_mut() {
            if state.resampler.is_none() && (speed - 1.0).abs() > f32::EPSILON {
                state.resampler = Some(LinearResampler::new(src_rate, dev_rate, dev_channels));
            }
            if let Some(resampler) = state.resampler.as_mut() {
                resampler.set_speed(speed);
            }
        }

        map_channels(&scratch, src_channels, dev_channels, &mut mapped);

        let output: &[f32] = match pipeline.as_mut().and_then(|p| p.resampler.as_mut()) {
            Some(resampler) => {
                resampler.process(&mapped, &mut resampled);
                &resampled
            }
            None => &mapped,
        };

        if !push_wait(&mut prod, output, dev_channels, shared, session_id) {
            return Ok(());
        }
    }
}

struct PipelineState {
    src_rate: u32,
    src_channels: usize,
    resampler: Option<LinearResampler>,
}

impl PipelineState {
    fn new(src_rate: u32, src_channels: usize, dev_rate: u32, dev_channels: usize) -> Self {
        let resampler = if src_rate == dev_rate {
            None
        } else {
            Some(LinearResampler::new(src_rate, dev_rate, dev_channels))
        };
        Self {
            src_rate,
            src_channels,
            resampler,
        }
    }

    /// Tras un seek, la interpolación y el buffer parcial del resampler ya no
    /// corresponden: se vacían para no arrastrar muestras de la posición vieja.
    fn reset(&mut self) {
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.reset();
        }
    }
}

struct LinearResampler {
    /// Paso base del rate matching (src_rate / dst_rate). El paso efectivo
    /// es `step_base * speed`.
    step_base: f64,
    /// Multiplicador de velocidad (T8), clampeado a `[SPEED_MIN, SPEED_MAX]`.
    speed: f32,
    frac: f64,
    channels: usize,
    buf: Vec<f32>,
}

impl LinearResampler {
    fn new(src_rate: u32, dst_rate: u32, channels: usize) -> Self {
        Self {
            step_base: src_rate as f64 / dst_rate as f64,
            speed: 1.0,
            frac: 0.0,
            channels,
            buf: Vec::new(),
        }
    }

    /// La velocidad multiplica el paso: a 2.0x se consumen ~2 muestras de
    /// entrada por cada muestra de salida (reproduce más rápido sin tocar el
    /// sample rate del device). Es resampling simple, NO time-stretching:
    /// cambia el pitch proporcionalmente (a 2.0x suena una octava arriba) —
    /// limitación conocida y aceptada del diseño, igual que la del drain
    /// timeout del seek.
    fn set_speed(&mut self, speed: f32) {
        self.speed = clamp_speed(speed);
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.buf.extend_from_slice(input);
        out.clear();

        let channels = self.channels;
        let step = self.step_base * f64::from(self.speed);
        let n_frames = self.buf.len() / channels;
        let mut index = 0usize;

        loop {
            while self.frac >= 1.0 && index + 1 < n_frames {
                self.frac -= 1.0;
                index += 1;
            }
            if index + 1 >= n_frames {
                break;
            }

            let s0 = &self.buf[index * channels..(index + 1) * channels];
            let s1 = &self.buf[(index + 1) * channels..(index + 2) * channels];
            for channel in 0..channels {
                let a = s0[channel];
                let b = s1[channel];
                out.push(a + (b - a) * self.frac as f32);
            }
            self.frac += step;
        }

        self.buf.drain(..index * channels);
    }

    /// Deja el resampler como recién creado (sin posición fraccionaria ni
    /// muestras pendientes); lo usan los seeks.
    fn reset(&mut self) {
        self.frac = 0.0;
        self.buf.clear();
    }
}

fn map_channels(src: &[f32], src_channels: usize, dst_channels: usize, out: &mut Vec<f32>) {
    out.clear();
    if src_channels == dst_channels {
        out.extend_from_slice(src);
        return;
    }

    out.reserve(src.len() / src_channels * dst_channels);
    match (src_channels, dst_channels) {
        (1, dst) => {
            for &sample in src {
                for _ in 0..dst {
                    out.push(sample);
                }
            }
        }
        (from, 1) => {
            for frame in src.chunks(from) {
                let sum: f32 = frame.iter().sum();
                out.push(sum / from as f32);
            }
        }
        (from, dst) => {
            for frame in src.chunks(from) {
                for channel in 0..dst {
                    out.push(frame[channel.min(from - 1)]);
                }
            }
        }
    }
}

/// Reposiciona el stream completo: descarta audio viejo (ring y tap),
/// hace el `seek` en el demuxer, resetea decoder y resampler y recalibra el
/// reloj. Si el seek falla, deja el error tomable y sigue en la posición
/// anterior sin cortar la reproducción.
#[allow(clippy::too_many_arguments)]
fn apply_seek(
    target: Duration,
    format: &mut Box<dyn FormatReader>,
    decoder: &mut Box<dyn AudioDecoder>,
    track_id: u32,
    pipeline: &mut Option<PipelineState>,
    prod: &mut HeapProd<f32>,
    dev_rate: u32,
    shared: &Shared,
) {
    // 1. Nada de la posición anterior en el ring de reproducción: se pide el
    //    clear al consumer (el productor no puede vaciar) y se espera a que el
    //    callback lo haga o a que el buffer drene solo.
    //    BACKLOG (carrera improbable, severidad baja): si el callback del
    //    driver se congela los 250 ms del timeout, este seek sigue igual y
    //    empuja muestras nuevas en un ring que aún tiene las viejas: un
    //    instante de audio mezclado en vez de un corte limpio. No hay fix
    //    simple sin un ack del consumer o fallar el seek (más invasivo de lo
    //    que vale para un stall de driver que ya deja audio roto igual).
    shared.playback_clear.store(true, Ordering::Release);
    let drain_deadline = Instant::now() + SEEK_DRAIN_TIMEOUT;
    while !prod.is_empty() && Instant::now() < drain_deadline {
        thread::sleep(Duration::from_millis(1));
    }

    // 2. Reposicionar el demuxer en la posición pedida.
    let seconds = target.as_secs().min(i64::MAX as u64) as i64;
    let time = match Time::try_new(seconds, target.subsec_nanos()) {
        Some(time) => time,
        None => {
            if let Ok(mut seek_err) = shared.seek_error.lock() {
                *seek_err = Some(AudioError::Decode("posición de seek inválida".to_string()));
            }
            return;
        }
    };
    match format.seek(
        SeekMode::Accurate,
        SeekTo::Time {
            time,
            track_id: Some(track_id),
        },
    ) {
        Ok(_) => {}
        Err(SymphoniaError::IoError(io)) => {
            if let Ok(mut seek_err) = shared.seek_error.lock() {
                *seek_err = Some(AudioError::Io(io.to_string()));
            }
            return;
        }
        Err(err) => {
            if let Ok(mut seek_err) = shared.seek_error.lock() {
                *seek_err = Some(AudioError::Decode(err.to_string()));
            }
            return;
        }
    }

    // 3. El decoder no debe emitir muestras del frame anterior al salto.
    decoder.reset();
    // 4. Estado fraccionario del resampler.
    if let Some(state) = pipeline.as_mut() {
        state.reset();
    }
    // 5. El reloj arranca exactamente desde el objetivo (y la UI ya limpió el
    //    tap del visualizador al hacer el pedido de seek).
    let rate = u64::from(dev_rate).max(1);
    let frames = (target.as_nanos() * u128::from(rate) / 1_000_000_000) as u64;
    shared.played_frames.store(frames, Ordering::Release);
}

fn push_wait(
    prod: &mut HeapProd<f32>,
    data: &[f32],
    channels: usize,
    shared: &Shared,
    session_id: u64,
) -> bool {
    let mut pushed = 0usize;
    while pushed < data.len() {
        if shared.session_id.load(Ordering::Acquire) != session_id {
            return false;
        }
        match shared.state.load(Ordering::Acquire) {
            STATE_STOPPED => return false,
            STATE_PAUSED => {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            _ => {}
        }

        let want = data.len() - pushed;
        let free = prod.vacant_len().min(want);
        let chunk = free - (free % channels);
        if chunk == 0 {
            thread::sleep(Duration::from_millis(1));
            continue;
        }

        let n = prod.push_slice(&data[pushed..pushed + chunk]);
        pushed += n;
        if n == 0 {
            thread::sleep(Duration::from_millis(1));
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seek simulado: tras procesar, el resampler arrastra posición
    /// fraccionaria y un buffer parcial; reset() lo deja recién creado.
    #[test]
    fn seek_simulation_resets_resampler_state() {
        let mut state = PipelineState::new(44_100, 2, 48_000, 2);
        let input: Vec<f32> = (0..1024).map(|i| (i as f32 / 64.0).sin()).collect();
        let mut out = Vec::new();

        let resampler = state.resampler.as_mut().unwrap();
        resampler.process(&input, &mut out);
        assert!(!out.is_empty(), "el resampler debe producir muestras");
        assert!(
            !resampler.buf.is_empty(),
            "quedan muestras pendientes del frame anterior al seek"
        );

        state.reset();
        let resampler = state.resampler.as_mut().unwrap();
        assert_eq!(resampler.frac, 0.0, "sin posición fraccionaria");
        assert!(resampler.buf.is_empty(), "sin buffer parcial");
    }

    /// Seek real sobre un wav del sistema (sin device de audio): el demuxer
    /// reposiciona y el decoder, reseteado, entrega packets del objetivo y
    /// sigue decodificando.
    #[test]
    fn seek_repositions_demuxer_and_decoder() {
        let path = Path::new(r"C:\Windows\Media\Ring05.wav");
        if !path.exists() {
            eprintln!("skip: no existe {}", path.display());
            return;
        }
        let open = open_media(path).expect("abrir wav");
        let time_base = open.time_base.expect("wav con timebase");
        let total = open.duration.unwrap_or_else(|| Duration::from_secs(6));
        let target_secs = (total.as_secs_f64() / 2.0).clamp(0.25, 8.0);
        let mut format = open.format;
        let mut decoder = open.decoder;

        // Algunos packets para dejar al decoder con estado previo.
        for _ in 0..5 {
            let Ok(Some(packet)) = format.next_packet() else {
                break;
            };
            let _ = decoder.decode(&packet);
        }

        let time = Time::try_from_nanos_u128((target_secs * 1e9) as u128).unwrap();
        let seeked = format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(open.track_id),
                },
            )
            .expect("seek en wav");
        decoder.reset();

        let packet = format
            .next_packet()
            .expect("leer tras seek")
            .expect("packet tras seek");
        assert_eq!(packet.track_id, open.track_id);
        let landed = time_base
            .calc_time(seeked.actual_ts)
            .expect("convertir actual_ts")
            .as_secs_f64();
        assert!(
            landed >= target_secs - 0.1 && landed <= target_secs + 0.1,
            "el seek no reposiciono: {landed} vs {target_secs}"
        );
        assert!(
            decoder.decode(&packet).is_ok(),
            "el decoder debe seguir decodificando tras reset"
        );
    }

    /// Medición manual del reloj tras un seek (requiere device de audio):
    /// `cargo test seek_position -- --ignored --nocapture`
    #[test]
    #[ignore = "requiere device de audio y un wav de prueba"]
    fn seek_position_matches_target_after_delay() {
        let path = Path::new(
            r"C:\Users\facu\AppData\Local\Temp\opencode\test_music\sub\Cancion Delta.wav",
        );
        if !path.exists() {
            eprintln!("skip: no existe {}", path.display());
            return;
        }
        let mut engine = AudioEngine::new().expect("device de audio");
        engine.play(path).expect("play");
        thread::sleep(Duration::from_millis(700));
        let before = engine.position();
        engine.seek(Duration::from_secs(5)).expect("seek");
        let after = engine.position();
        thread::sleep(Duration::from_millis(350));
        let later = engine.position();
        println!("before={before:?} after_seek={after:?} later={later:?}");
        engine.stop();
        assert!(after >= Duration::from_secs(5) && after < Duration::new(5, 500_000_000));
        assert!(later >= Duration::from_secs(5) && later < Duration::new(5, 700_000_000));
    }

    #[test]
    fn clamp_speed_bounds_and_default() {
        assert_eq!(clamp_speed(1.0), 1.0);
        assert_eq!(clamp_speed(1.25), 1.25);
        assert_eq!(clamp_speed(SPEED_MIN), 0.5);
        assert_eq!(clamp_speed(0.1), 0.5, "por debajo del mínimo se acota");
        assert_eq!(clamp_speed(3.0), 2.0, "por encima del máximo se acota");
        assert_eq!(
            clamp_speed(f32::NAN),
            1.0,
            "NaN vuelve a la velocidad normal"
        );
        assert_eq!(
            clamp_speed(f32::INFINITY),
            1.0,
            "no finito = velocidad normal"
        );
        assert_eq!(clamp_speed(f32::NEG_INFINITY), 1.0);
    }

    /// Con src == dst el paso base es 1.0: la velocidad controla cuánto
    /// consumo de entrada por muestra de salida (y viceversa en la salida).
    #[test]
    fn resampler_speed_controls_input_consumption() {
        let input: Vec<f32> = (0..2000).map(|i| i as f32).collect();

        let mut at_one = LinearResampler::new(48_000, 48_000, 1);
        let mut out_one = Vec::new();
        at_one.process(&input, &mut out_one);
        let base = out_one.len();
        assert!(
            base >= 1990,
            "a 1.0x la salida ~= entrada (obtenido {base})"
        );

        // 2.0x: paso 2 -> ~2 muestras de entrada consumidas por salida:
        // la mitad de muestras de salida para la misma entrada.
        let mut at_two = LinearResampler::new(48_000, 48_000, 1);
        at_two.set_speed(2.0);
        let mut out_two = Vec::new();
        at_two.process(&input, &mut out_two);
        let half = base / 2;
        assert!(
            out_two.len() >= half - 4 && out_two.len() <= half + 4,
            "a 2.0x la salida ~= mitad: base={base} dos={}",
            out_two.len()
        );

        // 0.5x: paso 0.5 -> el doble de muestras de salida.
        let mut at_half = LinearResampler::new(48_000, 48_000, 1);
        at_half.set_speed(0.5);
        let mut out_half = Vec::new();
        at_half.process(&input, &mut out_half);
        assert!(
            out_half.len() >= base * 2 - 4 && out_half.len() <= base * 2 + 4,
            "a 0.5x la salida ~= doble: base={base} half={}",
            out_half.len()
        );

        // Defensa en profundidad: el clampeo llega también al resampler.
        let mut clamped = LinearResampler::new(48_000, 48_000, 1);
        clamped.set_speed(5.0);
        assert_eq!(clamped.speed, SPEED_MAX);
        clamped.set_speed(f32::NAN);
        assert_eq!(clamped.speed, 1.0);
    }

    /// Interacción seek + velocidad: el reset del seek (fracción y buffer)
    /// no debe tocar la velocidad, y el próximo proceso la sigue aplicando.
    #[test]
    fn speed_survives_seek_reset() {
        let mut out = Vec::new();
        let input: Vec<f32> = (0..512).map(|i| (i as f32 / 32.0).sin()).collect();

        let mut resampler = LinearResampler::new(44_100, 44_100, 1);
        resampler.set_speed(1.5);
        resampler.process(&input, &mut out);
        let produced_with_speed = out.len();

        resampler.reset(); // lo que hace apply_seek
        assert_eq!(resampler.speed, 1.5, "reset() no debe tocar la velocidad");
        assert_eq!(resampler.frac, 0.0);
        resampler.process(&input, &mut out);
        assert_eq!(
            out.len(),
            produced_with_speed,
            "con la misma velocidad y entrada, igual de salida"
        );
    }

    /// Duración real con velocidad 2.0x (requiere device): la pista de
    /// ~12.8 s debe terminar en ~6.4 s. `cargo test speed_two -- --ignored --nocapture`
    #[test]
    #[ignore = "requiere device de audio y un wav de prueba"]
    fn speed_two_x_halves_wall_clock_duration() {
        let path = Path::new(
            r"C:\Users\facu\AppData\Local\Temp\opencode\test_music\sub\Cancion Delta.wav",
        );
        if !path.exists() {
            eprintln!("skip: no existe {}", path.display());
            return;
        }
        let mut engine = AudioEngine::new().expect("device de audio");
        engine.play(path).expect("play");
        let total = engine.duration().expect("duración conocida");
        engine.set_speed(2.0);
        assert_eq!(engine.speed(), 2.0);

        let started = Instant::now();
        while engine.is_playing() && started.elapsed() < total {
            thread::sleep(Duration::from_millis(50));
        }
        let elapsed = started.elapsed();
        engine.stop();
        println!("total={total:?} elapsed={elapsed:?} (esperado ~mitad)");

        let half = total / 2;
        // Tolerancias amplias: arranque del decoder y fin por drenaje del ring.
        assert!(
            elapsed >= half.saturating_sub(Duration::from_secs(1))
                && elapsed <= half + Duration::from_secs(2),
            "a 2.0x debería durar ~{half:?} (obtenido {elapsed:?}, total {total:?})"
        );
    }

    #[test]
    fn test_hint_from_url_and_content_type() {
        assert_eq!(
            extension_hint("http://example.com/audio", Some("audio/mpeg")),
            Some("mp3")
        );
        assert_eq!(
            extension_hint("http://example.com/audio", Some("audio/mp4")),
            Some("m4a")
        );
        assert_eq!(
            extension_hint("http://example.com/audio", Some("audio/m4a")),
            Some("m4a")
        );
        assert_eq!(
            extension_hint("http://example.com/audio", Some("audio/x-m4a")),
            Some("m4a")
        );
        assert_eq!(
            extension_hint("http://example.com/audio", Some("video/mp4")),
            Some("m4a")
        );
        assert_eq!(
            extension_hint("http://example.com/audio", Some("audio/aac")),
            Some("aac")
        );
        assert_eq!(
            extension_hint("http://example.com/audio", Some("audio/ogg")),
            Some("ogg")
        );
        assert_eq!(
            extension_hint("http://example.com/audio", Some("application/ogg")),
            Some("ogg")
        );
        assert_eq!(
            extension_hint("http://example.com/audio", Some("audio/flac")),
            Some("flac")
        );
        assert_eq!(
            extension_hint("http://example.com/audio", Some("audio/x-flac")),
            Some("flac")
        );
        assert_eq!(
            extension_hint("http://example.com/audio", Some("audio/wav")),
            Some("wav")
        );
        assert_eq!(
            extension_hint("http://example.com/audio", Some("audio/x-wav")),
            Some("wav")
        );
        assert_eq!(
            extension_hint("http://example.com/audio", Some("audio/wave")),
            Some("wav")
        );
        assert_eq!(
            extension_hint(
                "http://example.com/audio",
                Some("audio/mpeg; charset=utf-8")
            ),
            Some("mp3")
        );
        // URL extension fallback when content-type is unknown
        assert_eq!(
            extension_hint(
                "http://example.com/track.flac?token=123",
                Some("application/octet-stream")
            ),
            Some("flac")
        );
        assert_eq!(
            extension_hint("http://example.com/track.mp3", None),
            Some("mp3")
        );
        assert_eq!(extension_hint("http://example.com/unknown", None), None);

        let hint = hint_from_url_and_content_type("http://example.com/audio.mp3", None);
        assert!(format!("{hint:?}").contains("mp3"));
    }

    #[test]
    fn test_play_url_404_non_blocking_and_reports_error() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            // El servidor demora 500 ms en responder
            thread::sleep(Duration::from_millis(500));
            let response =
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response.as_bytes());
        });

        let url = format!("http://127.0.0.1:{port}/missing.mp3");
        let mut engine = match AudioEngine::new() {
            Ok(engine) => engine,
            Err(_) => {
                eprintln!("skip: sin device de audio cpal en el entorno");
                return;
            }
        };

        let start = Instant::now();
        let play_res = engine.play_url(&url);
        let elapsed = start.elapsed();

        assert!(play_res.is_ok(), "play_url debe retornar Ok inmediatamente");
        assert!(
            elapsed < Duration::from_millis(200),
            "play_url no debe bloquear al llamador, tardó {elapsed:?}"
        );

        // Esperar a que el hilo de fondo complete la conexión demorada y reporte el error
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut err_found = None;
        while Instant::now() < deadline {
            if let Some(err) = engine.take_error() {
                err_found = Some(err);
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        let err = err_found.expect("el error 404 debe llegar por take_error()");
        assert!(
            engine.had_playback_error(),
            "had_playback_error debe estar en true"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("404"),
            "el mensaje de error debe mencionar 404, recibido: {msg}"
        );
    }

    #[test]
    fn test_stop_does_not_clear_unread_playback_error() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let response =
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response.as_bytes());
        });

        let url = format!("http://127.0.0.1:{port}/missing.mp3");
        let mut engine = match AudioEngine::new() {
            Ok(engine) => engine,
            Err(_) => return,
        };

        engine.play_url(&url).expect("play_url debe retornar Ok");

        // Esperar a que el hilo falle con 404
        let deadline = Instant::now() + Duration::from_secs(2);
        while !engine.had_playback_error() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            engine.had_playback_error(),
            "debe registrar fallo de reproducción"
        );

        // Llamamos a stop() ANTES de que la UI drene take_error()
        engine.stop();

        // take_error() debe seguir devolviendo el error no leído
        let err = engine.take_error();
        assert!(
            err.is_some(),
            "stop() no debe descartar el error no leído de la sesión que falló"
        );
        let err_msg = err.unwrap().to_string();
        assert!(
            err_msg.contains("404"),
            "el error debe corresponder al 404, recibido: {err_msg}"
        );
    }

    #[test]
    fn test_new_play_clears_previous_unread_playback_error() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener1 = TcpListener::bind("127.0.0.1:0").unwrap();
        let port1 = listener1.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut socket, _) = listener1.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let response =
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response.as_bytes());
        });

        let wav = make_test_wav(44100 * 2);
        let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let port2 = listener2.local_addr().unwrap().port();
        let server_wav = wav.clone();
        thread::spawn(move || {
            let (mut socket, _) = listener2.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_wav.len()
            );
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.write_all(&server_wav);
        });

        let url_404 = format!("http://127.0.0.1:{port1}/missing.mp3");
        let url_good = format!("http://127.0.0.1:{port2}/good.wav");
        let mut engine = match AudioEngine::new() {
            Ok(engine) => engine,
            Err(_) => return,
        };

        // 1. URL falla con 404
        engine.play_url(&url_404).expect("play_url 404");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !engine.had_playback_error() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(engine.had_playback_error(), "debe registrar fallo 404");

        // 2. Usuario o play-next hace stop() sin drenar take_error()
        engine.stop();

        // 3. Inicia un nuevo play vía URL: debe limpiar el error no leído previo
        engine.play_url(&url_good).expect("play_url good");
        assert!(
            engine.take_error().is_none(),
            "iniciar una nueva reproducción por URL debe limpiar el error no leído previo"
        );

        // Esperar que la buena URL comience a sonar
        thread::sleep(Duration::from_millis(100));
        assert!(
            engine.take_error().is_none(),
            "la nueva pista no debe reportar el error previo"
        );
        engine.stop();

        // 4. Verificamos también con un archivo local (MediaInput::File)
        let listener3 = TcpListener::bind("127.0.0.1:0").unwrap();
        let port3 = listener3.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut socket, _) = listener3.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let response =
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response.as_bytes());
        });
        let url_404_2 = format!("http://127.0.0.1:{port3}/missing2.mp3");
        engine.play_url(&url_404_2).expect("play_url 404 2");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !engine.had_playback_error() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        engine.stop();

        // Escribir archivo WAV temporal para probar engine.play(path)
        let temp_wav_path = std::env::temp_dir().join(format!("mp3tui_test_{}.wav", port3));
        std::fs::write(&temp_wav_path, &wav).expect("escribir wav temporal");
        let play_file_res = engine.play(&temp_wav_path);
        let _ = std::fs::remove_file(&temp_wav_path);

        play_file_res.expect("play archivo local");
        assert!(
            engine.take_error().is_none(),
            "iniciar una nueva reproducción por archivo local debe limpiar el error previo"
        );
        engine.stop();
    }

    fn make_test_wav(num_samples: usize) -> Vec<u8> {
        let sample_rate = 44100u32;
        let channels = 1u16;
        let bits_per_sample = 16u16;
        let byte_rate = sample_rate * (channels as u32) * (bits_per_sample as u32 / 8);
        let block_align = channels * (bits_per_sample / 8);
        let data_len = (num_samples * 2) as u32;
        let riff_len = 36 + data_len;

        let mut wav = Vec::with_capacity(44 + num_samples * 2);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&riff_len.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bits_per_sample.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..num_samples {
            let sample = ((i as f32 / 10.0).sin() * 10000.0) as i16;
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    #[test]
    fn test_url_decode_pipeline_with_generated_wav() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let wav_data = make_test_wav(4410); // 0.1 s de audio
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_wav = wav_data.clone();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_wav.len()
            );
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.write_all(&server_wav);
        });

        let url = format!("http://127.0.0.1:{port}/test.wav");
        let (stream, info) = crate::source::http_stream::open(&url).expect("abrir stream http");
        assert_eq!(info.content_type.as_deref(), Some("audio/wav"));

        let hint = hint_from_url_and_content_type(&url, info.content_type.as_deref());
        let mss = MediaSourceStream::new(Box::new(stream), Default::default());
        let mut format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .expect("probe sobre wav remoto");

        let track = format
            .default_track(TrackType::Audio)
            .expect("pista de audio");
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .expect("codec params");
        let mut decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .expect("decoder");

        let packet = format.next_packet().expect("leer packet").expect("packet");
        let decoded = decoder.decode(&packet).expect("decodificar packet");
        assert!(decoded.frames() > 0, "debe decodificar muestras válidas");
    }

    #[test]
    fn test_seek_unseekable_stream_fails_gracefully() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let wav_data = make_test_wav(44100 * 5); // 5 s de audio
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            // Sin Content-Length (stream no seekable)
            let response =
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.write_all(&wav_data[..4096]);
            thread::sleep(Duration::from_millis(500));
            let _ = socket.write_all(&wav_data[4096..]);
        });

        let url = format!("http://127.0.0.1:{port}/stream.wav");
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        engine.play_url(&url).expect("play_url");
        // Esperar a que la conexión HTTP abra y detecte la ausencia de Content-Length
        let deadline = Instant::now() + Duration::from_secs(1);
        while engine.is_seekable() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        // El seek debe fallar amigablemente con error de no soportado
        let res = engine.seek(Duration::from_secs(1));
        assert!(
            res.is_err(),
            "seek en stream sin Content-Length debe retornar Err"
        );
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("seek no soportado"),
            "mensaje debe indicar seek no soportado, recibido: {err_msg}"
        );
        engine.stop();
    }

    #[test]
    fn test_cancel_on_stop_promptly_terminates_decoder() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let response =
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response.as_bytes());
            // No envía datos adicionales, manteniendo la lectura en espera
            thread::sleep(Duration::from_secs(5));
        });

        let url = format!("http://127.0.0.1:{port}/infinite.wav");
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        engine.play_url(&url).expect("play_url");
        thread::sleep(Duration::from_millis(100));

        let stop_start = Instant::now();
        engine.stop();
        let stop_elapsed = stop_start.elapsed();

        assert!(
            stop_elapsed < Duration::from_millis(300),
            "stop() debe cancelar y unir el hilo prontamente, tardó {stop_elapsed:?}"
        );
    }

    #[test]
    fn test_stop_while_connecting_returns_promptly() {
        use std::io::Read;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            // El servidor no responde durante 3 segundos
            thread::sleep(Duration::from_secs(3));
        });

        let url = format!("http://127.0.0.1:{port}/slow_connect.mp3");
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        engine.play_url(&url).expect("play_url");
        thread::sleep(Duration::from_millis(50));

        let stop_start = Instant::now();
        engine.stop();
        let stop_elapsed = stop_start.elapsed();

        assert!(
            stop_elapsed < Duration::from_millis(300),
            "stop() mientras conecta debe retornar prontamente sin esperar a la red, tardó {stop_elapsed:?}"
        );
    }

    #[test]
    fn test_drop_while_connecting_returns_promptly() {
        use std::io::Read;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            thread::sleep(Duration::from_secs(3));
        });

        let url = format!("http://127.0.0.1:{port}/slow_drop.mp3");
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        engine.play_url(&url).expect("play_url");
        thread::sleep(Duration::from_millis(50));

        let drop_start = Instant::now();
        drop(engine);
        let drop_elapsed = drop_start.elapsed();

        assert!(
            drop_elapsed < Duration::from_millis(300),
            "drop mientras conecta debe retornar prontamente, tardó {drop_elapsed:?}"
        );
    }

    #[test]
    fn test_stale_url_thread_does_not_pollute_new_session() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener1 = TcpListener::bind("127.0.0.1:0").unwrap();
        let port1 = listener1.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener1.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            // Demora 500 ms y luego responde con 404
            thread::sleep(Duration::from_millis(500));
            let response =
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response.as_bytes());
        });

        let wav_data = make_test_wav(44100 * 3);
        let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let port2 = listener2.local_addr().unwrap().port();

        let server_wav = wav_data.clone();
        thread::spawn(move || {
            let (mut socket, _) = listener2.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_wav.len()
            );
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.write_all(&server_wav);
        });

        let slow_url = format!("http://127.0.0.1:{port1}/slow_404.mp3");
        let wav_url = format!("http://127.0.0.1:{port2}/good.wav");

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        engine.play_url(&slow_url).expect("play primer url");
        thread::sleep(Duration::from_millis(50));

        // Cambiamos a la segunda URL: debe cambiar de sesión de inmediato
        let switch_start = Instant::now();
        engine.play_url(&wav_url).expect("play segundo url");
        let switch_elapsed = switch_start.elapsed();
        assert!(
            switch_elapsed < Duration::from_millis(300),
            "cambiar de URL mientras la anterior conecta debe retornar prontamente, tardó {switch_elapsed:?}"
        );

        // Esperar a que la primera URL termine su demora de 500 ms y responda 404
        thread::sleep(Duration::from_millis(600));

        // El error de la primera URL no debe haber contaminado la sesión de la segunda
        assert!(
            engine.take_error().is_none(),
            "el error de la sesión previa no debe aparecer en take_error()"
        );
        assert!(
            !engine.had_playback_error(),
            "had_playback_error no debe marcarse por una sesión previa"
        );
        engine.stop();
    }

    #[test]
    fn test_stale_session_cannot_overwrite_current_cancel_handle() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        // Servidor 1 para URL 1 (sesión vieja)
        let listener1 = TcpListener::bind("127.0.0.1:0").unwrap();
        let port1 = listener1.local_addr().unwrap().port();
        let wav1 = make_test_wav(44100);
        thread::spawn(move || {
            let (mut socket, _) = listener1.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                wav1.len()
            );
            let _ = socket.write_all(resp.as_bytes());
            let _ = socket.write_all(&wav1);
        });

        // Servidor 2 para URL 2 (sesión vigente). Monitorea si el socket sigue vivo o se cancela.
        let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let port2 = listener2.local_addr().unwrap().port();
        let session2_connected = Arc::new(AtomicBool::new(false));
        let session2_cancelled = Arc::new(AtomicBool::new(false));
        let s2_conn = Arc::clone(&session2_connected);
        let s2_canc = Arc::clone(&session2_cancelled);

        let server_wav2 = make_test_wav(44100 * 5);
        thread::spawn(move || {
            let (mut socket, _) = listener2.accept().unwrap();
            s2_conn.store(true, Ordering::SeqCst);
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_wav2.len()
            );
            let _ = socket.write_all(resp.as_bytes());
            let _ = socket.write_all(&server_wav2[..4096]);
            let mut sent = 4096;
            while sent < server_wav2.len() {
                thread::sleep(Duration::from_millis(20));
                let end = (sent + 1024).min(server_wav2.len());
                if socket.write_all(&server_wav2[sent..end]).is_err() {
                    s2_canc.store(true, Ordering::SeqCst);
                    return;
                }
                sent = end;
            }
        });

        let url1 = format!("http://127.0.0.1:{port1}/track1.wav");
        let url2 = format!("http://127.0.0.1:{port2}/track2.wav");

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        engine.set_test_pre_publish(Some((ready_tx, resume_rx)));

        engine.play_url(&url1).expect("play url1");

        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("sesión 1 debe alcanzar pre-publish");

        engine.play_url(&url2).expect("play url2");

        let deadline = Instant::now() + Duration::from_secs(1);
        while !session2_connected.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            session2_connected.load(Ordering::SeqCst),
            "sesión 2 debe haber conectado"
        );

        thread::sleep(Duration::from_millis(100));

        // Liberamos sesión 1: en código con bug sobreescribirá http_cancel
        resume_tx.send(()).unwrap();
        thread::sleep(Duration::from_millis(100));

        engine.stop();

        let cancel_deadline = Instant::now() + Duration::from_millis(400);
        while !session2_cancelled.load(Ordering::SeqCst) && Instant::now() < cancel_deadline {
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            session2_cancelled.load(Ordering::SeqCst),
            "stop() debe cancelar la descarga de la sesión 2 vigente, no dejarla corriendo por haber sido pisada por la sesión 1"
        );
    }

    #[test]
    fn test_stale_404_does_not_pollute_newer_session() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener1 = TcpListener::bind("127.0.0.1:0").unwrap();
        let port1 = listener1.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut socket, _) = listener1.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(resp.as_bytes());
        });

        let wav2 = make_test_wav(44100 * 2);
        let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let port2 = listener2.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut socket, _) = listener2.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                wav2.len()
            );
            let _ = socket.write_all(resp.as_bytes());
            let _ = socket.write_all(&wav2);
        });

        let url1 = format!("http://127.0.0.1:{port1}/404.mp3");
        let url2 = format!("http://127.0.0.1:{port2}/good.wav");

        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        engine.set_test_pre_error(Some((ready_tx, resume_rx)));

        engine.play_url(&url1).expect("play url1");

        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("sesión 1 debe alcanzar pre-error");

        engine.play_url(&url2).expect("play url2");

        thread::sleep(Duration::from_millis(100));

        // Liberamos sesión 1: en código con bug escribirá error y STOPPED
        resume_tx.send(()).unwrap();
        thread::sleep(Duration::from_millis(100));

        assert!(
            engine.take_error().is_none(),
            "el error 404 de la sesión obsoleta no debe contaminar take_error() de la sesión 2"
        );
        assert!(
            !engine.had_playback_error(),
            "had_playback_error no debe marcarse por la sesión 404 obsoleta"
        );
        assert!(
            engine.is_playing(),
            "la sesión 2 debe seguir reproduciendo, no haber sido detenida a STOPPED por la sesión 1"
        );

        engine.stop();
    }

    #[test]
    fn test_seek_during_url_connect_is_rejected_synchronously() {
        use std::io::Read;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            // El servidor no responde de inmediato para mantener la conexión abierta
            thread::sleep(Duration::from_millis(500));
        });

        let url = format!("http://127.0.0.1:{port}/slow_connect.mp3");
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        engine.play_url(&url).expect("play_url");

        // Durante la ventana de conexión, seek() debe ser rechazado sincrónicamente
        // con error de seek no soportado (porque aún no se conoce si el stream es seekable).
        let seek_res = engine.seek(Duration::from_secs(1));
        assert!(
            matches!(seek_res, Err(AudioError::Io(_))),
            "seek durante conexión debe ser rechazado sincrónicamente con AudioError::Io, pero dio: {seek_res:?}"
        );
        engine.stop();
    }

    #[test]
    fn test_position_does_not_advance_while_connecting() {
        use std::io::Read;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            thread::sleep(Duration::from_millis(500));
        });

        let url = format!("http://127.0.0.1:{port}/slow_connect.mp3");
        let mut engine = match AudioEngine::new() {
            Ok(e) => e,
            Err(_) => return,
        };

        engine.play_url(&url).expect("play_url");
        assert!(engine.is_connecting());

        // Wait a bit while connecting so audio callback runs and counts silence frames
        thread::sleep(Duration::from_millis(100));
        assert!(engine.is_connecting());
        assert_eq!(
            engine.position(),
            Duration::ZERO,
            "position must not advance while is_connecting is true"
        );
        engine.stop();
    }
}
