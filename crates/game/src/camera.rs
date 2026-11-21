//! Free-flying camera and view-frustum culling.
//!
//! The camera position is stored in `f64` (DVec3). At the edge of a full-size
//! world (x ≈ 30,000,000) an `f32` can only represent positions in steps of
//! 2 blocks, which makes movement and rendering jitter. We keep `f64` on the
//! CPU and only hand the GPU camera-relative offsets (see `terrain.vert`).

use glam::{DVec3, Mat4, Vec3, Vec4};

pub struct Camera {
    pub pos: DVec3,
    /// Radians. 0 looks towards -Z; positive turns right.
    pub yaw: f32,
    /// Radians, clamped to just under ±90°.
    pub pitch: f32,
    pub fov_y_deg: f32,
}

impl Camera {
    pub fn new(pos: DVec3) -> Self {
        Self { pos, yaw: 0.0, pitch: -0.3, fov_y_deg: 70.0 }
    }

    pub fn forward(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        Vec3::new(sy * cp, sp, -cy * cp)
    }

    /// Forward direction flattened onto the ground plane.
    pub fn forward_flat(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        Vec3::new(sy, 0.0, -cy)
    }

    pub fn right(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        Vec3::new(cy, 0.0, sy)
    }

    pub fn rotate(&mut self, dx: f32, dy: f32) {
        self.yaw += dx;
        self.pitch = (self.pitch - dy).clamp(-1.55, 1.55);
    }

    /// View-projection for camera-relative coordinates (camera at origin).
    /// Vulkan clip space: Y pointing down, hence the flip. Depth is
    /// *reversed* (1 at the near plane, 0 at infinity) with no far plane:
    /// float depth then keeps ~7 significant digits at any distance, which a
    /// 4,000-block LOD view needs (standard depth would be ~15 blocks coarse
    /// out there).
    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        let view = Mat4::look_to_rh(Vec3::ZERO, self.forward(), Vec3::Y);
        let mut proj = Mat4::perspective_infinite_reverse_rh(self.fov_y_deg.to_radians(), aspect, 0.1);
        proj.y_axis.y *= -1.0;
        proj * view
    }
}

/// Six clip planes extracted from a view-projection matrix
/// (Gribb & Hartmann method). A box is skipped if it lies entirely behind any
/// plane. Cheap, and removes roughly half the chunks every frame.
pub struct Frustum {
    planes: [Vec4; 6],
}

impl Frustum {
    pub fn from_matrix(m: Mat4) -> Self {
        let r = [m.row(0), m.row(1), m.row(2), m.row(3)];
        let mut planes = [r[3] + r[0], r[3] - r[0], r[3] + r[1], r[3] - r[1], r[2], r[3] - r[2]];
        for p in &mut planes {
            let len = p.truncate().length();
            // An infinite far plane has no normal: make it accept everything.
            *p = if len > 1e-6 { *p / len } else { Vec4::new(0.0, 0.0, 0.0, 1.0) };
        }
        Self { planes }
    }

    pub fn intersects_aabb(&self, min: Vec3, max: Vec3) -> bool {
        for p in &self.planes {
            // The box corner furthest along the plane normal.
            let v = Vec3::new(
                if p.x >= 0.0 { max.x } else { min.x },
                if p.y >= 0.0 { max.y } else { min.y },
                if p.z >= 0.0 { max.z } else { min.z },
            );
            if p.truncate().dot(v) + p.w < 0.0 {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frustum_keeps_boxes_in_front_and_drops_boxes_behind() {
        let cam = Camera { pos: DVec3::ZERO, yaw: 0.0, pitch: 0.0, fov_y_deg: 70.0 };
        let f = Frustum::from_matrix(cam.view_proj(16.0 / 9.0));
        assert!(f.intersects_aabb(Vec3::new(-1.0, -1.0, -20.0), Vec3::new(1.0, 1.0, -18.0)));
        assert!(!f.intersects_aabb(Vec3::new(-1.0, -1.0, 18.0), Vec3::new(1.0, 1.0, 20.0)));
        assert!(!f.intersects_aabb(Vec3::new(-100.0, -1.0, -20.0), Vec3::new(-90.0, 1.0, -18.0)), "off to the left");
        // No far plane: 100 km ahead is still inside.
        assert!(f.intersects_aabb(Vec3::new(-1.0, -1.0, -100_000.0), Vec3::new(1.0, 1.0, -99_990.0)));
    }

    #[test]
    fn depth_is_reversed() {
        let cam = Camera { pos: DVec3::ZERO, yaw: 0.0, pitch: 0.0, fov_y_deg: 70.0 };
        let m = cam.view_proj(1.0);
        let depth = |d: f32| {
            let p = m * Vec4::new(0.0, 0.0, -d, 1.0);
            p.z / p.w
        };
        assert!((depth(0.1) - 1.0).abs() < 1e-5, "near plane at depth 1");
        assert!(depth(1.0) > depth(10.0) && depth(10.0) > depth(4000.0) && depth(4000.0) > 0.0);
        // Two surfaces 0.5 blocks apart 4,000 blocks away get different depths.
        assert!(depth(4000.0) != depth(4000.5));
    }

    #[test]
    fn forward_matches_yaw() {
        let mut cam = Camera::new(DVec3::ZERO);
        cam.pitch = 0.0;
        assert!((cam.forward() - Vec3::new(0.0, 0.0, -1.0)).length() < 1e-5);
        cam.yaw = std::f32::consts::FRAC_PI_2;
        assert!((cam.forward() - Vec3::new(1.0, 0.0, 0.0)).length() < 1e-5);
        assert!((cam.right() - Vec3::new(0.0, 0.0, 1.0)).length() < 1e-5);
    }
}

#[cfg(test)]
mod orientation_tests {
    use super::*;
    #[test]
    fn point_below_camera_is_in_lower_half_of_vulkan_screen() {
        let cam = Camera { pos: DVec3::ZERO, yaw: 0.0, pitch: 0.0, fov_y_deg: 70.0 };
        let p = cam.view_proj(1.0) * glam::Vec4::new(0.0, -5.0, -20.0, 1.0);
        let ndc_y = p.y / p.w;
        eprintln!("ndc_y = {ndc_y}");
        // Vulkan: +y in NDC is the bottom of the screen.
        assert!(ndc_y > 0.0);
    }
}
