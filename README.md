# disky — ¿qué creció en mi disco?

WinDirStat te dice cuánto pesa cada carpeta *hoy*. disky te dice **qué creció
desde la semana pasada y por culpa de quién**: *"Discord creció 6 GB en 3 días"*.

Solo lectura, 100% local, sin servidores.

## Capturas

![Vista completa de disky](docs/screenshots/app-completa.png)

![Treemap: el área de cada rectángulo es su tamaño; verde creció, naranja se encogió](docs/screenshots/treemap.png)

![Timeline de crecimiento comparado](docs/screenshots/timeline-crecimiento.png)

## Estado

✅ **MVP completo: escaneo sin admin + UAC + treemap + timeline.**

- ✅ Pipeline end-to-end funcional: volúmenes reales (Win32) → core → Tauri → UI.
- ✅ Parser de registros `USN_RECORD_V2` probado con buffers sintéticos.
- ⚠️ Hallazgo del spike: leer la MFT/journal exige **proceso elevado** — las
  pruebas de integración reales quedan `#[ignore]` localmente y corren en CI.
- ✅ **Escaneo sin admin**: walker portable en post-orden (`std::fs`) que emite
  cada directorio con el roll-up de su subárbol, cancelable y con progreso en
  vivo por eventos.
- ✅ **Snapshots en SQLite** (`rusqlite`, WAL): escritura atómica (invisible
  hasta `finish`; rollback si se cancela), prune de los últimos 10 por raíz,
  esquema versionado (`user_version = 1`).
- ✅ **Diff "¿qué creció?"**: comparación de los dos snapshots de una raíz vía
  `match_by_path` + `growth_ranking` (las mismas funciones puras del dominio).
- ✅ **Escaneo rápido con UAC**: disky se relanza a sí mismo elevado
  (`--elevated-scan`), guarda el snapshot desde el hijo y reporta el resultado
  por JSON; cubre las carpetas protegidas que el walk normal no puede leer.
- ✅ **Drill-down**: clic en cualquier carpeta del ranking para ver el crecimiento
  de sus hijos directos (navegable en profundidad, con "volver").
- ✅ **Treemap squarify** (Bruls et al. 2000): implementación pura en el core
  (probada: cobertura sin solapes, aspect ratios acotados), SVG en el frontend
  con colores por crecimiento, breadcrumb y navegación por clic.
- ✅ **Timeline multi-línea**: la serie temporal de la carpeta vista y sus hijos
  más grandes, en un solo gráfico comparable (escala común, leyenda, tooltips).

## Guía de usuario

### Primeros 5 minutos

1. Escribe una ruta en el campo de escaneo (ej. `C:\Users\tu-usuario\Downloads`).
2. Pulsa **Escanear** y espera la barra de progreso.
3. Crea, borra o agranda algunos archivos de esa carpeta.
4. Escanea **la misma ruta** otra vez → aparece todo lo demás.
5. Lee **¿Qué creció?**, explora el **treemap** y mira el **timeline**.

Con dos escaneos de la misma raíz, disky tiene todo lo que necesita.

### Volúmenes montados

Tabla informativa de arranque: unidad, tipo (fija, extraíble, remota…),
etiqueta, uso y porcentaje. No requiere acción.

```
Unidad   Tipo   Etiqueta   Uso                %
C:       Fija   Windows    412 GB / 931 GB    44%
D:       Fija   Datos      1.2 TB / 1.8 TB    67%
```

### Escaneo

```
[ C:\Users\tu-usuario\Downloads ] [Escanear] [Escaneo rápido (admin)] [Cancelar]
  ▓▓▓▓▓▓▓▓▓░░░░░░░  12.480 carpetas · 340.112 archivos · 18,4 GB leídos
```

- **Escanear** (sin admin): recorre el árbol con permisos normales y guarda un
  snapshot en SQLite. Muestra progreso en vivo y se puede **cancelar** sin
  dejar rastro (el snapshot nunca se llega a guardar a medias).
- **Escaneo rápido (admin)**: Windows pide el permiso UAC y disky se relanza
  elevado para leer también las carpetas protegidas del sistema. No muestra
  progreso ni se puede cancelar mientras corre (dura poco).
- Las carpetas sin permiso **no abortan nada**: se cuentan como *errores de
  lectura* y aparecen en la tabla de escaneos guardados.
- Se conservan los **últimos 10 escaneos por raíz**; los más viejos se podan
  solos. La BD vive en `%APPDATA%\com.breiner.disky\snapshots.db`.

### ¿Qué creció?

La respuesta de disky, ordenada por crecimiento: la comparación entre los dos
escaneos más recientes de la raíz escaneada.

```
Carpeta                          Antes    Ahora      Δ      Por día
...\Downloads\Torrents           3,1 GB   6,4 GB  +3,3 GB   +820 MB
...\Cache                        42 B     140 B     +98 B    +25 B
...\Temp\disky-demo\Docs         5 B      0 B        −5 B    −2 B
```

- Δ en **verde** creció, en **naranja** se encogió.
- **Por día** normaliza el delta a la distancia entre escaneos.
- **Clic en cualquier fila** → drill-down: los hijos directos de esa carpeta,
  con su propio delta (y así en profundidad, con enlace «↑ volver»).

### Treemap

```
┌────────────────┬───────┬──────────────────┐
│                │ Cache │                  │
│   Torrents     │ (verde│     Steam        │
│   (grande)     │   🟢) │                  │
│                ├───────┴──────────────────┤
├────────────────┴───────┬──────────────────┤
│  [archivos]            │  Docs (naranja)  │
└────────────────────────┴──────────────────┘
raíz › Torrents › 2026-09            ← breadcrumb
```

- Cada rectángulo es un hijo directo; el **área = tamaño** (algoritmo
  *squarify*, rectángulos lo más cuadrados posible).
- **Clic para entrar** a una carpeta; breadcrumb navegable para volver.
- El nodo sintético `[archivos]` agrupa los ficheros sueltos de la carpeta.
- **Verde** = creció desde el escaneo anterior, **naranja** = se encogió,
  gris = sin cambios. Se refresca solo tras cada escaneo.

### Timeline

```
      224 ┤                          ●── raíz
      140 ┤            ●─────────●──┬── Cache
        5 ┤ ●━━━━━━━━●              └── Docs
          └────────────────────────────
            esc.1      esc.2     esc.3
```

- Una línea por carpeta: la que estás viendo (según el treemap/breadcrumb) y
  sus hasta 5 hijos más grandes, con **escala temporal y de bytes comunes**.
- Pasa el mouse por un punto: fecha, tamaño y delta respecto al punto anterior.
- Leyenda bajo el gráfico con el color de cada carpeta.
- Necesita **al menos dos escaneos** para dibujar; las series sin suficientes
  puntos se descartan sin romper el resto.

### Escaneos guardados

Historial de la raíz escaneada (raíz, fecha, uso medido, archivos, errores de
lectura). Útil para confirmar cuándo se tomó cada snapshot.

### USN Journal (experimental)

Panel del spike original de NTFS: estado del Change Journal y últimos cambios
detectados por el kernel, por letra de unidad. Requiere permisos de
administrador; es exploratorio y no alimenta (todavía) las demás secciones.

### Preguntas frecuentes

**La UI dice «Aún no hay escaneos de esta raíz» pero yo escaneé esa carpeta.**
Comprueba que la ruta sea exactamente la misma. Desde la versión con
normalización de separadores, `C:/x` y `C:\x` son equivalentes; antes de ese
arreglo, los escaneos hechos desde consola podían quedar bajo otra clave.

**Windows marcó el instalador como sospechoso (SmartScreen).**
El instalador no está firmado con certificado de código (cuesta dinero).
«Más información» → «Ejecutar de todas formas». Es molestia, no bloqueo.

**Cancelé el UAC del escaneo rápido.**
No pasa nada: disky lo detecta y te lo dice; el escaneo normal sigue disponible.

**¿Puede borrar o mover archivos?** No. disky es **solo lectura** por diseño:
escanea, compara y muestra.

### Capturas de pantalla

Genera las capturas de cada sección con el script incluido (necesita la app
abierta):

```powershell
powershell -File scripts/captura.ps1 -Nombre treemap   # → docs/screenshots/treemap.png
```

## Roadmap

1. ~~Spike USN Journal~~ ✅ — parser + FSCTL probados; límite de elevación documentado
2. ~~Escaneo base sin admin + snapshot en SQLite~~ ✅ — walker, store atómico y diff
3. ~~Escaneo rápido con UAC~~ ✅ — hijo elevado con `--elevated-scan`
4. ~~Treemap squarify~~ ✅ — core puro + SVG interactivo con breadcrumb
5. ~~Timeline de crecimiento~~ ✅ — serie por carpeta + comparación multi-línea
6. Agente elevado compartido con Frostbyte (opción B), a futuro

## Arquitectura (hexagonal)

| Pieza | Rol |
|---|---|
| `crates/disky-core` | Dominio puro + adaptadores de plataforma. Sin UI, sin IPC, 100% testeable |
| `src-tauri` | Shell delgado: traduce comandos IPC a llamadas del core, nada más |
| `src/` | Frontend TypeScript (vanilla por ahora) |

**Regla de oro:** la lógica vive en el core; el shell traduce, no decide.

## Desarrollo

```bash
npm install
npm run tauri dev
```

## Distribución

```bash
npm run tauri build        # genera instalador NSIS + ejecutable
```

Artefactos en `target/release/bundle/nsis/`. Nota: sin certificado de firma
código, SmartScreen mostrará una advertencia la primera vez — es molestia,
no bloqueo (ver estrategia de costos en el backlog).

### Releases automáticas (CI)

Al empujar un tag de versión se compila y publican los instaladores solos:

```bash
git tag v0.1.0 && git push origin v0.1.0
# → GitHub Actions compila en windows-latest y adjunta a la release:
#   disky_0.1.0_x64-setup.exe (NSIS) y disky_0.1.0_x64_en-US.msi
```

El workflow (`.github/workflows/release.yml`) valida que el tag coincida con
la versión de `tauri.conf.json` antes de compilar (fail fast) y genera las
notas de release automáticamente.

## Calidad

```bash
cargo fmt --all -- --check                               # formato
cargo clippy --workspace --all-targets -- -D warnings    # pedantic + deny
cargo test -p disky-core                                 # tests (los de integración: --include-ignored, elevado)
npx tsc --noEmit                                         # typecheck del frontend
```

CI (`.github/workflows/ci.yml`): runner Windows elevado que corre los tests de
integración con `--include-ignored`; frontend en runner separado.
Releases (`.github/workflows/release.yml`): tag `v*` → instaladores NSIS/MSI
adjuntos a la GitHub Release.
