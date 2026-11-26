use std::collections::HashMap;
use std::mem;
use std::sync::Mutex;

use super::ffi::*;

lazy_static::lazy_static! {
    /// Dynamic controller-requested target viewport (width, height) per CGDirectDisplayID.
    /// When set, macOS HiDPI capture picks the optimal integer divisor of the native
    /// Retina backing store without modifying the local macOS display mode.
    static ref TARGET_RESOLUTIONS: Mutex<HashMap<u32, (usize, usize)>> = Mutex::new(HashMap::new());
}

/// Set or clear the remote capture target viewport for a macOS display.
pub fn set_target_resolution(display_id: u32, width: usize, height: usize) {
    let mut map = TARGET_RESOLUTIONS.lock().unwrap();
    if width == 0 || height == 0 {
        map.remove(&display_id);
    } else {
        map.insert(display_id, (width, height));
    }
}

/// Clear all dynamic capture target overrides (called when the last peer disconnects).
pub fn clear_target_resolutions() {
    TARGET_RESOLUTIONS.lock().unwrap().clear();
}

#[inline]
fn round_even_min2(v: f64) -> usize {
    (((v * 0.5).round() as usize) * 2).max(2)
}

/// Pick the best integer downscale divisor `k >= 1` for a Retina backing store
/// `(raw_w, raw_h)` given an optional controller viewport `(req_w, req_h)`.
///
/// Design rules (for e.g. 4K HiDPI 3840x2160):
/// - `target = None` (initial connect before controller sends ChangeResolution):
///   default to `k = round(backing_scale)` (2 -> exact 2:1 1920x1080, zero 1.5x blur).
/// - `target = Some((req_w, req_h))`:
///   pick the largest integer divisor `k` whose downscaled long edge stays within
///   12% of `req_long` (so windowed 1080p like 1920x1010 still snaps cleanly to k=2,
///   while 4K -> k=1, 2K 2560x1440 -> k=1 native 4K supersampled, 1080p -> k=2,
///   720p -> k=3, 540p -> k=4). Prefer divisors yielding exact even integers
///   (`raw_w % (2*k) == 0 && raw_h % (2*k) == 0`).
pub(crate) fn choose_integer_divisor(
    raw_w: usize,
    raw_h: usize,
    backing_scale: f64,
    target: Option<(usize, usize)>,
) -> usize {
    let default_k = (backing_scale.round() as usize).max(1);
    let Some((req_w, req_h)) = target else {
        return default_k;
    };
    if req_w == 0 || req_h == 0 {
        return default_k;
    }

    let raw_long = raw_w.max(raw_h);
    let raw_short = raw_w.min(raw_h);
    let req_long = req_w.max(req_h).max(2);

    // Ratio based on long edge so Windows taskbar/titlebar height in windowed mode
    // (e.g. 1920x1010) never mistakenly triggers a drop from k=2 (1080p) to k=3 (720p).
    let ratio = (raw_long as f64) / (req_long as f64);
    let max_k_by_ratio = (ratio + 0.12).floor().max(1.0) as usize;

    // Never shrink below 640x360 usable floor.
    let mut best_exact: Option<usize> = None;
    let mut best_any: usize = 1;
    for k in 1..=max_k_by_ratio.min(8) {
        if raw_long / k < 640 || raw_short / k < 360 {
            break;
        }
        best_any = k;
        if raw_w % (2 * k) == 0 && raw_h % (2 * k) == 0 {
            best_exact = Some(k);
        }
    }

    best_exact.unwrap_or(best_any)
}

pub(crate) fn compute_stream_geometry(
    logical_w: usize,
    logical_h: usize,
    backing_scale: f64,
    enable_retina: bool,
    target: Option<(usize, usize)>,
) -> (usize, usize, f64) {
    let lw = logical_w.max(2);
    let lh = logical_h.max(2);
    if enable_retina && backing_scale > 1.0 {
        let raw_w = ((lw as f64) * backing_scale).round() as usize;
        let raw_h = ((lh as f64) * backing_scale).round() as usize;
        let k = choose_integer_divisor(raw_w, raw_h, backing_scale, target);
        let out_w = if raw_w % (2 * k) == 0 {
            (raw_w / k).max(2)
        } else {
            round_even_min2((raw_w as f64) / (k as f64))
        };
        let out_h = if raw_h % (2 * k) == 0 {
            (raw_h / k).max(2)
        } else {
            round_even_min2((raw_h as f64) / (k as f64))
        };
        let effective_scale = (out_w as f64) / (lw as f64);
        (out_w, out_h, effective_scale)
    } else if let Some((req_w, req_h)) = target {
        // Non-HiDPI display (e.g. 3840x2160 @ 1x) with explicit smaller controller viewport:
        // also apply clean integer downscale if controller is smaller, never upscale.
        let k = choose_integer_divisor(lw, lh, 1.0, Some((req_w, req_h)));
        let out_w = if lw % (2 * k) == 0 {
            (lw / k).max(2)
        } else {
            round_even_min2((lw as f64) / (k as f64))
        };
        let out_h = if lh % (2 * k) == 0 {
            (lh / k).max(2)
        } else {
            round_even_min2((lh as f64) / (k as f64))
        };
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
        let target = TARGET_RESOLUTIONS.lock().unwrap().get(&self.0).copied();
        compute_stream_geometry(logical_w, logical_h, backing_scale, enable_retina, target)
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

    /// True when a HiDPI display (backing_scale > 1.0) is being downscaled for remote
    /// capture (e.g. 4K HiDPI 3840x2160 captured at 1920x1080), where a subtle Y-plane
    /// sharpening pass restores crisp text edges softened by ScreenCaptureKit downscaling.
    pub fn is_hidpi_downscaled(self) -> bool {
        let backing_scale = unsafe { BackingScaleFactor(self.0) as f64 };
        if backing_scale <= 1.0 {
            return false;
        }
        let effective_scale = self.stream_geometry().2;
        effective_scale + 0.01 < backing_scale
    }

    pub fn bounds(self) -> CGRect {
        unsafe { CGDisplayBounds(self.0) }
    }
}

#[cfg(test)]
mod tests {
    use super::compute_stream_geometry;

    #[test]
    fn hidpi_integer_divisor_adaptive_capture() {
        // Default (no controller request yet): 4K HiDPI (1920x1080 @ 2x) -> exact 2:1 1920x1080
        let (w, h, s) = compute_stream_geometry(1920, 1080, 2.0, true, None);
        assert_eq!((w, h), (1920, 1080));
        assert!((s - 1.0).abs() < 1e-9);

        // 4K controller -> divisor 1 -> native 3840x2160, scale 2.0
        let (w, h, s) = compute_stream_geometry(1920, 1080, 2.0, true, Some((3840, 2160)));
        assert_eq!((w, h), (3840, 2160));
        assert!((s - 2.0).abs() < 1e-9);

        // 2K controller (2560x1440) -> divisor 1 (native 4K supersampled, avoids 1.5x double blur)
        let (w, h, s) = compute_stream_geometry(1920, 1080, 2.0, true, Some((2560, 1440)));
        assert_eq!((w, h), (3840, 2160));
        assert!((s - 2.0).abs() < 1e-9);

        // 1080p controller (fullscreen or windowed with taskbar) -> divisor 2 -> exact 1920x1080
        let (w, h, s) = compute_stream_geometry(1920, 1080, 2.0, true, Some((1920, 1080)));
        assert_eq!((w, h), (1920, 1080));
        assert!((s - 1.0).abs() < 1e-9);

        let (w, h, s) = compute_stream_geometry(1920, 1080, 2.0, true, Some((1920, 1010)));
        assert_eq!((w, h), (1920, 1080));
        assert!((s - 1.0).abs() < 1e-9);

        // 720p controller -> divisor 3 -> exact 1280x720, scale 2/3
        let (w, h, s) = compute_stream_geometry(1920, 1080, 2.0, true, Some((1280, 720)));
        assert_eq!((w, h), (1280, 720));
        assert!((s - 1280.0 / 1920.0).abs() < 1e-9);
        assert_eq!(((w as f64) / s).round() as usize, 1920);
        assert_eq!(((h as f64) / s).round() as usize, 1080);

        // 540p / small window -> divisor 4 -> exact 960x540, scale 0.5
        let (w, h, s) = compute_stream_geometry(1920, 1080, 2.0, true, Some((960, 540)));
        assert_eq!((w, h), (960, 540));
        assert!((s - 0.5).abs() < 1e-9);

        // 5K HiDPI (2560x1440 @ 2x) default -> exact 2:1 2560x1440
        let (w, h, s) = compute_stream_geometry(2560, 1440, 2.0, true, None);
        assert_eq!((w, h), (2560, 1440));
        assert!((s - 1.0).abs() < 1e-9);

        // Non-HiDPI 1x with no target stays untouched
        let (w, h, s) = compute_stream_geometry(3840, 2160, 1.0, true, None);
        assert_eq!((w, h), (3840, 2160));
        assert_eq!(s, 1.0);
    }
}
