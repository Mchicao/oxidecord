# Selección de dispositivo de salida de voz

Paridad con el cliente oficial: elegir por qué altavoz se reproduce una llamada de voz,
con selección persistente por id y opción de predeterminado del sistema.

## Evidencia en el cliente oficial

El motor de voz del cliente oficial es nativo y no está disponible como fuente legible:

- `output/discord-desktop-1.0.9242/readable/modules/discord_voice-1/discord_voice/index.js:2`
  carga el motor con `const VoiceEngine = require("./discord_voice.node")`. La lógica interna
  de enumeración y selección de dispositivos vive dentro de ese binario y no está descompilada.
- El mismo archivo declara las capacidades que la UI puede asumir presentes:
  `features.declareSupported("media_devices")` (línea 138) y
  `features.declareSupported("set_audio_device_by_id")` (línea 143).

El wrapper declara soporte para estas capacidades; los flags no recuperan su algoritmo ni
verifican por sí solos el comportamiento del cliente oficial. El cambio implementa el equivalente
nativo con CPAL, sin incorporar código del motor de Discord.

## Gap en este proyecto antes del cambio

- `src/voice/audio/mod.rs` (`AudioIo::start`) abría siempre
  `host.default_output_device()` para la reproducción: no había elección posible.
- `src/voice/audio/devices.rs` solo enumeraba entradas (`input_devices`).
- `VoiceEngine` no tenía comando para cambiar la salida; la preferencia
  `output_device` no existía en `src/platform/prefs.rs`; la UI solo tenía el
  selector de micrófono (`src/screens/home/view/voice/controls.rs`).

La fila correspondiente de `MISSING_FEATURES.md` decía "Output device selection | Missing".

## Diseño

La feature replica el mecanismo que ya existía para el micrófono, con una mejora en el
criterio de fallback:

- **Enumeración en demanda** (`output_devices`): se hace al abrir el menú, no al arrancar,
  así un altavoz conectado después de abrir la app aparece en la lista. El id de cpal
  (`DeviceId`, cadena estable entre reinicios en WASAPI) es lo que se guarda, no el nombre.
- **Resolución y configuración** (`devices.rs::chosen`): el id guardado se parsea, se resuelve
  con `host.device_by_id` y se consulta `default_output_config` en la dirección pedida. Si
  cualquiera de las tres etapas falla —dispositivo desenchufado, id corrupto, o un id que era
  de captura y no ofrece configuración de salida— se pide lo mismo al predeterminado del
  sistema. Antes del cambio, un id resolvable con configuración fallida no caía al
  predeterminado; ahora sí.
- **Persistencia** (`prefs.output_device`) y estado de pantalla (`voice_output_device`),
  entregados al `VoiceEngine` al arrancar la gateway y en cada conexión.
- **Cambio en llamada**: `VoiceEngine::set_output_device` reutiliza el único mecanismo de
  reconfiguración que existe (`swap_devices`, antes `swap_microphone`), que reabre ambos
  streams. Un guard temprano ignora el comando si el id ya es el activo (también para la
  entrada). Los setters de pantalla también omiten persistencia, comandos y notificaciones
  redundantes al seleccionar la opción ya guardada.

### Límites conocidos

- **Sin hotplug en llamada**: el fallback solo se evalúa al abrir los dispositivos (join o
  cambio manual). Si el altavoz elegido se desenchufa a mitad de llamada, el stream muere con
  un error en consola y el sonido no migra solo al predeterminado; la recuperación llega al
  reconectar o al elegir otro dispositivo.
- **Interrupción local al cambiar en llamada**: reabrir los streams corta la reproducción
  hasta que los nuevos arrancan, y los decodificadores de cada emisor pierden lo que tenían
  en búfer o ya decodificado; quien esté hablando se retoma desde su siguiente paquete. Mute,
  deafen y la conexión no se tocan. No se ha medido la duración real de la interrupción.
- **El fallback no distingue "sin salida"**: si no hay ningún dispositivo de salida utilizable,
  la llamada sigue en marcha solo con captura, igual que antes del cambio.
- **Consultar configuración no abre un stream**: `build_output_stream` y `play()` se ejecutan
  después, en el hilo de audio. Si el driver rechaza esas operaciones, este cambio no reintenta
  la apertura contra otro dispositivo.

## Alcance: voz frente a reproducción de vídeo

Esta feature solo afecta a la **salida de las llamadas de voz** (`src/voice/audio/`). La
reproducción de vídeo es una ruta de audio separada a propósito: `platform/video/output.rs`
abre su propio stream cpal sobre el predeterminado del sistema, con búfering más somero y
resampling delegado en Media Foundation, porque un vídeo local no necesita sobrevivir jitter
de red. Cambiar el dispositivo de voz no redirige el audio de los vídeos, y viceversa. El
audio de Go Live/watch (`src/voice/stream/`) tampoco pasa por esta ruta: hoy no reproduce
audio de stream y su conexión es independiente.

## Pruebas

Cinco pruebas en `src/voice/audio/devices.rs` y `src/platform/prefs.rs` verifican resultados
explícitos sin depender de que exista hardware de audio. El helper `chosen` utilizado por
ambas direcciones permite inyectar resolución y consulta de configuración; los tests usan
dispositivos simbólicos y el parser real de `DeviceId`, sin duplicar el algoritmo:

- Una elección válida distinta del predeterminado se conserva y no consulta el default.
- Ausencia de elección, formato inválido e id válido no resoluble seleccionan el default.
- Un dispositivo resoluble que no admite configuración de salida selecciona el default.
- Sin default, o sin configuración válida para este, la dirección queda vacía.
- Las preferencias conservan ambas selecciones al serializar y leen archivos anteriores
  que no contienen `output_device`, preservando los demás campos.

No se abren streams ni se reproduce audio en estas pruebas. El sonido físico, la selección
visual y la conmutación durante una llamada requieren verificación nativa adicional.

## Validación ejecutada

Con Rust 1.99, MSVC y Ninja en Windows, usando el lockfile y un target aislado:

- `cargo fmt --check` y `git diff --check`: correctos.
- `cargo check --locked --tests`: correcto.
- `cargo test --locked`: 66 aprobados, 0 fallos; 2 pruebas preexistentes de captura/GPU ignoradas.
- `cargo clippy --locked --tests`: correcto. Los 12 avisos corresponden a código preexistente; no se añadieron dependencias ni se modificó Cargo.lock.

La revisión corrigió el caso de selección válida no predeterminada que la primera batería no
podía detectar sin hardware, evitó I/O de preferencias al repetir una elección y aclaró el
límite entre consultar configuración y abrir streams. La implementación conserva los
callbacks de captura, resampling y reproducción; no hay nuevas asignaciones ni locks por muestra.
No se midió una mejora de CPU/RAM ni la latencia física de la conmutación.
