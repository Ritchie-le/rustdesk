use crate::{quartz, Frame, Pixfmt};
use std::io;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, TryLockError};

pub struct Capturer {
    inner: quartz::Capturer,
    frame: Arc<Mutex<Option<quartz::RawSurface>>>,
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        let frame = Arc::new(Mutex::new(None));

        let f = frame.clone();
        let inner = quartz::Capturer::new_raw(
            display.0,
            display.width(),
            display.height(),
            quartz::PixelFormat::YCbCr420Video,
            Default::default(),
            move |raw: quartz::RawSurface| {
                if let Ok(mut f) = f.lock() {
                    *f = Some(raw);
                }
            },
        )
        .map_err(|_| io::Error::from(io::ErrorKind::Other))?;

        Ok(Capturer { inner, frame })
    }

    pub fn width(&self) -> usize {
        self.inner.width()
    }

    pub fn height(&self) -> usize {
        self.inner.height()
    }
}

impl crate::TraitCapturer for Capturer {
    fn frame<'a>(&'a mut self, _timeout_ms: std::time::Duration) -> io::Result<Frame<'a>> {
        if self.inner.has_error() {
            return Err(io::ErrorKind::ConnectionReset.into());
        }
        match self.frame.try_lock() {
            Ok(mut handle) => match handle.take() {
                Some(raw) => {
                    let frame = quartz::Frame::from_raw(raw);
                    if frame.primary_plane().is_empty() {
                        return Err(io::ErrorKind::WouldBlock.into());
                    }
                    Ok(Frame::PixelBuffer(PixelBuffer {
                        frame,
                        data: PhantomData,
                        width: self.width(),
                        height: self.height(),
                    }))
                }
                None => Err(io::ErrorKind::WouldBlock.into()),
            },

            Err(TryLockError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),

            Err(TryLockError::Poisoned(..)) => Err(io::ErrorKind::Other.into()),
        }
    }
}

pub struct PixelBuffer<'a> {
    frame: quartz::Frame,
    data: PhantomData<&'a [u8]>,
    width: usize,
    height: usize,
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
