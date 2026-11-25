use std::ptr;
use std::sync::{Arc, Mutex};

use block::{Block, ConcreteBlock};
use hbb_common::libc::c_void;
use hbb_common::log;

use super::config::Config;
use super::display::Display;
use super::ffi::*;
use super::frame::{Frame, RawSurface};

type RawCallback = Box<dyn Fn(RawSurface) + Send + 'static>;

unsafe extern "C" fn sck_frame_trampoline(user_data: *mut c_void, surface: IOSurfaceRef) {
    if user_data.is_null() || surface.is_null() {
        return;
    }
    if let Some(raw) = RawSurface::retain(surface) {
        let cb = &*(user_data as *const RawCallback);
        cb(raw);
    }
}

enum Backend {
    ScreenCaptureKit {
        handle: *mut c_void,
        cb_ptr: *mut RawCallback,
    },
    DisplayStream {
        stream: CGDisplayStreamRef,
        queue: DispatchQueue,
        stopped: Arc<Mutex<bool>>,
    },
}

pub struct Capturer {
    backend: Backend,
    width: usize,
    height: usize,
    format: PixelFormat,
    display: Display,
}

impl Capturer {
    pub fn new_raw<F: Fn(RawSurface) + Send + 'static>(
        display: Display,
        width: usize,
        height: usize,
        format: PixelFormat,
        config: Config,
        handler: F,
    ) -> Result<Capturer, CGError> {
        let width = (width & !1).max(2);
        let height = (height & !1).max(2);
        let cb: RawCallback = Box::new(handler);

        if format == PixelFormat::YCbCr420Video || format == PixelFormat::YCbCr420Full {
            let cb_ptr = Box::into_raw(Box::new(cb));
            let sck_handle = unsafe {
                rustdesk_sck_capturer_create(
                    display.id(),
                    width,
                    height,
                    config.cursor,
                    config.throttle,
                    config.queue_length as i32,
                    sck_frame_trampoline,
                    cb_ptr as *mut c_void,
                )
            };
            if !sck_handle.is_null() {
                log::info!(
                    "macOS ScreenCaptureKit NV12 capturer started: display={}, {}x{}, scale={:.3}",
                    display.id(),
                    width,
                    height,
                    display.scale()
                );
                return Ok(Capturer {
                    backend: Backend::ScreenCaptureKit {
                        handle: sck_handle,
                        cb_ptr,
                    },
                    width,
                    height,
                    format,
                    display,
                });
            }
            // Reclaim callback box and fall back to CGDisplayStream NV12 (e.g. LoginWindow / macOS < 12.3).
            let cb = unsafe { *Box::from_raw(cb_ptr) };
            log::info!(
                "ScreenCaptureKit unavailable for display={}, falling back to CGDisplayStream NV12 ({}x{})",
                display.id(),
                width,
                height
            );
            return Self::new_display_stream(display, width, height, format, config, cb);
        }

        Self::new_display_stream(display, width, height, format, config, cb)
    }

    pub fn new<F: Fn(Frame) + Send + 'static>(
        display: Display,
        width: usize,
        height: usize,
        format: PixelFormat,
        config: Config,
        handler: F,
    ) -> Result<Capturer, CGError> {
        Self::new_raw(display, width, height, format, config, move |raw: RawSurface| {
            handler(Frame::from_raw(raw));
        })
    }

    fn new_display_stream(
        display: Display,
        width: usize,
        height: usize,
        format: PixelFormat,
        config: Config,
        cb: RawCallback,
    ) -> Result<Capturer, CGError> {
        let stopped = Arc::new(Mutex::new(false));
        let cloned_stopped = stopped.clone();
        let handler: FrameAvailableHandler = ConcreteBlock::new(
            move |status: CGDisplayStreamFrameStatus,
                  _display_time: u64,
                  surface: IOSurfaceRef,
                  update_ref: CGDisplayStreamUpdateRef| {
                use self::CGDisplayStreamFrameStatus::*;
                if status == Stopped {
                    if let Ok(mut lock) = cloned_stopped.lock() {
                        *lock = true;
                    }
                    return;
                }
                if status == FrameComplete && !surface.is_null() {
                    if !update_ref.is_null() {
                        let mut rect_count: usize = 0;
                        unsafe {
                            let _ = CGDisplayStreamUpdateGetRects(
                                update_ref,
                                DISPLAY_STREAM_UPDATE_DIRTY_RECTS,
                                &mut rect_count,
                            );
                        }
                        if rect_count == 0 {
                            return;
                        }
                    }
                    if let Some(raw) = unsafe { RawSurface::retain(surface) } {
                        cb(raw);
                    }
                }
            },
        )
        .copy();

        let queue = unsafe {
            dispatch_queue_create(
                b"quadrupleslap.scrap\0".as_ptr() as *const i8,
                ptr::null_mut(),
            )
        };

        let stream = unsafe {
            let cf_config = config.build_for_format(format);
            let stream = CGDisplayStreamCreateWithDispatchQueue(
                display.id(),
                width,
                height,
                format,
                cf_config,
                queue,
                &*handler as *const Block<_, _> as *const c_void,
            );
            CFRelease(cf_config);
            stream
        };

        if stream.is_null() {
            unsafe {
                dispatch_release(queue);
            }
            return Err(CGError::Failure);
        }

        match unsafe { CGDisplayStreamStart(stream) } {
            CGError::Success => Ok(Capturer {
                backend: Backend::DisplayStream {
                    stream,
                    queue,
                    stopped,
                },
                width,
                height,
                format,
                display,
            }),
            x => {
                unsafe {
                    CFRelease(stream);
                    dispatch_release(queue);
                }
                Err(x)
            }
        }
    }

    #[inline]
    pub fn has_error(&self) -> bool {
        match &self.backend {
            Backend::ScreenCaptureKit { handle, .. } => unsafe {
                rustdesk_sck_capturer_has_error(*handle)
            },
            Backend::DisplayStream { .. } => false,
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }
    pub fn format(&self) -> PixelFormat {
        self.format
    }
    pub fn display(&self) -> Display {
        self.display
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        match &mut self.backend {
            Backend::ScreenCaptureKit { handle, cb_ptr } => unsafe {
                rustdesk_sck_capturer_destroy(*handle);
                *handle = ptr::null_mut();
                if !cb_ptr.is_null() {
                    drop(Box::from_raw(*cb_ptr));
                    *cb_ptr = ptr::null_mut();
                }
            },
            Backend::DisplayStream {
                stream,
                queue,
                stopped,
            } => unsafe {
                let err = CGDisplayStreamStop(*stream);
                if err == CGError::Success {
                    for _ in 0..20 {
                        if stopped.lock().map(|g| *g).unwrap_or(true) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                }
                CFRelease(*stream);
                dispatch_release(*queue);
            },
        }
    }
}
