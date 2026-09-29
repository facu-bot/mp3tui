# Reproductor MP3 visual para terminal (Windows)

## Objetivo
Construir una aplicación de terminal (TUI) para Windows que reproduce archivos de audio
(mp3 y otros formatos comunes), con un diseño visual cuidado (no solo funcional) y una
visualización de espectro en vivo estilo "cava" (barras de frecuencia con gradiente de
color truecolor), con un explorador de archivos para buscar y elegir pistas, seek,
velocidad de reproducción variable, playlist, metadata ID3 y atajos de teclado
configurables.

## Problema / por qué
El usuario quiere un reproductor de música que corra 100% en terminal en Windows, con
una visualización atractiva del audio en tiempo real y un diseño de terminal prolijo,
para uso personal.

## Alcance
- Reproducción de audio (play/pause/stop/volumen) para mp3/flac/ogg/wav.
- Seek: adelantar/retroceder segundos en la pista actual.
- Velocidad de reproducción variable (más rápida/más lenta).
- Visualización de espectro FFT en tiempo real, estilo cava, con colores truecolor.
- Diseño visual cuidado en toda la TUI (no solo el visualizador): layout prolijo,
  paleta de colores coherente, no una interfaz meramente funcional. Criterio de
  aceptación explícito del usuario, transversal a T4/T5/T9.
- Explorador de archivos interactivo: navegar carpetas, buscar/filtrar mp3s, elegir y
  reproducir. Biblioteca escaneada recursivamente desde una carpeta.
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

## Estrategia de entrega
`ask-on-risk` (default): se sigue commiteando en `feature/reproductor-mp3-terminal`
sin slicing intermedio; si en algún punto hay riesgo real de entrega (push/PR), se
pregunta una vez por la estrategia de encadenado (`stacked-to-main` o
`feature-branch-chain`). Confirmado por el usuario al cruzar el heurístico de ~400
líneas acumuladas (T1-T3 ya suman ~1150 líneas autoría propia). Distribución sigue
siendo uso personal (`cargo run --release`), sin push/PR planeado por ahora.

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
- [x] T2 — Motor de audio: decodificar con symphonia y reproducir con cpal un archivo
      pasado por CLI; tap de muestras PCM a un ring buffer compartido. Ruta: delegada.
      Nota tamaño: 673 líneas en audio/mod.rs, por encima del heurístico de ~400; se
      mantiene como una sola tarea porque decode+resample+channel-mapping+tap+manejo
      de errores sin panics son una única unidad coherente (motor de audio completo),
      no se fuerza un split artificial.
      Evidencia: `cargo build` y `cargo clippy --all-targets` en 0/0 (reportado por el
      agente); verificado además por el orquestador: `mp3tui.exe` con archivo
      inexistente devuelve error claro y exit 1 sin panic; reproducción real de un
      mp3 del sistema (`C:\Users\facu\Music\90210.mp3`) confirmada de forma
      independiente (el proceso sigue vivo y sonando 4s después de arrancar, sin
      errores en stderr).
      RDD: riesgo alto (thread spawn interno); consentimiento otorgado; revisión con
      4 lentes, 1 finding SUGGESTION no bloqueante en review-resilience
      (`AudioEngine::stop()` puede bloquear brevemente si `next_packet()` está en I/O
      cuando se llama stop/pause; sin impacto para archivos locales, relevante si en
      el futuro se soporta streaming/red) — anotado como backlog. Review aprobada y
      confirmada. Boundary revisado avanza a `ebc56ee`.
- [x] T3 — Shell de TUI con ratatui: layout base, controles de reproducción
      (play/pause/volumen) conectados al motor de audio. Ruta: delegada.
      Alcance real: play/pause (espacio, con replay tras fin natural), volumen
      ↑/↓ (±5%), salir con q/Esc/Ctrl+C restaurando la terminal (guard con Drop),
      loop de eventos no bloqueante a ~60fps. Seek queda pendiente para T7 (el motor
      de audio de T2 no expone posición/duración todavía; no se agregó en T3 para no
      inventar una API a medias).
      Evidencia: `cargo build`/`cargo clippy --all-targets` en 0/0. El agente corrió
      un harness E2E propio (AttachConsole + inyección de teclas + lectura de
      screen buffer) con 3 escenarios (pausa/volumen/resume, fin natural + replay,
      Ctrl+C) verificando exit 0, raw mode restaurado y sin procesos zombie.
      Orquestador verificó de forma independiente: arranque real con
      `C:\Users\facu\Music\90210.mp3`, proceso vivo 3s sin stderr, kill forzado sin
      dejar zombie.
      RDD: riesgo alto (mismo motivo, hilos en audio/mod.rs); consentimiento
      otorgado; revisión con 4 lentes, 1 finding SUGGESTION no bloqueante en
      review-resilience (`app.error` en ui/mod.rs nunca se limpia tras un fallo de
      replay, queda pegado en pantalla) — anotado como backlog. Review aprobada y
      confirmada. Boundary revisado avanza a `a190aca`.
- [x] T4 — Visualizador FFT estilo cava: rustfft sobre las muestras del ring buffer,
      barras logarítmicas con suavizado attack/decay y gradiente de color truecolor.
      Reforzado por pedido del usuario: la visualización y el layout que la rodea
      deben verse cuidados/atractivos, no solo funcionales. Ruta: delegada.
      Implementación: ventana Hann + FFT de 2048 muestras (rustfft), 32 bandas con
      mapeo logarítmico de frecuencia (bordes contiguos, sin huecos/traslapes),
      attack 0.55 / decay 0.16 para suavizado temporal. `ingest()` (copia del tap,
      con el Mutex tomado brevemente) separado de `analyze()` (FFT, sin lock) para
      que el callback de audio nunca espere una FFT. Barras pintadas celda a celda
      con gradiente verde→amarillo→rojo (Color::Rgb).
      Evidencia: `cargo build`/`cargo clippy --all-targets` en 0/0. 7 tests unitarios
      (contigüidad de bandas en 22.05/44.1/48/96 kHz, decaimiento sin muestras,
      colores del gradiente en los extremos y en el medio, alturas de barra
      completa/media/vacía). El agente verificó con audio real (90210.mp3): log
      temporal confirmó variación real de magnitudes entre ventanas (perfil con
      graves dominando, coherente con música), y un escenario de pausa mostró las
      barras cayendo a 0 en ~2.5s tras SPACE. Orquestador verificó de forma
      independiente que el binario se recompiló con los cambios (timestamp
      posterior al commit anterior).
      RDD: riesgo alto (mismo motivo estructural); consentimiento otorgado;
      revisión con 4 lentes, sin findings. Review aprobada y confirmada. Boundary
      revisado avanza a `88bc4c6`.
- [x] T5 — Explorador de archivos + biblioteca (alcance extendido por pedido del
      usuario: "agrega una tarea para agregar un explorador de archivos mp3, para
      buscar audios, reproducirlos"): panel interactivo para navegar carpetas, listar
      archivos de audio con metadata (título/artista/álbum vía lofty), elegir uno con
      el teclado y reproducirlo. Reescaneo recursivo de la carpeta pasada por CLI (o
      cwd si no se pasa ninguna). Ruta: delegada.
      Implementación: modo Explorer (navegación ↑/↓, Enter reproduce, b/Tab
      vuelve del reproductor al explorador re-sincronizando la selección con la
      pista sonando), diseño con paleta única de acento, filas alineadas con
      duración a la derecha y resaltado de selección/pista sonando.
      Hallazgo real detectado en revisión de código (fork) antes de comitear:
      `Playlist::from_dir_scan` (recorrido + `lofty::read_from_path` por archivo)
      corría de forma síncrona en el hilo de la UI tanto al arrancar como en cada
      `b`/Tab, lo que congelaba el teclado/render varios segundos en una
      biblioteca real de miles de archivos. Corregido: el escaneo ahora corre en
      un hilo aparte y entrega el resultado por `mpsc::channel`, sondeado sin
      bloquear una vez por frame (`poll_scan`, mismo espíritu que
      ingest()/analyze() del visualizador); un segundo `b` mientras ya hay un
      escaneo en curso no lanza uno nuevo; un canal desconectado no deja la UI en
      "Escaneando…" para siempre. Verificado con un escenario de 10.000 mp3 con
      tags ID3v2 reales: el estado de carga se pinta desde el primer frame, el
      scan completo tardó ~450ms, y `q` durante el escaneo cerró en 15ms sin
      dejar la UI trabada. Segundo hallazgo menor (fila no respeta el ancho
      exacto documentado con terminal muy angosta + duración ≥1h) también
      corregido en el mismo pase, con test dedicado.
      Evidencia: `cargo build`/`cargo clippy --all-targets` en 0/0, `cargo test`
      21/21 (16 previos + 5 nuevos de scan en background/selección/fila angosta).
      El agente extendió su harness E2E con escenarios F/G (arranque sin archivo,
      navegación con Enter, vuelta con b, regresión de A/B/C) y el nuevo H
      (biblioteca de 10k archivos), todos PASS. Orquestador verificó de forma
      independiente: arranque sobre una carpeta real con música y sobre una
      carpeta vacía, ambos sin crash ni proceso zombie; revisión de código vía
      fork detectó el problema de bloqueo antes de comitear, evitando mergear un
      defecto real de UX.
      RDD: riesgo alto (mismo motivo estructural); consentimiento otorgado;
      revisión con 4 lentes, sin findings adicionales (el hallazgo real ya estaba
      corregido en el propio commit revisado). Review aprobada y confirmada.
      Boundary revisado avanza a `b989127`.
- [x] T6 — Atajos de teclado, shuffle/repeat, búsqueda/filtro de playlist. Ruta:
      delegada.
      Implementación: nuevo módulo `src/playback.rs` (422 líneas) con el orden de
      reproducción (shuffle Fisher-Yates, next/previous con wraparound según modo
      de repeat, re-derivación de posición sin cachear un índice viejo) separado
      de la UI. Atajos: `s` shuffle on/off, `r` cicla repeat off→uno→todo, `n`/`p`
      siguiente/anterior (respetando shuffle), `/` búsqueda incremental por
      título/artista/álbum/nombre de archivo (Enter confirma el filtro, Esc
      cancela y restaura la lista completa). Footer de atajos actualizado.
      Evidencia: `cargo build`/`cargo clippy --all-targets` en 0/0, `cargo test`
      42/42 (11 nuevos: permutación de shuffle en 6 longitudes × 3 semillas, ciclo
      de repeat, wraparound por modo, filtro sobre 4 campos). Harness E2E 8/8 PASS
      (regresión A/B/C/F/G/H + nuevos I con teclas reales de repeat/next/prev/
      shuffle, J con búsqueda incremental en vivo). Orquestador verificó de forma
      independiente que el binario se recompiló y arranca sin stderr ni proceso
      zombie. Revisión de código vía fork (segunda pasada, mismo criterio que
      T5): sin hallazgos de correctitud — confirmó permutación válida de Fisher-
      Yates, disparo exactamente una vez del fin natural de pista (sin doble
      disparo con espacio), sin index out-of-bounds en next/previous con
      playlist vacía/de 1 pista, manejo correcto de Unicode multi-byte en
      backspace de búsqueda, y que un rescan en curso no pierde el filtro activo.
- [ ] T7 — Seek (nueva, pedido del usuario: "adelantar segundos de la cancion"):
      extender AudioEngine (T2) para exponer posición/duración de la pista actual y
      un método `seek(Duration)` que reposicione la decodificación con la API de
      seek de symphonia, reseteando el pipeline de resample/ring buffers de forma
      consistente. En la UI: atajos ←/→ para retroceder/adelantar (propuesta: 5s por
      paso, ajustable en T9), y una barra de progreso con tiempo transcurrido/total.
      Ruta: delegada.
- [ ] T8 — Velocidad de reproducción variable (nueva, pedido del usuario: "poner en
      una reproducion de velocidad mas rapida o lenta"): extender AudioEngine con un
      multiplicador de velocidad (ej. 0.5x–2.0x) aplicado en el pipeline de resample
      ya existente (T2), sin distorsionar el pitch más de lo esperable para un
      resampler simple (documentar la limitación si no se hace pitch-correction).
      En la UI: atajos para subir/bajar velocidad (propuesta: `[`/`]`) y mostrar la
      velocidad actual. Ruta: delegada.
- [ ] T9 — Config y temas vía TOML (colores del visualizador, keybindings, incluyendo
      los nuevos de T7/T8). Reforzado por pedido del usuario: el tema por defecto
      debe verse pulido y atractivo (paleta coherente, no colores por defecto sin
      cuidar), no solo ser funcional. Ruta: delegada.
- [ ] T10 — Pulido: manejo de errores (archivos corruptos/formatos no soportados),
      README con instrucciones de build/run. Ruta: delegada.

## Backlog de hardening (no bloqueante, detectado en revisiones RDD)
- `AudioEngine::stop()`/`pause()` pueden tardar hasta la duración de un
  `next_packet()` en curso antes de que el hilo decodificador note el cambio de
  estado (src/audio/mod.rs, ver stop() y el chequeo al tope de run_decode). Sin
  impacto para archivos locales; revisar si en el futuro se soporta streaming remoto.
- El agente de T3 reportó un colgado intermitente de conhost en una sola corrida de
  su harness E2E, justo después de un force-kill previo; no se reprodujo en 4+
  corridas posteriores. Probablemente flake del entorno de test (conhost heredado
  de un proceso matado a la fuerza), no del código; mantener en observación si
  vuelve a aparecer al usar la app real.
- `app.error` en src/ui/mod.rs (T3) se setea si falla el replay tras fin de pista,
  pero nunca se limpia a `None` después; un error transitorio queda pegado en
  pantalla aunque luego la reproducción funcione bien. Limpiar en un próximo touch
  de ui/mod.rs (p.ej. al arrancar T4/T5), no bloqueante hoy.

## Progreso
- Documento de tareas creado. Repo git inicializado en
  `C:\Users\facu\Desktop\reproductor-mp3-terminal`.
- T1, T2, T3 completadas, revisadas por RDD y commiteadas (ver evidencia en cada
  tarea arriba).
- Mientras T4 corría delegada, el usuario pidió: (a) que el diseño visual de toda la
  TUI se vea cuidado, no solo funcional — incorporado como criterio transversal en
  Alcance y en T4/T5/T9; (b) un explorador de archivos para buscar/reproducir mp3s
  — incorporado como alcance extendido de T5; (c) seek (adelantar segundos) —
  nueva T7; (d) velocidad de reproducción variable — nueva T8. Se renumeraron las
  tareas siguientes (T7 temas → T9, T8 pulido → T10).

## Próximo paso
Esperar a que termine la delegación en curso de T4 (visualizador FFT), revisar y
commitear: luego delegar T5 (explorador de archivos, alcance extendido).
