use crate::{quartz, Frame, Pixfmt};
use std::io;
use std::marker::PhantomData;
use std::sync::{Arc, Condvar, Mutex};

pub struct Capturer {
    inner: quartz::Capturer,
    frame: Arc<(Mutex<Option<quartz::RawSurface>>, Condvar)>,
    sharpen_luma: bool,
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        let frame = Arc::new((Mutex::new(None), Condvar::new()));
        let sharpen_luma = display.0.is_hidpi_downscaled();

        let pixel_format = if crate::codec::Encoder::prefer_i444_capture() {
            quartz::PixelFormat::Argb8888
        } else {
            quartz::PixelFormat::YCbCr420Video
        };
        let f = frame.clone();
        let inner = quartz::Capturer::new_raw(
            display.0,
            display.width(),
            display.height(),
            pixel_format,
            Default::default(),
            move |raw: quartz::RawSurface| {
                let (lock, cvar) = &*f;
                if let Ok(mut slot) = lock.lock() {
                    *slot = Some(raw);
                    cvar.notify_one();
                }
            },
        )
        .map_err(|_| io::Error::from(io::ErrorKind::Other))?;

        Ok(Capturer {
            inner,
            frame,
            sharpen_luma,
        })
    }

    pub fn width(&self) -> usize {
        self.inner.width()
    }

    pub fn height(&self) -> usize {
        self.inner.height()
    }
}

impl crate::TraitCapturer for Capturer {
    fn frame<'a>(&'a mut self, timeout_ms: std::time::Duration) -> io::Result<Frame<'a>> {
        if self.inner.has_error() {
            return Err(io::ErrorKind::ConnectionReset.into());
        }
        let (lock, cvar) = &*self.frame;
        let mut handle = lock.lock().map_err(|_| io::Error::from(io::ErrorKind::Other))?;
        if handle.is_none() && !timeout_ms.is_zero() {
            let (guard, _) = cvar
                .wait_timeout(handle, timeout_ms)
                .map_err(|_| io::Error::from(io::ErrorKind::Other))?;
            handle = guard;
        }
        match handle.take() {
            Some(raw) => {
                drop(handle);
                let frame = quartz::Frame::from_raw(raw);
                if frame.primary_plane().is_empty() {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                Ok(Frame::PixelBuffer(PixelBuffer {
                    frame,
                    data: PhantomData,
                    width: self.width(),
                    height: self.height(),
                    sharpen_luma: self.sharpen_luma,
                }))
            }
            None => Err(io::ErrorKind::WouldBlock.into()),
        }
    }
}

pub struct PixelBuffer<'a> {
    frame: quartz::Frame,
    data: PhantomData<&'a [u8]>,
    width: usize,
    height: usize,
    sharpen_luma: bool,
}

impl<'a> crate::TraitPixelBuffer for PixelBuffer<'a> {
    fn data(&self) -> &[u8] {
        self.frame.primary_plane()
    }

    fn width(&self) -> usize {
        self.width
    }

    fn height(&self) -> usize {
        self.height
    }

    fn stride(&self) -> Vec<usize> {
        self.frame.strides().to_vec()
    }

    fn pixfmt(&self) -> Pixfmt {
        self.frame.pixfmt()
    }

    fn nv12_planes(&self) -> Option<(&[u8], &[u8])> {
        self.frame.nv12_planes()
    }

    fn should_sharpen_luma(&self) -> bool {
        self.sharpen_luma
    }
}

pub struct Display(quartz::Display);

impl Display {
    pub fn primary() -> io::Result<Display> {
        Ok(Display(quartz::Display::primary()))
    }

    pub fn all() -> io::Result<Vec<Display>> {
        Ok(quartz::Display::online()
            .map_err(|_| io::Error::from(io::ErrorKind::Other))?
            .into_iter()
            .map(Display)
            .collect())
    }

    pub fn width(&self) -> usize {
        self.0.width()
    }

    pub fn height(&self) -> usize {
        self.0.height()
    }

    pub fn scale(&self) -> f64 {
        self.0.scale()
    }

    pub fn name(&self) -> String {
        self.0.id().to_string()
    }

    pub fn is_online(&self) -> bool {
        self.0.is_online()
    }

    pub fn origin(&self) -> (i32, i32) {
        let o = self.0.bounds().origin;
        (o.x as _, o.y as _)
    }

    pub fn is_primary(&self) -> bool {
        self.0.is_primary()
    }
}
