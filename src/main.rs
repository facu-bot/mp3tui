mod audio;
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

    let Some(path) = cli.path else {
        eprintln!("mp3tui: falta el path. Uso: mp3tui <archivo-de-audio>");
        return ExitCode::FAILURE;
    };

    if path.is_dir() {
        eprintln!(
            "mp3tui: '{}' es una carpeta; la biblioteca por carpetas llega en T5. Pasá un archivo de audio.",
            path.display()
        );
        return ExitCode::FAILURE;
    }

    let mut engine = match audio::AudioEngine::new() {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("mp3tui: no se pudo inicializar el device de audio: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(err) = engine.play(&path) {
        eprintln!("mp3tui: no se pudo reproducir '{}': {err}", path.display());
        return ExitCode::FAILURE;
    }

    println!("Reproduciendo: {}", path.display());

    match engine.wait_until_done() {
        Ok(()) => {
            println!("mp3tui: fin de la pista.");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("mp3tui: error durante la reproducción: {err}");
            ExitCode::FAILURE
        }
    }
}
