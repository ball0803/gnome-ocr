/// The part of the captured frame that is sent to the recogniser.
///
/// The rectangle is kept in fractions of the frame, so it stays correct when
/// the captured window changes size or resolution.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Region {
    /// True when the rectangle covers nothing.
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// Builds a rectangle from two corners given in fractions.
    pub fn from_corners(start: (f64, f64), end: (f64, f64)) -> Self {
        let x = start.0.min(end.0);
        let y = start.1.min(end.1);
        Self {
            x,
            y,
            width: (end.0 - start.0).abs(),
            height: (end.1 - start.1).abs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_may_be_dragged_in_any_direction() {
        let region = Region::from_corners((0.8, 0.9), (0.2, 0.3));
        assert!((region.x - 0.2).abs() < 1e-9);
        assert!((region.y - 0.3).abs() < 1e-9);
        assert!((region.width - 0.6).abs() < 1e-9);
        assert!((region.height - 0.6).abs() < 1e-9);
    }

    #[test]
    fn a_region_with_no_area_is_empty() {
        assert!(Region::from_corners((0.5, 0.5), (0.5, 0.5)).is_empty());
        assert!(!Region::from_corners((0.0, 0.0), (0.5, 0.5)).is_empty());
    }
}
