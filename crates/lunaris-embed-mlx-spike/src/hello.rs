//! O-02 smoke test — verify that `mlx-rs 0.25.3` builds, links to its
//! bundled `mlx-c` C++ runtime, and can run a trivial `matmul` on the
//! default Metal device.
//!
//! If this module fails to compile or its test fails, the spike returns
//! NO-GO at hour 1 with O-02-A as the only landed deliverable.

use mlx_rs::Array;
use mlx_rs::ops::matmul;
use mlx_rs::transforms;

/// Multiply a 2x3 by a 3x4 array and return the result as `Vec<f32>` (12
/// elements). Smokes the toolchain end-to-end: array construction, matmul
/// dispatch, materialisation via `transforms::eval`, host-side readback.
pub fn mlx_hello_world() -> Result<Vec<f32>, mlx_rs::error::Exception> {
    // Identity-ish inputs: a = [[1,2,3],[4,5,6]], b = I3 padded to 3x4.
    let a = Array::from_slice(&[1.0_f32, 2.0, 3.0, 4.0, 5.0, 6.0], &[2, 3]);
    let b = Array::from_slice(
        &[
            1.0_f32, 0.0, 0.0, 0.0, // row 0 of I3 + zero col
            0.0, 1.0, 0.0, 0.0, // row 1
            0.0, 0.0, 1.0, 0.0, // row 2
        ],
        &[3, 4],
    );
    let c = matmul(&a, &b)?;
    // Force materialisation via the free function in `transforms` so the
    // device-side compute graph runs before we read the slice host-side.
    transforms::eval([&c])?;
    Ok(c.as_slice::<f32>().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The matmul output should equal the first 4 columns of `a` left-padded
    /// with the right-most zero column. i.e. `[[1,2,3,0],[4,5,6,0]]`.
    #[test]
    fn mlx_links_and_matmuls() {
        let got = mlx_hello_world().expect("mlx matmul must succeed");
        let want = vec![1.0_f32, 2.0, 3.0, 0.0, 4.0, 5.0, 6.0, 0.0];
        assert_eq!(got.len(), want.len(), "shape mismatch");
        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
            assert!((g - w).abs() < 1e-5, "i={i}: got {g}, want {w}");
        }
    }
}
