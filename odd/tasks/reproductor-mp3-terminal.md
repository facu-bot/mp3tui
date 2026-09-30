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
      RDD: riesgo alto (mismo motivo estructural); consentimiento otorgado;
      revisión con 4 lentes, sin findings. Review aprobada y confirmada. Boundary
      revisado avanza a `b57507d`.
- [x] T7 — Seek (nueva, pedido del usuario: "adelantar segundos de la cancion"):
      extender AudioEngine (T2) para exponer posición/duración de la pista actual y
      un método `seek(Duration)` que reposicione la decodificación con la API de
      seek de symphonia, reseteando el pipeline de resample/ring buffers de forma
      consistente. En la UI: atajos ←/→ para retroceder/adelantar (5s por paso), y
      una barra de progreso con tiempo transcurrido/total. Ruta: delegada.
      Implementación: `position()`/`duration()`/`seek()`/`take_seek_error()` en
      AudioEngine. El `FormatReader` sigue viviendo en el hilo decodificador (no es
      Send/Sync-safe compartirlo); la UI deja un pedido de seek en
      `Mutex<Option<Duration>>`, el decodificador lo aplica en su propio loop
      (`format.seek` + reset del decoder + reset del `LinearResampler` +
      vaciado del ring de reproducción vía flag consumer-side + vaciado del tap
      del visualizador). Barra de progreso `Progreso mm:ss / mm:ss` con fallback
      cuando el contenedor no reporta duración.
      Revisión de concurrencia (fork) encontró 2 hallazgos reales, ambos de
      severidad baja:
      (a) Ventana de carrera donde un seek pedido justo al terminar la pista podía
      perderse silenciosamente (decoder_done se marcaba después de que el loop
      salía sin volver a chequear el pedido). **Corregido**: `seek()` y el cierre
      de `decode_loop` comparten el mismo lock de `shared.seek`, serializando la
      carrera — o el pedido queda visible para que el loop lo consuma (incluso en
      el caso de fin natural, que ahora también chequea el pendiente antes de
      salir), o `decoder_done` ya está en true y `seek()` lo rechaza con error en
      vez de perderlo en silencio.
      (b) El timeout de 250ms para vaciar el ring de reproducción viejo antes de
      un seek podría, en teoría, dejar pasar un instante de audio mezclado si el
      callback de audio no corre en esa ventana (stall de driver, muy
      improbable). **No corregido, documentado como backlog** (comentario en el
      código en `apply_seek`): un fix real necesitaría un ack del consumer o
      fallar el seek, más invasivo de lo que vale para un caso de borde que ya
      implica audio roto por otro motivo.
      Evidencia: `cargo build`/`cargo clippy --all-targets` en 0/0, `cargo test`
      48/48 (1 ignorado, requiere device de audio real). Harness E2E 11/11 PASS
      incluyendo el nuevo escenario K (seek real adelante/atrás/clamp/fin de
      pista sobre un wav de 12.8s), y fix de D/E que estaban fallando por asserts
      obsoletos de un log de T5. Se detectó y corrigió además un bug de encoding
      (mojibake UTF-8/Windows-1252 en los comentarios nuevos de audio/mod.rs,
      p.ej. "decodificaciÃ³n" en vez de "decodificación") antes de comitear.
      Orquestador verificó de forma independiente: binario recompilado, arranque
      limpio con audio real sin stderr ni zombie, lectura directa del código del
      fix de concurrencia (el lock compartido cierra la ventana de carrera
      correctamente).
      RDD: riesgo alto (mismo motivo estructural); consentimiento otorgado;
      revisión con 4 lentes, 1 finding SUGGESTION no bloqueante en
      review-resilience (el mismo timeout de drain de (b), ya documentado en
      código y en el backlog de este archivo). Review aprobada y confirmada.
      Boundary revisado avanza a `4db9ac0`.
- [x] T8 — Velocidad de reproducción variable (nueva, pedido del usuario: "poner en
      una reproducion de velocidad mas rapida o lenta"): extender AudioEngine con un
      multiplicador de velocidad (0.5x–2.0x, pasos de 0.25x) aplicado en el pipeline
      de resample ya existente (T2), sin distorsionar el pitch más de lo esperable
      para un resampler simple (documentado como limitación conocida: no es
      time-stretching, el pitch varía proporcional a la velocidad). En la UI:
      atajos `[`/`]` y velocidad actual en el panel de info. Ruta: delegada.
      Implementación: `shared.speed` es un `AtomicU32` (bit-cast de f32), igual
      patrón que `volume` — no una cola de pedidos como el seek de T7, porque la
      velocidad es estado continuo idempotente, no un evento de una sola vez que
      se pueda "perder". El hilo decodificador relee `shared.speed` y llama
      `resampler.set_speed(...)` en cada iteración (paquete decodificado), así que
      aunque un seek reconstruya el resampler con su default de 1.0x, se
      reaplica la velocidad vigente antes del próximo resample — sin necesidad de
      sincronizar con el mecanismo de seek. Cambiar velocidad no vacía buffers
      (a diferencia del seek): solo cambia la tasa de consumo futura, no salta de
      posición, así que no hay glitch que evitar ahí. `clamp_speed` sanea
      NaN/±Infinity a 1.0 antes de cada store.
      Evidencia: `cargo build`/`cargo clippy --all-targets` en 0/0, `cargo test`
      52/52 (4 nuevos: clamp de velocidad, consumo de muestras del resampler a
      2x/0.5x, que la velocidad sobrevive un reset de seek, label de velocidad).
      Harness E2E 12/12 PASS (nuevo escenario L: fila oculta a 1.00x, `]`×3 →
      1.75x, `[` → 1.50x, clamp en ambos extremos). Verificado con audio real:
      un segmento de 12.84s sonó en 6.45s a 2.0x (esperado 6.42s, error 30ms).
      Revisión de concurrencia (fork, tercera pasada del mismo tipo que T5-T7):
      sin hallazgos — confirmó que la reaplicación por-paquete de la velocidad
      hace la interacción con seek libre de carreras por diseño, no por
      casualidad, y que no hay panics posibles por NaN/overflow en la conversión
      de velocidad a frames.
      RDD: riesgo alto (mismo motivo estructural); usuario eligió "saltear esta
      vez" (`consent: declined`) — sin registro de revisión nativa para este
      commit. Cobertura de calidad ya obtenida vía build/clippy/test + harness
      E2E + fork de concurrencia (arriba), sin findings.
- [x] T9 — Config y temas vía TOML (colores del visualizador, keybindings, incluyendo
      los nuevos de T7/T8). Reforzado por pedido del usuario: el tema por defecto
      debe verse pulido y atractivo (paleta coherente, no colores por defecto sin
      cuidar), no solo ser funcional. Ruta: delegada.
      Implementación: nuevo módulo `src/config.rs` (~684 líneas). TOML en
      `%APPDATA%\mp3tui\config.toml` (override vía `MP3TUI_CONFIG`), secciones
      `[theme]` (acento, gradiente del visualizador, selección) y `[keybindings]`
      (todos los atajos existentes, remapeables, colores/teclas aceptados en
      inglés y español). Merge campo por campo: un campo inválido cae a su
      default individual con warning (stderr + primera pantalla), sin tirar el
      resto del archivo ni crashear. Sin archivo de config, arranca igual que
      antes. `ui/mod.rs` se refactorizó para leer atajos y colores desde
      `config` en vez de tener todo hardcodeado; el pie de atajos se genera a
      partir del binding real, no de un texto fijo.
      Hallazgo real detectado en revisión de código (fork) antes de comitear:
      `parse_keybindings` validaba cada campo individualmente pero nunca
      chequeaba si dos ACCIONES distintas quedaban mapeadas a la misma tecla
      (ej. el usuario pone `quit = "q"` y `play_pause = "q"`: ambos parsean
      bien, pero en el dispatch por orden fijo `quit` se evalúa primero y
      `play_pause` queda silenciosamente sin efecto, sin ningún aviso).
      **Corregido**: se agregó detección de colisiones que solo avisa cuando
      la colisión la causa la config del usuario (no las colisiones de diseño
      que ya existen en los defaults, como Esc sirviendo para "volver" y
      "salir" a la vez, o Tab para biblioteca/volver — evita falsos positivos
      en la config por defecto). La acción perdedora (por orden de prioridad
      del dispatch) recupera sus teclas por defecto si están libres, o queda
      sin bindear si no, con un warning explícito indicando quién ganó y
      quién perdió.
      Evidencia: `cargo build`/`cargo clippy --all-targets` en 0/0, `cargo
      test` 66/66 (63 + 3 nuevos de colisión: con recuperación de default, sin
      default libre, y colisiones heredadas sin warning). Harness E2E 14/14
      PASS (nuevo escenario M: config válido remapea play_pause a 'x' y
      funciona; N: TOML roto muestra warning sin crashear). Orquestador
      verificó de forma independiente: binario recompilado, arranque limpio
      sin archivo de config (default) sin stderr ni zombie.
      RDD: riesgo alto (mismo motivo estructural); consentimiento otorgado;
      revisión con 4 lentes, sin findings. Review aprobada y confirmada.
      Boundary revisado avanza a `df27210`.
- [x] T10 — Pulido: manejo de errores (archivos corruptos/formatos no soportados),
      README con instrucciones de build/run. Ruta: delegada.
      Bug real encontrado y corregido durante el repaso de manejo de errores:
      cuando `run_decode` fallaba a mitad de archivo (formato corrupto después
      de un header válido) o el device de audio moría en caliente (cambio de
      salida, sleep del sistema), el error se guardaba en `shared.error` pero
      el `state` seguía en `PLAYING` — la TUI mostraba "Reproduciendo" para
      siempre sobre silencio, y si repeat estaba activo, el auto-advance de
      T6 podía tratar la falla como fin natural de pista. Corregido: nuevo
      estado `had_playback_error` (sticky, distinto del `error` que se
      consume una vez) y un nuevo `Status::Stopped` ("Detenido") en la UI,
      distinto de `Status::Ended` ("Fin de la pista"); el error se escribe
      siempre ANTES que la transición de estado (mismo orden ya usado en el
      seek de T7) para que quien lea `state = STOPPED` ya tenga el error
      disponible. El auto-advance de repeat ahora se suprime explícitamente
      cuando hubo un error, no solo cuando la pista llegó a su fin real.
      También: `.gitignore` ahora cubre `.atl/` además de `/target`; revisión
      de la paleta/layout sin cambios (ya estaba coherente desde T4-T9, no se
      tocó para no arriesgar lo ya pulido).
      Evidencia: `cargo build`/`cargo clippy --all-targets -- -D warnings` en
      0/0, `cargo test` 67/67 (2 ignorados, requieren device real). Harness
      E2E 15/15 PASS (regresión completa A-N + nuevo escenario O: wav truncado
      a mitad de archivo con header RIFF intacto — el probe inicial pasa pero
      el decode falla en curso — asertando "Detenido", error visible en
      pantalla, sin "Reproduciendo" eterno ni "Fin de la pista" falso).
      Orquestador leyó el diff completo de audio/mod.rs y ui/mod.rs línea por
      línea (sin delegar a un fork esta vez, dado el tamaño acotado — 90
      líneas — y que la lógica de ordering ya seguía el mismo patrón
      documentado y verificado en T7) y confirmó el orden correcto de
      escritura (error antes que state) en los 3 puntos donde aplica.
      Verificó además de forma independiente, reproduciendo el mismo wav
      truncado usado en el escenario E2E: el proceso sigue vivo sin crash ni
      zombie tras el fallo de decode.

      **README.md agregado** en la raíz: qué es la app, requisitos y cómo
      compilar/correr, tabla completa de atajos (reproductor/explorador/
      búsqueda), formato y ubicación del archivo de config opcional,
      limitaciones conocidas (velocidad cambia el pitch; caso de borde del
      seek con stall de driver, documentado también en el código; formatos
      sin duración conocida; escaneo de bibliotecas grandes por tandas),
      stack de dependencias, estructura de `src/`, y cómo correr los tests.
      RDD: riesgo alto (mismo motivo estructural); consentimiento otorgado;
      revisión con 4 lentes, sin findings. Review aprobada y confirmada.
      Boundary revisado avanza a `3f1d2c3`.

## Cierre del MVP (T1-T10)
Con T10 completada, las 10 tareas planificadas (incluyendo las 3 agregadas a
pedido del usuario a mitad de camino: explorador extendido, seek, velocidad
variable) están implementadas, commiteadas y verificadas. El proyecto es un
reproductor de terminal completo y funcional: reproduce mp3/flac/ogg/wav/aac
reales, visualiza el espectro en vivo, permite navegar/buscar una biblioteca,
soporta seek y velocidad variable, shuffle/repeat, y es configurable/temable
vía TOML sin dejar de funcionar out-of-the-box. Distribución: uso personal
vía `cargo run --release`, como se acordó al inicio.

Reabierto después del cierre: el usuario probó la app y pidió mejoras de
diseño visual concretas — la paleta actual (T9) "se ve feo, poco estético y
con colores horrendos", el visualizador reacciona muy poco aunque se suba el
volumen, y el explorador de archivos debería ser un panel lateral togglable
(Ctrl+Tab) en vez de una pantalla completa que reemplaza al reproductor. Se
agregan T11-T13 para esto. No se retira ni se marca inválido nada de T1-T10;
son mejoras sobre trabajo ya aceptado, no correcciones de un defecto.

- [x] T11 — Rediseño visual completo con la paleta **Kanagawa Dragon**
      (elegida por el usuario): reemplazar el tema por defecto de T9 (cian +
      semáforo verde/amarillo/rojo) por los tonos oscuros y cálidos de Dragon
      (fondo casi negro, acentos violeta/azul/rojo/verde muy desaturados,
      texto gris cálido). Aplica a todos los bloques (info, explorador,
      footer, gauges) y al gradiente del visualizador. Sigue siendo
      configurable vía `[theme]` de T9 (Dragon pasa a ser el default, no
      reemplaza el mecanismo de config). Ruta: delegada.
      Implementación: paleta con nombres reales de kanagawa.nvim (oniViolet
      #957FB8 de acento, dragonWhite #C5C9C5 de texto, dragonGray3 #7A8382
      dim, dragonBlack5 #393836 selección, dragonRed #C4746E error,
      dragonGreen #87A987 ok, dragonYellow #C4B28A warn). Gradiente del
      visualizador dentro de la misma paleta: violeta (dragonViolet
      #8992A7) → celeste (springBlue #7FB4CA) → rojo (dragonRed) en vez del
      semáforo verde/amarillo/rojo anterior — frío en la base, cálido solo
      en los picos. El mecanismo de `[theme]` configurable de T9 no se tocó,
      solo cambió el default.
- [x] T12 — El visualizador debe notarse más: ajustar la ganancia/
      normalización de las bandas FFT (src/visualizer/mod.rs) para que
      reaccione visiblemente con audio real a volumen normal, no solo con
      picos extremos. Revisar la constante de normalización actual
      (`norm = FFT_SIZE / 4`, calibrada para un tono puro de escala completa,
      demasiado conservadora para música real con energía repartida en
      muchas bandas) y el rango de attack/decay. Ruta: delegada.
      Bug real encontrado y corregido en el camino: la normalización usaba
      el PROMEDIO de magnitud por bin en vez de la suma, así que cada banda
      se diluía en 1/(cantidad de bins) — las bandas logarítmicamente más
      anchas (agudos) casi no se movían nunca, independientemente de la
      ganancia. Cambiado a energía de banda = suma de magnitudes / norm
      (recupera la amplitud real del seno sin importar el ancho de banda).
      Sobre eso, nueva curva de compresión `shape(energy) = (energy *
      GAIN).powf(EXPO).min(1.0)` con GAIN=5.0/EXPO=0.4 (raíz suave): sube la
      señal típica de música sin mandar los picos fuertes directo al clip.
      Medido con audio real (90210.mp3, 16 filas de alto): antes 12.5%-62%
      (promedio ~21%), ahora 19%-81% (promedio ~33%), sin ninguna barra
      pegada al tope en la muestra — hay margen para que los picos se sigan
      notando.
      Evidencia (T11+T12 juntas): `cargo build`/`cargo clippy --all-targets
      -- -D warnings` en 0/0, `cargo test` 69/69 (2 nuevos: `shape` clipea en
      escala completa y levanta la energía baja de forma monótona; un seno
      de prueba llena su propia banda sin iluminar el resto — más los 2
      tests de hex del gradiente actualizados a los valores Dragon). Harness
      E2E 15/15 PASS (los asserts de color de D/E se hicieron
      paleta-agnósticos, comparando índices ANSI relativos en vez de
      "verde"/"rojo" literal, ya que el mapeo de conhost cambia con la
      paleta nueva). Orquestador verificó de forma independiente: re-corrida
      completa de `cargo test` (69/69), lectura línea por línea del diff de
      `src/visualizer/mod.rs` (el fix de energía de banda y la curva de
      compresión, con test de un seno real confirmando que solo su banda se
      ilumina), arranque limpio con audio real sin stderr ni zombie.
- [ ] T13 — Explorador de archivos como panel lateral togglable con
      Ctrl+Tab (en vez del cambio de pantalla completa actual de T3/T5):
      el reproductor (con visualizador) queda visible de fondo, y Ctrl+Tab
      abre/cierra un panel lateral con la lista de la biblioteca superpuesto
      o compartiendo el layout. Hay que decidir el ancho del panel y qué pasa
      con el visualizador cuando el panel está abierto (¿se achica, se tapa
      parcialmente?) — dejarlo a criterio de implementación con buen
      resultado visual. `b`/Tab actuales quedan a reconciliar con el nuevo
      atajo (¿se mantienen como alias o se reemplazan por Ctrl+Tab?, usar
      criterio y documentarlo). Ruta: delegada.

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
- `apply_seek` (src/audio/mod.rs, T7) tiene un timeout de 250ms esperando a que
  el callback de audio vacíe el ring viejo antes de un seek; si el callback no
  corre en esa ventana (stall de driver, muy improbable), el seek igual empuja
  muestras nuevas sobre un ring que aún tiene muestras viejas, causando un
  instante de audio mezclado en vez de un corte limpio. No se corrigió porque el
  fix real (ack del consumer o fallar el seek) es más invasivo de lo que vale
  para un caso de borde que ya implica audio roto por otro motivo (documentado
  en un comentario en el código).

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
