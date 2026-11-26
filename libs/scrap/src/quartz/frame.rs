use std::{ops, ptr, slice};

use super::ffi::*;
use crate::Pixfmt;

pub struct RawSurface(IOSurfaceRef);

unsafe impl Send for RawSurface {}

impl RawSurface {
    #[inline]
    pub unsafe fn retain(surface: IOSurfaceRef) -> Option<Self> {
        if surface.is_null() {
            return None;
        }
        CFRetain(surface);
        IOSurfaceIncrementUseCount(surface);
        Some(Self(surface))
    }

    #[inline]
    pub fn as_ptr(&self) -> IOSurfaceRef {
        self.0
    }
}

impl Drop for RawSurface {
    fn drop(&mut self) {
        unsafe {
            IOSurfaceDecrementUseCount(self.0);
            CFRelease(self.0);
        }
    }
}

pub struct Frame {
    raw: RawSurface,
    pixfmt: Pixfmt,
    strides: [usize; 2],
    plane_count: usize,
    y_or_bgra: &'static [u8],
    uv: &'static [u8],
}

unsafe impl Send for Frame {}

impl Frame {
    pub unsafe fn new(surface: IOSurfaceRef) -> Frame {
        let raw = RawSurface::retain(surface).expect("null IOSurfaceRef in Frame::new");
        Self::from_raw(raw)
    }

    pub fn from_raw(raw: RawSurface) -> Frame {
        unsafe {
            let surface = raw.as_ptr();
            IOSurfaceLock(surface, SURFACE_LOCK_READ_ONLY, ptr::null_mut());

            let fourcc = IOSurfaceGetPixelFormat(surface);
            let planes = IOSurfaceGetPlaneCount(surface);
            let is_nv12 = (fourcc == PixelFormat::YCbCr420Video as i32
                || fourcc == PixelFormat::YCbCr420Full as i32)
                && planes >= 2;

            if is_nv12 {
                let y_ptr = IOSurfaceGetBaseAddressOfPlane(surface, 0) as *const u8;
                let y_stride = IOSurfaceGetBytesPerRowOfPlane(surface, 0);
                let y_h = IOSurfaceGetHeightOfPlane(surface, 0);
                let uv_ptr = IOSurfaceGetBaseAddressOfPlane(surface, 1) as *const u8;
                let uv_stride = IOSurfaceGetBytesPerRowOfPlane(surface, 1);
                let uv_h = IOSurfaceGetHeightOfPlane(surface, 1);

                let y_slice = if !y_ptr.is_null() && y_stride > 0 && y_h > 0 {
                    slice::from_raw_parts(y_ptr, y_stride * y_h)
                } else {
                    &[]
                };
                let uv_slice = if !uv_ptr.is_null() && uv_stride > 0 && uv_h > 0 {
                    slice::from_raw_parts(uv_ptr, uv_stride * uv_h)
                } else {
                    &[]
                };

                Frame {
                    raw,
                    pixfmt: Pixfmt::NV12,
                    strides: [y_stride, uv_stride],
                    plane_count: 2,
                    y_or_bgra: y_slice,
                    uv: uv_slice,
                }
            } else {
                let (ptr0, stride0, h0) = if planes >= 1 {
                    (
                        IOSurfaceGetBaseAddressOfPlane(surface, 0) as *const u8,
                        IOSurfaceGetBytesPerRowOfPlane(surface, 0),
                        IOSurfaceGetHeightOfPlane(surface, 0),
                    )
                } else {
                    (
                        IOSurfaceGetBaseAddress(surface) as *const u8,
                        IOSurfaceGetBytesPerRow(surface),
                        IOSurfaceGetHeight(surface),
                    )
                };
                let bgra_slice = if !ptr0.is_null() && stride0 > 0 && h0 > 0 {
                    slice::from_raw_parts(ptr0, stride0 * h0)
                } else {
                    &[]
                };
                Frame {
                    raw,
                    pixfmt: Pixfmt::BGRA,
                    strides: [stride0, 0],
                    plane_count: 1,
                    y_or_bgra: bgra_slice,
                    uv: &[],
                }
            }
        }
    }

    #[inline]
    pub fn primary_plane(&self) -> &[u8] {
        self.y_or_bgra
    }

    #[inline]
    pub fn nv12_planes(&self) -> Option<(&[u8], &[u8])> {
        if self.pixfmt == Pixfmt::NV12 {
            Some((self.y_or_bgra, self.uv))
        } else {
            None
        }
    }

    #[inline]
    pub fn pixfmt(&self) -> Pixfmt {
        self.pixfmt
    }

    #[inline]
    pub fn strides(&self) -> &[usize] {
        &self.strides[..self.plane_count]
    }

    #[inline]
    pub fn stride(&self) -> usize {
        self.strides[0]
    }
}

impl ops::Deref for Frame {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.y_or_bgra
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe {
            IOSurfaceUnlock(self.raw.as_ptr(), SURFACE_LOCK_READ_ONLY, ptr::null_mut());
        }
    }
}
