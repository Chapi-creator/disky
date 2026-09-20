# disky — ¿qué creció en mi disco?

WinDirStat te dice cuánto pesa cada carpeta *hoy*. disky te dice **qué creció
desde la semana pasada y por culpa de quién**: *"Discord creció 6 GB en 3 días"*.

Solo lectura, 100% local, sin servidores.

## Estado

✅ **MVP funcional: fallback sin admin (opción C) + spike USN.**

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
- ✅ **Escaneo rápido con UAC (opción A)**: disky se relanza a sí mismo elevado
  (`--elevated-scan`), guarda el snapshot desde el hijo y reporta el resultado
  por JSON; cubre las carpetas protegidas que el walk normal no puede leer.
- ✅ **Drill-down**: clic en cualquier carpeta del ranking para ver el crecimiento
  de sus hijos directos (navegable en profundidad, con "volver").
- ✅ **Treemap squarify** (Bruls et al. 2000): implementación pura en el core
  (probada: cobertura sin solapes, aspect ratios acotados), SVG en el frontend
  con colores por crecimiento, breadcrumb y navegación por clic.
- 🔲 Siguiente: timeline de crecimiento en el tiempo y drill-down por archivo.

## Roadmap

1. ~~Spike USN Journal~~ ✅ — parser + FSCTL probados; límite de elevación documentado
2. ~~Escaneo base sin admin + snapshot en SQLite~~ ✅ — walker, store atómico y diff
3. ~~Escaneo rápido con UAC (opción A)~~ ✅ — hijo elevado con `--elevated-scan`
4. ~~Treemap squarify~~ ✅ — core puro + SVG interactivo con breadcrumb
5. **Timeline**: el crecimiento a lo largo del tiempo, la pieza visual que falta
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

## Calidad

```bash
cargo fmt --all -- --check                               # formato
cargo clippy --workspace --all-targets -- -D warnings    # pedantic + deny
cargo test -p disky-core                                 # tests (los de integración: --include-ignored, elevado)
npx tsc --noEmit                                         # typecheck del frontend
```

CI (`.github/workflows/ci.yml`): runner Windows elevado que corre los tests de
integración con `--include-ignored`; frontend en runner separado.
