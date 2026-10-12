# mp3tui

Reproductor de audio para la terminal, escrito en Rust: reproduce archivos
locales, URLs directas y links de YouTube (vía `yt-dlp`), con una TUI en
[ratatui] con visualizador de espectro en vivo y explorador de biblioteca.
Se maneja por teclado.

[ratatui]: https://github.com/ratatui/ratatui

## Capturas

<p align="center">
  <img src="assets/mp3tui-1.png" width="820" alt="Captura de mp3tui">
  <img src="assets/mp3tui-2.png" width="820" alt="Captura de mp3tui">
</p>

## Requisitos

- [Rust](https://rustup.rs) (toolchain estable).
- Un dispositivo de audio disponible.

## Clonar y correr

```sh
git clone https://github.com/facu-bot/mp3tui-prototipo.git
cd mp3tui-prototipo

cargo run "cancion.mp3"        # archivo local
cargo run "C:/Musica"          # carpeta con la biblioteca
cargo run "https://www.youtube.com/watch?v=dQw4w9WgXcQ"   # YouTube (requiere yt-dlp)
cargo run                      # biblioteca por defecto
```

## YouTube (yt-dlp)

Para reproducir links de YouTube, mp3tui usa `yt-dlp`. Instalalo según tu
sistema:

- **Windows:** `winget install yt-dlp`
- **Linux / macOS:** el gestor de paquetes de tu distribución o `pip install yt-dlp`

Si no está instalado, la app avisa al intentar reproducir una URL de YouTube.

## Instalar el ejecutable

Para dejar el comando `mp3tui` disponible en la terminal:

```sh
cargo install --path .
```

En Windows el ejecutable queda en `%USERPROFILE%\.cargo\bin` (ya incluido en
el `PATH`), así que después podés ejecutar:

```sh
mp3tui "cancion.mp3"
mp3tui "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
```

## Atajos

| Tecla | Acción |
| --- | --- |
| `espacio` | Reproducir / pausar |
| `↑` / `↓` | Volumen |
| `←` / `→` | Seek ±5 s |
| `n` / `p` | Pista siguiente / anterior |
| `s` / `r` | Aleatorio / repetir |
| `Tab` | Biblioteca (panel lateral) |
| `b` | Buscar |
| `u` | Reproducir una URL |
| `Ctrl+P` | Configuración (temas, bandas, modo) |
| `Esc` `Esc` | Salir |
