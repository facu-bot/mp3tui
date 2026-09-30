mod audio;
mod config;
mod playback;
mod playlist;
mod ui;
mod visualizer;

use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

/// Reproductor MP3 visual para terminal.
#[derive(Parser, Debug)]
#[command(name = "mp3tui", about = "Reproductor MP3 visual para terminal")]
struct Cli {
    /// Archivo de audio o carpeta con la biblioteca a reproducir.
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Config (T9): se carga una vez acá para que los avisos salgan a stderr
    // antes de que la TUI tome la pantalla; cada campo inválido cayó a su
    // default en config::parse.
    for warning in &config::init().warnings {
        eprintln!("mp3tui: config: {warning}");
    }

    // Sin path: biblioteca por defecto (T15: carpetas conocidas del sistema
    // que existan, unidas a las [library] roots configuradas).
    // Con carpeta: explorador solo en esa carpeta (la CLI tiene prioría).
    // Con archivo: comportamiento original (reproducción directa a la TUI,
    // con la biblioteca de la carpeta contenedora a mano en modo explorador).
    let start = match cli.path.as_deref() {
        None => ui::StartMode::Browse(config::config().library.scan_roots()),
        Some(path) if path.is_dir() => {
            ui::StartMode::Browse(vec![path.to_path_buf()])
        }
        Some(path) if path.is_file() => ui::StartMode::PlayFile(path.to_path_buf()),
        Some(path) => {
            eprintln!("mp3tui: '{}' no existe.", path.display());
            return ExitCode::FAILURE;
        }
    };

    let mut engine = match audio::AudioEngine::new() {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("mp3tui: no se pudo inicializar el device de audio: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let ui::StartMode::PlayFile(path) = &start
        && let Err(err) = engine.play(path)
    {
        eprintln!("mp3tui: no se pudo reproducir '{}': {err}", path.display());
        return ExitCode::FAILURE;
    }

    match ui::run(&mut engine, start) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("mp3tui: error de la TUI: {err}");
            ExitCode::FAILURE
        }
    }
}
