//! Build script de Tauri: genera el contexto de la app e integra recursos.
//!
//! Reparto de recursos (una sola copia por binario): `tauri-build` embebe
//! iconos y versión sin manifiesto, y este script compila `app-manifest.rc`
//! (solo manifiesto comctl32 v6, copia exacta del que trae Tauri) enlazado en
//! todos los targets. Así el binario suma tipos distintos y los tests obtienen
//! el manifiesto que necesitan para no morir con `STATUS_ENTRYPOINT_NOT_FOUND`
//! (comctl32 v5 no exporta `TaskDialogIndirect`). Pasar el `.rsrc` completo de
//! Tauri a todos los targets duplicaba VERSION y tumbaba a MSVC nuevos
//! (CVT1100 + LNK1123).

fn main() {
    build_tauri_without_manifest();
    link_manifest_everywhere();
}

/// Corre `tauri-build` con codegen/capacidades/iconos/versión pero SIN el
/// manifiesto (ya lo aporta `link_manifest_everywhere`).
fn build_tauri_without_manifest() {
    let attrs = tauri_build::Attributes::new()
        .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
    if tauri_build::try_build(attrs).is_err() {
        // Si la variante falla por lo que sea, comportamiento histórico exacto.
        tauri_build::build();
    }
}

/// Compila `app-manifest.rc` y lo enlaza en todos los targets (bins, libs y
/// tests). Solo en Windows: en otros SO no hay manifiestos ni comctl32.
fn link_manifest_everywhere() {
    if std::env::var("TARGET").is_ok_and(|target| target.contains("windows")) {
        let _ = embed_resource::compile_for_everything("app-manifest.rc", embed_resource::NONE);
    }
}
