//! Self-calibration: really compress one contiguous span of the source at a
//! few anchor levels and measure, per strategy family, the log-space gap
//! between the real span ratio and the model's tiled-cold replay of the same
//! bytes. The gap absorbs whole-file history effects (deep windows,
//! block-to-block correlations) that cold sampled chunks cannot observe, plus
//! residual model bias.

use super::levels;

/// Log-space correction fit, piecewise-linear within each strategy family.
#[derive(Debug, Clone)]
pub struct AnchorFit {
    /// `(level, ln(real / modeled))` anchor points, ascending by level.
    points: Vec<(i32, f64)>,
}

impl AnchorFit {
    pub fn identity() -> Self {
        AnchorFit { points: Vec::new() }
    }

    pub fn fit(mut points: Vec<(i32, f64)>) -> Self {
        points.sort_unstable_by_key(|p| p.0);
        AnchorFit { points }
    }

    pub fn is_identity(&self) -> bool {
        self.points.is_empty()
    }

    /// Log-space correction for `level`: linear interpolation between the
    /// anchors of its strategy family, flat outside them, and 0 (uncorrected)
    /// when the family has no anchor at all.
    pub fn delta(&self, level: i32) -> f64 {
        let fam = levels::family_of(level);
        let pts: Vec<(i32, f64)> = self
            .points
            .iter()
            .copied()
            .filter(|p| levels::family_of(p.0) == fam)
            .collect();
        match pts.len() {
            0 => 0.0,
            1 => pts[0].1,
            _ => {
                if level <= pts[0].0 {
                    return pts[0].1;
                }
                let last = *pts.last().unwrap();
                if level >= last.0 {
                    return last.1;
                }
                for w in pts.windows(2) {
                    if (w[0].0..=w[1].0).contains(&level) {
                        let t = (level - w[0].0) as f64 / (w[1].0 - w[0].0) as f64;
                        return w[0].1 + t * (w[1].1 - w[0].1);
                    }
                }
                0.0
            }
        }
    }

    /// Apply the correction to a modeled compressed fraction.
    pub fn correct(&self, level: i32, fraction: f64) -> f64 {
        (fraction * self.delta(level).exp()).clamp(1e-7, 1.0)
    }
}

/// Compress `data` at `level` and return the real compressed fraction.
pub fn real_fraction(data: &[u8], level: i32) -> Option<f64> {
    if data.is_empty() {
        return None;
    }
    let out = zstd::bulk::compress(data, level).ok()?;
    Some(out.len() as f64 / data.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_anchor_is_family_constant() {
        let f = AnchorFit::fit(vec![(3, 0.1)]);
        // Same family (dfast): the constant shift applies.
        assert!((f.delta(4) - 0.1).abs() < 1e-12);
        // Other families are uncorrected.
        assert_eq!(f.delta(10), 0.0);
        assert_eq!(f.delta(19), 0.0);
    }

    #[test]
    fn two_anchors_interpolate_within_family() {
        let f = AnchorFit::fit(vec![(5, 0.1), (11, 0.3)]);
        assert!((f.delta(5) - 0.1).abs() < 1e-12);
        assert!((f.delta(11) - 0.3).abs() < 1e-12);
        assert!((f.delta(8) - 0.2).abs() < 1e-12);
        // Flat outside the anchor range.
        assert!((f.delta(12) - 0.3).abs() < 1e-12);
        assert!((f.delta(5) - 0.1).abs() < 1e-12);
    }

    #[test]
    fn families_are_isolated() {
        let f = AnchorFit::fit(vec![(1, 0.1), (3, -0.3)]);
        assert!((f.delta(2) - 0.1).abs() < 1e-12);
        assert!((f.delta(3) - -0.3).abs() < 1e-12);
        assert_eq!(f.delta(7), 0.0);
    }

    #[test]
    fn real_fraction_zeros() {
        let data = vec![0u8; 100_000];
        let f = real_fraction(&data, 3).unwrap();
        assert!(f < 0.01, "fraction {f}");
    }
}
