use std::ptr;

use hbb_common::libc::c_void;

use super::ffi::*;

#[derive(Clone, Copy)]
pub struct Config {
    /// Whether the cursor is visible.
    pub cursor: bool,
    /// Whether it should letterbox or stretch.
    pub letterbox: bool,
    /// Minimum seconds per frame.
    pub throttle: f64,
    /// How many frames are allocated.
    /// 5 gives headroom while 1 surface is queued and 1 is read.
    /// 8 is the maximum value.
    pub queue_length: i8,
}

impl Config {
    /// Don't forget to CFRelease this!
    pub fn build(self) -> CFDictionaryRef {
        self.build_for_format(PixelFormat::YCbCr420Video)
    }

    pub fn build_for_format(self, format: PixelFormat) -> CFDictionaryRef {
        unsafe {
            let throttle = CFNumberCreate(
                ptr::null_mut(),
                CFNumberType::Float64,
                &self.throttle as *const _ as *const c_void,
            );
            let queue_length = CFNumberCreate(
                ptr::null_mut(),
                CFNumberType::SInt8,
                &self.queue_length as *const _ as *const c_void,
            );

            let is_ycbcr =
                format == PixelFormat::YCbCr420Video || format == PixelFormat::YCbCr420Full;
            let srgb = if is_ycbcr {
                CGColorSpaceCreateWithName(kCGColorSpaceSRGB)
            } else {
                ptr::null_mut()
            };

            let res = if is_ycbcr && !srgb.is_null() {
                let keys: [CFStringRef; 6] = [
                    kCGDisplayStreamShowCursor,
                    kCGDisplayStreamPreserveAspectRatio,
                    kCGDisplayStreamMinimumFrameTime,
                    kCGDisplayStreamQueueDepth,
                    kCGDisplayStreamYCbCrMatrix,
                    kCGDisplayStreamColorSpace,
                ];
                let values: [*mut c_void; 6] = [
                    cfbool(self.cursor),
                    cfbool(self.letterbox),
                    throttle,
                    queue_length,
                    kCGDisplayStreamYCbCrMatrix_ITU_R_601_4,
                    srgb,
                ];
                CFDictionaryCreate(
                    ptr::null_mut(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    6,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                )
            } else {
                let keys: [CFStringRef; 4] = [
                    kCGDisplayStreamShowCursor,
                    kCGDisplayStreamPreserveAspectRatio,
                    kCGDisplayStreamMinimumFrameTime,
                    kCGDisplayStreamQueueDepth,
                ];
                let values: [*mut c_void; 4] = [
                    cfbool(self.cursor),
                    cfbool(self.letterbox),
                    throttle,
                    queue_length,
                ];
                CFDictionaryCreate(
                    ptr::null_mut(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    4,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                )
            };

            if !srgb.is_null() {
                CFRelease(srgb);
            }
            CFRelease(throttle);
            CFRelease(queue_length);

            res
        }
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            cursor: false,
            letterbox: true,
            throttle: 1.0 / 60.0,
            queue_length: 5,
        }
    }
}
