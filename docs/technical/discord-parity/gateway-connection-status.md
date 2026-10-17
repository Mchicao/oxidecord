# Estado nativo de conexión

## Alcance y auditoría

Base: `e580fb31ac9b7dd5e33c4535f25cef99ef90be17`. Se inspeccionaron tres capacidades antes de elegir una:

1. **Recuperación de carga web.** En `readable/modules/discord_desktop_core-1/discord_desktop_core/core/bundle.js:3898` hay un backoff de carga. `4083–4099` invoca `loadURL` y registra un timeout; `4328–4346` filtra errores de navegación y reintenta la carga fallida. No es evidencia de reconexión websocket ni de su indicador visual.
2. **Confirmación del arranque.** En ese mismo archivo, `4393–4455` espera `APP_ASYNC_INDEX_TSX_LOADED`; solo sin fallo de carga resetea el backoff y confirma `pageReady`. En `readable/app/bundle.js:3890–3904`, el splash registra errores de carga/preload. Copiar esta mecánica Electron no tiene sentido para una pantalla GPUI nativa.
3. **Estado de sesión gateway.** En la base Rust, `src/discord/gateway/mod.rs:211–234` descartaba cierres y errores; `src/screens/home/data/messages/live.rs:32–45` terminaba silenciosamente cuando cerraba el canal. `RESUMED` no generaba un evento utilizado por Home. Se eligió cerrar este gap con un indicador observable, sin implementar otros subsistemas.

Los paths `readable/` anteriores son relativos al artefacto local `output/discord-desktop-1.0.9242/`. Su `manifest.json` identifica ambos bundles y sus hashes de extracción. No se incluye código de Discord en este cambio.

**Límite de evidencia:** el core selecciona un endpoint remoto (`bundle.js:3910–3953`) y carga su URL (`4089`). Los bundles inspeccionados no proporcionan el frontend remoto completo. No se atribuyen a Electron textos, transiciones ni políticas websocket que no son visibles en estos archivos.

## Contrato implementado

- Home empieza en `Connecting`; si no existe token disponible, muestra `Disconnected`, sin modificar autenticación.
- Solo un dispatch `READY` o `RESUMED`, opcode 0, con `ShardState::Active` confirmado por Twilight, permite `Connected`. Ni `HELLO`, ni heartbeat ACK, ni dispatches ajenos lo permiten.
- Cierre websocket, error de conexión o transición fuera de `Active` invalidan el estado conectado. Un error `Deserializing` con `ShardState::Active` no indica desconexión y conserva el estado confirmado; el mismo error con shard desconectado sí invalida la conexión. Se observa el estado después de **cada poll del stream**, también cuando devuelve `Pending`: Twilight puede cerrar el transporte y empezar otra conexión sin entregar primero un frame al consumidor.
- `FatallyClosed`, fin del stream o cierre definitivo del canal de eventos producen `Disconnected`. El consumidor drena primero los eventos pendientes y deja de consumir inmediatamente si Home ya no existe.
- El cierre recuperable muestra `Reconnecting` hasta una nueva confirmación de sesión. Ni mensajes ordinarios ni errores de datos restauran una conexión previamente invalidada.
- Los cambios repetidos no generan eventos de estado desde el observador. Home comprueba igualdad antes de `cx.notify()`, incluyendo el aviso final redundante de cierre del canal.

Twilight sigue siendo el único dueño del websocket, identify, heartbeat, backoff y resume. No hay temporizadores nuevos, polling periódico, tareas permanentes adicionales, llamadas REST, deps nuevas ni modificaciones de audio.

### Referencias de protocolo y dependencia

- [Discord: Gateway](https://docs.discord.com/developers/events/gateway) y [Gateway Events](https://docs.discord.com/developers/events/gateway-events): estructura del envelope, `Reconnect`, `Invalid Session`, `Ready` y `Resumed`. Estas referencias documentan el protocolo común, sin atribuirles payloads específicos de cuentas de usuario.
- [Fuente de Twilight fijada por Cargo.lock](https://github.com/s1099/twilight/blob/5efb5c6e30d5f5dca059ee1a68656eef85fdc780/twilight-gateway/src/shard.rs): `100–127` define estados; `740–779` confirma `READY`/`RESUMED`; `821–843` gestiona resume/reconnect; `854–936` implementa errores, cierres y transiciones internas de `poll_next`.

## UI

El aviso nativo usa `gpui_component::label::Label` y colores del tema. Está al pie del panel, fuera de la conversación; funciona también durante carga, error y sin canal seleccionado. Usa texto explícito, no depende del color ni roba foco. Los textos mantienen el inglés existente: `Connecting to Discord…`, `Reconnecting…`, `Disconnected`. Al conectar, desaparece.

El contenedor reserva espacio mediante `flex_1` y `min_h_0`; el aviso no puede encoger ni debe desplazar el composer fuera del panel. No se añade animación ni acción manual de retry. La presencia del usuario sigue siendo independiente: este indicador no afirma que el usuario aparezca online ante otras personas.

## Pruebas y límites

Siete pruebas nuevas en `src/discord/gateway/tests.rs` ejercitan payloads sintéticos `READY`, `RESUMED`, `HELLO`, ACK, reconnect e invalid session; estados Twilight; cierre anormal de transporte; un error real producido por el parser Twilight con sesión activa y desconectada; `Pending`; fin del stream; duplicados; cierre del canal con eventos pendientes y desaparición del consumidor. También verifican que `READY` sigue entregando el usuario al handler existente. No abren sockets ni necesitan credenciales.

La matriz sintética reproduce estados descritos en el código de Twilight; no es una prueba de red contra Discord ni sustituye una inspección visual nativa. No se ejecuta la aplicación conectada ni se usa el escritorio del usuario. La compatibilidad con lectores de pantalla y la disposición visual real quedan pendientes de verificación nativa aislada.

Validación ejecutada:

- `cargo fmt --check`: correcto.
- `git diff --check`: correcto; Git avisa únicamente sobre conversión LF/CRLF de la configuración existente.
- `cargo check --locked`, con el helper MSVC facilitado por el coordinador y `CARGO_TARGET_DIR=D:/Proyectos_B/.cache/oxidecord-cargo-verify`, `CARGO_BUILD_JOBS=3`: correcto, código 0. Tres avisos deprecados de `fetch_update` en `src/platform/h264/mod.rs` y `src/platform/video/mod.rs`, fuera del diff; aviso de incompatibilidad futura de `proc-macro-error2`.
- `cargo test --locked discord::gateway::tests`, con el mismo entorno: correcto, código 0; 20 pruebas aprobadas, 0 fallidas, 50 filtradas. Incluye las siete nuevas pruebas de conexión y las trece existentes de dispatch.
- `cargo clippy --locked`, con el mismo entorno: correcto, código 0; 12 avisos en archivos no modificados (settings, H.264/video, panel de voz, audio y stream). Ningún aviso apunta al diff de esta feature.

Las comprobaciones usan un target de compilación aislado del ejecutable habitual.

## PR propuesto

`feat: mostrar el estado de conexión y reconexión`

Una sola feature: seguimiento del estado Twilight, transporte de eventos a Home, aviso discreto, pruebas y esta evidencia. No incluye cambios en preferencias, audio, updater, credenciales ni envío de mensajes. No se creó commit, push ni PR.

