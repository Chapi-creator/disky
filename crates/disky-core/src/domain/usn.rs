//! Tipos y parseo puro de registros del USN Journal (Change Journal de NTFS).
//!
//! El parseo es byte-a-byte con `from_le_bytes`: sin `unsafe`, sin dependencia
//! de alineación, y los tests usan buffers sintéticos para validar el parser
//! contra la especificación de `winioctl.h` sin tocar el kernel.
//!
//! Referencia: `USN_RECORD_V2` y `USN_JOURNAL_DATA_V0` en la documentación de
//! Windows (Change Journal).

use serde::{Deserialize, Serialize};

/// Tamaño mínimo de un registro USN V2: los 64 bytes de campos fijos.
pub const USN_RECORD_V2_MIN_LEN: usize = 64;

/// Distancia del epoch FILETIME (1601) al epoch UNIX (1970), en unidades de 100 ns.
/// Referencia: `11_644_473_600` segundos × `10_000_000` unidades/segundo.
const FILETIME_TO_UNIX_100NS: i64 = 116_444_736_000_000_000;

/// Convierte un FILETIME (100 ns desde 1601) a segundos UNIX.
#[must_use]
pub fn filetime_to_unix(filetime: i64) -> i64 {
    (filetime - FILETIME_TO_UNIX_100NS) / 10_000_000
}

/// Bits de "reason" del USN Journal relevantes para crecimiento de disco.
///
/// Constantes tomadas de `winioctl.h` (`USN_REASON_*`); solo las que
/// importan para rastrear cambios de tamaño y ciclo de vida de archivos.
pub mod reason {
    /// Se añadió contenido al archivo.
    pub const DATA_EXTEND: u32 = 0x0000_0002;
    /// Se truncó contenido del archivo.
    pub const DATA_TRUNCATION: u32 = 0x0000_0004;
    /// El archivo fue creado.
    pub const FILE_CREATE: u32 = 0x0000_0100;
    /// El archivo fue borrado.
    pub const FILE_DELETE: u32 = 0x0000_0200;
    /// Renombrado: este registro lleva el nombre antiguo.
    pub const RENAME_OLD_NAME: u32 = 0x0000_1000;
    /// Renombrado: este registro lleva el nombre nuevo.
    pub const RENAME_NEW_NAME: u32 = 0x0000_2000;
    /// Marcador de cierre: el registro acumulado quedó confirmado.
    pub const CLOSE: u32 = 0x8000_0000;

    /// Pares (bit, etiqueta) en orden de bit ascendente — estable para la UI.
    const TABLE: [(u32, &str); 7] = [
        (DATA_EXTEND, "extend"),
        (DATA_TRUNCATION, "truncation"),
        (FILE_CREATE, "create"),
        (FILE_DELETE, "delete"),
        (RENAME_OLD_NAME, "rename_old"),
        (RENAME_NEW_NAME, "rename_new"),
        (CLOSE, "close"),
    ];

    /// Traduce bits de reason a etiquetas legibles, en orden de bit ascendente.
    #[must_use]
    pub fn describe(flags: u32) -> Vec<&'static str> {
        TABLE
            .iter()
            .filter(|(bit, _)| flags & bit != 0)
            .map(|(_, label)| *label)
            .collect()
    }
}

/// Un registro del USN Journal ya parseado.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[must_use]
pub struct JournalRecord {
    /// FRN del archivo (número de registro en la MFT).
    pub frn: u64,
    /// FRN del directorio padre.
    pub parent_frn: u64,
    /// Secuencia del registro dentro del journal.
    pub usn: i64,
    /// Momento del cambio (segundos UNIX).
    pub timestamp_unix: i64,
    /// Bits de reason crudos; ver [`reason`].
    pub reasons: u32,
    /// Etiquetas legibles de `reasons` (en orden de bit).
    pub reason_labels: Vec<String>,
    /// Nombre del archivo en el momento del registro.
    pub file_name: String,
}

/// Parsea un registro USN V2 que inicia en el byte 0 de `buf`.
///
/// Devuelve `(registro, bytes_consumidos)`; `None` si está incompleto,
/// tiene versión desconocida o campos fuera de rango. Nunca hace panic.
#[must_use]
pub fn parse_usn_record_v2(buf: &[u8]) -> Option<(JournalRecord, usize)> {
    if buf.len() < USN_RECORD_V2_MIN_LEN {
        return None;
    }

    let record_len = u32::from_le_bytes(buf[0..4].try_into().ok()?) as usize;
    let major = u16::from_le_bytes(buf[4..6].try_into().ok()?);
    if major != 2 || record_len < USN_RECORD_V2_MIN_LEN || record_len > buf.len() {
        return None;
    }

    let frn = u64::from_le_bytes(buf[8..16].try_into().ok()?);
    let parent_frn = u64::from_le_bytes(buf[16..24].try_into().ok()?);
    let usn = i64::from_le_bytes(buf[24..32].try_into().ok()?);
    let filetime = i64::from_le_bytes(buf[32..40].try_into().ok()?);
    let reasons = u32::from_le_bytes(buf[40..44].try_into().ok()?);
    let name_len = u32::from_le_bytes(buf[56..60].try_into().ok()?) as usize;
    let name_off = u32::from_le_bytes(buf[60..64].try_into().ok()?) as usize;

    let name_end = name_off.checked_add(name_len)?;
    if name_end > record_len || !name_off.is_multiple_of(2) || !name_end.is_multiple_of(2) {
        return None;
    }
    let name_u16: Vec<u16> = buf[name_off..name_end]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();

    Some((
        JournalRecord {
            frn,
            parent_frn,
            usn,
            timestamp_unix: filetime_to_unix(filetime),
            reasons,
            reason_labels: reason::describe(reasons)
                .into_iter()
                .map(str::to_owned)
                .collect(),
            file_name: String::from_utf16_lossy(&name_u16),
        },
        record_len,
    ))
}

/// Parsea registros USN V2 contiguos desde el byte 0 de `buf`.
///
/// Se detiene ante relleno a cero (`RecordLength == 0`), registro incompleto
/// o inválido. Los buffers deben venir rellenados a cero para que el corte
/// sea determinista.
#[must_use]
pub fn parse_usn_records(buf: &[u8]) -> Vec<JournalRecord> {
    let mut records = Vec::new();
    let mut offset = 0;
    while buf.len() - offset >= 4 {
        let Ok(len_arr) = buf[offset..offset + 4].try_into() else {
            break;
        };
        let record_len = u32::from_le_bytes(len_arr) as usize;
        // Relleno a cero tras la región escrita por el kernel, o registro
        // imposible: fin del lote.
        if record_len == 0 || record_len > buf.len() - offset {
            break;
        }
        let Some((record, _consumed)) = parse_usn_record_v2(&buf[offset..]) else {
            break;
        };
        records.push(record);
        offset += record_len;
    }
    records
}

/// Parsea un lote crudo de `FSCTL_READ_USN_JOURNAL`.
///
/// Layout del buffer: 8 bytes de `NextUsn` seguidos de registros contiguos.
/// El llamador debe pasar un buffer **rellenado a cero** (así el final no
/// escrito corta el parseo de forma determinista: `RecordLength == 0`).
#[must_use]
pub fn parse_usn_batch(buf: &[u8]) -> (i64, Vec<JournalRecord>) {
    let Some(next_arr) = buf.first_chunk::<8>() else {
        return (0, Vec::new());
    };
    let next_usn = i64::from_le_bytes(*next_arr);
    (next_usn, parse_usn_records(&buf[8..]))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::float_cmp)]
    #![allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]

    use super::*;
    use pretty_assertions::assert_eq;

    /// Construye un registro USN V2 sintético fiel al layout de `winioctl.h`.
    fn fake_record(
        name: &str,
        frn: u64,
        parent: u64,
        usn: i64,
        filetime: i64,
        reasons: u32,
    ) -> Vec<u8> {
        let name_bytes: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let record_len = u32::try_from(USN_RECORD_V2_MIN_LEN + name_bytes.len())
            .expect("registro de prueba pequeño");

        let mut buf = Vec::with_capacity(record_len as usize);
        buf.extend_from_slice(&record_len.to_le_bytes()); // 0..4
        buf.extend_from_slice(&2u16.to_le_bytes()); // 4..6  Major
        buf.extend_from_slice(&0u16.to_le_bytes()); // 6..8  Minor
        buf.extend_from_slice(&frn.to_le_bytes()); // 8..16
        buf.extend_from_slice(&parent.to_le_bytes()); // 16..24
        buf.extend_from_slice(&usn.to_le_bytes()); // 24..32
        buf.extend_from_slice(&filetime.to_le_bytes()); // 32..40
        buf.extend_from_slice(&reasons.to_le_bytes()); // 40..44
        buf.extend_from_slice(&0u32.to_le_bytes()); // 44..48 SourceInfo
        buf.extend_from_slice(&0u32.to_le_bytes()); // 48..52 SecurityId
        buf.extend_from_slice(&0u32.to_le_bytes()); // 52..56 FileAttributes
        let name_len = u32::try_from(name_bytes.len()).expect("nombre de prueba pequeño");
        buf.extend_from_slice(&name_len.to_le_bytes()); // 56..60
        buf.extend_from_slice(&64u32.to_le_bytes()); // 60..64 FileNameOffset
        buf.extend_from_slice(&name_bytes);
        buf
    }

    #[test]
    fn parses_a_wellformed_record() {
        // FILETIME de 2021-01-01 00:00:00 UTC → 1_609_459_200 UNIX.
        const FT_2021: i64 = 132_539_328_000_000_000;
        let buf = fake_record(
            "facturas.pdf",
            0xDEAD_BEEF,
            0x5,
            42,
            FT_2021,
            reason::FILE_CREATE | reason::CLOSE,
        );

        let (record, consumed) = parse_usn_record_v2(&buf).expect("registro bien formado");

        assert_eq!(consumed, buf.len());
        assert_eq!(record.frn, 0xDEAD_BEEF);
        assert_eq!(record.parent_frn, 0x5);
        assert_eq!(record.usn, 42);
        assert_eq!(record.timestamp_unix, 1_609_459_200);
        assert_eq!(record.file_name, "facturas.pdf");
        assert_eq!(record.reason_labels, ["create", "close"]);
    }

    #[test]
    fn rejects_truncated_record() {
        let mut buf = fake_record("a.txt", 1, 1, 1, 0, 0);
        buf.truncate(40); // menos que los 64 bytes mínimos

        assert_eq!(parse_usn_record_v2(&buf), None);
    }

    #[test]
    fn rejects_unknown_major_version() {
        let mut buf = fake_record("a.txt", 1, 1, 1, 0, 0);
        buf[4..6].copy_from_slice(&3u16.to_le_bytes()); // V3

        assert_eq!(parse_usn_record_v2(&buf), None);
    }

    #[test]
    fn rejects_name_offset_out_of_bounds() {
        let mut buf = fake_record("a.txt", 1, 1, 1, 0, 0);
        // FileNameOffset (60..64) más allá del propio registro.
        buf[60..64].copy_from_slice(&10_000u32.to_le_bytes());

        assert_eq!(parse_usn_record_v2(&buf), None);
    }

    #[test]
    fn parses_batch_with_next_usn_and_stops_at_zero_fill() {
        let rec1 = fake_record("uno.log", 1, 2, 10, 0, reason::DATA_EXTEND);
        let rec2 = fake_record("dos.log", 3, 4, 11, 0, reason::FILE_DELETE | reason::CLOSE);

        let mut batch = Vec::new();
        batch.extend_from_slice(&999i64.to_le_bytes()); // NextUsn del lote
        batch.extend_from_slice(&rec1);
        batch.extend_from_slice(&rec2);
        batch.resize(batch.len() + 16, 0); // relleno de la región no escrita

        let (next_usn, records) = parse_usn_batch(&batch);

        assert_eq!(next_usn, 999);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].file_name, "uno.log");
        assert_eq!(records[1].file_name, "dos.log");
        assert_eq!(records[1].reason_labels, ["delete", "close"]);
    }

    #[test]
    fn parses_empty_batch() {
        assert_eq!(parse_usn_batch(&[]), (0, Vec::new()));
        assert_eq!(parse_usn_batch(&[0; 7]), (0, Vec::new()));
        let only_usn = 77i64.to_le_bytes().to_vec();
        assert_eq!(parse_usn_batch(&only_usn), (77, Vec::new()));
    }

    #[test]
    fn filetime_conversion_is_monotonic() {
        let base = 132_539_328_000_000_000i64; // 2021-01-01 00:00:00 UTC
        assert_eq!(filetime_to_unix(base), 1_609_459_200);
        assert_eq!(filetime_to_unix(base + 10_000_000), 1_609_459_201);
    }
}
