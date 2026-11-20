//! Spline curves: the "shaping" layer between noise and terrain.
//!
//! Noise gives smooth random numbers in about [-1, 1]. Terrain needs
//! something much more specific: "continentalness −0.2 is the continental
//! shelf at 18 blocks below sea level, 0.1 is lowland 8 blocks above it". A
//! [`Curve`] is a list of control points `(input, output)` joined by a smooth
//! cubic, so the shape of the world can be tuned by editing a table instead of
//! the noise.
//!
//! We use monotone cubic Hermite interpolation (Fritsch–Carlson). Between two
//! control points the curve never overshoots the higher one or undershoots
//! the lower one, so a curve that is meant to stay below sea level actually
//! stays below sea level. Outside the first/last point the value is clamped.

#[derive(Clone, Debug)]
pub struct Curve {
    xs: Vec<f64>,
    ys: Vec<f64>,
    /// Tangent at each point, computed once.
    ms: Vec<f64>,
}

impl Curve {
    /// `points` must be sorted by x with distinct x values.
    pub fn new(points: &[(f64, f64)]) -> Self {
        assert!(points.len() >= 2, "a curve needs at least two points");
        let xs: Vec<f64> = points.iter().map(|p| p.0).collect();
        let ys: Vec<f64> = points.iter().map(|p| p.1).collect();
        for w in xs.windows(2) {
            assert!(w[1] > w[0], "curve points must be sorted by x");
        }
        let n = xs.len();
        // Secant slopes between neighbours.
        let d: Vec<f64> = (0..n - 1).map(|i| (ys[i + 1] - ys[i]) / (xs[i + 1] - xs[i])).collect();
        let mut ms = vec![0.0; n];
        ms[0] = d[0];
        ms[n - 1] = d[n - 2];
        for i in 1..n - 1 {
            ms[i] = if d[i - 1] * d[i] <= 0.0 { 0.0 } else { (d[i - 1] + d[i]) / 2.0 };
        }
        // Fritsch–Carlson limiter: keeps each segment monotone.
        for i in 0..n - 1 {
            if d[i] == 0.0 {
                ms[i] = 0.0;
                ms[i + 1] = 0.0;
                continue;
            }
            let a = ms[i] / d[i];
            let b = ms[i + 1] / d[i];
            let s = a * a + b * b;
            if s > 9.0 {
                let t = 3.0 / s.sqrt();
                ms[i] = t * a * d[i];
                ms[i + 1] = t * b * d[i];
            }
        }
        Self { xs, ys, ms }
    }

    #[inline]
    pub fn eval(&self, x: f64) -> f64 {
        let n = self.xs.len();
        if x <= self.xs[0] {
            return self.ys[0];
        }
        if x >= self.xs[n - 1] {
            return self.ys[n - 1];
        }
        // Curves have a handful of points: a linear scan beats binary search.
        let mut i = 0;
        while x > self.xs[i + 1] {
            i += 1;
        }
        let h = self.xs[i + 1] - self.xs[i];
        let t = (x - self.xs[i]) / h;
        let (t2, t3) = (t * t, t * t * t);
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        h00 * self.ys[i] + h10 * h * self.ms[i] + h01 * self.ys[i + 1] + h11 * h * self.ms[i + 1]
    }
}

/// Smooth 0→1 step between `a` and `b` (Hermite, like GLSL `smoothstep`).
#[inline]
pub fn smoothstep(a: f64, b: f64, x: f64) -> f64 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline]
pub fn lerp(t: f64, a: f64, b: f64) -> f64 {
    a + t * (b - a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_points_and_clamps() {
        let c = Curve::new(&[(-1.0, -50.0), (0.0, 0.0), (0.5, 10.0), (1.0, 100.0)]);
        for (x, y) in [(-1.0, -50.0), (0.0, 0.0), (0.5, 10.0), (1.0, 100.0)] {
            assert!((c.eval(x) - y).abs() < 1e-9);
        }
        assert_eq!(c.eval(-5.0), -50.0);
        assert_eq!(c.eval(5.0), 100.0);
    }

    #[test]
    fn monotone_data_gives_monotone_curve_without_overshoot() {
        let c = Curve::new(&[(0.0, 0.0), (0.1, 0.0), (0.2, 50.0), (0.3, 51.0), (1.0, 52.0)]);
        let mut prev = f64::MIN;
        for i in 0..=1000 {
            let v = c.eval(i as f64 / 1000.0);
            assert!(v >= prev - 1e-9, "not monotone at {i}");
            assert!((0.0..=52.0).contains(&v));
            prev = v;
        }
    }

    #[test]
    fn continuous() {
        let c = Curve::new(&[(-1.0, 3.0), (-0.3, -2.0), (0.4, 8.0), (1.0, 1.0)]);
        for i in 0..2000 {
            let x = -1.0 + i as f64 / 1000.0;
            assert!((c.eval(x) - c.eval(x + 1e-7)).abs() < 1e-4);
        }
    }

    #[test]
    fn smoothstep_edges() {
        assert_eq!(smoothstep(0.0, 1.0, -1.0), 0.0);
        assert_eq!(smoothstep(0.0, 1.0, 2.0), 1.0);
        assert!((smoothstep(0.0, 1.0, 0.5) - 0.5).abs() < 1e-12);
    }
}
