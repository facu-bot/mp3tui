# mp3tui

Reproductor de audio para la terminal: suena de verdad (no es una demo), y
acompaña con una TUI en [ratatui] con visualizador de espectro FFT en vivo,
explorador de biblioteca, seek, velocidad variable y configuración opcional
por TOML.

Desarrollado en Rust, con interfaz completamente operable por teclado.

[ratatui]: https://github.com/ratatui/ratatui

## Compilar y correr

Requisitos: [Rust](https://rustup.rs) (toolchain estable) y un device de
audio disponible en el sistema.

```sh
# Carpeta con la biblioteca (se escanea recursivamente en background):
cargo run --release -- "C:\Users\yo\Music"

# Archivo puntual (reproduce directo; su carpeta queda como biblioteca):
cargo run --release -- "C:\Users\yo\Music\tema.mp3"

# Sin argumentos: explorador sobre el directorio actual.
cargo run --release
```

Windows es el sistema desarrollado y probado; el código usa APIs portables
(cpal y symphonia funcionan en Linux y macOS) pero no está probado fuera de
Windows.

## Atajos de teclado (valores por defecto)

### Reproductor

| Tecla | Acción |
| --- | --- |
| `espacio` | Reproducir / pausar |
| `↑` / `↓` | Volumen |
| `←` / `→` | Seek ±5 s |
| `n` / `p` | Pista siguiente / anterior |
| `s` | Aleatorio (shuffle) |
| `r` | Modo repetir (off → una → todas) |
| `[` / `]` | Velocidad 0.50x – 2.00x (pasos de 0.25x) |
| `b` / `Tab` | Abrir el explorador |
| `/` | Abrir el explorador con la búsqueda |
| `q` / `Esc` | Salir |

### Explorador

| Tecla | Acción |
| --- | --- |
| `↑` / `↓` | Mover la selección |
| `PgUp` / `PgDn` | Mover de a pantalla |
| `Inicio` / `Fin` | Primera / última pista |
| `Enter` | Reproducir la seleccionada |
| `espacio` | Reproducir / pausar (sin salir del explorador) |
| `←` / `→` | Seek ±5 s |
| `[` / `]` | Velocidad |
| `n` / `p` | Pista siguiente / anterior |
| `s` | Aleatorio |
| `r` | Modo repetir |
| `/` | Búsqueda incremental |
| `Esc` / `Tab` | Volver al reproductor (si hay uno activo) |
| `q` / `Esc` | Salir (solo si no hay reproducción activa) |

`Ctrl+C` sale siempre, en cualquier modo.

### Búsqueda (cuando está abierta)

Escribir filtra por título, artista, álbum o nombre de archivo.
`Backspace` borra, `Enter` aplica, `Esc` cancela y devuelve la lista completa.

## Configuración opcional

Archivo: `%APPDATA%\mp3tui\config.toml`. La variable de entorno
`MP3TUI_CONFIG` permite apuntar a otro archivo (útil para instalaciones
portables y para probar). Sin archivo, todo arranca con los defaults.

```toml
[theme]
accent = "#22D3EE"       # #RRGGBB o nombre (cyan, cian, gris oscuro, ...)
text = "#E2E8F0"
dim = "#64748B"
select_bg = "#1E293B"
error = "#F87171"
ok = "#4ADE80"
warn = "#FACC15"
spectrum_low = "#22C55E" # gradiente del visualizador
spectrum_mid = "#EAB308"
spectrum_high = "#EF4444"

[keybindings]
quit = "q,esc"           # una tecla o lista separada por comas
play_pause = "space"
seek_back = "left"
# Acciones disponibles:
#   quit, play_pause, volume_up, volume_down, seek_back, seek_fwd,
#   prev_track, next_track, shuffle, repeat, search, speed_up,
#   speed_down, explorer, move_up, move_down, page_up, page_down,
#   home, end, select, back
# Nombres de tecla: space, enter, esc, tab, up, down, left, right,
#   pageup, pagedown, home, end, backspace, delete, insert, backtab,
#   comma, minus, plus (o un carácter simple como "q" o "/")
```

Notas del comportamiento de la config:

- Se mergea campo a campo: un valor inválido vuelve a su default sin
  arrastrar al resto. Los avisos salen en stderr antes de la TUI y en la
  primera pantalla.
- Si dos acciones distintas quedan mapeadas a la misma tecla, gana la de
  mayor prioridad en el dispatch y se genera un warning que lo explicita
  (la perdedora vuelve a sus defaults libres).
- La config se lee una sola vez al arrancar: no hay recarga en caliente.

## Limitaciones conocidas

- **La velocidad variable cambia el tono.** `[` / `]` hacen resampling
  lineal simple (muestreo a distinto paso), no time-stretching: acelerar
  sube la nota y frenarla la baja. No está implementado pitch-preserving
  (p. ej. phase vocoder) — queda como trabajo futuro.
- **Seek con stall de driver (caso de borde documentado en el código).** En
  `apply_seek` (audio/mod.rs) el buffer se drena esperando hasta 250 ms al
  callback del device; si el driver se congela más tiempo, el seek se aplica
  igual y puede quedar un instante de audio viejo mezclado con el nuevo en
  vez de un corte limpio. Carrera improbable, severidad baja, sin fix simple
  sin un ack del consumer (BACKLOG en el código).
- **Formas de audio sin duración conocida** (algunos flujos) muestran la
  posición sin barra de progreso en vez de adivinar el total.
- El explorador escanea bibliotecas grandes en background, pero el escaneo
  de tags es por archivo: en carpetas de miles de archivos la lista completa
  tarda en aparecer (se va poblando por tandas).

## Stack

| Crate | Para qué |
| --- | --- |
| [ratatui] + [crossterm] | Dibujo de la TUI y eventos de teclado/raw mode |
| [symphonia] | Decodificación de mp3, flac, ogg, wav, aac, isomp4 |
| [cpal] | Salida de audio al device |
| [rustfft] | FFT para el visualizador de espectro |
| [ringbuf] | Buffer entre el decodificador (productor) y el callback (consumidor) |
| [lofty] | Tags de audio (título, artista, álbum) |
| [clap] | CLI |

[ratatui]: https://github.com/ratatui/ratatui
[crossterm]: https://github.com/crossterm-rs/crossterm
[symphonia]: https://github.com/pdeljanov/Symphonia
[cpal]: https://github.com/RustAudio/cpal
[rustfft]: https://github.com/e-ftim/rustfft
[ringbuf]: https://github.com/mbr/ringbuf
[lofty]: https://github.com/Serial-Studio/lofty
[clap]: https://github.com/clap-rs/clap

## Estructura

```
src/
  main.rs        CLI, carga de config, ciclo de vida
  ui/            TUI: player, explorador, búsqueda, visualizador, atajos
  audio/         Motor: decodificación, device, seek, velocidad, volumen
  visualizer/    FFT y bandas para las barras
  playlist/      Escaneo de biblioteca, orden shuffle, siguiente/anterior
  playback.rs    Lógica pura (objetivos de seek, shuffle, repeat)
  config.rs      Config TOML: tema, teclas, avisos
```

## Tests

```sh
cargo test          # unitarios (lógica pura, config, render a buffer)
cargo clippy --all-targets
```

Algunos tests de audio requieren wav del sistema
(`C:\Windows\Media\Ring0*.wav`) y se auto-omiten si no existen; dos tests de
device real están anotados `#[ignore]` (se corren con `cargo test -- --ignored`).
