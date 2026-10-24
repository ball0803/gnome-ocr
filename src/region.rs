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
    /// Builds a rectangle from two corners given in fractions.
    pub fn from_corners(start: (f64, f64), end: (f64, f64)) -> Self {
        Self {
            x: start.0.min(end.0),
            y: start.1.min(end.1),
            width: (end.0 - start.0).abs(),
            height: (end.1 - start.1).abs(),
        }
    }

    /// True when the rectangle covers nothing.
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_region_with_no_area_is_empty() {
        assert!(Region::default().is_empty());
        assert!(!Region {
            x: 0.0,
            y: 0.0,
            width: 0.5,
            height: 0.5
        }
        .is_empty());
    }
}
