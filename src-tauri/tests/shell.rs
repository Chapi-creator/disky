//! Test de integración del shell: el contrato público que el proceso elevado y
//! el padre comparten (la línea JSONL por unidad).
//!
//! Vive aquí —y no en los tests unitarios de `disky_lib`— porque el protocolo
//! cruza procesos y merece un binario de test propio: serializa con la misma
//! `serde` que el hijo y comprueba que el padre lo vuelve a leer intacto.
#![allow(clippy::expect_used)]

use disky_lib::UnitResult;

/// Una unidad correcta viaja como una línea JSON y vuelve intacta.
#[test]
fn unit_result_round_trips_through_jsonl() {
    let unit = UnitResult {
        letter: "C:\\".to_owned(),
        error: None,
    };
    let line = serde_json::to_string(&unit).expect("serializar UnitResult");
    assert!(
        !line.contains('\n'),
        "una línea JSONL no puede llevar salto"
    );

    let back: UnitResult = serde_json::from_str(&line).expect("deserializar UnitResult");
    assert_eq!(back.letter, "C:\\");
    assert_eq!(back.error, None);
}

/// Una unidad fallida conserva el motivo para que el padre lo muestre.
#[test]
fn unit_result_keeps_the_failure_reason() {
    let unit = UnitResult {
        letter: "D:\\".to_owned(),
        error: Some("Leer la MFT requiere permisos de administrador".to_owned()),
    };
    let line = serde_json::to_string(&unit).expect("serializar UnitResult");
    let back: UnitResult = serde_json::from_str(&line).expect("deserializar UnitResult");
    assert_eq!(back.letter, "D:\\");
    assert_eq!(
        back.error.as_deref(),
        Some("Leer la MFT requiere permisos de administrador")
    );
}
