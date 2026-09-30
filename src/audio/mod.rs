#![allow(dead_code)]

//! Motor de audio (T2): decodificación con symphonia, salida con cpal y un ring
//! buffer compartido con muestras PCM para el visualizador FFT (T4).

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
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
use symphonia::core::formats::{
    FormatOptions, FormatReader, SeekMode, SeekTo, TrackType,
};
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
        }
    }
}

impl std::error::Error for AudioError {}

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

struct Shared {
    state: AtomicU8,
    decoder_done: AtomicBool,
    finished: AtomicBool,
    volume: AtomicU32,
    error: Mutex<Option<AudioError>>,
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
                error: Mutex::new(None),
                seek: Mutex::new(None),
                seek_error: Mutex::new(None),
                playback_clear: AtomicBool::new(false),
                played_frames: AtomicU64::new(0),
                duration_ns: AtomicU64::new(0),
            }),
            tap: Arc::new(Mutex::new(VisualizerTap {
                consumer: None,
                sample_rate: 0,
            })),
            stream: None,
            decoder: None,
        })
    }

    pub fn play(&mut self, path: &Path) -> Result<(), AudioError> {
        self.stop();

        if !path.exists() {
            return Err(AudioError::FileNotFound(path.to_path_buf()));
        }
        if !path.is_file() {
            return Err(AudioError::NotAFile(path.to_path_buf()));
        }

        let dev_channels = self.config.channels as usize;
        if dev_channels == 0 {
            return Err(AudioError::Device(
                "el device reportó 0 canales".to_string(),
            ));
        }
        let dev_rate = self.config.sample_rate;

        let media = open_media(path)?;
        let OpenedMedia {
            format,
            decoder,
            track_id,
            duration,
            ..
        } = media;

        let (prod, cons) = HeapRb::<f32>::new(PLAYBACK_BUF_SAMPLES).split();
        let (tap_prod, tap_cons) = HeapRb::<f32>::new(TAP_BUF_SAMPLES).split();
        *self.tap.lock().unwrap() = VisualizerTap::new(tap_cons, dev_rate);

        *self.shared.error.lock().unwrap() = None;
        // Pistas nuevas no heredan seeks ni errores de la anterior.
        *self.shared.seek.lock().unwrap() = None;
        *self.shared.seek_error.lock().unwrap() = None;
        self.shared.playback_clear.store(false, Ordering::Release);
        self.shared.played_frames.store(0, Ordering::Release);
        self.shared
            .duration_ns
            .store(duration.map(ns_u64).unwrap_or(0), Ordering::Release);
        self.shared.decoder_done.store(false, Ordering::Release);
        self.shared.finished.store(false, Ordering::Release);
        self.shared
            .state
            .store(STATE_PLAYING, Ordering::Release);

        let stream = match self.build_stream(cons, tap_prod) {
            Ok(stream) => stream,
            Err(err) => {
                self.reset_state();
                return Err(err);
            }
        };

        let thread_shared = Arc::clone(&self.shared);
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
                )
            });
        let decoder_thread = match spawned {
            Ok(handle) => handle,
            Err(err) => {
                drop(stream);
                self.reset_state();
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
            return Err(AudioError::Device(err.to_string()));
        }

        self.stream = Some(stream);
        self.decoder = Some(decoder_thread);
        Ok(())
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
        self.shared.state.store(STATE_STOPPED, Ordering::Release);
        // Un seek pedido pero no aplicado no debe colarse en la próxima pista.
        *self.shared.seek.lock().unwrap() = None;
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

    /// Tiempo transcurrido de la pista actual, contado sobre los frames que ya
    /// salieron al device (se recalibra al instante en cada seek).
    pub fn position(&self) -> Duration {
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
        let mut pending = self.shared.seek.lock().unwrap();
        if self.stream.is_none() || self.shared.decoder_done.load(Ordering::Acquire) {
            return Err(AudioError::Decode(
                "no hay una pista en reproducción".to_string(),
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

    /// Error del último seek fallido (si lo hubo), para mostrarlo en la UI.
    pub fn take_seek_error(&self) -> Option<AudioError> {
        self.shared.seek_error.lock().unwrap().take()
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

    fn take_error(&self) -> Option<AudioError> {
        self.shared.error.lock().unwrap().take()
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
            SampleFormat::I24 => build_stream_impl::<cpal::I24>(device, config, shared, cons, tap_prod),
            SampleFormat::I32 => build_stream_impl::<i32>(device, config, shared, cons, tap_prod),
            SampleFormat::I64 => build_stream_impl::<i64>(device, config, shared, cons, tap_prod),
            SampleFormat::U8 => build_stream_impl::<u8>(device, config, shared, cons, tap_prod),
            SampleFormat::U16 => build_stream_impl::<u16>(device, config, shared, cons, tap_prod),
            SampleFormat::U24 => build_stream_impl::<cpal::U24>(device, config, shared, cons, tap_prod),
            SampleFormat::U32 => build_stream_impl::<u32>(device, config, shared, cons, tap_prod),
            SampleFormat::U64 => build_stream_impl::<u64>(device, config, shared, cons, tap_prod),
            other => Err(AudioError::UnsupportedSampleFormat(other.to_string())),
        }
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
        shared
            .played_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
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
        *error_shared.error.lock().unwrap() = Some(AudioError::Device(err.to_string()));
        error_shared.finished.store(true, Ordering::Release);
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
    let file = File::open(path)
        .map_err(|err| AudioError::Io(format!("{}: {err}", path.display())))?;
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

fn decode_loop(
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    dev_rate: u32,
    dev_channels: usize,
    prod: HeapProd<f32>,
    shared: Arc<Shared>,
) {
    if let Err(err) = run_decode(
        format,
        decoder,
        track_id,
        dev_rate,
        dev_channels,
        prod,
        &shared,
    ) {
        *shared.error.lock().unwrap() = Some(err);
    }
    // Cierre atómico con AudioEngine::seek() (mismo lock): o el último pedido
    // lo tomó el chequeo de fin de pista del loop, o se descarta acá. Si se
    // descarta, la pista terminó antes de poder aplicarlo: se fija la posición
    // en el fin para no reportar un seek que nunca se ejecutó (si el
    // contenedor no reporta duración, se conserva el último valor).
    let mut pending = shared.seek.lock().unwrap();
    if pending.take().is_some() {
        let nanos = shared.duration_ns.load(Ordering::Acquire);
        if nanos > 0 {
            let rate = u64::from(dev_rate).max(1);
            let frames = (u128::from(nanos) * u128::from(rate) / 1_000_000_000) as u64;
            shared.played_frames.store(frames, Ordering::Release);
        }
    }
    shared.decoder_done.store(true, Ordering::Release);
    drop(pending);
}

fn run_decode(
    mut format: Box<dyn FormatReader>,
    mut decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    dev_rate: u32,
    dev_channels: usize,
    mut prod: HeapProd<f32>,
    shared: &Shared,
) -> Result<(), AudioError> {
    let mut scratch: Vec<f32> = Vec::new();
    let mut mapped: Vec<f32> = Vec::new();
    let mut resampled: Vec<f32> = Vec::new();
    let mut pipeline: Option<PipelineState> = None;

    loop {
        if shared.state.load(Ordering::Acquire) == STATE_STOPPED {
            return Ok(());
        }

        // Un seek pedido por la UI se aplica aquí (el FormatReader vive en
        // este hilo) y luego se reanuda el decode desde la nueva posición.
        let pending_seek = shared.seek.lock().unwrap().take();
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
                if let Some(target) = shared.seek.lock().unwrap().take() {
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

        map_channels(&scratch, src_channels, dev_channels, &mut mapped);

        let output: &[f32] = match pipeline.as_mut().and_then(|p| p.resampler.as_mut()) {
            Some(resampler) => {
                resampler.process(&mapped, &mut resampled);
                &resampled
            }
            None => &mapped,
        };

        if !push_wait(&mut prod, output, dev_channels, shared) {
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
    step: f64,
    frac: f64,
    channels: usize,
    buf: Vec<f32>,
}

impl LinearResampler {
    fn new(src_rate: u32, dst_rate: u32, channels: usize) -> Self {
        Self {
            step: src_rate as f64 / dst_rate as f64,
            frac: 0.0,
            channels,
            buf: Vec::new(),
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.buf.extend_from_slice(input);
        out.clear();

        let channels = self.channels;
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
            self.frac += self.step;
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
            *shared.seek_error.lock().unwrap() = Some(AudioError::Decode(
                "posición de seek inválida".to_string(),
            ));
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
            *shared.seek_error.lock().unwrap() = Some(AudioError::Io(io.to_string()));
            return;
        }
        Err(err) => {
            *shared.seek_error.lock().unwrap() = Some(AudioError::Decode(err.to_string()));
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

fn push_wait(prod: &mut HeapProd<f32>, data: &[f32], channels: usize, shared: &Shared) -> bool {
    let mut pushed = 0usize;
    while pushed < data.len() {
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
}
