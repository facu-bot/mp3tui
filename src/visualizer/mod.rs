#![allow(dead_code)]

//! Analizador de espectro FFT (T4): ventana Hann, rustfft, bandas logarítmicas
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
/// attack/decay); solo el pico tiene decay propio (testeado en
/// `vu_decays_without_samples_and_peak_falls_with_peak_fall`).
const PEAK_FALL: f32 = 1.0 / 48.0;
/// Tamaño del staging de lectura (igual al ring del tap: 8192 muestras).
const TAP_READ_MAX: usize = 1 << 13;
/// Ganancia previa a la compensación de potencia: la energía real de una
/// banda de música queda muy por debajo del máximo teórico (un seno a
/// escala completa), y sin esto las barras casi nunca se movían (T12).
const GAIN: f32 = 5.0;
/// Exponente de la compensación (< 1 aplana: sube lo bajo sin mandar lo
/// alto directo al clip). 0.4 ≈ raíz suave; con GAIN=5 el clip cae en
/// energy≈0.2 (picos fuertes), y lo típico (0.008–0.05) cae en 28–60%.
const EXPO: f32 = 0.4;

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
    /// Nivel VU mono para el modo `vu` (T4 modos-visualizador): RMS suavizado
    /// de la señal del tap actual; ver `update_vu`.
    vu: f32,
    /// Pico del medidor VU (T4): máximo visto del nivel, con decay
    /// `PEAK_FALL`, nunca por debajo del nivel actual.
    vu_peak: f32,
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
        // casi muertas — el problema de «las barras se ven chicas» (T12).
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

    /// Nivel del medidor VU (modo `vu` de T4): RMS suavizado en 0.0..=1.0.
    pub fn vu(&self) -> f32 {
        self.vu
    }

    /// Pico del medidor VU (modo `vu` de T4): máximo visto del nivel, en
    /// 0.0..=1.0. Siempre >= el nivel actual.
    pub fn vu_peak(&self) -> f32 {
        self.vu_peak
    }

    /// RMS del ring mono actual (0.0..=1.0 con clamp del llamador): raíz de la
    /// media cuadrática de las muestras acumuladas; sin historia devuelve 0.
    fn rms_level(&self) -> f32 {
        let take = self.filled.min(FFT_SIZE);
        if take == 0 {
            return 0.0;
        }
        let sum: f32 = self.ring[..take].iter().map(|sample| sample * sample).sum();
        (sum / take as f32).sqrt()
    }

    /// Nivel VU mono del modo `vu` (T4 modos-visualizador): sigue al RMS con
    /// el mismo suavizado attack/decay de las bandas (sin jitter) y el pico
    /// sube en el acto y baja con `PEAK_FALL`, sin quedar nunca debajo del
    /// nivel.
    ///
    /// Sin ganancia extra: el VU mide RMS honesto, a diferencia de las
    /// bandas (`GAIN = 5.0`). Un seno a escala completa da ≈ 0.707 y usa el
    /// 70% de la escala, dejando headroom (testeado en
    /// `vu_is_a_single_mono_level_from_current_tap`).
    ///
    /// Decisión mono documentada (nota T1/T4): el tap ya promedia a mono en
    /// el callback (`tap.push_slice(&mono)` en `src/audio/mod.rs`, que suma
    /// los canales y divide); un VU estéreo L/R exigiría llevar los canales
    /// por separado por ese hot path, fuera del margen de T4 (sin refactors
    /// del motor). El render solo lee con `vu()`/`vu_peak()`.
    fn update_vu(&mut self, target: Option<f32>) {
        match target {
            Some(rms) => {
                let rms = rms.clamp(0.0, 1.0);
                self.vu += (rms - self.vu) * if rms > self.vu { ATTACK } else { DECAY };
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

    /// Las `max_points` muestras crudas más recientes del ring, de más vieja
    /// a más nueva (para que la más reciente caiga a la derecha del
    /// osciloscopio de T2). Si hay menos historia, devuelve la que haya;
    /// sin muestras o con tope 0 devuelve vacío.
    pub fn waveform(&self, max_points: usize) -> Vec<f32> {
        let take = self.filled.min(FFT_SIZE).min(max_points);
        let mut out = Vec::with_capacity(take);
        let start = (self.ring_pos + FFT_SIZE - take) % FFT_SIZE;
        for i in 0..take {
            out.push(self.ring[(start + i) % FFT_SIZE]);
        }
        out
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
/// con clip en 1.0. Objetivo de T12: con música real a volumen normal las
/// barras usan un rango amplio de la altura disponible y los picos siguen
/// notándose (llegan al tope, no todo el tiempo).
fn shape(energy: f32) -> f32 {
    (energy * GAIN).powf(EXPO).min(1.0)
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_returns_most_recent_oldest_first() {
        let mut analyzer = SpectrumAnalyzer::new();
        // Ring a medio llenar: las muestras van en orden de llegada.
        for (i, slot) in analyzer.ring.iter_mut().enumerate().take(100) {
            *slot = i as f32;
        }
        analyzer.filled = 100;
        analyzer.ring_pos = 100;

        let wave = analyzer.waveform(8);
        assert_eq!(
            wave,
            vec![92.0, 93.0, 94.0, 95.0, 96.0, 97.0, 98.0, 99.0],
            "las 8 más recientes, de más vieja a más nueva (la última es la más nueva)"
        );

        // Con wrap-around: el orden cronológico se mantiene.
        analyzer.filled = FFT_SIZE;
        analyzer.ring_pos = 3;
        for (i, slot) in analyzer.ring.iter_mut().enumerate() {
            *slot = i as f32;
        }
        // ring_pos = 3 → la más nueva es ring[2]; pedir 5 devuelve
        // ring[FFT-2..] ++ ring[..3] en orden cronológico.
        let wave = analyzer.waveform(5);
        assert_eq!(
            wave,
            vec![
                (FFT_SIZE - 2) as f32,
                (FFT_SIZE - 1) as f32,
                0.0,
                1.0,
                2.0,
            ],
            "el wrap-around no rompe el orden"
        );

        // Sin muestras o con tope 0: vacío, sin pánico.
        let fresh = SpectrumAnalyzer::new();
        assert!(fresh.waveform(80).is_empty());
        assert!(analyzer.waveform(0).is_empty());
    }

    // T4 (modos-visualizador): medidor VU MONO con RMS de la señal.
    // ------------------------------------------------------------------

    #[test]
    fn vu_starts_silent() {
        let analyzer = SpectrumAnalyzer::new();
        assert_eq!(analyzer.vu(), 0.0, "sin señal el medidor arranca en cero");
        assert_eq!(analyzer.vu_peak(), 0.0, "sin señal no hay pico");
    }

    #[test]
    fn vu_is_a_single_mono_level_from_current_tap() {
        // Decisión estéreo-vs-mono con test en mano (nota T1/T4): el tap ya
        // promedia a MONO en el callback (`tap.push_slice(&mono)` en
        // `src/audio/mod.rs`); sacar L/R exigiría tocar ese hot path, fuera
        // del margen de T4. El VU expone UN solo (nivel, pico) desde el ring
        // mono actual, sin canales por separado.
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
        // Seno a escala completa: RMS = 1/√2 ≈ 0.707 (sin ganancia extra: el
        // VU mide honesto, sin el GAIN de las barras), con headroom.
        assert!(
            (analyzer.vu() - 0.707).abs() < 0.05,
            "el nivel converge al RMS del mono, obtenido {}",
            analyzer.vu()
        );
        assert!(
            analyzer.vu() < 0.8,
            "queda headroom en el medidor (obtenido {})",
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

        // El objetivo de T12: música real (energías bajas) se ve.
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
