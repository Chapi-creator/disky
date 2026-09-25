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

/// Sustituye los separadores `/` por el separador nativo del SO y elimina los
/// separadores finales (`C:\Users\\` → `C:\Users`), para que una raíz tipeada
/// como `C:\Users\` coincida con la propia carpeta `C:\Users` vista como hijo
/// de otro escaneo. La única ruta que conserva su barra final es la raíz de un
/// volumen (`C:\`). No toca `\\?\` ni los prefijos UNC.
///
/// En Unix no transforma nada: `/` es el único separador válido y cualquier
/// `\` sería parte legítima de un nombre de archivo.
#[must_use]
pub fn normalize_path_separators(path: &str) -> String {
    if cfg!(windows) {
        let converted = path.replace('/', "\\");
        let trimmed = converted.trim_end_matches('\\');
        if trimmed.is_empty() {
            return converted; // "\\" o "\\" sin ruta: sin barra que decidir
        }
        // `C:`, `D:`, … → raíz de volumen con su barra de raíz.
        if trimmed.len() == 2 && trimmed.as_bytes()[1] == b':' {
            format!("{trimmed}\\")
        } else {
            trimmed.to_owned()
        }
    } else {
        path.to_owned()
    }
}

/// Prefijo canónico para consultar los **hijos directos** de `path`: añade el
/// separador final (salvo raíces de volumen, que ya lo tienen). Garantiza que
/// un filtro/consulta prefijada no arrastre hermanos como `C:\Users2` al pedir
/// los hijos de `C:\Users`.
#[must_use]
pub fn child_prefix(path: &str) -> String {
    let path = normalize_path_separators(path);
    if path.ends_with(std::path::MAIN_SEPARATOR) {
        path
    } else {
        format!("{path}{}", std::path::MAIN_SEPARATOR)
    }
}

#[cfg(test)]
mod tests {
    use super::child_prefix;
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
    fn collapses_trailing_separators() {
        assert_eq!(normalize_path_separators("C:\\\\"), "C:\\");
        assert_eq!(normalize_path_separators("C:\\Users\\\\"), "C:\\Users");
        assert_eq!(normalize_path_separators("C:\\Users"), "C:\\Users");
        assert_eq!(normalize_path_separators("C:\\"), "C:\\");
    }

    #[cfg(windows)]
    #[test]
    fn strips_trailing_separators_except_volume_root() {
        assert_eq!(
            normalize_path_separators("C:\\Users\\Breiner\\"),
            "C:\\Users\\Breiner"
        );
        assert_eq!(normalize_path_separators("C:\\"), "C:\\");
        assert_eq!(normalize_path_separators("C:"), "C:\\");
        assert_eq!(
            normalize_path_separators("\\\\server\\share\\"),
            "\\\\server\\share"
        );
    }

    #[cfg(windows)]
    #[test]
    fn child_prefix_separates_siblings() {
        assert_eq!(child_prefix("C:\\Users"), "C:\\Users\\");
        assert_eq!(child_prefix("C:\\Users\\"), "C:\\Users\\");
        assert_eq!(child_prefix("C:/Users"), "C:\\Users\\");
        assert_eq!(child_prefix("C:\\"), "C:\\");
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
