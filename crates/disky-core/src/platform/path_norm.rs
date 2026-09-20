//! Normalización de separadores de ruta: la clave canónica del almacenamiento.
//!
//! La misma carpeta puede llegar escrita como `C:/a/b` (bash, CLI, Git) o
//! `C:\a\b` (UI de Windows). Para que ambas hablen de **el mismo** snapshot en
//! la BD, el adaptador `SQLite` normaliza toda ruta al separador nativo del SO
//! (canónico en Windows, único separador válido en Unix) **antes** de escribir
//! o filtrar. Así existe una sola forma por ruta y las consultas siempre
//! encuentran lo guardado.
//!
//! No se toca nada más: mayúsculas, `\\?\` ni unidades — la comparación exacta
//! de `String` es deliberada (simple, testeada y suficiente para el diff).

/// Sustituye los separadores `/` por el separador nativo del SO.
///
/// En Unix no transforma nada: `/` es el único separador válido y cualquier
/// `\` sería parte legítima de un nombre de archivo.
#[must_use]
pub fn normalize_path_separators(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_path_separators;

    #[cfg(windows)]
    #[test]
    fn converts_forward_slashes_on_windows() {
        assert_eq!(
            normalize_path_separators("C:/Users/Breiner/Projects"),
            "C:\\Users\\Breiner\\Projects"
        );
        assert_eq!(normalize_path_separators("C:/Mix/Sub"), "C:\\Mix\\Sub");
    }

    #[cfg(windows)]
    #[test]
    fn keeps_native_backslashes_untouched() {
        assert_eq!(
            normalize_path_separators("C:\\Users\\Breiner"),
            "C:\\Users\\Breiner"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn keeps_paths_untouched_on_unix() {
        assert_eq!(
            normalize_path_separators("/home/user/Projects"),
            "/home/user/Projects"
        );
    }
}
