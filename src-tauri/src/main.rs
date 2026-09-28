//! Punto de entrada del binario: GUI normal o modo hijo de escaneo elevado.
//!
//! El atributo `windows_subsystem` evita que la app de release abra una
//! consola extra en Windows. No remover.
//!
//! Modo hijo (lo lanza el propio disky elevado con UAC):
//! `disky.exe --elevated-scan <root> --out <result.json> --db <snapshots.db>`
//! Escanea la raíz, escribe el snapshot en la base de datos y el resultado en
//! el JSON, y sale con 0 (ok) o 2 (fallo). Sin ventana, sin Tauri.
//!
//! Modo hijo multiunidad (un solo UAC para todas las unidades fijas):
//! `disky.exe --elevated-scan-all --out <result.jsonl> --db <snapshots.db>`
//! Recorre las unidades y **añade una línea JSONL** por unidad terminada.
//!
//! `--cancel-file <ruta>` es opcional: si se pasa, el hijo vigila ese archivo y
//! se cancela en cuanto exista (el padre no puede matar a un proceso elevado).
//!
//! Modo hijo del panel USN:
//! `disky.exe --elevated-usn C --out <result.json> --limit 200`
//! Lee los cambios recientes del journal de la unidad, reconstruye la ruta de
//! cada uno desde la MFT y escribe el JSON con el resultado. Sin ventana.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let slice = args.as_slice();

    if args.iter().any(|a| a == "--elevated-scan-all") {
        let out = arg_value(slice, "--out").unwrap_or_default();
        let db = arg_value(slice, "--db").unwrap_or_default();
        let cancel_file = arg_value(slice, "--cancel-file").unwrap_or_default();
        std::process::exit(disky_lib::elevated_scan_all(out, db, cancel_file));
    }

    if args.iter().any(|a| a == "--elevated-usn") {
        let letter = arg_value(slice, "--elevated-usn").unwrap_or_default();
        let out = arg_value(slice, "--out").unwrap_or_default();
        let limit = arg_value(slice, "--limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(disky_lib::USN_DEFAULT_LIMIT);
        std::process::exit(disky_lib::elevated_usn(letter, out, limit));
    }

    if args.iter().any(|a| a == "--elevated-scan") {
        let root = arg_value(slice, "--elevated-scan").unwrap_or_default();
        let out = arg_value(slice, "--out").unwrap_or_default();
        let db = arg_value(slice, "--db").unwrap_or_default();
        let cancel_file = arg_value(slice, "--cancel-file").unwrap_or_default();
        std::process::exit(disky_lib::elevated_scan(root, out, db, cancel_file));
    }

    disky_lib::run();
}

/// Valor del argumento `flag` en la línea de comandos (el que le sigue).
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}
