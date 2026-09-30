//! Build script de Tauri: genera el contexto de la app e integra recursos.

fn main() {
    tauri_build::build();
    link_tests_with_app_resource();
}

/// Enlaza en los binarios de test el mismo `.rsrc` (manifiesto, iconos, versión)
/// que `tauri-build` embebe en los binarios de la aplicación.
///
/// Sin ese recurso el proceso de test no lleva manifiesto, así que Windows lo
/// ata a `comctl32.dll` v5 (`System32`), que no exporta `TaskDialogIndirect`:
/// cargarlo es `STATUS_ENTRYPOINT_NOT_FOUND` (0xc0000139) y el harness de tests
/// no llega a arrancar («process didn't exit successfully»). El binario de la
/// app no lo sufre porque sí lleva el manifiesto de Common Controls v6. Aquí
/// solo se enlaza el recurso ya compilado, de modo que el manifiesto es
/// exactamente el mismo que el de la app.
fn link_tests_with_app_resource() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    // `embed-resource` nombra el recurso según la cadena de herramientas: objeto
    // COFF con nombre de archivo `.a` en GNU, biblioteca en MSVC.
    let resource = ["libresource.a", "resource.lib", "resource.o"]
        .iter()
        .map(|name| std::path::Path::new(&out_dir).join(name))
        .find(|path| path.exists());
    // `rustc-link-arg` (todos los targets) y no `rustc-link-arg-tests`: esta
    // última solo alcanza a los tests de integración, no a los unitarios de la
    // lib, que son justo los que crashean sin manifiesto. El binario recibe el
    // recurso dos veces (`-bins` de `tauri-build` + este arg), pero es el MISMO
    // archivo, así que el merge del linker deja el manifiesto correcto y solo
    // emite un aviso de recursos duplicados.
    match resource {
        Some(path) => println!("cargo:rustc-link-arg={}", path.display()),
        None => println!(
            "cargo:warning=no se encontró el recurso de la app; los tests pueden no arrancar en Windows"
        ),
    }
}
