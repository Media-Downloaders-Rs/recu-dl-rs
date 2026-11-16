# Recu DL

Descargador independiente para perfiles de Recu.me, por ejemplo `XXX`.

La aplicación recorre **Grabaciones** y **Clips** por separado, y muestra primero las grabaciones más recientes. Pulsa **Listar**, marca los archivos deseados o usa **Todos/Ninguno**, y luego **Descargar**. Si pulsas **Descargar** sin listar antes, descarga todos. El botón **Vista** abre una miniatura opcional en segundo plano sin detener la lista. Abre Chrome con un perfil exclusivo en `downloads/.recu-dl/browser-profile` para conservar la sesión de Recu.me. La primera vez, inicia sesión y resuelve cualquier comprobación del sitio en esa ventana. La aplicación consulta la fuente que el reproductor entrega a esa sesión. Si el sitio niega la reproducción o la cuenta alcanzó un límite, informa el motivo y deja el archivo pendiente para otro intento.

Las grabaciones se guardan como MP4 con video AV1 y audio AAC. La aplicacion descarga los segmentos HLS a un temporal recuperable y convierte con FFmpeg (`libsvtav1`, resolucion original, CRF 34, preset 6). Conserva el temporal si falla la conversion y publica el MP4 solo tras validar las pistas. En Windows limita FFmpeg a la mitad de los procesadores logicos disponibles (hasta ocho) y mantiene una cola acotada de cuatro trabajos para solapar descargas y conversiones. Si ya descargaste una grabacion `.ts`, vuelve a listarla: se convierte el archivo local. Los clips conservan el formato que entregue el reproductor. Para convertir grabaciones, coloca `ffmpeg.exe` con `libsvtav1` en `tools/` junto a `recu-dl.exe`, o instala FFmpeg en PATH.

## Compilar

Instala Rust estable, las herramientas MSVC para Windows y Chrome. Ejecuta:

```powershell
cargo build --release --locked
powershell -NoProfile -File tools/install-build.ps1
```

El ejecutable es `recu-dl.exe`; `abrir.cmd` lo inicia. Los archivos descargados, la sesión de Chrome y el estado local permanecen en `downloads/`, fuera de Git.

## Verificar

```powershell
cargo test --locked --offline
cargo fmt --check
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
powershell -File tools/verify-standalone.ps1
```

La descarga de la siguiente grabacion puede avanzar mientras FFmpeg convierte la anterior; se limita a una transferencia y una conversion simultaneas para evitar saturar el navegador y el procesador.

Si Chrome se cierra durante una descarga, la aplicacion reabre automaticamente su perfil dedicado y solicita una fuente nueva. Las cookies de Chrome se conservan en ese perfil; si Recu.me vence la sesion, hay que iniciar sesion otra vez en la ventana que aparezca. Tras dos desconexiones consecutivas, el resto del lote queda pendiente.

Antes de publicar, FFmpeg comprueba que la pista de video alcance practicamente el final del archivo. Si faltan mas de 10 segundos de imagen, conserva el temporal y deja la descarga pendiente. Los MP4 antiguos con ese defecto se detectan al volver a descargarlos y solo se reemplazan tras obtener una conversion valida.

## Licencia y uso responsable

El código propio se distribuye bajo la [licencia MIT](LICENSE). El propósito educativo y las responsabilidades de uso se describen en [USAGE_NOTICE.md](USAGE_NOTICE.md). Esta licencia no concede derechos sobre contenido ajeno.
