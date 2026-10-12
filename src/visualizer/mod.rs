#![allow(dead_code)]

//! Analizador de espectro FFT: ventana Hann, rustfft, bandas logarítmicas
//! estilo cava y suavizado temporal con attack rápido y decay lento.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::audio::VisualizerTap;

/// Cantidad de barras del visualizador estilo cava.
pub const NUM_BANDS: usize = 32;

/// Tamaño de la ventana FFT (potencia de 2). El tap entrega ~700 muestras por
/// frame a 60 fps; con 2048 se analiza ~46 ms de historia por actualización.
const FFT_SIZE: usize = 2048;
/// Frecuencia de la primera banda (Hz): más resolución en graves.
const F_MIN: f32 = 30.0;
/// Tope superior del rango de bandas (Hz).
const F_MAX_CAP: f32 = 16_000.0;
/// Peso del valor nuevo cuando la barra sube (attack rápido).
const ATTACK: f32 = 0.55;
/// Peso del valor nuevo cuando la barra baja (decay más lento).
const DECAY: f32 = 0.16;
/// Caída lineal del pico VU por `analyze()`: de techo a cero en 48 frames
/// (≈ 800 ms a 60 fps). Las barras bajan con el DECAY existente (suavizado
/// attack/decay); solo el pico tiene decay propio.
const PEAK_FALL: f32 = 1.0 / 48.0;
/// Piso de la escala dBFS del medidor VU: por debajo de esto (o en
/// silencio) el nivel mapeado es 0.0.
const VU_FLOOR_DB: f32 = -60.0;
/// Techo de la escala dBFS del medidor VU: 0 dBFS (escala completa) → 1.0.
const VU_CEIL_DB: f32 = 0.0;
/// Tamaño del staging de lectura (igual al ring del tap: 8192 muestras).
const TAP_READ_MAX: usize = 1 << 13;
/// Ganancia previa a la compensación de potencia: la energía real de una
/// banda de música queda muy por debajo del máximo teórico (un seno a
/// escala completa), y sin esto las barras casi nunca se movían.
const GAIN: f32 = 5.0;
/// Exponente de la compensación (< 1 aplana: sube lo bajo sin mandar lo
/// alto directo al clip). 0.4 ≈ raíz suave; con GAIN=5 el clip cae en
/// energy≈0.2 (picos fuertes), y lo típico (0.008–0.05) cae en 28–60%.
const EXPO: f32 = 0.4;
/// Ventana del osciloscopio (modo `onda`) en milisegundos: lo bastante
/// ancha para mostrar la forma de la onda (no un solo ciclo de ruido de
/// alta frecuencia) y lo bastante angosta para que un trazo se vea fluido
/// cuadro a cuadro.
const WAVE_WINDOW_MS: f32 = 22.0;
/// Histéresis del disparador de cruce por cero ascendente del osciloscopio:
/// el nivel debe bajar de `-WAVE_TRIGGER_HYSTERESIS` y volver a subir sobre
/// `+WAVE_TRIGGER_HYSTERESIS` para contar como cruce real, así el ruido que
/// solo tiembla cerca de cero no dispara una y otra vez.
const WAVE_TRIGGER_HYSTERESIS: f32 = 0.02;
/// Attack del pico que alimenta la ganancia automática del osciloscopio:
/// sigue un pico nuevo casi al instante.
const WAVE_GAIN_ATTACK: f32 = 0.5;
/// Release del mismo pico: baja despacio para no parpadear la ganancia
/// entre cuadros silenciosos sueltos.
const WAVE_GAIN_RELEASE: f32 = 0.05;
/// Piso del pico de ganancia del osciloscopio: sin esto, silencio o ruido
/// de piso (pico ≈ 0) dispararía una ganancia enorme (división por casi
/// cero) y amplificaría ese ruido a escala completa.
const WAVE_GAIN_FLOOR: f32 = 0.05;
/// Objetivo de ocupación de la ganancia automática del osciloscopio: una
/// señal cuyo pico suavizado es `WAVE_GAIN_FLOOR..1.0` queda escalada para
/// usar ~80% del alto disponible.
const WAVE_GAIN_TARGET: f32 = 0.8;

pub struct SpectrumAnalyzer {
    fft: Arc<dyn Fft<f32>>,
    fft_scratch: Vec<Complex<f32>>,
    window: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    magnitudes: Vec<f32>,
    ring: Vec<f32>,
    ring_pos: usize,
    filled: usize,
    staging: Vec<f32>,
    pending: usize,
    bands: [f32; NUM_BANDS],
    /// Nivel VU mono para el modo `vu`: nivel dBFS (`db_meter_level` del
    /// RMS) suavizado de la señal del tap actual; ver `update_vu`.
    vu: f32,
    /// Pico del medidor VU: máximo visto del nivel, con decay
    /// `PEAK_FALL`, nunca por debajo del nivel actual.
    vu_peak: f32,
    /// Pico suavizado de |muestra| del osciloscopio (modo `onda`): attack
    /// rápido / release lento, con piso; alimenta la ganancia automática de
    /// `scope_window` (ver `WAVE_GAIN_*`). Se actualiza en `analyze()`, el
    /// render solo lee a través de `scope_window`.
    wave_peak: f32,
    band_ranges: Vec<(usize, usize)>,
    sample_rate: u32,
}

impl SpectrumAnalyzer {
    pub fn new() -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let fft_scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let window = (0..FFT_SIZE)
            .map(|i| {
                let phase = std::f32::consts::TAU * i as f32 / FFT_SIZE as f32;
                0.5 * (1.0 - phase.cos())
            })
            .collect();

        Self {
            fft,
            fft_scratch,
            window,
            spectrum: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            magnitudes: vec![0.0; FFT_SIZE / 2],
            ring: vec![0.0; FFT_SIZE],
            ring_pos: 0,
            filled: 0,
            staging: vec![0.0; TAP_READ_MAX],
            pending: 0,
            bands: [0.0; NUM_BANDS],
            vu: 0.0,
            vu_peak: 0.0,
            wave_peak: 0.0,
            band_ranges: Self::compute_ranges(44_100),
            sample_rate: 0,
        }
    }

    /// Copia las muestras disponibles del tap al buffer circular del analizador.
    /// Pensado para llamarse con el Mutex del tap tomado: solo hace trabajo de
    /// memoria, sin FFT, para no retener el lock que usa el callback de audio.
    pub fn ingest(&mut self, tap: &mut VisualizerTap) {
        let sample_rate = tap.sample_rate();
        if sample_rate > 0 && sample_rate != self.sample_rate {
            self.sample_rate = sample_rate;
            self.band_ranges = Self::compute_ranges(sample_rate);
        }

        let read = tap.read_mono(&mut self.staging);
        if read == 0 {
            return;
        }
        for &sample in &self.staging[..read] {
            self.ring[self.ring_pos] = sample;
            self.ring_pos = (self.ring_pos + 1) % FFT_SIZE;
            if self.filled < FFT_SIZE {
                self.filled += 1;
            }
        }
        self.pending += read;
    }

    /// Analiza las muestras acumuladas (ventana Hann + FFT + bandas log con
    /// attack/decay). Se llama sin lock: no toca el tap.
    pub fn analyze(&mut self) {
        let has_new_samples = self.pending > 0;
        self.pending = 0;

        if !has_new_samples || self.filled < FFT_SIZE || self.sample_rate == 0 {
            for band in &mut self.bands {
                *band *= 1.0 - DECAY;
            }
            self.update_vu(None);
            self.update_wave_gain();
            return;
        }

        let base = self.ring_pos;
        for (index, item) in self.spectrum.iter_mut().enumerate() {
            let sample = self.ring[(base + index) % FFT_SIZE];
            *item = Complex::new(sample * self.window[index], 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.spectrum, &mut self.fft_scratch);

        let n_bins = FFT_SIZE / 2;
        for (index, complex) in self.spectrum.iter().enumerate().take(n_bins) {
            self.magnitudes[index] = complex.norm();
        }

        // Un seno puro de amplitud A concentra |X| = A * N/2 * media(Hann)
        // = A * N/4 en un bin. La SUMA de la banda sobre ese factor devuelve
        // A sin importar cuántos bins tenga la banda; con la media anterior,
        // cada banda se diluía en 1/k y las bandas anchas (agudos) quedaban
        // casi muertas — el problema de «las barras se ven chicas».
        let norm = FFT_SIZE as f32 / 4.0;
        for (band, &(low, high)) in self.band_ranges.iter().enumerate() {
            let low = low.min(n_bins - 1);
            let high = high.max(low + 1).min(n_bins);
            let slice = &self.magnitudes[low..high];
            let energy = slice.iter().sum::<f32>() / norm;
            let target = shape(energy);

            let current = self.bands[band];
            self.bands[band] = if target > current {
                current + (target - current) * ATTACK
            } else {
                current + (target - current) * DECAY
            };
        }
        let rms = self.rms_level();
        self.update_vu(Some(rms));
        self.update_wave_gain();
    }

    /// Atajo: ingest + analyze. El loop de la TUI usa ingest/analyze por
    /// separado para no sostener el lock del tap durante la FFT.
    pub fn update(&mut self, tap: &mut VisualizerTap) {
        self.ingest(tap);
        self.analyze();
    }

    /// Magnitudes por banda normalizadas en 0.0..=1.0.
    pub fn bands(&self) -> &[f32; NUM_BANDS] {
        &self.bands
    }

    /// Nivel del medidor VU (modo `vu`): nivel dBFS del RMS mapeado a
    /// 0.0..=1.0 (ver `db_meter_level`), suavizado con attack/decay.
    pub fn vu(&self) -> f32 {
        self.vu
    }

    /// Pico del medidor VU (modo `vu`): máximo visto del nivel, en
    /// 0.0..=1.0. Siempre >= el nivel actual.
    pub fn vu_peak(&self) -> f32 {
        self.vu_peak
    }

    /// RMS lineal del ring mono actual (0.0..=1.0, sin mapear a dBFS):
    /// raíz de la media cuadrática de las muestras acumuladas; sin
    /// historia devuelve 0. Alimenta `db_meter_level` en `update_vu`.
    fn rms_level(&self) -> f32 {
        let take = self.filled.min(FFT_SIZE);
        if take == 0 {
            return 0.0;
        }
        let sum: f32 = self.ring[..take].iter().map(|sample| sample * sample).sum();
        (sum / take as f32).sqrt()
    }

    /// Actualiza el nivel y el pico del modo `vu` a partir del RMS, en
    /// escala dBFS (ver `db_meter_level`) y con el suavizado attack/decay de
    /// las bandas. El pico sube en el acto y baja con `PEAK_FALL`, sin quedar
    /// nunca debajo del nivel. La fuente ya es mono (el tap promedia en el
    /// callback de audio).
    fn update_vu(&mut self, target: Option<f32>) {
        match target {
            Some(rms) => {
                let level = db_meter_level(rms);
                self.vu +=
                    (level - self.vu) * if level > self.vu { ATTACK } else { DECAY };
            }
            None => {
                // Sin muestras nuevas el RMS del ring sería rancio: el nivel
                // decae como las bandas.
                self.vu *= 1.0 - DECAY;
            }
        }
        if self.vu >= self.vu_peak {
            self.vu_peak = self.vu;
        } else {
            self.vu_peak = (self.vu_peak - PEAK_FALL).max(self.vu);
        }
    }

    /// Ventana estable del osciloscopio (modo `onda`): busca un cruce por
    /// cero ascendente reciente en la historia (con histéresis, para
    /// ignorar ruido) y devuelve ~20–25 ms de muestras a partir de ahí, de
    /// más vieja a más nueva (la más nueva queda a la derecha del
    /// osciloscopio). Sin cruce, cae a la ventana más reciente disponible.
    /// Ya viene con la ganancia automática aplicada y clampeada a
    /// `[-1, 1]` (ver `wave_gain`); el render solo lee, sin reescalar.
    pub fn scope_window(&self) -> Vec<f32> {
        let raw = self.trigger_window();
        if raw.is_empty() {
            return raw;
        }
        let gain = self.wave_gain();
        raw.into_iter().map(|s| (s * gain).clamp(-1.0, 1.0)).collect()
    }

    /// Cantidad de muestras de la ventana del osciloscopio para la
    /// frecuencia de muestreo actual (`WAVE_WINDOW_MS`), acotada por la
    /// historia disponible en el ring. Con frecuencia de muestreo
    /// desconocida (0, antes del primer `ingest`) asume 44.1 kHz.
    fn wave_window_samples(&self) -> usize {
        let rate = if self.sample_rate > 0 {
            self.sample_rate as f32
        } else {
            44_100.0
        };
        let samples = ((rate * WAVE_WINDOW_MS / 1000.0).round() as usize).max(1);
        samples.min(FFT_SIZE)
    }

    /// Muestra a `back` posiciones de la más reciente escrita (`back == 1`
    /// es la última muestra que entró al ring; `back == filled` es la más
    /// vieja que queda en la historia). No valida `back`: quien llama
    /// garantiza `1 <= back <= filled.min(FFT_SIZE)`.
    fn sample_back(&self, back: usize) -> f32 {
        self.ring[(self.ring_pos + FFT_SIZE - back) % FFT_SIZE]
    }

    /// Busca, dentro de la historia disponible, un cruce por cero
    /// ascendente con histéresis (bajó de `-WAVE_TRIGGER_HYSTERESIS` y
    /// volvió a subir sobre `+WAVE_TRIGGER_HYSTERESIS`) que deje al menos
    /// `window` muestras por delante hasta `margin` muestras antes de la
    /// más reciente (así la ventana devuelta termina ANTES de lo más
    /// nuevo, dejando lugar para que el disparador tenga margen de
    /// búsqueda). Devuelve el `back` de la muestra del cruce (el extremo
    /// más viejo de la ventana), o `None` si no hay un cruce así.
    fn find_rising_crossing(&self, window: usize, margin: usize, available: usize) -> Option<usize> {
        let search_start = margin + window;
        if search_start > available.saturating_sub(1) {
            return None;
        }
        let search_end = available - 1;
        let mut armed = false;
        // De más vieja a más nueva dentro del rango permitido, así
        // "armada" tiene sentido cronológico (bajó, y DESPUÉS subió).
        for back in (search_start..=search_end).rev() {
            let value = self.sample_back(back);
            if value <= -WAVE_TRIGGER_HYSTERESIS {
                armed = true;
            } else if armed && value >= WAVE_TRIGGER_HYSTERESIS {
                return Some(back);
            }
        }
        None
    }

    /// Ventana de `window` muestras cronológicas (más vieja primero)
    /// terminando en `start_back - window + 1` (en `back`), empezando en
    /// `start_back`.
    fn window_from_start(&self, start_back: usize, window: usize) -> Vec<f32> {
        (0..window).map(|i| self.sample_back(start_back - i)).collect()
    }

    /// Ventana cruda (sin ganancia) del osciloscopio: triggered cuando hay
    /// margen e historia suficiente para buscar un cruce; si no, la
    /// ventana más reciente disponible (con menos historia que `window`
    /// devuelve la que haya, sin pánico).
    fn trigger_window(&self) -> Vec<f32> {
        let available = self.filled.min(FFT_SIZE);
        if available == 0 {
            return Vec::new();
        }
        let window = self.wave_window_samples().min(available);
        if window == 0 {
            return Vec::new();
        }
        let margin = window.min(available.saturating_sub(window));
        if margin > 0
            && let Some(start_back) = self.find_rising_crossing(window, margin, available)
        {
            return self.window_from_start(start_back, window);
        }

        self.window_from_start(window, window)
    }

    /// Ganancia automática del osciloscopio: `WAVE_GAIN_TARGET` dividido
    /// por el pico suavizado actual (con piso `WAVE_GAIN_FLOOR`), así una
    /// señal floja crece hasta usar ~80% del alto y el silencio (pico 0,
    /// multiplicado por lo que sea) sigue siendo silencio.
    fn wave_gain(&self) -> f32 {
        WAVE_GAIN_TARGET / self.wave_peak.max(WAVE_GAIN_FLOOR)
    }

    /// Actualiza el pico suavizado que alimenta `wave_gain`: toma el pico
    /// de |muestra| en la ventana más reciente disponible (no
    /// necesariamente la triggered, para no atarse a si hubo o no cruce) y
    /// lo sigue con attack rápido / release lento.
    fn update_wave_gain(&mut self) {
        let available = self.filled.min(FFT_SIZE);
        let window = self.wave_window_samples().min(available);
        let mut peak = 0.0f32;
        for back in 1..=window {
            let value = self.sample_back(back).abs();
            if value > peak {
                peak = value;
            }
        }
        self.wave_peak += (peak - self.wave_peak)
            * if peak > self.wave_peak {
                WAVE_GAIN_ATTACK
            } else {
                WAVE_GAIN_RELEASE
            };
    }

    /// Mapeo logarítmico de frecuencias a bins de la FFT (bordes compartidos
    /// entre bandas, sin huecos), como hace cava.
    fn compute_ranges(sample_rate: u32) -> Vec<(usize, usize)> {
        let n_bins = FFT_SIZE / 2;
        let nyquist = sample_rate as f32 / 2.0;
        let f_max = nyquist.clamp(F_MIN * 4.0, F_MAX_CAP);
        let ratio = f_max / F_MIN;

        let mut edges = Vec::with_capacity(NUM_BANDS + 1);
        for band in 0..=NUM_BANDS {
            let freq = F_MIN * ratio.powf(band as f32 / NUM_BANDS as f32);
            let bin = ((freq * FFT_SIZE as f32) / sample_rate as f32).round() as usize;
            edges.push(bin.clamp(1, n_bins));
        }
        for index in 1..edges.len() {
            if edges[index] <= edges[index - 1] {
                edges[index] = edges[index - 1] + 1;
            }
        }

        edges
            .windows(2)
            .map(|pair| {
                let low = pair[0].min(n_bins - 1);
                let high = pair[1].max(low + 1).min(n_bins);
                (low, high)
            })
            .collect()
    }
}

impl Default for SpectrumAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// Energía de banda → altura 0..=1: ganancia fija + compresión de potencia,
/// con clip en 1.0. Objetivo: con música real a volumen normal las
/// barras usan un rango amplio de la altura disponible y los picos siguen
/// notándose (llegan al tope, no todo el tiempo).
fn shape(energy: f32) -> f32 {
    (energy * GAIN).powf(EXPO).min(1.0)
}

/// RMS lineal (0.0..=1.0) → nivel del medidor VU en 0.0..=1.0, mapeando
/// dBFS (`20·log10(rms)`) linealmente entre `VU_FLOOR_DB` y `VU_CEIL_DB`,
/// con clamp en ambos extremos. Silencio y cualquier RMS por debajo de
/// 1e-6 dan 0.0 directo (evita `log10(0) == -inf` y NaN).
fn db_meter_level(rms: f32) -> f32 {
    if !rms.is_finite() || rms <= 1e-6 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db - VU_FLOOR_DB) / (VU_CEIL_DB - VU_FLOOR_DB)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Osciloscopio triggered con ganancia
    // automática (ver `scope_window`/`trigger_window`/`wave_gain`).
    // ------------------------------------------------------------------

    fn fill_sine(analyzer: &mut SpectrumAnalyzer, amplitude: f32, freq: f32, ring_pos: usize) {
        analyzer.sample_rate = 44_100;
        analyzer.filled = FFT_SIZE;
        analyzer.ring_pos = ring_pos;
        for i in 0..FFT_SIZE {
            let t = i as f32 / 44_100.0;
            analyzer.ring[i] = amplitude * (std::f32::consts::TAU * freq * t).sin();
        }
    }

    #[test]
    fn trigger_window_spans_about_20_to_25_ms_at_44_1khz_not_width_samples() {
        let mut analyzer = SpectrumAnalyzer::new();
        fill_sine(&mut analyzer, 1.0, 440.0, 0);
        let window = analyzer.trigger_window();
        let len = window.len();
        assert!(
            (882..=1_103).contains(&len),
            "~20-25 ms a 44.1 kHz (882-1103 muestras), obtenido {len} (NO `width` muestras)"
        );
    }

    #[test]
    fn trigger_window_stabilizes_phase_across_write_offsets() {
        // La misma señal continua, vista desde tres punteros de escritura
        // distintos: el disparador debe arrancar cerca de un cruce por
        // cero ascendente las tres veces, no en una fase arbitraria que
        // dependa de dónde esté `ring_pos`.
        let mut first_sample: Option<f32> = None;
        for &ring_pos in &[0usize, 777, 1_500] {
            let mut analyzer = SpectrumAnalyzer::new();
            fill_sine(&mut analyzer, 1.0, 440.0, ring_pos);
            let window = analyzer.trigger_window();
            assert!(window.len() >= 2, "ring_pos={ring_pos}");
            assert!(
                (0.0..0.1).contains(&window[0]),
                "arranca cerca de un cruce ascendente (>= 0, chico), ring_pos={ring_pos}, obtenido {}",
                window[0]
            );
            assert!(
                window[1] >= window[0],
                "sigue subiendo justo después del cruce, ring_pos={ring_pos}"
            );
            if let Some(previous) = first_sample {
                assert!(
                    (window[0] - previous).abs() < 0.15,
                    "misma fase entre punteros distintos, ring_pos={ring_pos}"
                );
            }
            first_sample = Some(window[0]);
        }
    }

    #[test]
    fn scope_window_auto_gain_lifts_quiet_signal_to_large_share_of_range() {
        let mut analyzer = SpectrumAnalyzer::new();
        fill_sine(&mut analyzer, 0.1, 440.0, 0);
        // Ganancia ya convergida para esta señal floja (amplitud 0.1):
        // `wave_gain` = WAVE_GAIN_TARGET / 0.1 = 8x.
        analyzer.wave_peak = 0.1;
        let window = analyzer.scope_window();
        assert!(!window.is_empty());
        let max_abs = window.iter().cloned().fold(0.0f32, |acc, v| acc.max(v.abs()));
        assert!(
            max_abs > 0.6,
            "una señal floja debe usar buena parte del alto disponible, obtenido {max_abs}"
        );
        assert!(max_abs <= 1.0, "clamp a [-1,1], obtenido {max_abs}");
    }

    #[test]
    fn scope_window_silence_stays_flat_without_noise_blowup() {
        // Silencio real: el ring queda en ceros y `wave_peak` arranca en
        // 0.0 (sin historia). Aunque el piso de ganancia evita dividir por
        // cero, 0 multiplicado por cualquier ganancia sigue siendo 0.
        let mut silent = SpectrumAnalyzer::new();
        silent.sample_rate = 44_100;
        silent.filled = FFT_SIZE;
        let window = silent.scope_window();
        assert!(!window.is_empty());
        for &sample in &window {
            assert!(
                sample.abs() < 1e-6,
                "silencio debe quedar plano, obtenido {sample}"
            );
        }

        // Ruido de piso diminuto (muy por debajo de la histéresis del
        // disparador): con la ganancia máxima posible (piso de ganancia,
        // pico sin converger) no debe dispararse a escala completa.
        let mut noisy = SpectrumAnalyzer::new();
        noisy.sample_rate = 44_100;
        noisy.filled = FFT_SIZE;
        for (i, slot) in noisy.ring.iter_mut().enumerate() {
            *slot = if i % 2 == 0 { 0.001 } else { -0.001 };
        }
        let window = noisy.scope_window();
        let max_abs = window.iter().cloned().fold(0.0f32, |acc, v| acc.max(v.abs()));
        assert!(
            max_abs < 0.1,
            "ruido de piso no debe amplificarse a escala completa, obtenido {max_abs}"
        );
    }

    #[test]
    fn scope_window_handles_short_history_and_empty_state_without_panic() {
        let fresh = SpectrumAnalyzer::new();
        assert!(fresh.scope_window().is_empty(), "sin ingest, sin pánico");

        let mut short = SpectrumAnalyzer::new();
        short.sample_rate = 44_100;
        short.filled = 10; // mucho menos que la ventana típica (~970 muestras)
        short.ring_pos = 10;
        for slot in short.ring.iter_mut().take(10) {
            *slot = 0.5;
        }
        short.wave_peak = 0.5; // ganancia ≈ 1.6x, sin clampear raro
        let window = short.scope_window();
        assert_eq!(
            window.len(),
            10,
            "con poca historia devuelve lo que hay, sin pánico"
        );
        assert!(
            window.iter().all(|&s| (s - 0.8).abs() < 1e-5),
            "0.5 * (0.8/0.5) = 0.8 en toda la ventana corta, obtenido {window:?}"
        );
    }

    // Medidor VU MONO con RMS de la señal, mapeado
    // a una escala dBFS (ver `db_meter_level`).
    // ------------------------------------------------------------------

    #[test]
    fn db_meter_level_maps_known_dbfs_anchors() {
        // -60 dBFS (piso) y silencio → 0.0; 0 dBFS (techo) → 1.0; -3 dBFS
        // (seno a escala completa) → ~0.95; -12 dBFS → ~0.8.
        assert_eq!(db_meter_level(0.0), 0.0, "silencio");
        assert!(
            db_meter_level(0.001).abs() < 1e-3,
            "-60 dBFS exacto es el piso, obtenido {}",
            db_meter_level(0.001)
        );
        let full_scale_sine = std::f32::consts::FRAC_1_SQRT_2; // ≈ 0.707
        assert!(
            (db_meter_level(full_scale_sine) - 0.95).abs() < 0.01,
            "seno a escala completa (-3 dBFS) ≈ 0.95, obtenido {}",
            db_meter_level(full_scale_sine)
        );
        assert!(
            (db_meter_level(0.251) - 0.8).abs() < 0.01,
            "-12 dBFS ≈ 0.8, obtenido {}",
            db_meter_level(0.251)
        );
        assert!(
            (db_meter_level(1.0) - 1.0).abs() < 1e-5,
            "0 dBFS es el techo, obtenido {}",
            db_meter_level(1.0)
        );
    }

    #[test]
    fn db_meter_level_clamps_and_never_panics_or_nans() {
        // Por encima de 0 dBFS (rms > 1.0, no pasa en la práctica pero no
        // debe romper): clamp al techo.
        assert_eq!(db_meter_level(2.0), 1.0, "sobre el techo: clamp a 1.0");
        // Silencio y RMS por debajo del umbral: siempre 0.0, nunca NaN/-inf
        // (log10(0) sería -inf sin el corte explícito).
        for rms in [0.0f32, 1e-7, 1e-6, -0.0f32] {
            let level = db_meter_level(rms);
            assert!(level.is_finite(), "rms={rms} dio {level}");
            assert_eq!(level, 0.0, "rms={rms} por debajo del umbral");
        }
    }

    #[test]
    fn vu_starts_silent() {
        let analyzer = SpectrumAnalyzer::new();
        assert_eq!(analyzer.vu(), 0.0, "sin señal el medidor arranca en cero");
        assert_eq!(analyzer.vu_peak(), 0.0, "sin señal no hay pico");
    }

    #[test]
    fn vu_is_a_single_mono_level_from_current_tap() {
        // El tap ya promedia a MONO en el callback (`tap.push_slice(&mono)`
        // en `src/audio/mod.rs`); sacar L/R exigiría tocar ese hot path. El
        // VU expone UN solo (nivel, pico) desde el ring mono actual, sin
        // canales por separado.
        let mut analyzer = SpectrumAnalyzer::new();
        analyzer.sample_rate = 44_100;
        analyzer.band_ranges = SpectrumAnalyzer::compute_ranges(44_100);
        for i in 0..FFT_SIZE {
            let t = i as f32 / 44_100.0;
            analyzer.ring[i] = (std::f32::consts::TAU * 440.0 * t).sin();
        }
        analyzer.filled = FFT_SIZE;
        for _ in 0..8 {
            analyzer.pending = 1;
            analyzer.analyze();
        }
        // Seno a escala completa (RMS ≈ 0.707, -3 dBFS) converge cerca del
        // techo de la escala dBFS del medidor (ver `db_meter_level`): ~0.95,
        // no ~0.707 de un RMS lineal sin mapear.
        assert!(
            (analyzer.vu() - 0.95).abs() < 0.05,
            "el nivel converge al mapeo dBFS del mono, obtenido {}",
            analyzer.vu()
        );
        assert!(
            analyzer.vu_peak() >= analyzer.vu(),
            "el pico nunca baja del nivel"
        );
    }

    #[test]
    fn vu_decays_without_samples_and_peak_falls_with_peak_fall() {
        let mut analyzer = SpectrumAnalyzer::new();
        analyzer.vu = 0.8;
        analyzer.vu_peak = 0.8;
        analyzer.analyze();
        assert!(
            analyzer.vu() > 0.0 && analyzer.vu() < 0.8,
            "sin muestras el nivel decae como las bandas, obtenido {}",
            analyzer.vu()
        );
        assert!(
            analyzer.vu_peak() >= analyzer.vu(),
            "el pico nunca baja del nivel"
        );
        // El pico baja linealmente con PEAK_FALL (~800 ms de techo a cero
        // a 60 fps).
        analyzer.vu = 0.0;
        analyzer.vu_peak = 1.0;
        for _ in 0..48 {
            analyzer.analyze();
        }
        assert!(
            analyzer.vu_peak() < 1e-5,
            "48 frames: el pico llegó a cero, obtenido {}",
            analyzer.vu_peak()
        );
    }

    #[test]
    fn band_ranges_are_contiguous_and_in_bounds() {
        for sample_rate in [22_050, 44_100, 48_000, 96_000] {
            let ranges = SpectrumAnalyzer::compute_ranges(sample_rate);
            assert_eq!(ranges.len(), NUM_BANDS, "sr={sample_rate}");

            let n_bins = FFT_SIZE / 2;
            let mut previous_high = 0;
            for (index, (low, high)) in ranges.iter().copied().enumerate() {
                assert!(low >= 1 && low < n_bins, "sr={sample_rate} low={low}");
                assert!(high > low && high <= n_bins, "sr={sample_rate} {low}..{high}");
                if index > 0 {
                    assert_eq!(low, previous_high, "hueco o traslape en banda {index} (sr={sample_rate})");
                }
                previous_high = high;
            }
        }
    }

    #[test]
    fn analyzer_starts_silent() {
        let analyzer = SpectrumAnalyzer::new();
        assert_eq!(analyzer.bands(), &[0.0f32; NUM_BANDS]);
    }

    #[test]
    fn analyze_without_samples_decays_toward_zero() {
        let mut analyzer = SpectrumAnalyzer::new();
        analyzer.bands[0] = 0.8;
        analyzer.analyze();
        let after = analyzer.bands()[0];
        assert!(after > 0.0 && after < 0.8, "debe decaer lentamente, obtenido {after}");
    }

    #[test]
    fn shape_clips_full_scale_and_lifts_quiet_energy() {
        assert_eq!(shape(0.0), 0.0, "silencio = barra en cero");
        assert!(
            (shape(1.0) - 1.0).abs() < 1e-6,
            "un seno a escala completa llega al tope"
        );

        // El objetivo: música real (energías bajas) se ve.
        let quiet = shape(0.05);
        assert!(quiet >= 0.3, "señal débil visible, obtenido {quiet}");

        // Sin clipear en seguida: todavía hay margen sobre música típica.
        let typical = shape(0.1);
        assert!(typical > quiet && typical < 1.0, "obtenido {typical}");

        // Monótono creciente en todo el rango.
        let mut previous = 0.0;
        for step in 0..=40 {
            let value = shape(step as f32 * 0.05);
            assert!(value >= previous, "no monótono en {step}");
            previous = value;
        }
    }

    #[test]
    fn sine_full_scale_fills_its_band_but_not_the_rest() {
        let mut analyzer = SpectrumAnalyzer::new();
        analyzer.sample_rate = 44_100;
        analyzer.band_ranges = SpectrumAnalyzer::compute_ranges(44_100);
        for i in 0..FFT_SIZE {
            let t = i as f32 / 44_100.0;
            analyzer.ring[i] = (std::f32::consts::TAU * 1_000.0 * t).sin();
        }
        analyzer.filled = FFT_SIZE;

        // Varias corridas: el attack (0.55) converge al objetivo en ~4 frames.
        for _ in 0..6 {
            analyzer.pending = 1;
            analyzer.analyze();
        }

        let max = analyzer.bands.iter().cloned().fold(0.0_f32, f32::max);
        assert!(max >= 0.9, "la banda del seno llena la altura: {max}");

        // Las bandas lejanas de 1 kHz solo ven fuga de la ventana Hann.
        let elsewhere: f32 = analyzer
            .bands
            .iter()
            .enumerate()
            .filter(|(index, _)| !(*index >= 16 && *index <= 20))
            .map(|(_, value)| *value)
            .sum();
        assert!(
            elsewhere < 1.0,
            "el resto no se ilumina con el seno: {elsewhere}"
        );
    }
}
