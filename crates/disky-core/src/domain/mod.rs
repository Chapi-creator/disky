//! Tipos del dominio y motor de crecimiento de disco.
//!
//! Módulo 100% puro: sin I/O, sin `unsafe`, sin dependencias de plataforma.
//! Todo lo que decide *qué significa* que una carpeta haya crecido vive aquí,
//! de modo que los adaptadores (`WinAPI`, USN Journal, UI) solo aportan datos.

pub mod scan;
pub mod usn;

use std::collections::BTreeMap;

/// Tipo de unidad según `GetDriveTypeW`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum DriveKind {
    /// Disco fijo (HDD/SSD/NVMe).
    Fixed,
    /// Unidad extraíble (USB, SD).
    Removable,
    /// Unidad de red.
    Remote,
    /// Unidad óptica.
    CdRom,
    /// Disco RAM u otro tipo no clasificado.
    RamDisk,
    /// Tipo desconocido para la plataforma.
    Unknown,
}

/// Un volumen montado con letra de unidad.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[must_use]
pub struct Volume {
    /// Letra de unidad normalizada, p. ej. `"C:"`.
    pub letter: String,
    /// Etiqueta del volumen, si el sistema la expone.
    pub label: Option<String>,
    /// Capacidad total en bytes.
    pub total_bytes: u64,
    /// Bytes libres en este instante.
    pub free_bytes: u64,
    /// Tipo de unidad.
    pub kind: DriveKind,
}

impl Volume {
    /// Bytes usados (`total - free`).
    #[must_use]
    pub const fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }

    /// Porcentaje de uso (0.0–100.0), redondeado a un decimal.
    // u64→f64: para un porcentaje de UI la pérdida de precisión es irrelevante.
    #[allow(clippy::cast_precision_loss)]
    #[must_use]
    pub fn usage_percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        let used = self.used_bytes();
        // `(used * 1000 / total) / 10` evita `f64` y mantiene un decimal exacto.
        (used * 1_000 / self.total_bytes) as f64 / 10.0
    }
}

/// Una medición puntual de cuánto espacio usa una ruta (carpeta o volumen).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[must_use]
pub struct UsageSample {
    /// Ruta medida, p. ej. `"C:\\Users\\Breiner\\AppData\\Local\\Discord"`.
    pub path: String,
    /// Cuándo se tomó la medición (timestamp UNIX, segundos).
    pub measured_at: i64,
    /// Tamaño en bytes en ese momento.
    pub size_bytes: u64,
}

/// Diferencia de tamaño entre dos muestras de la misma ruta.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[must_use]
pub struct GrowthReport {
    /// Ruta analizada.
    pub path: String,
    /// Tamaño en la medición más antigua.
    pub old_bytes: u64,
    /// Tamaño en la medición más reciente.
    pub new_bytes: u64,
    /// Crecimiento (`new - old`); negativo = la ruta se encogió.
    pub delta_bytes: i64,
    /// Duración entre mediciones, en segundos.
    pub elapsed_seconds: i64,
}

impl GrowthReport {
    /// Velocidad media de crecimiento en bytes por día.
    // La pérdida de precisión i64→f64 es aceptable: los bytes por día son
    // una magnitud orientativa para la UI, no para contabilidad exacta.
    #[allow(clippy::cast_precision_loss)]
    #[must_use]
    pub fn bytes_per_day(&self) -> f64 {
        if self.elapsed_seconds == 0 {
            return 0.0;
        }
        self.delta_bytes as f64 * 86_400.0 / self.elapsed_seconds as f64
    }
}

/// Error del dominio: la operación de crecimiento no puede hacerse con estas muestras.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrowthError {
    /// Las dos muestras no corresponden a la misma ruta.
    #[error("las muestras no son de la misma ruta: `{0}` vs `{1}`")]
    PathMismatch(String, String),
    /// Las mediciones están en orden inverso o son simultáneas.
    #[error("las mediciones deben estar en orden temporal (old: {old}, new: {new})")]
    TimeOrder {
        /// Timestamp de la medición antigua.
        old: i64,
        /// Timestamp de la medición nueva.
        new: i64,
    },
}

/// Compara dos mediciones de la **misma** ruta y produce el reporte de crecimiento.
///
/// `old` debe ser anterior en el tiempo a `new`.
///
/// # Errors
/// Devuelve [`GrowthError`] si las rutas difieren o el orden temporal es inválido.
pub fn growth_between(old: &UsageSample, new: &UsageSample) -> Result<GrowthReport, GrowthError> {
    if old.path != new.path {
        return Err(GrowthError::PathMismatch(
            old.path.clone(),
            new.path.clone(),
        ));
    }
    if new.measured_at <= old.measured_at {
        return Err(GrowthError::TimeOrder {
            old: old.measured_at,
            new: new.measured_at,
        });
    }

    // `i64::cast_unsigned` evita el truncamiento silencioso de `as` cuando
    // un tamaño supera `i64::MAX` (irreal hoy, pero el costo de ser correcto es cero).
    let old_bytes = i64::try_from(old.size_bytes).unwrap_or(i64::MAX);
    let new_bytes = i64::try_from(new.size_bytes).unwrap_or(i64::MAX);

    Ok(GrowthReport {
        path: old.path.clone(),
        old_bytes: old.size_bytes,
        new_bytes: new.size_bytes,
        delta_bytes: new_bytes.saturating_sub(old_bytes),
        elapsed_seconds: new.measured_at.saturating_sub(old.measured_at),
    })
}

/// Calcula los crecimientos de muchas rutas a la vez, descartando las inválidas.
///
/// Pensado para el flujo real: el store devuelve pares (antes, ahora) por ruta y
/// la UI quiere un ranking ordenado por `delta_bytes` descendente.
#[must_use]
pub fn growth_ranking(pairs: &[(&UsageSample, &UsageSample)]) -> Vec<GrowthReport> {
    let mut reports: Vec<GrowthReport> = pairs
        .iter()
        .filter_map(|(old, new)| growth_between(old, new).ok())
        .collect();
    reports.sort_by_key(|r| std::cmp::Reverse(r.delta_bytes));
    reports
}

/// Acumula bytes por carpeta de primer nivel bajo un prefijo dado.
///
/// Caso de uso real: pasar de "Discord creció 6 GB" a
/// "Discord creció 6 GB, sobre todo en `Cache` y `IndexedDB`".
/// Las rutas que no están bajo `prefix` se ignoran; el filtrado garantiza
/// no devolver la propia carpeta raíz ni fragmentos vacíos.
#[must_use]
pub fn rollup_by_child(prefix: &str, samples: &[UsageSample]) -> BTreeMap<String, u64> {
    let prefix = prefix.trim_end_matches(['\\', '/']);
    let mut rollup: BTreeMap<String, u64> = BTreeMap::new();

    for sample in samples {
        let Some(rest) = sample
            .path
            .strip_prefix(prefix)
            .map(|rest| rest.trim_start_matches(['\\', '/']))
            .filter(|rest| !rest.is_empty())
        else {
            continue;
        };
        let Some(first_component) = rest.split(['\\', '/']).next().filter(|c| !c.is_empty()) else {
            continue;
        };
        *rollup
            .entry(format!("{prefix}\\{first_component}"))
            .or_default() += sample.size_bytes;
    }
    rollup
}

#[cfg(test)]
mod tests {
    // En tests, `expect` es idiomático (queremos panic legible) y comparar
    // floats exactos está bien porque los valores son representables exactos.
    #![allow(clippy::expect_used, clippy::float_cmp)]
    #![allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]

    use super::*;
    use pretty_assertions::assert_eq;

    const BASE: i64 = 1_700_000_000;

    fn sample(path: &str, at: i64, size: u64) -> UsageSample {
        UsageSample {
            path: path.to_owned(),
            measured_at: at,
            size_bytes: size,
        }
    }

    #[test]
    fn growth_detects_positive_delta() {
        let old = sample("C:\\Discord", BASE, 4 * GB);
        let new = sample("C:\\Discord", BASE + 3 * DAY, 10 * GB);

        let report = growth_between(&old, &new).expect("muestras válidas");

        assert_eq!(report.delta_bytes, 6 * GB as i64);
        assert_eq!(report.elapsed_seconds, 3 * DAY);
        assert_eq!(report.bytes_per_day(), 2.0 * GB as f64);
    }

    #[test]
    fn growth_detects_shrink_as_negative() {
        let old = sample("C:\\Cache", BASE, 1_000);
        let new = sample("C:\\Cache", BASE + DAY, 400);

        let report = growth_between(&old, &new).expect("muestras válidas");

        assert_eq!(report.delta_bytes, -600);
        assert!(report.bytes_per_day() < 0.0);
    }

    #[test]
    fn growth_rejects_different_paths() {
        let old = sample("C:\\A", BASE, 1);
        let new = sample("C:\\B", BASE + DAY, 2);

        assert_eq!(
            growth_between(&old, &new),
            Err(GrowthError::PathMismatch("C:\\A".into(), "C:\\B".into()))
        );
    }

    #[test]
    fn growth_rejects_reversed_time() {
        let old = sample("C:\\A", BASE + DAY, 1);
        let new = sample("C:\\A", BASE, 2);

        assert_eq!(
            growth_between(&old, &new),
            Err(GrowthError::TimeOrder {
                old: BASE + DAY,
                new: BASE
            })
        );
    }

    #[test]
    fn ranking_sorts_by_delta_descending() {
        let a_old = sample("C:\\A", BASE, 100);
        let a_new = sample("C:\\A", BASE + DAY, 100 + 5 * GB);
        let b_old = sample("C:\\B", BASE, 100);
        let b_new = sample("C:\\B", BASE + DAY, 100 + GB);
        // Par inválido (rutas distintas) que el ranking debe descartar sin fallar.
        let bad_old = sample("C:\\X", BASE, 0);
        let bad_new = sample("C:\\Y", BASE + DAY, 9 * GB);

        let ranking = growth_ranking(&[(&b_old, &b_new), (&bad_old, &bad_new), (&a_old, &a_new)]);

        assert_eq!(ranking.len(), 2);
        assert_eq!(ranking[0].path, "C:\\A");
        assert_eq!(ranking[1].path, "C:\\B");
    }

    #[test]
    fn rollup_groups_first_level_children() {
        let samples = vec![
            sample("C:\\Discord\\Cache", BASE, 3 * GB),
            sample("C:\\Discord\\Cache\\v8", BASE, GB),
            sample("C:\\Discord\\IndexedDB", BASE, 2 * GB),
            sample("C:\\Otra\\Carpeta", BASE, 10 * GB),
        ];

        let rollup = rollup_by_child("C:\\Discord", &samples);

        assert_eq!(rollup.len(), 2);
        assert_eq!(rollup["C:\\Discord\\Cache"], 4 * GB);
        assert_eq!(rollup["C:\\Discord\\IndexedDB"], 2 * GB);
    }

    #[test]
    fn rollup_normalizes_trailing_separator() {
        let samples = vec![sample("C:\\Discord\\Cache", BASE, 500)];

        assert_eq!(
            rollup_by_child("C:\\Discord\\", &samples)["C:\\Discord\\Cache"],
            500
        );
    }

    #[test]
    fn volume_usage_percent_handles_zero_total() {
        let empty = Volume {
            letter: "X:".into(),
            label: None,
            total_bytes: 0,
            free_bytes: 0,
            kind: DriveKind::Unknown,
        };
        assert_eq!(empty.usage_percent(), 0.0);
    }

    const GB: u64 = 1_073_741_824;
    const DAY: i64 = 86_400;
}
