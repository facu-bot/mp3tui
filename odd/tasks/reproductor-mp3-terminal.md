# Reproductor MP3 visual para terminal (Windows)

## Objetivo
Construir una aplicación de terminal (TUI) para Windows que reproduce archivos de audio
(mp3 y otros formatos comunes) y muestra una visualización de espectro en vivo estilo
"cava" (barras de frecuencia con gradiente de color truecolor), con playlist, biblioteca
por carpetas, metadata ID3 y atajos de teclado configurables.

## Problema / por qué
El usuario quiere un reproductor de música que corra 100% en terminal en Windows, con
una visualización atractiva del audio en tiempo real, para uso personal.

## Alcance
- Reproducción de audio (play/pause/stop/seek/volumen) para mp3/flac/ogg/wav.
- Visualización de espectro FFT en tiempo real, estilo cava, con colores truecolor.
- Playlist y biblioteca escaneada desde una carpeta.
- Metadata ID3/Vorbis (título, artista, álbum, duración).
- Shuffle, repeat, búsqueda/filtro en la playlist.
- Atajos de teclado configurables y temas de color vía archivo TOML.

Fuera de alcance (backlog, no MVP): carátulas de álbum con graphics protocol, letras
sincronizadas, soporte de mouse, hot-reload de config, empaquetado/instalador (uso
personal vía `cargo run --release`).

## Constraints
- Windows como plataforma objetivo principal.
- Distribución: uso personal, sin instalador (`cargo run --release`).
- Stack: Rust, ratatui + crossterm (TUI), symphonia + cpal (decodificación/salida de
  audio, con tap manual de PCM para no depender de la abstracción de alto nivel de
  rodio), rustfft (análisis espectral), ringbuf (buffer productor/consumidor entre hilo
  de audio y UI), lofty (metadata), serde + toml (config/temas), clap (args de CLI).

## TDD mode
No había configuración previa de TDD en este proyecto (greenfield). Se resuelve por
defecto: TDD **off**, con checks funcionales por tarea (`cargo build`, `cargo clippy`,
`cargo test` para la lógica que sea testeable de forma aislada — parsing de metadata,
lógica de playlist/shuffle, cálculo de bins de FFT). La integración audio+TUI se verifica
ejecutando la app manualmente, no por TDD estricto, dado que no es practicable testear
salida de audio real ni el render de terminal de forma automatizada en esta fase.
Fuente de la decisión: default del orquestador ante ausencia de configuración explícita,
sin objeción del usuario a este momento.

## Delegación
Ejecución delegada a un agente `opencode` corriendo dentro de una pane de Herdr
(pedido explícito del usuario: "usá opencode run para codear"). El orquestador (Claude)
arma el árbol de tareas, prepara cada prompt de trabajo, revisa el resultado (build/test)
y hace el commit de cada work-unit.

## Engram mirror
No disponible en esta sesión (no hay servidor MCP de Engram conectado). Progreso
mantenido solo localmente en este archivo; mirror pendiente de sincronizar cuando
Engram esté disponible.

## Tareas

- [x] T1 — Scaffold del proyecto Cargo: dependencias (ratatui, crossterm, symphonia,
      cpal, rustfft, ringbuf, lofty, serde, toml, clap), estructura de módulos, `main.rs`
      que compila y corre un stub. Ruta: delegada (herdr/opencode) — toca 2+ archivos
      no triviales (Cargo.toml + módulos iniciales).
      Evidencia: `cargo build` OK (verificado además ejecutando
      `target\debug\mp3tui.exe --help` directamente, sin cargo disponible en el shell
      del orquestador). Commit: `d5dfe5b` en `feature/reproductor-mp3-terminal`.
      RDD: riesgo alto (detectado por mención de proceso en src/audio/mod.rs);
      consentimiento solicitado al usuario y otorgado ("Revisar este cambio"); revisión
      nativa con 4 lentes (risk, resilience, readability, reliability) corrida por el
      orquestador como reviewer, sin findings; review aprobada y confirmada
      (`review.acknowledge-approved`). Boundary revisado avanza a `d5dfe5b`.
- [ ] T2 — Motor de audio: decodificar con symphonia y reproducir con cpal un archivo
      pasado por CLI; tap de muestras PCM a un ring buffer compartido. Ruta: delegada.
- [ ] T3 — Shell de TUI con ratatui: layout base, controles de reproducción
      (play/pause/seek/volumen) conectados al motor de audio. Ruta: delegada.
- [ ] T4 — Visualizador FFT estilo cava: rustfft sobre las muestras del ring buffer,
      barras logarítmicas con suavizado attack/decay y gradiente de color truecolor.
      Ruta: delegada.
- [ ] T5 — Playlist + biblioteca: escaneo recursivo de carpeta, parsing de metadata con
      lofty, panel de playlist en la UI. Ruta: delegada.
- [ ] T6 — Atajos de teclado, shuffle/repeat, búsqueda/filtro de playlist. Ruta:
      delegada.
- [ ] T7 — Config y temas vía TOML (colores del visualizador, keybindings). Ruta:
      delegada.
- [ ] T8 — Pulido: manejo de errores (archivos corruptos/formatos no soportados),
      README con instrucciones de build/run. Ruta: delegada.

## Progreso
- Documento de tareas creado. Repo git inicializado en
  `C:\Users\facu\Desktop\reproductor-mp3-terminal`.
- T1 completada por el agente `mp3builder` (opencode, pane wG:p2 de Herdr): scaffold de
  Cargo con todas las dependencias y stubs de los 4 módulos, build verificado por el
  orquestador de forma independiente.

## Próximo paso
Delegar T2 (motor de audio: decodificar con symphonia y reproducir con cpal, tap de PCM
a ring buffer) al agente `mp3builder`.
