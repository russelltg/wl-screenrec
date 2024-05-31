use wayland_client::protocol::wl_output::Transform;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

fn transform_is_transposed(transform: Transform) -> bool {
    transform_basis(transform).0[0] == 0
}

pub fn transpose_if_transform_transposed((w, h): (i32, i32), transform: Transform) -> (i32, i32) {
    if transform_is_transposed(transform) {
        (h, w)
    } else {
        (w, h)
    }
}

fn screen_point_to_frame(
    capture_w: i32,
    capture_h: i32,
    transform: Transform,
    x: i32,
    y: i32,
) -> (i32, i32) {
    let screen_origin_frame_coord = match transform {
        Transform::Flipped180 | Transform::_90 => (0, capture_h),
        Transform::Flipped270 | Transform::_180 => (capture_w, capture_h),
        Transform::Flipped | Transform::_270 => (capture_w, 0),
        Transform::Flipped90 | Transform::Normal => (0, 0),
        _ => (0, 0),
    };

    let screen_basis_frame_coord = transform_basis(transform);

    (
        screen_origin_frame_coord.0
            + x * screen_basis_frame_coord.0[0]
            + y * screen_basis_frame_coord.1[0],
        screen_origin_frame_coord.1
            + x * screen_basis_frame_coord.0[1]
            + y * screen_basis_frame_coord.1[1],
    )
}

// for each (x, y) in screen space, how many steps in frame space
fn transform_basis(transform: Transform) -> ([i32; 2], [i32; 2]) {
    match transform {
        Transform::_90 => ([0, -1], [1, 0]),
        Transform::_180 => ([-1, 0], [0, -1]),
        Transform::_270 => ([0, 1], [-1, 0]),
        Transform::Flipped => ([-1, 0], [0, 1]),
        Transform::Flipped90 => ([0, 1], [1, 0]),
        Transform::Flipped180 => ([1, 0], [0, -1]),
        Transform::Flipped270 => ([0, -1], [-1, 0]),
        _ => ([1, 0], [0, 1]),
    }
}

impl Rect {
    pub fn new((x, y): (i32, i32), (w, h): (i32, i32)) -> Self {
        Rect { x, y, w, h }
    }

    pub fn screen_to_frame(&self, capture_w: i32, capture_h: i32, transform: Transform) -> Rect {
        let (x1, y1) = screen_point_to_frame(capture_w, capture_h, transform, self.x, self.y);
        let (x2, y2) = screen_point_to_frame(
            capture_w,
            capture_h,
            transform,
            self.x + self.w,
            self.y + self.h,
        );

        Rect {
            x: x1.min(x2),
            y: y1.min(y2),
            w: (x1 - x2).abs(),
            h: (y1 - y2).abs(),
        }
    }
}

#[cfg(test)]
mod test {
    use wayland_client::protocol::wl_output::Transform;

    use crate::transform::transform_is_transposed;

    use super::Rect;

    #[test]
    fn screen_to_frame_normal() {
        assert_eq!(
            Rect {
                x: 10,
                y: 20,
                w: 30,
                h: 40
            }
            .screen_to_frame(1920, 1080, Transform::Normal),
            Rect {
                x: 10,
                y: 20,
                w: 30,
                h: 40
            }
        );
    }

    #[test]
    fn screen_to_frame_90() {
        assert_eq!(
            Rect {
                x: 10,
                y: 20,
                w: 30,
                h: 40
            }
            .screen_to_frame(1920, 1080, Transform::_90),
            Rect {
                x: 20,
                y: 1040,
                w: 40,
                h: 30
            }
        );

        assert_eq!(
            Rect {
                x: 0,
                y: 0,
                w: 1200,
                h: 1920
            }
            .screen_to_frame(1920, 1200, Transform::_90),
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1200
            }
        );

        assert_eq!(
            Rect {
                x: 743,
                y: 1359,
                w: 312,
                h: 264,
            }
            .screen_to_frame(1920, 1200, Transform::_90),
            Rect {
                x: 1359,
                y: 145,
                w: 264,
                h: 312
            }
        )
    }

    #[test]
    fn screen_to_frame_270() {
        assert_eq!(
            Rect {
                x: 274,
                y: 962,
                w: 639,
                h: 412,
            }
            .screen_to_frame(1920, 1200, Transform::_270),
            Rect {
                x: 546,
                y: 274,
                w: 412,
                h: 639
            }
        );
    }

    #[test]
    fn transform_transposed() {
        assert!(!transform_is_transposed(Transform::Normal));
        assert!(transform_is_transposed(Transform::_90));
    }
}
