#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(improper_ctypes)]
#![allow(dead_code)]

include!(concat!(env!("OUT_DIR"), "/yuv_ffi.rs"));

#[cfg(not(target_os = "ios"))]
use crate::PixelBuffer;
use crate::{generate_call_macro, EncodeYuvFormat, TraitPixelBuffer};
use hbb_common::{bail, log, ResultType};

generate_call_macro!(call_yuv, false);

#[cfg(not(target_os = "ios"))]
static LUMA_SHARPEN_STRENGTH: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(55);

#[cfg(not(target_os = "ios"))]
// 0 = off, 1 = HiDPI downscale only (default), 2 = always
static LUMA_SHARPEN_MODE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(1);

#[cfg(not(target_os = "ios"))]
pub fn set_luma_sharpen(strength: u32, mode: u32) {
    LUMA_SHARPEN_STRENGTH.store(strength.min(100), std::sync::atomic::Ordering::Relaxed);
    LUMA_SHARPEN_MODE.store(mode.min(2), std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(target_os = "ios"))]
#[inline]
fn effective_sharpen_strength(is_hidpi_downscaled: bool) -> i32 {
    let mode = LUMA_SHARPEN_MODE.load(std::sync::atomic::Ordering::Relaxed);
    let strength = LUMA_SHARPEN_STRENGTH.load(std::sync::atomic::Ordering::Relaxed).min(100);
    match mode {
        0 => 0,
        2 => strength as i32,
        _ => {
            if is_hidpi_downscaled {
                strength as i32
            } else {
                0
            }
        }
    }
}

#[cfg(not(target_os = "ios"))]
#[inline]
fn sharpen_y_pixel(c: i32, u: i32, d: i32, l: i32, r: i32, strength: i32) -> u8 {
    let mn = c.min(u).min(d).min(l).min(r);
    let mx = c.max(u).max(d).max(l).max(r);
    let span = mx - mn;
    if span < 8 || strength <= 0 {
        return c as u8;
    }
    let avg = (u + d + l + r + 2) >> 2;
    let diff = c - avg;
    // Fixed-point contrast-adaptive unsharp scaled by user strength (1..=100%).
    // Tight overshoot clamp prevents white/dark halo rings on flat backgrounds
    // while restoring crisp 1x contrast on HiDPI-downscaled text stems.
    let max_delta = 12 + (strength * 28) / 100;
    let delta = ((diff * strength * 10) >> 10).clamp(-max_delta, max_delta);
    let overshoot = (span / 3).min(10 + (strength * 22) / 100);
    let lo = (mn - overshoot).max(0);
    let hi = (mx + overshoot).min(255);
    (c + delta).clamp(lo, hi) as u8
}

#[cfg(not(target_os = "ios"))]
unsafe fn copy_plane_sharpen_y(
    src: *const u8,
    src_stride: usize,
    dst: *mut u8,
    dst_stride: usize,
    width: usize,
    height: usize,
    strength: i32,
) {
    if height == 0 || width == 0 {
        return;
    }
    if width < 3 || height < 3 || strength <= 0 {
        copy_plane(src, src_stride, dst, dst_stride, width, height);
        return;
    }
    std::ptr::copy_nonoverlapping(src, dst, width);
    for row in 1..(height - 1) {
        let up = src.add((row - 1) * src_stride);
        let cur = src.add(row * src_stride);
        let dn = src.add((row + 1) * src_stride);
        let out = dst.add(row * dst_stride);
        *out = *cur;
        for x in 1..(width - 1) {
            let c = *cur.add(x) as i32;
            let u = *up.add(x) as i32;
            let d = *dn.add(x) as i32;
            let l = *cur.add(x - 1) as i32;
            let r = *cur.add(x + 1) as i32;
            *out.add(x) = sharpen_y_pixel(c, u, d, l, r, strength);
        }
        *out.add(width - 1) = *cur.add(width - 1);
    }
    std::ptr::copy_nonoverlapping(
        src.add((height - 1) * src_stride),
        dst.add((height - 1) * dst_stride),
        width,
    );
}

#[cfg(not(target_os = "ios"))]
unsafe fn sharpen_plane_in_place(
    dst: *mut u8,
    stride: usize,
    width: usize,
    height: usize,
    strength: i32,
) {
    if width < 3 || height < 3 || strength <= 0 {
        return;
    }
    let mut prev = vec![0u8; width];
    let mut cur = vec![0u8; width];
    std::ptr::copy_nonoverlapping(dst, prev.as_mut_ptr(), width);
    std::ptr::copy_nonoverlapping(dst.add(stride), cur.as_mut_ptr(), width);
    for row in 1..(height - 1) {
        let dn = dst.add((row + 1) * stride);
        let out = dst.add(row * stride);
        for x in 1..(width - 1) {
            let c = cur[x] as i32;
            let u = prev[x] as i32;
            let d = *dn.add(x) as i32;
            let l = cur[x - 1] as i32;
            let r = cur[x + 1] as i32;
            *out.add(x) = sharpen_y_pixel(c, u, d, l, r, strength);
        }
        if row + 1 < height - 1 {
            prev.copy_from_slice(&cur);
            std::ptr::copy_nonoverlapping(dn, cur.as_mut_ptr(), width);
        }
    }
}

#[cfg(not(target_os = "ios"))]
#[inline]
unsafe fn copy_plane(
    src: *const u8,
    src_stride: usize,
    dst: *mut u8,
    dst_stride: usize,
    row_bytes: usize,
    rows: usize,
) {
    if rows == 0 || row_bytes == 0 {
        return;
    }
    if src_stride == dst_stride && src_stride == row_bytes {
        std::ptr::copy_nonoverlapping(src, dst, row_bytes * rows);
    } else {
        for r in 0..rows {
            std::ptr::copy_nonoverlapping(
                src.add(r * src_stride),
                dst.add(r * dst_stride),
                row_bytes,
            );
        }
    }
}

#[cfg(not(target_os = "ios"))]
fn extract_nv12_planes<'a, 'b>(
    captured: &'a PixelBuffer<'b>,
    src: &'a [u8],
    src_stride: &[usize],
    src_width: usize,
    src_height: usize,
) -> ResultType<(&'a [u8], &'a [u8])> {
    let uv_rows = (src_height + 1) / 2;
    let uv_row_bytes = ((src_width + 1) / 2) * 2;
    if src_stride.len() < 2 || src_stride[0] < src_width || src_stride[1] < uv_row_bytes {
        bail!(
            "invalid nv12 stride {:?} for {}x{}",
            src_stride,
            src_width,
            src_height
        );
    }
    let (src_y, src_uv) = if let Some(planes) = captured.nv12_planes() {
        planes
    } else {
        let y_len = src_stride[0] * src_height;
        if src.len() < y_len {
            bail!("nv12 buffer too short for Y plane: {} < {}", src.len(), y_len);
        }
        (&src[..y_len], &src[y_len..])
    };
    if src_height > 0 {
        let min_y = src_stride[0] * (src_height - 1) + src_width;
        if src_y.len() < min_y {
            bail!("nv12 Y plane too short: {} < {}", src_y.len(), min_y);
        }
    }
    if uv_rows > 0 {
        let min_uv = src_stride[1] * (uv_rows - 1) + uv_row_bytes;
        if src_uv.len() < min_uv {
            bail!("nv12 UV plane too short: {} < {}", src_uv.len(), min_uv);
        }
    }
    Ok((src_y, src_uv))
}

#[cfg(not(target_os = "ios"))]
pub fn convert_to_yuv(
    captured: &PixelBuffer,
    dst_fmt: EncodeYuvFormat,
    dst: &mut Vec<u8>,
    mid_data: &mut Vec<u8>,
) -> ResultType<()> {
    let src = captured.data();
    let src_stride = captured.stride();
    let src_pixfmt = captured.pixfmt();
    let src_width = captured.width();
    let src_height = captured.height();
    if src_width > dst_fmt.w || src_height > dst_fmt.h {
        bail!(
            "src rect > dst rect: ({src_width}, {src_height}) > ({},{})",
            dst_fmt.w,
            dst_fmt.h
        );
    }
    if src_pixfmt == crate::Pixfmt::BGRA
        || src_pixfmt == crate::Pixfmt::RGBA
        || src_pixfmt == crate::Pixfmt::RGB565LE
    {
        // stride is calculated, not real, so we need to check it
        if src_stride[0] < src_width * src_pixfmt.bytes_per_pixel() {
            bail!(
                "src_stride too small: {} < {}",
                src_stride[0],
                src_width * src_pixfmt.bytes_per_pixel()
            );
        }
        if src.len() < src_stride[0] * src_height {
            bail!(
                "wrong src len, {} < {} * {}",
                src.len(),
                src_stride[0],
                src_height
            );
        }
    }
    let align = |x: usize| (x + 63) / 64 * 64;
    let unsupported = format!(
        "unsupported pixfmt conversion: {src_pixfmt:?} -> {:?}",
        dst_fmt.pixfmt
    );

    match (src_pixfmt, dst_fmt.pixfmt) {
        (crate::Pixfmt::NV12, crate::Pixfmt::NV12) => {
            let (src_y, src_uv) =
                extract_nv12_planes(captured, src, &src_stride, src_width, src_height)?;
            let dst_stride_y = dst_fmt.stride[0];
            let dst_stride_uv = dst_fmt.stride[1];
            let uv_rows = (src_height + 1) / 2;
            let uv_row_bytes = ((src_width + 1) / 2) * 2;
            if dst_stride_y < src_width || dst_stride_uv < uv_row_bytes {
                bail!(
                    "dst nv12 stride too small: ({dst_stride_y}, {dst_stride_uv}) for {}x{}",
                    src_width,
                    src_height
                );
            }
            let min_len = dst_fmt.u + dst_stride_uv * uv_rows;
            let aligned_len = align(dst_fmt.h) * (align(dst_stride_y) + align(dst_stride_uv / 2));
            dst.resize(aligned_len.max(min_len), 0);
            unsafe {
                let dst_y = dst.as_mut_ptr();
                let dst_uv = dst.as_mut_ptr().add(dst_fmt.u);
                let sharpen_strength = effective_sharpen_strength(captured.should_sharpen_luma());
                if sharpen_strength > 0 {
                    copy_plane_sharpen_y(
                        src_y.as_ptr(),
                        src_stride[0],
                        dst_y,
                        dst_stride_y,
                        src_width,
                        src_height,
                        sharpen_strength,
                    );
                } else {
                    copy_plane(
                        src_y.as_ptr(),
                        src_stride[0],
                        dst_y,
                        dst_stride_y,
                        src_width,
                        src_height,
                    );
                }
                copy_plane(
                    src_uv.as_ptr(),
                    src_stride[1],
                    dst_uv,
                    dst_stride_uv,
                    uv_row_bytes,
                    uv_rows,
                );
            }
        }
        (crate::Pixfmt::NV12, crate::Pixfmt::I420) => {
            let (src_y, src_uv) =
                extract_nv12_planes(captured, src, &src_stride, src_width, src_height)?;
            let dst_stride_y = dst_fmt.stride[0];
            let dst_stride_uv = dst_fmt.stride[1];
            dst.resize(dst_fmt.h * dst_stride_y * 2, 0);
            let dst_y = dst.as_mut_ptr();
            let dst_u = dst[dst_fmt.u..].as_mut_ptr();
            let dst_v = dst[dst_fmt.v..].as_mut_ptr();
            call_yuv!(NV12ToI420(
                src_y.as_ptr(),
                src_stride[0] as _,
                src_uv.as_ptr(),
                src_stride[1] as _,
                dst_y,
                dst_stride_y as _,
                dst_u,
                dst_stride_uv as _,
                dst_v,
                dst_stride_uv as _,
                src_width as _,
                src_height as _,
            ));
            let sharpen_strength = effective_sharpen_strength(captured.should_sharpen_luma());
            if sharpen_strength > 0 {
                unsafe {
                    sharpen_plane_in_place(
                        dst_y,
                        dst_stride_y,
                        src_width,
                        src_height,
                        sharpen_strength,
                    );
                }
            }
        }
        (crate::Pixfmt::NV12, crate::Pixfmt::I444) => {
            let (src_y, src_uv) =
                extract_nv12_planes(captured, src, &src_stride, src_width, src_height)?;
            let dst_stride_y = dst_fmt.stride[0];
            let dst_stride_u = dst_fmt.stride[1];
            let dst_stride_v = dst_fmt.stride[2];
            dst.resize(
                align(dst_fmt.h)
                    * (align(dst_stride_y) + align(dst_stride_u) + align(dst_stride_v)),
                0,
            );
            let mid_stride = src_width * 4;
            mid_data.resize(mid_stride * src_height, 0);
            call_yuv!(NV12ToARGB(
                src_y.as_ptr(),
                src_stride[0] as _,
                src_uv.as_ptr(),
                src_stride[1] as _,
                mid_data.as_mut_ptr(),
                mid_stride as _,
                src_width as _,
                src_height as _,
            ));
            let dst_y = dst.as_mut_ptr();
            let dst_u = dst[dst_fmt.u..].as_mut_ptr();
            let dst_v = dst[dst_fmt.v..].as_mut_ptr();
            call_yuv!(ARGBToI444(
                mid_data.as_ptr(),
                mid_stride as _,
                dst_y,
                dst_stride_y as _,
                dst_u,
                dst_stride_u as _,
                dst_v,
                dst_stride_v as _,
                src_width as _,
                src_height as _,
            ));
        }
        (crate::Pixfmt::BGRA, crate::Pixfmt::I420)
        | (crate::Pixfmt::RGBA, crate::Pixfmt::I420)
        | (crate::Pixfmt::RGB565LE, crate::Pixfmt::I420) => {
            let dst_stride_y = dst_fmt.stride[0];
            let dst_stride_uv = dst_fmt.stride[1];
            dst.resize(dst_fmt.h * dst_stride_y * 2, 0); // waste some memory to ensure memory safety
            let dst_y = dst.as_mut_ptr();
            let dst_u = dst[dst_fmt.u..].as_mut_ptr();
            let dst_v = dst[dst_fmt.v..].as_mut_ptr();
            let f = match src_pixfmt {
                crate::Pixfmt::BGRA => ARGBToI420,
                crate::Pixfmt::RGBA => ABGRToI420,
                crate::Pixfmt::RGB565LE => RGB565ToI420,
                _ => bail!(unsupported),
            };
            call_yuv!(f(
                src.as_ptr(),
                src_stride[0] as _,
                dst_y,
                dst_stride_y as _,
                dst_u,
                dst_stride_uv as _,
                dst_v,
                dst_stride_uv as _,
                src_width as _,
                src_height as _,
            ));
        }
        (crate::Pixfmt::BGRA, crate::Pixfmt::NV12)
        | (crate::Pixfmt::RGBA, crate::Pixfmt::NV12)
        | (crate::Pixfmt::RGB565LE, crate::Pixfmt::NV12) => {
            let dst_stride_y = dst_fmt.stride[0];
            let dst_stride_uv = dst_fmt.stride[1];
            dst.resize(
                align(dst_fmt.h) * (align(dst_stride_y) + align(dst_stride_uv / 2)),
                0,
            );
            let dst_y = dst.as_mut_ptr();
            let dst_uv = dst[dst_fmt.u..].as_mut_ptr();
            let (input, input_stride) = match src_pixfmt {
                crate::Pixfmt::BGRA => (src.as_ptr(), src_stride[0]),
                crate::Pixfmt::RGBA => (src.as_ptr(), src_stride[0]),
                crate::Pixfmt::RGB565LE => {
                    let mid_stride = src_width * 4;
                    mid_data.resize(mid_stride * src_height, 0);
                    call_yuv!(RGB565ToARGB(
                        src.as_ptr(),
                        src_stride[0] as _,
                        mid_data.as_mut_ptr(),
                        mid_stride as _,
                        src_width as _,
                        src_height as _,
                    ));
                    (mid_data.as_ptr(), mid_stride)
                }
                _ => bail!(unsupported),
            };
            let f = match src_pixfmt {
                crate::Pixfmt::BGRA => ARGBToNV12,
                crate::Pixfmt::RGBA => ABGRToNV12,
                crate::Pixfmt::RGB565LE => ARGBToNV12,
                _ => bail!(unsupported),
            };
            call_yuv!(f(
                input,
                input_stride as _,
                dst_y,
                dst_stride_y as _,
                dst_uv,
                dst_stride_uv as _,
                src_width as _,
                src_height as _,
            ));
        }
        (crate::Pixfmt::BGRA, crate::Pixfmt::I444)
        | (crate::Pixfmt::RGBA, crate::Pixfmt::I444)
        | (crate::Pixfmt::RGB565LE, crate::Pixfmt::I444) => {
            let dst_stride_y = dst_fmt.stride[0];
            let dst_stride_u = dst_fmt.stride[1];
            let dst_stride_v = dst_fmt.stride[2];
            dst.resize(
                align(dst_fmt.h)
                    * (align(dst_stride_y) + align(dst_stride_u) + align(dst_stride_v)),
                0,
            );
            let dst_y = dst.as_mut_ptr();
            let dst_u = dst[dst_fmt.u..].as_mut_ptr();
            let dst_v = dst[dst_fmt.v..].as_mut_ptr();
            let (input, input_stride) = match src_pixfmt {
                crate::Pixfmt::BGRA => (src.as_ptr(), src_stride[0]),
                crate::Pixfmt::RGBA => {
                    mid_data.resize(src.len(), 0);
                    call_yuv!(ABGRToARGB(
                        src.as_ptr(),
                        src_stride[0] as _,
                        mid_data.as_mut_ptr(),
                        src_stride[0] as _,
                        src_width as _,
                        src_height as _,
                    ));
                    (mid_data.as_ptr(), src_stride[0])
                }
                crate::Pixfmt::RGB565LE => {
                    let mid_stride = src_width * 4;
                    mid_data.resize(mid_stride * src_height, 0);
                    call_yuv!(RGB565ToARGB(
                        src.as_ptr(),
                        src_stride[0] as _,
                        mid_data.as_mut_ptr(),
                        mid_stride as _,
                        src_width as _,
                        src_height as _,
                    ));
                    (mid_data.as_ptr(), mid_stride)
                }
                _ => bail!(unsupported),
            };

            call_yuv!(ARGBToI444(
                input,
                input_stride as _,
                dst_y,
                dst_stride_y as _,
                dst_u,
                dst_stride_u as _,
                dst_v,
                dst_stride_v as _,
                src_width as _,
                src_height as _,
            ));
            let sharpen_strength = effective_sharpen_strength(captured.should_sharpen_luma());
            if sharpen_strength > 0 {
                unsafe {
                    sharpen_plane_in_place(
                        dst_y,
                        dst_stride_y,
                        src_width,
                        src_height,
                        sharpen_strength,
                    );
                }
            }
        }
        _ => {
            bail!(unsupported);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "ios"))]
pub fn convert(captured: &PixelBuffer, pixfmt: crate::Pixfmt, dst: &mut Vec<u8>) -> ResultType<()> {
    if captured.pixfmt() == pixfmt {
        if let Some((y, uv)) = captured.nv12_planes() {
            dst.extend_from_slice(y);
            dst.extend_from_slice(uv);
        } else {
            dst.extend_from_slice(captured.data());
        }
        return Ok(());
    }

    let src = captured.data();
    let src_stride = captured.stride();
    let src_pixfmt = captured.pixfmt();
    let src_width = captured.width();
    let src_height = captured.height();

    let unsupported = format!(
        "unsupported pixfmt conversion: {src_pixfmt:?} -> {:?}",
        pixfmt
    );

    match (src_pixfmt, pixfmt) {
        (crate::Pixfmt::NV12, crate::Pixfmt::RGBA) | (crate::Pixfmt::NV12, crate::Pixfmt::BGRA) => {
            let (src_y, src_uv) =
                extract_nv12_planes(captured, src, &src_stride, src_width, src_height)?;
            let dst_stride = src_width * 4;
            dst.resize(dst_stride * src_height, 0);
            let f = if pixfmt == crate::Pixfmt::RGBA {
                NV12ToABGR
            } else {
                NV12ToARGB
            };
            call_yuv!(f(
                src_y.as_ptr(),
                src_stride[0] as _,
                src_uv.as_ptr(),
                src_stride[1] as _,
                dst.as_mut_ptr(),
                dst_stride as _,
                src_width as _,
                src_height as _,
            ));
        }
        (crate::Pixfmt::BGRA, crate::Pixfmt::RGBA) | (crate::Pixfmt::RGBA, crate::Pixfmt::BGRA) => {
            dst.resize(src.len(), 0);
            call_yuv!(ABGRToARGB(
                src.as_ptr(),
                src_stride[0] as _,
                dst.as_mut_ptr(),
                src_stride[0] as _,
                src_width as _,
                src_height as _,
            ));
        }
        _ => {
            bail!(unsupported);
        }
    }
    Ok(())
}

#[cfg(all(test, not(target_os = "ios")))]
mod luma_sharpen_tests {
    use super::{copy_plane_sharpen_y, sharpen_plane_in_place, sharpen_y_pixel};

    #[test]
    fn hidpi_y_sharpen_preserves_flat_and_enhances_text_stems() {
        // Flat or gentle gradient region (span < 8) must remain completely untouched.
        assert_eq!(sharpen_y_pixel(128, 126, 130, 127, 129, 80), 128);
        assert_eq!(sharpen_y_pixel(100, 100, 100, 235, 235, 0), 100);

        // Monotonic strengthening on softened thin dark text stem on white background (235).
        let s35 = sharpen_y_pixel(100, 100, 100, 235, 235, 35);
        let s60 = sharpen_y_pixel(100, 100, 100, 235, 235, 60);
        let s100 = sharpen_y_pixel(100, 100, 100, 235, 235, 100);
        assert!(s35 < 100 && s60 <= s35 && s100 < s60, "got {s35} {s60} {s100}");

        // Softened thin light text stem on dark-mode background (30) gets crisper (brighter).
        let light_stem = sharpen_y_pixel(180, 180, 180, 30, 30, 60);
        assert!(light_stem > 180 && light_stem <= 212, "got {}", light_stem);

        // Single-pass copy_plane_sharpen_y and in-place sharpen_plane_in_place must match exactly.
        let w = 6;
        let h = 5;
        let mut src = vec![235u8; w * h];
        for r in 1..4 {
            src[r * w + 2] = 100;
        }
        let mut out_copy = vec![0u8; w * h];
        let mut out_inplace = src.clone();
        unsafe {
            copy_plane_sharpen_y(src.as_ptr(), w, out_copy.as_mut_ptr(), w, w, h, 55);
            sharpen_plane_in_place(out_inplace.as_mut_ptr(), w, w, h, 55);
        }
        assert_eq!(out_copy, out_inplace);
        assert!(out_copy[2 * w + 2] < 100);
        assert_eq!(out_copy[0], 235);
    }
}
