use std::mem;

use super::ffi::*;

/// Cap macOS HiDPI remote capture output to 2K (2560 long edge) so 4K/5K/6K
/// Retina offscreen backing buffers are downsampled on the GPU instead of
/// flooding the capture/encode pipeline, while keeping local macOS HiDPI intact.
pub const MAX_HIDPI_STREAM_LONG_EDGE: usize = 2560;

#[inline]
fn round_even_min2(v: f64) -> usize {
    (((v * 0.5).round() as usize) * 2).max(2)
}

pub(crate) fn compute_stream_geometry(
    logical_w: usize,
    logical_h: usize,
    backing_scale: f64,
    enable_retina: bool,
) -> (usize, usize, f64) {
    let lw = logical_w.max(2);
    let lh = logical_h.max(2);
    if enable_retina && backing_scale > 1.0 {
        let raw_w = (lw as f64) * backing_scale;
        let raw_h = (lh as f64) * backing_scale;
        let raw_long = raw_w.max(raw_h);
        let cap = MAX_HIDPI_STREAM_LONG_EDGE as f64;
        let factor = if raw_long > cap { cap / raw_long } else { 1.0 };
        let out_w = round_even_min2(raw_w * factor);
        let out_h = round_even_min2(raw_h * factor);
        let effective_scale = (out_w as f64) / (lw as f64);
        (out_w, out_h, effective_scale)
    } else {
        let out_w = (lw & !1).max(2);
        let out_h = (lh & !1).max(2);
        (out_w, out_h, 1.0)
    }
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
#[repr(C)]
pub struct Display(u32);

impl Display {
    pub fn primary() -> Display {
        Display(unsafe { CGMainDisplayID() })
    }

    pub fn online() -> Result<Vec<Display>, CGError> {
        unsafe {
            #[allow(invalid_value)]
            let mut arr: [u32; 16] = mem::MaybeUninit::uninit().assume_init();
            let mut len: u32 = 0;

            match CGGetOnlineDisplayList(16, arr.as_mut_ptr(), &mut len) {
                CGError::Success => (),
                x => return Err(x),
            }

            let mut res = Vec::with_capacity(16);
            for i in 0..len as usize {
                res.push(Display(*arr.get_unchecked(i)));
            }
            Ok(res)
        }
    }

    pub fn id(self) -> u32 {
        self.0
    }

    #[inline]
    fn stream_geometry(self) -> (usize, usize, f64) {
        let logical_w = unsafe { CGDisplayPixelsWide(self.0) };
        let logical_h = unsafe { CGDisplayPixelsHigh(self.0) };
        let backing_scale = unsafe { BackingScaleFactor(self.0) as f64 };
        let enable_retina = if backing_scale > 1.0 {
            *super::ENABLE_RETINA.lock().unwrap()
        } else {
            false
        };
        compute_stream_geometry(logical_w, logical_h, backing_scale, enable_retina)
    }

    pub fn width(self) -> usize {
        self.stream_geometry().0
    }

    pub fn height(self) -> usize {
        self.stream_geometry().1
    }

    pub fn is_builtin(self) -> bool {
        unsafe { CGDisplayIsBuiltin(self.0) != 0 }
    }

    pub fn is_primary(self) -> bool {
        unsafe { CGDisplayIsMain(self.0) != 0 }
    }

    pub fn is_active(self) -> bool {
        unsafe { CGDisplayIsActive(self.0) != 0 }
    }

    pub fn is_online(self) -> bool {
        unsafe { CGDisplayIsOnline(self.0) != 0 }
    }

    pub fn scale(self) -> f64 {
        self.stream_geometry().2
    }

    pub fn bounds(self) -> CGRect {
        unsafe { CGDisplayBounds(self.0) }
    }
}

#[cfg(test)]
mod tests {
    use super::compute_stream_geometry;

    #[test]
    fn hidpi_4k_modes_cap_cleanly_at_2560x1440() {
        // 4K "Looks like 1920x1080" (3840x2160 backing) -> 2560x1440, scale 4/3
        let (w, h, s) = compute_stream_geometry(1920, 1080, 2.0, true);
        assert_eq!((w, h), (2560, 1440));
        assert!((s - 2560.0 / 1920.0).abs() < 1e-9);
        assert_eq!(((w as f64) / s).round() as usize, 1920);
        assert_eq!(((h as f64) / s).round() as usize, 1080);

        // 4K "Looks like 2560x1440" (5120x2880 backing) -> 2560x1440, scale 1.0
        let (w, h, s) = compute_stream_geometry(2560, 1440, 2.0, true);
        assert_eq!((w, h), (2560, 1440));
        assert!((s - 1.0).abs() < 1e-9);

        // 4K "Looks like 3008x1692" (6016x3384 backing) -> 2560x1440, scale < 1.0
        let (w, h, s) = compute_stream_geometry(3008, 1692, 2.0, true);
        assert_eq!((w, h), (2560, 1440));
        assert!((s - 2560.0 / 3008.0).abs() < 1e-9);
        assert_eq!(((w as f64) / s).round() as usize, 3008);
        assert_eq!(((h as f64) / s).round() as usize, 1692);

        // Portrait 4K "Looks like 1080x1920" -> 1440x2560
        let (w, h, _) = compute_stream_geometry(1080, 1920, 2.0, true);
        assert_eq!((w, h), (1440, 2560));

        // Non-HiDPI 1x stays untouched
        let (w, h, s) = compute_stream_geometry(3840, 2160, 1.0, true);
        assert_eq!((w, h), (3840, 2160));
        assert_eq!(s, 1.0);
    }
}
