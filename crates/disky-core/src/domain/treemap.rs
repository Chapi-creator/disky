//! Algoritmo *squarify* para el treemap (Bruls, Huizing & van Wijk, 2000).
//!
//! Convierte tamaños de carpetas en rectángulos lo más cuadrados posible:
//! cada fila se llena a lo largo del lado más corto del rectángulo restante y
//! crece item a item mientras mejore el peor aspect ratio. Es 100% puro:
//! entra una lista de (ruta, bytes) y sale geometría; el SVG lo pinta el
//! frontend y los deltas viajan como metadatos adjuntos.
//!
//! Referencia: <https://www.win.tue.nl/~vanwijk/stm.pdf>

// u64→f64: para geometría de UI la pérdida de precisión es irrelevante
// (ningún tamaño real se acerca a 2^53 bytes, donde f64 deja de ser exacto).
#![allow(clippy::cast_precision_loss)]

/// Rectángulo del layout, en coordenadas del propio treemap.
#[derive(Debug, Clone, Copy, PartialEq)]
#[must_use]
pub struct Rect {
    /// Coordenada X de la esquina superior izquierda.
    pub x: f64,
    /// Coordenada Y de la esquina superior izquierda.
    pub y: f64,
    /// Ancho.
    pub w: f64,
    /// Alto.
    pub h: f64,
}

impl Rect {
    /// Área del rectángulo.
    #[must_use]
    pub const fn area(self) -> f64 {
        self.w * self.h
    }
}

/// Un nodo hoja del treemap: identidad + geometría.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub struct TreemapNode {
    /// Ruta de la carpeta (identidad estable para el diff).
    pub path: String,
    /// Bytes roll-up del item (copia de la entrada).
    pub size_bytes: u64,
    /// Geometría asignada por squarify.
    pub rect: Rect,
}

/// Entrada del treemap: una carpeta con su tamaño roll-up.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub struct TreemapItem {
    /// Ruta de la carpeta.
    pub path: String,
    /// Bytes roll-up del subárbol.
    pub size_bytes: u64,
}

/// Ejecuta squarify sobre `items` dentro de un lienzo de `width × height`.
///
/// Los items con tamaño 0 se filtran (área 0 no es representable) y el orden
/// de salida sigue el de entrada; el llamador debe ordenar por tamaño
/// descendente antes de llamar, como exige el algoritmo.
#[must_use]
pub fn squarify(items: &[TreemapItem], width: f64, height: f64) -> Vec<TreemapNode> {
    let positive: Vec<&TreemapItem> = items.iter().filter(|i| i.size_bytes > 0).collect();
    if positive.is_empty() || width <= 0.0 || height <= 0.0 {
        return Vec::new();
    }

    let total: f64 = positive.iter().map(|i| i.size_bytes as f64).sum();
    let scale = (width * height) / total;
    let areas: Vec<f64> = positive
        .iter()
        .map(|i| i.size_bytes as f64 * scale)
        .collect();

    let mut nodes = Vec::with_capacity(positive.len());
    let mut remaining = Rect {
        x: 0.0,
        y: 0.0,
        w: width,
        h: height,
    };
    let mut first = 0;

    while first < areas.len() {
        // La fila empieza con un item y se extiende mientras el peor aspect
        // ratio de sus rectángulos mejore (o no empeore).
        let mut last = first;
        let mut row_area = areas[first];
        let mut best = row_worst_ratio(&remaining, row_area, first, last, &areas);
        while last + 1 < areas.len() {
            let candidate = row_worst_ratio(
                &remaining,
                row_area + areas[last + 1],
                first,
                last + 1,
                &areas,
            );
            if candidate > best {
                break;
            }
            best = candidate;
            row_area += areas[last + 1];
            last += 1;
        }

        // La fila se llena a lo largo del lado más corto del restante.
        let wide = remaining.w >= remaining.h;
        let short = if wide { remaining.h } else { remaining.w };
        let thickness = row_area / short;

        let mut offset = 0.0;
        for i in first..=last {
            let length = areas[i] / thickness;
            let rect = if wide {
                Rect {
                    x: remaining.x,
                    y: remaining.y + offset,
                    w: thickness,
                    h: length,
                }
            } else {
                Rect {
                    x: remaining.x + offset,
                    y: remaining.y,
                    w: length,
                    h: thickness,
                }
            };
            offset += length;
            nodes.push(TreemapNode {
                path: positive[i].path.clone(),
                size_bytes: positive[i].size_bytes,
                rect,
            });
        }

        remaining = if wide {
            Rect {
                x: remaining.x + thickness,
                y: remaining.y,
                w: (remaining.w - thickness).max(0.0),
                h: remaining.h,
            }
        } else {
            Rect {
                x: remaining.x,
                y: remaining.y + thickness,
                w: remaining.w,
                h: (remaining.h - thickness).max(0.0),
            }
        };
        first = last + 1;
    }

    nodes
}

/// Peor aspect ratio de la fila hipotética `first..=last` dentro de `bounds`.
fn row_worst_ratio(bounds: &Rect, row_area: f64, first: usize, last: usize, areas: &[f64]) -> f64 {
    let short = bounds.w.min(bounds.h);
    let thickness = row_area / short;
    areas[first..=last]
        .iter()
        .map(|&area| aspect(area / thickness, thickness))
        .fold(f64::MIN, f64::max)
}

/// Aspect ratio de un rectángulo de lados `a × b`, siempre ≥ 1.
fn aspect(a: f64, b: f64) -> f64 {
    if a >= b {
        a / b
    } else {
        b / a
    }
}

#[cfg(test)]
mod tests {
    // Comparaciones de float con tolerancias deliberadas y `expect` idiomático.
    #![allow(clippy::float_cmp, clippy::expect_used)]

    use super::*;

    /// Tolerancia absoluta para comparar áreas en el lienzo de los tests.
    const EPS: f64 = 1e-3;

    fn items(sizes: &[u64]) -> Vec<TreemapItem> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &s)| TreemapItem {
                path: format!("C:\\F{i}"),
                size_bytes: s,
            })
            .collect()
    }

    #[test]
    fn degenerate_inputs_yield_nothing() {
        assert!(squarify(&[], 100.0, 100.0).is_empty());
        assert!(squarify(&items(&[10]), 0.0, 100.0).is_empty());
        assert!(squarify(&items(&[10]), 100.0, 0.0).is_empty());
        assert!(squarify(&items(&[0, 0]), 100.0, 100.0).is_empty());
    }

    #[test]
    fn covers_the_canvas_without_gaps_or_overlaps() {
        let layout = squarify(&items(&[60, 25, 10, 5]), 120.0, 80.0);
        assert_eq!(layout.len(), 4);

        let covered: f64 = layout.iter().map(|n| n.rect.area()).sum();
        assert!((covered - 120.0 * 80.0).abs() < EPS);

        for (i, a) in layout.iter().enumerate() {
            for (j, b) in layout.iter().enumerate().skip(i + 1) {
                // Rectángulos que comparten arista se proyectan solapados en un
                // eje; lo que no pueden es tener ÁREA de intersección.
                let overlap_x = ((a.rect.x + a.rect.w).min(b.rect.x + b.rect.w)
                    - a.rect.x.max(b.rect.x))
                .max(0.0);
                let overlap_y = ((a.rect.y + a.rect.h).min(b.rect.y + b.rect.h)
                    - a.rect.y.max(b.rect.y))
                .max(0.0);
                assert!(
                    overlap_x * overlap_y <= EPS,
                    "solape entre los nodos {i} y {j}"
                );
            }
        }
    }

    #[test]
    fn keeps_aspect_ratios_bounded_on_moderate_inputs() {
        // Con distribuciones extremas (un item al 90%) ningún layout por
        // franjas puede ser cuadrado; con una distribución moderada, squarify
        // sí garantiza ratios razonables.
        const MAX_ASPECT: f64 = 6.0;
        let layout = squarify(&items(&[50, 25, 15, 10]), 160.0, 90.0);
        assert_eq!(layout.len(), 4);
        for node in &layout {
            let ratio = aspect(node.rect.w, node.rect.h);
            assert!(
                ratio <= MAX_ASPECT,
                "`{}` tiene aspect ratio {ratio}",
                node.path
            );
        }
    }

    #[test]
    fn bigger_items_get_bigger_areas() {
        let layout = squarify(&items(&[60, 30, 10]), 100.0, 100.0);
        let area_of = |path: &str| {
            layout
                .iter()
                .find(|n| n.path == path)
                .expect("nodo presente")
                .rect
                .area()
        };
        assert!(area_of("C:\\F0") >= area_of("C:\\F1"));
        assert!(area_of("C:\\F1") >= area_of("C:\\F2"));
    }

    #[test]
    fn preserves_input_order() {
        // El llamador ordena; squarify solo asigna geometría.
        let layout = squarify(&items(&[30, 60, 10]), 100.0, 100.0);
        let paths: Vec<&str> = layout.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(paths, ["C:\\F0", "C:\\F1", "C:\\F2"]);
    }
}
