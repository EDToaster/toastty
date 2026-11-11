//! Clip and translate declared regions without changing the terminal grid.
//! Splitting quads at the source rectangle keeps neighbouring panes stationary;
//! clipping again at the destination hides the overscan cells.

use toastty_term::SmoothRegion;

use crate::image::instance::ImageInstance;
use crate::text::instance::CellInstance;

#[derive(Clone, Copy)]
struct Geometry {
    pos: [f32; 2],
    size: [f32; 2],
    uv_min: [f32; 2],
    uv_max: [f32; 2],
}

impl Geometry {
    fn clip(self, rect: [f32; 4]) -> Option<Self> {
        let mut clipped = self;
        for axis in 0..2 {
            let start = self.pos[axis].max(rect[axis]);
            let end = (self.pos[axis] + self.size[axis]).min(rect[axis + 2]);
            if end <= start || self.size[axis] <= 0.0 {
                return None;
            }
            let uv_span = self.uv_max[axis] - self.uv_min[axis];
            clipped.pos[axis] = start;
            clipped.size[axis] = end - start;
            clipped.uv_min[axis] =
                self.uv_min[axis] + uv_span * (start - self.pos[axis]) / self.size[axis];
            clipped.uv_max[axis] =
                self.uv_min[axis] + uv_span * (end - self.pos[axis]) / self.size[axis];
        }
        Some(clipped)
    }
}

fn transform<T: Copy>(
    instances: &mut Vec<T>,
    regions: &[SmoothRegion],
    cell: (f32, f32),
    geometry: impl Fn(T) -> Geometry,
    with_geometry: impl Fn(T, Geometry) -> T,
) {
    let mut output = Vec::new();
    for region in regions {
        output.clear();
        let r = region.rect;
        let rect = [
            f32::from(r.left) * cell.0,
            f32::from(r.top) * cell.1,
            (f32::from(r.right) + 1.0) * cell.0,
            (f32::from(r.bottom) + 1.0) * cell.1,
        ];
        let visible = [
            rect[0],
            rect[1],
            rect[2]
                - if region.horizontal_overscan {
                    cell.0
                } else {
                    0.0
                },
            rect[3] - cell.1,
        ];
        let shift = region.shift();
        for &instance in instances.iter() {
            let g = geometry(instance);
            let Some(mut inside) = g.clip(rect) else {
                output.push(instance);
                continue;
            };
            // Four non-overlapping strips partition everything outside rect.
            for outside in [
                [f32::NEG_INFINITY, f32::NEG_INFINITY, f32::INFINITY, rect[1]],
                [f32::NEG_INFINITY, rect[3], f32::INFINITY, f32::INFINITY],
                [f32::NEG_INFINITY, rect[1], rect[0], rect[3]],
                [rect[2], rect[1], f32::INFINITY, rect[3]],
            ] {
                if let Some(piece) = g.clip(outside) {
                    output.push(with_geometry(instance, piece));
                }
            }
            inside.pos[0] -= shift[0] * cell.0;
            inside.pos[1] -= shift[1] * cell.1;
            if let Some(piece) = inside.clip(visible) {
                output.push(with_geometry(instance, piece));
            }
        }
        std::mem::swap(instances, &mut output);
    }
}

pub(crate) fn cells(instances: &mut Vec<CellInstance>, regions: &[SmoothRegion], cell: (f32, f32)) {
    transform(
        instances,
        regions,
        cell,
        |i| Geometry {
            pos: i.pos,
            size: i.size,
            uv_min: i.uv_min,
            uv_max: i.uv_max,
        },
        |i, g| CellInstance {
            pos: g.pos,
            size: g.size,
            uv_min: g.uv_min,
            uv_max: g.uv_max,
            ..i
        },
    );
}

pub(crate) fn images(
    instances: &mut Vec<ImageInstance>,
    regions: &[SmoothRegion],
    cell: (f32, f32),
) {
    transform(
        instances,
        regions,
        cell,
        |i| Geometry {
            pos: i.pos,
            size: i.size,
            uv_min: i.uv_min,
            uv_max: i.uv_max,
        },
        |i, g| ImageInstance {
            pos: g.pos,
            size: g.size,
            uv_min: g.uv_min,
            uv_max: g.uv_max,
            ..i
        },
    );
}

pub(crate) fn cursor_rect(rect: [f32; 4], regions: &[SmoothRegion], cell: (f32, f32)) -> [f32; 4] {
    if regions.is_empty() {
        return rect;
    }
    let mut cursor = vec![CellInstance::background(
        [rect[0], rect[1]],
        [rect[2] - rect[0], rect[3] - rect[1]],
        [0.0; 4],
    )];
    cells(&mut cursor, regions, cell);
    cursor.first().map_or([0.0; 4], |c| {
        [
            c.pos[0],
            c.pos[1],
            c.pos[0] + c.size[0],
            c.pos[1] + c.size[1],
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use toastty_parser::Parser;
    use toastty_term::Term;

    #[test]
    fn clipping_preserves_texture_coordinates_and_stationary_neighbours() {
        let mut term = Term::new(6, 10, 0);
        Parser::new().advance(
            &mut term,
            b"\x1b[?1049h\x1b_smooth;1;1;4;2;6;0;0;500;500\x1b\\",
        );
        let image = ImageInstance {
            pos: [0.0, 0.0],
            size: [100.0, 60.0],
            uv_min: [0.0, 0.0],
            uv_max: [1.0, 1.0],
            texture_index: 7,
            z: 0,
            image_id: 1,
            key: 1,
        };
        let mut images = vec![image];
        super::images(&mut images, term.smooth_regions(), (10.0, 10.0));
        assert_eq!(images.len(), 5);
        let moved = images.last().unwrap();
        assert_eq!(moved.pos, [20.0, 10.0]);
        assert_eq!(moved.size, [40.0, 30.0]);
        assert!((moved.uv_min[0] - 0.25).abs() < 1e-6);
        assert!((moved.uv_min[1] - 0.25).abs() < 1e-6);
        assert_eq!(moved.texture_index, 7);
        let left = &images[2];
        assert_eq!(left.pos, [0.0, 10.0]);
        assert_eq!(left.size, [20.0, 40.0]);
        assert_eq!(left.uv_min[0], 0.0);
        assert!((left.uv_max[0] - 0.2).abs() < 1e-6);
        assert_eq!(
            cursor_rect(
                [20.0, 20.0, 30.0, 30.0],
                term.smooth_regions(),
                (10.0, 10.0)
            ),
            [20.0, 15.0, 25.0, 25.0],
            "cursor inversion must follow the translated and clipped cursor quad"
        );
        assert_eq!(
            cursor_rect(
                [60.0, 40.0, 70.0, 50.0],
                term.smooth_regions(),
                (10.0, 10.0)
            ),
            [55.0, 35.0, 60.0, 40.0],
            "only the revealed part of overscan can invert glyphs"
        );
    }
}
