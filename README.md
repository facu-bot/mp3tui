# mp3tui

Reproductor de audio para la terminal: suena de verdad (no es una demo), y
acompaña con una TUI en [ratatui] con visualizador de espectro FFT en vivo,
explorador de biblioteca como panel lateral superpuesto, seek, velocidad
variable y configuración opcional por TOML.

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

# Sin argumentos: biblioteca por defecto con las carpetas conocidas que
# existan en %USERPROFILE% (Music, Downloads, Desktop, Documents) unidas a
# las [library] roots de config.toml; la lista se agrupa por carpeta.
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
| `b` / `Tab` / `Ctrl+Tab` | Abrir o cerrar el panel del explorador |
| `/` | Abrir el panel del explorador con la búsqueda |
| `q` / `Esc` | Salir |

Con música sonando, el explorador ya no reemplaza la pantalla: se superpone
como panel lateral derecho (~36% del ancho) y el reproductor con su
visualizador queda visible de fondo, achicado pero animando. Sin sesión de
audio (arranque sin archivo) el explorador sigue ocupando toda la pantalla.

### Panel lateral del explorador

| Tecla | Acción |
| --- | --- |
| `↑` / `↓` | Mover la selección |
| `PgUp` / `PgDn` | Mover de a pantalla |
| `Inicio` / `Fin` | Primera / última pista |
| `Enter` | Reproducir la seleccionada (**no cierra** el panel: la pista arranca en el reproductor de fondo) |
| `espacio` | Reproducir / pausar (el panel sigue abierto) |
| `←` / `→` | Seek ±5 s |
| `[` / `]` | Velocidad |
| `n` / `p` | Pista siguiente / anterior |
| `s` | Aleatorio |
| `r` | Modo repetir |
| `/` | Búsqueda incremental (vive en el panel) |
| `Esc` / `Tab` / `Ctrl+Tab` / `b` | Cerrar el panel (el reproductor de fondo queda solo) |
| `q` | Salir |

Mientras el panel está abierto **los atajos de reproducción siguen
activos**: puedes pausar, hacer seek, cambiar de pista o ajustar la
velocidad sin cerrarlo primero. La única excepción es el volumen: `↑`/`↓`
navegan la lista mientras el panel está abierto (coinciden con los atajos de
volumen del reproductor), así que el volumen se ajusta con el panel cerrado.
El footer de atajos muestra este contexto y se actualiza al abrir/cerrar.

### Explorador a pantalla completa (sin reproducción activa)

Sin sesión de audio (arranque sin archivo) la lista ocupa toda la pantalla y
se maneja con las mismas teclas que el panel lateral, con dos diferencias:
`Esc` / `q` salen de la app (no hay panel que cerrar ni reproductor
al que volver) y `Enter` pasa al modo reproducción con la pista sonando.
`Tab` no sale: solo alterna el panel, que sin sesión no hace nada.
`Ctrl+C` sale siempre, en cualquier modo.

### Biblioteca agrupada por carpeta

La lista del explorador agrupa los tracks por su carpeta de origen, con un
encabezado por grupo (la ruta relativa a la raíz escaneada). Los encabezados
no son seleccionables: `↑`/`↓` los saltan solos. Con varias raíces los
grupos siguen el orden de las raíces y, dentro de cada una, las carpetas en
orden alfabético.

Sin argumento de CLI la biblioteca por defecto es la unión de las carpetas
conocidas que existan en el sistema (`%USERPROFILE%\Music`, `\Downloads`,
`\Desktop` y `\Documents`; las que falten se saltean sin aviso) más las
`[library] roots` configuradas — si las hay, se suman a las conocidas, no
las reemplazan. Un archivo o carpeta pasado por CLI manda sobre todo lo
anterior.

### Búsqueda (cuando está abierta)

Escribir filtra por título, artista, álbum o nombre de archivo.
`Backspace` borra, `Enter` aplica, `Esc` cancela y devuelve la lista completa.

## Configuración opcional

Archivo: `%APPDATA%\mp3tui\config.toml`. La variable de entorno
`MP3TUI_CONFIG` permite apuntar a otro archivo (útil para instalaciones
portables y para probar). Sin archivo, todo arranca con los defaults.

El tema por defecto es **Kanagawa Dragon** (la variante `dragon` de
[kanagawa.nvim](https://github.com/rebelot/kanagawa.nvim)). El ejemplo de
abajo son justamente esos valores: solo hace falta escribir las claves que
se quieran cambiar.

```toml
[theme]
accent = "#957FB8"       # #RRGGBB o nombre (cyan, cian, gris oscuro, ...)
text = "#C5C9C5"
dim = "#7A8382"
select_bg = "#393836"
error = "#C4746E"
ok = "#87A987"
warn = "#C4B28A"
spectrum_low = "#8992A7" # gradiente del visualizador (violeta)
spectrum_mid = "#7FB4CA" # celeste
spectrum_high = "#C4746E" # rojo (solo en los picos)

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

[library]
# Carpetas EXTRA a escanear sin argumento de CLI (T15): se AGREGAN a las
# carpetas conocidas (Music, Downloads, Desktop, Documents en %USERPROFILE%),
# nunca las reemplazan. Varios roots agrupan la lista por carpeta siguiendo
# este orden (primero las conocidas, después estas).
# Con backslashes usá strings literales de TOML ('C:\Musica') o barras
# normales ("C:/Musica").
roots = ["C:/Musica"]
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
