use crate::{
    codec::{base_bitrate, codec_thread_num, enable_hwcodec_option, EncoderApi, EncoderCfg},
    convert::*,
    CodecFormat, EncodeInput, ImageFormat, ImageRgb, Pixfmt, HW_STRIDE_ALIGN,
};
use base::message_proto::{EncodedVideoFrame, EncodedVideoFrames, VideoFrame};
use hbb_common::{
    anyhow::{anyhow, bail, Context},
    bytes::Bytes,
    log,
    serde_derive::{Deserialize, Serialize},
    serde_json, ResultType,
};
use hwcodec::{
    common::{
        DataFormat, HwcodecErrno,
        Quality::{self, *},
        RateControl::{self, *},
    },
    ffmpeg::AVPixelFormat,
    ffmpeg_ram::{
        decode::{DecodeContext, DecodeFrame, Decoder},
        encode::{EncodeContext, EncodeFrame, Encoder},
        ffmpeg_linesize_offset_length, CodecInfo,
    },
};

#[cfg(all(test, windows, feature = "vram"))]
mod tests;

const DEFAULT_PIXFMT: AVPixelFormat = AVPixelFormat::AV_PIX_FMT_NV12;
pub const DEFAULT_FPS: i32 = 30;

static TARGET_FPS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(DEFAULT_FPS as u32);
static MACOS_H265_MAX_KBS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(14_000);

pub fn set_target_fps(fps: u32) {
    TARGET_FPS.store(fps.clamp(10, 120), std::sync::atomic::Ordering::Relaxed);
}

pub fn target_fps() -> u32 {
    TARGET_FPS.load(std::sync::atomic::Ordering::Relaxed).clamp(10, 120)
}

pub fn set_macos_h265_max_kbs(kbs: u32) {
    MACOS_H265_MAX_KBS.store(kbs.clamp(6_000, 42_000), std::sync::atomic::Ordering::Relaxed);
}

pub fn macos_h265_max_kbs() -> u32 {
    MACOS_H265_MAX_KBS
        .load(std::sync::atomic::Ordering::Relaxed)
        .clamp(6_000, 42_000)
}
const DEFAULT_GOP: i32 = i32::MAX;
const DEFAULT_HW_QUALITY: Quality = Quality_Default;
pub const ERR_HEVC_POC: i32 = HwcodecErrno::HWCODEC_ERR_HEVC_COULD_NOT_FIND_POC as i32;

crate::generate_call_macro!(call_yuv, false);

#[cfg(not(target_os = "android"))]
lazy_static::lazy_static! {
    static ref CONFIG: std::sync::Arc<std::sync::Mutex<Option<HwCodecConfig>>> = Default::default();
    static ref CONFIG_SET_BY_IPC: std::sync::Arc<std::sync::Mutex<bool>> = Default::default();
}

#[derive(Debug, Clone)]
pub struct HwRamEncoderConfig {
    pub name: String,
    pub mc_name: Option<String>,
    pub width: usize,
    pub height: usize,
    pub quality: f32,
    pub keyframe_interval: Option<usize>,
}

#[cfg(target_os = "macos")]
struct NativeVtHevc {
    handle: *mut hbb_common::libc::c_void,
    width: usize,
    height: usize,
    y_stride: usize,
    uv_stride: usize,
    uv_offset: usize,
    first_frame: bool,
    low_latency_rc: bool,
}

#[cfg(target_os = "macos")]
unsafe impl Send for NativeVtHevc {}

#[cfg(target_os = "macos")]
unsafe extern "C" fn vt_hevc_output_trampoline(
    user_data: *mut hbb_common::libc::c_void,
    data: *const u8,
    len: usize,
    pts_ms: i64,
    is_key: i32,
) {
    if user_data.is_null() || data.is_null() || len == 0 {
        return;
    }
    let frames = &mut *(user_data as *mut Vec<EncodeFrame>);
    let slice = std::slice::from_raw_parts(data, len);
    frames.push(EncodeFrame {
        data: slice.to_vec(),
        pts: pts_ms,
        key: is_key,
    });
}

#[cfg(target_os = "macos")]
impl NativeVtHevc {
    fn new(width: usize, height: usize, kbs: u32, fps: u32, gop: i32) -> Option<Self> {
        let mut llrc = false;
        let handle = unsafe {
            crate::quartz::ffi::rustdesk_vt_hevc_create(
                width as i32,
                height as i32,
                kbs as i32,
                fps as i32,
                gop,
                &mut llrc as *mut bool,
            )
        };
        if handle.is_null() {
            return None;
        }
        let y_stride = (width + HW_STRIDE_ALIGN - 1) & !(HW_STRIDE_ALIGN - 1);
        let uv_stride = y_stride;
        let uv_offset = y_stride * height;
        Some(Self {
            handle,
            width,
            height,
            y_stride,
            uv_stride,
            uv_offset,
            first_frame: true,
            low_latency_rc: llrc,
        })
    }

    fn update(&mut self, kbs: u32, fps: u32) {
        unsafe {
            crate::quartz::ffi::rustdesk_vt_hevc_update(self.handle, kbs as i32, fps as i32);
        }
    }

    fn encode(&mut self, yuv: &[u8], ms: i64) -> ResultType<Vec<EncodeFrame>> {
        let mut frames = Vec::<EncodeFrame>::with_capacity(1);
        let force_key = self.first_frame;
        let ret = unsafe {
            crate::quartz::ffi::rustdesk_vt_hevc_encode(
                self.handle,
                yuv.as_ptr(),
                yuv.len(),
                self.y_stride,
                self.uv_offset,
                self.uv_stride,
                ms,
                force_key,
                vt_hevc_output_trampoline,
                &mut frames as *mut Vec<EncodeFrame> as *mut hbb_common::libc::c_void,
            )
        };
        if ret == 0 && !frames.is_empty() {
            self.first_frame = false;
            Ok(frames)
        } else {
            Err(anyhow!("Native VT HEVC encode failed: {}", ret))
        }
    }

    fn yuvfmt(&self) -> crate::EncodeYuvFormat {
        crate::EncodeYuvFormat {
            pixfmt: Pixfmt::NV12,
            w: self.width,
            h: self.height,
            stride: vec![self.y_stride, self.uv_stride],
            u: self.uv_offset,
            v: 0,
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for NativeVtHevc {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                crate::quartz::ffi::rustdesk_vt_hevc_destroy(self.handle);
            }
            self.handle = std::ptr::null_mut();
        }
    }
}

enum HwEncoderBackend {
    Ffmpeg(Encoder),
    #[cfg(target_os = "macos")]
    NativeVt(NativeVtHevc),
}

pub struct HwRamEncoder {
    encoder: HwEncoderBackend,
    pub format: DataFormat,
    pub pixfmt: AVPixelFormat,
    bitrate: u32, //kbs
    fps: u32,
    applied_fps: u32,
    last_reconfig: Option<std::time::Instant>,
    config: HwRamEncoderConfig,
}

impl EncoderApi for HwRamEncoder {
    fn new(cfg: EncoderCfg, _i444: bool) -> ResultType<Self>
    where
        Self: Sized,
    {
        match cfg {
            EncoderCfg::HWRAM(config) => {
                let is_vt = cfg!(target_os = "macos") && config.name.contains("videotoolbox");
                let fps = if is_vt {
                    target_fps()
                } else {
                    DEFAULT_FPS as u32
                };
                let bitrate = Self::calc_bitrate_for_config(&config, config.quality, fps);
                let gop = config.keyframe_interval.unwrap_or(DEFAULT_GOP as _) as i32;
                let format = match Encoder::format_from_name(config.name.clone()) {
                    Ok(format) => format,
                    Err(_) => {
                        return Err(anyhow!(format!(
                            "failed to get format from name:{}",
                            config.name
                        )))
                    }
                };
                #[cfg(target_os = "macos")]
                if config.name == "hevc_videotoolbox" {
                    if let Some(vt) = NativeVtHevc::new(config.width, config.height, bitrate, fps, gop) {
                        log::info!(
                            "macOS native VTCompressionSession HEVC encoder started: {}x{}, fps={}, bitrate={} kbps (quality={:.2}, low_latency_rc={}, latency_free=true)",
                            config.width,
                            config.height,
                            fps,
                            bitrate,
                            config.quality,
                            vt.low_latency_rc
                        );
                        return Ok(HwRamEncoder {
                            encoder: HwEncoderBackend::NativeVt(vt),
                            format,
                            pixfmt: DEFAULT_PIXFMT,
                            bitrate,
                            fps,
                            applied_fps: fps,
                            last_reconfig: None,
                            config,
                        });
                    }
                    log::warn!("macOS native VTCompressionSession HEVC unavailable, falling back to FFmpeg hwcodec");
                }
                let ctx = Self::build_encode_context(&config, bitrate, fps);
                match Encoder::new(ctx.clone()) {
                    Ok(encoder) => {
                        if is_vt {
                            log::info!(
                                "macOS FFmpeg videotoolbox encoder started: {} {}x{}, fps={}, bitrate={} kbps (quality={:.2}, rc=VBR)",
                                config.name,
                                config.width,
                                config.height,
                                fps,
                                bitrate,
                                config.quality
                            );
                        }
                        Ok(HwRamEncoder {
                            encoder: HwEncoderBackend::Ffmpeg(encoder),
                            format,
                            pixfmt: ctx.pixfmt,
                            bitrate,
                            fps,
                            applied_fps: fps,
                            last_reconfig: None,
                            config,
                        })
                    }
                    Err(_) => Err(anyhow!(format!("Failed to create encoder"))),
                }
            }
            _ => Err(anyhow!("encoder type mismatch")),
        }
    }

    fn encode_to_message(&mut self, input: EncodeInput, ms: i64) -> ResultType<VideoFrame> {
        let mut vf = VideoFrame::new();
        let mut frames = Vec::new();
        for frame in self
            .encode(input.yuv()?, ms)
            .with_context(|| "Failed to encode")?
        {
            frames.push(EncodedVideoFrame {
                data: Bytes::from(frame.data),
                pts: frame.pts,
                key: frame.key == 1,
                ..Default::default()
            });
        }
        if frames.len() > 0 {
            let frames = EncodedVideoFrames {
                frames: frames.into(),
                ..Default::default()
            };
            match self.format {
                DataFormat::H264 => vf.set_h264s(frames),
                DataFormat::H265 => vf.set_h265s(frames),
                _ => bail!("unsupported format: {:?}", self.format),
            }
            Ok(vf)
        } else {
            Err(anyhow!("no valid frame"))
        }
    }

    fn yuvfmt(&self) -> crate::EncodeYuvFormat {
        match &self.encoder {
            #[cfg(target_os = "macos")]
            HwEncoderBackend::NativeVt(vt) => vt.yuvfmt(),
            HwEncoderBackend::Ffmpeg(enc) => {
                let pixfmt = if self.pixfmt == AVPixelFormat::AV_PIX_FMT_NV12 {
                    Pixfmt::NV12
                } else {
                    Pixfmt::I420
                };
                let stride = enc
                    .linesize
                    .clone()
                    .drain(..)
                    .map(|i| i as usize)
                    .collect();
                crate::EncodeYuvFormat {
                    pixfmt,
                    w: enc.ctx.width as _,
                    h: enc.ctx.height as _,
                    stride,
                    u: enc.offset[0] as _,
                    v: if pixfmt == Pixfmt::NV12 {
                        0
                    } else {
                        enc.offset[1] as _
                    },
                }
            }
        }
    }

    #[cfg(feature = "vram")]
    fn input_texture(&self) -> bool {
        false
    }

    fn set_quality(&mut self, ratio: f32) -> ResultType<()> {
        self.config.quality = ratio;
        if cfg!(target_os = "macos") && self.config.name.contains("videotoolbox") {
            self.maybe_reconfigure_videotoolbox("quality");
            return Ok(());
        }
        let mut bitrate = Self::bitrate(
            &self.config.name,
            self.config.width,
            self.config.height,
            ratio,
        );
        if bitrate > 0 {
            bitrate = Self::check_bitrate_range(&self.config, bitrate);
            if let HwEncoderBackend::Ffmpeg(enc) = &mut self.encoder {
                enc.set_bitrate(bitrate as _).ok();
            }
            self.bitrate = bitrate;
        }
        Ok(())
    }

    fn set_fps(&mut self, fps: u32) -> ResultType<()> {
        let clamped = fps.clamp(10, 120);
        set_target_fps(clamped);
        if cfg!(target_os = "macos") && self.config.name.contains("videotoolbox") {
            self.fps = clamped;
            self.maybe_reconfigure_videotoolbox("fps");
        }
        Ok(())
    }

    fn set_max_bitrate_kbs(&mut self, kbs: u32) -> ResultType<()> {
        set_macos_h265_max_kbs(kbs);
        if cfg!(target_os = "macos") && self.config.name.contains("videotoolbox") {
            self.maybe_reconfigure_videotoolbox("max_bitrate");
        }
        Ok(())
    }

    fn bitrate(&self) -> u32 {
        self.bitrate
    }

    fn support_changing_quality(&self) -> bool {
        ["vaapi"].iter().all(|&x| !self.config.name.contains(x))
    }

    fn latency_free(&self) -> bool {
        #[cfg(target_os = "macos")]
        if matches!(self.encoder, HwEncoderBackend::NativeVt(_)) {
            return true;
        }
        ["mediacodec", "videotoolbox"]
            .iter()
            .all(|&x| !self.config.name.contains(x))
    }

    fn is_hardware(&self) -> bool {
        true
    }

    fn disable(&self) {
        HwCodecConfig::clear(false, true);
    }
}

impl HwRamEncoder {
    pub fn try_get(format: CodecFormat) -> Option<CodecInfo> {
        let mut info = None;
        let best = CodecInfo::prioritized(HwCodecConfig::get().ram_encode);
        match format {
            CodecFormat::H264 => {
                if let Some(v) = best.h264 {
                    info = Some(v);
                }
            }
            CodecFormat::H265 => {
                if let Some(v) = best.h265 {
                    info = Some(v);
                }
            }
            _ => {}
        }
        info
    }

    pub fn encode(&mut self, yuv: &[u8], ms: i64) -> ResultType<Vec<EncodeFrame>> {
        match &mut self.encoder {
            #[cfg(target_os = "macos")]
            HwEncoderBackend::NativeVt(vt) => vt.encode(yuv, ms),
            HwEncoderBackend::Ffmpeg(enc) => match enc.encode(yuv, ms) {
                Ok(v) => {
                    let mut data = Vec::<EncodeFrame>::new();
                    data.append(v);
                    Ok(data)
                }
                Err(code) => Err(anyhow!("RAM encode failed: {}", code)),
            },
        }
    }

    fn build_encode_context(config: &HwRamEncoderConfig, bitrate: u32, fps: u32) -> EncodeContext {
        let rc = Self::rate_control(config);
        let gop = config.keyframe_interval.unwrap_or(DEFAULT_GOP as _) as i32;
        EncodeContext {
            name: config.name.clone(),
            mc_name: config.mc_name.clone(),
            width: config.width as _,
            height: config.height as _,
            pixfmt: DEFAULT_PIXFMT,
            align: HW_STRIDE_ALIGN as _,
            kbs: bitrate as i32,
            fps: fps.clamp(10, 120) as i32,
            gop,
            quality: DEFAULT_HW_QUALITY,
            rc,
            q: -1,
            thread_count: codec_thread_num(16) as _, // ffmpeg's thread_count is used for cpu
        }
    }

    fn calc_bitrate_for_config(config: &HwRamEncoderConfig, ratio: f32, fps: u32) -> u32 {
        let mut bitrate = Self::bitrate(&config.name, config.width, config.height, ratio);
        #[cfg(target_os = "macos")]
        if config.name.contains("videotoolbox") && bitrate > 0 {
            // Smooth FPS bitrate compensation (baseline 30fps, gentle 0.45 exponent) so
            // 60+ fps stays crisp without causing scene-change bufferbloat spikes.
            let fps_scale = (fps.clamp(15, 120) as f32 / 30.0).powf(0.45);
            bitrate = ((bitrate as f32) * fps_scale).round() as u32;
        }
        Self::check_bitrate_range(config, bitrate)
    }

    fn maybe_reconfigure_videotoolbox(&mut self, reason: &str) {
        let new_bitrate = Self::calc_bitrate_for_config(&self.config, self.config.quality, self.fps);
        if new_bitrate == 0 {
            return;
        }
        let old_bitrate = self.bitrate.max(1);
        let old_fps = self.applied_fps.max(1);

        // Native Apple VTCompressionSession updates AverageBitRate, DataRateLimits, and
        // ExpectedFrameRate live via VTSessionSetProperty with zero session recreation,
        // zero IDR spike, and zero encode_fail!
        #[cfg(target_os = "macos")]
        if let HwEncoderBackend::NativeVt(vt) = &mut self.encoder {
            if new_bitrate == old_bitrate && self.fps == old_fps {
                return;
            }
            vt.update(new_bitrate, self.fps);
            if reason == "max_bitrate"
                || new_bitrate.abs_diff(old_bitrate) * 20 >= old_bitrate
                || self.fps.abs_diff(old_fps) >= 5
            {
                log::info!(
                    "macOS native VT HEVC live-updated ({reason}): {}x{}, fps={}->{}, bitrate={}->{} kbps (quality={:.2})",
                    self.config.width,
                    self.config.height,
                    old_fps,
                    self.fps,
                    old_bitrate,
                    new_bitrate,
                    self.config.quality
                );
            }
            self.bitrate = new_bitrate;
            self.applied_fps = self.fps;
            return;
        }

        // Fallback FFmpeg hwcodec path: only rebuild on large changes with cooldown.
        let bitrate_rel = new_bitrate.abs_diff(old_bitrate) as f32 / old_bitrate as f32;
        let fps_diff = self.fps.abs_diff(old_fps);
        let fps_rel = fps_diff as f32 / old_fps as f32;

        let bitrate_changed = bitrate_rel >= 0.18;
        let fps_changed = fps_diff >= 8 && fps_rel >= 0.18;
        if !bitrate_changed && !fps_changed {
            return;
        }

        let major_jump = new_bitrate >= old_bitrate.saturating_mul(2)
            || old_bitrate >= new_bitrate.saturating_mul(2)
            || self.fps >= old_fps.saturating_mul(2)
            || old_fps >= self.fps.saturating_mul(2);
        if !major_jump {
            if let Some(last) = self.last_reconfig {
                if last.elapsed() < std::time::Duration::from_millis(2500) {
                    return;
                }
            }
        }

        let ctx = Self::build_encode_context(&self.config, new_bitrate, self.fps);
        match Encoder::new(ctx) {
            Ok(new_encoder) => {
                self.encoder = HwEncoderBackend::Ffmpeg(new_encoder);
                log::info!(
                    "macOS FFmpeg videotoolbox reconfigured ({reason}): {}x{}, fps={}->{}, bitrate={}->{} kbps (quality={:.2})",
                    self.config.width,
                    self.config.height,
                    old_fps,
                    self.fps,
                    old_bitrate,
                    new_bitrate,
                    self.config.quality
                );
                self.bitrate = new_bitrate;
                self.applied_fps = self.fps;
                self.last_reconfig = Some(std::time::Instant::now());
            }
            Err(_) => {
                log::warn!(
                    "macOS FFmpeg videotoolbox reconfigure ({reason}) failed for fps={}, bitrate={} kbps; keeping active encoder",
                    self.fps,
                    new_bitrate
                );
            }
        }
    }

    fn rate_control(_config: &HwRamEncoderConfig) -> RateControl {
        #[cfg(target_os = "android")]
        if _config.name.contains("mediacodec") {
            return RC_VBR;
        }
        #[cfg(target_os = "macos")]
        if _config.name.contains("videotoolbox") {
            return RC_VBR;
        }
        RC_CBR
    }

    pub fn bitrate(name: &str, width: usize, height: usize, ratio: f32) -> u32 {
        Self::calc_bitrate(width, height, ratio, name.contains("h264"))
    }

    pub fn calc_bitrate(width: usize, height: usize, ratio: f32, h264: bool) -> u32 {
        let base = base_bitrate(width as _, height as _) as f32 * ratio;
        let threshold = 2000.0;
        let decay_rate = 0.001; // 1000 * 0.001 = 1
        let factor: f32 = if cfg!(target_os = "android") {
            // https://stackoverflow.com/questions/26110337/what-are-valid-bit-rates-to-set-for-mediacodec?rq=3
            if base > threshold {
                1.0 + 4.0 / (1.0 + (base - threshold) * decay_rate)
            } else {
                5.0
            }
        } else if h264 {
            if base > threshold {
                1.0 + 1.0 / (1.0 + (base - threshold) * decay_rate)
            } else {
                2.0
            }
        } else {
            if base > threshold {
                1.0 + 0.5 / (1.0 + (base - threshold) * decay_rate)
            } else {
                1.5
            }
        };
        (base * factor) as u32
    }

    pub fn check_bitrate_range(_config: &HwRamEncoderConfig, bitrate: u32) -> u32 {
        #[cfg(target_os = "android")]
        if _config.name.contains("mediacodec") {
            let info = crate::android::ffi::get_codec_info();
            if let Some(info) = info {
                if let Some(codec) = info
                    .codecs
                    .iter()
                    .find(|c| Some(c.name.clone()) == _config.mc_name && c.is_encoder)
                {
                    if codec.max_bitrate > codec.min_bitrate {
                        if bitrate > codec.max_bitrate {
                            return codec.max_bitrate;
                        }
                        if bitrate < codec.min_bitrate {
                            return codec.min_bitrate;
                        }
                    }
                }
            }
        }
        #[cfg(target_os = "macos")]
        if _config.name.contains("videotoolbox") {
            let user_cap = macos_h265_max_kbs();
            let max_kbs = if _config.width * _config.height <= 1920 * 1080 {
                user_cap
            } else {
                user_cap.max(24_000).min(42_000)
            };
            if bitrate > max_kbs {
                return max_kbs;
            }
        }
        bitrate
    }
}

pub struct HwRamDecoder {
    decoder: Decoder,
    pub info: CodecInfo,
}

impl HwRamDecoder {
    pub fn try_get(format: CodecFormat) -> Option<CodecInfo> {
        let mut info = None;
        let soft = CodecInfo::soft();
        match format {
            CodecFormat::H264 => {
                if let Some(v) = soft.h264 {
                    info = Some(v);
                }
            }
            CodecFormat::H265 => {
                if let Some(v) = soft.h265 {
                    info = Some(v);
                }
            }
            _ => {}
        }
        if enable_hwcodec_option() {
            let best = CodecInfo::prioritized(HwCodecConfig::get().ram_decode);
            match format {
                CodecFormat::H264 => {
                    if let Some(v) = best.h264 {
                        info = Some(v);
                    }
                }
                CodecFormat::H265 => {
                    if let Some(v) = best.h265 {
                        info = Some(v);
                    }
                }
                _ => {}
            }
        }
        info
    }

    pub fn new(format: CodecFormat) -> ResultType<Self> {
        let info = HwRamDecoder::try_get(format);
        log::info!("try create {info:?} ram decoder");
        let Some(info) = info else {
            bail!("unsupported format: {:?}", format);
        };
        let ctx = DecodeContext {
            name: info.name.clone(),
            device_type: info.hwdevice.clone(),
            thread_count: codec_thread_num(16) as _,
        };
        match Decoder::new(ctx) {
            Ok(decoder) => Ok(HwRamDecoder { decoder, info }),
            Err(_) => {
                HwCodecConfig::clear(false, false);
                Err(anyhow!(format!("Failed to create decoder")))
            }
        }
    }
    pub fn decode<'a>(&'a mut self, data: &[u8]) -> ResultType<Vec<HwRamDecoderImage<'a>>> {
        match self.decoder.decode(data) {
            Ok(v) => Ok(v.iter().map(|f| HwRamDecoderImage { frame: f }).collect()),
            Err(e) => Err(anyhow!(e)),
        }
    }
}

pub struct HwRamDecoderImage<'a> {
    frame: &'a DecodeFrame,
}

impl HwRamDecoderImage<'_> {
    // rgb [in/out] fmt and stride must be set in ImageRgb
    pub fn to_fmt(&self, rgb: &mut ImageRgb, i420: &mut Vec<u8>) -> ResultType<()> {
        let frame = self.frame;
        let width = frame.width;
        let height = frame.height;
        rgb.w = width as _;
        rgb.h = height as _;
        let dst_align = rgb.align();
        let bytes_per_row = (rgb.w * 4 + dst_align - 1) & !(dst_align - 1);
        rgb.raw.resize(rgb.h * bytes_per_row, 0);
        match frame.pixfmt {
            AVPixelFormat::AV_PIX_FMT_NV12 => {
                // I420ToARGB is much faster than NV12ToARGB in tests on Windows
                if cfg!(windows) {
                    let Ok((linesize_i420, offset_i420, len_i420)) = ffmpeg_linesize_offset_length(
                        AVPixelFormat::AV_PIX_FMT_YUV420P,
                        width as _,
                        height as _,
                        HW_STRIDE_ALIGN,
                    ) else {
                        bail!("failed to get i420 linesize, offset, length");
                    };
                    i420.resize(len_i420 as _, 0);
                    let i420_offset_y = unsafe { i420.as_ptr().add(0) as _ };
                    let i420_offset_u = unsafe { i420.as_ptr().add(offset_i420[0] as _) as _ };
                    let i420_offset_v = unsafe { i420.as_ptr().add(offset_i420[1] as _) as _ };
                    call_yuv!(NV12ToI420(
                        frame.data[0].as_ptr(),
                        frame.linesize[0],
                        frame.data[1].as_ptr(),
                        frame.linesize[1],
                        i420_offset_y,
                        linesize_i420[0],
                        i420_offset_u,
                        linesize_i420[1],
                        i420_offset_v,
                        linesize_i420[2],
                        width,
                        height,
                    ));
                    let f = match rgb.fmt() {
                        ImageFormat::ARGB => I420ToARGB,
                        ImageFormat::ABGR => I420ToABGR,
                        _ => bail!("unsupported format: {:?} -> {:?}", frame.pixfmt, rgb.fmt()),
                    };
                    call_yuv!(f(
                        i420_offset_y,
                        linesize_i420[0],
                        i420_offset_u,
                        linesize_i420[1],
                        i420_offset_v,
                        linesize_i420[2],
                        rgb.raw.as_mut_ptr(),
                        bytes_per_row as _,
                        width,
                        height,
                    ));
                } else {
                    let f = match rgb.fmt() {
                        ImageFormat::ARGB => NV12ToARGB,
                        ImageFormat::ABGR => NV12ToABGR,
                        _ => bail!("unsupported format: {:?} -> {:?}", frame.pixfmt, rgb.fmt()),
                    };
                    call_yuv!(f(
                        frame.data[0].as_ptr(),
                        frame.linesize[0],
                        frame.data[1].as_ptr(),
                        frame.linesize[1],
                        rgb.raw.as_mut_ptr(),
                        bytes_per_row as _,
                        width,
                        height,
                    ));
                }
            }
            AVPixelFormat::AV_PIX_FMT_YUV420P => {
                let f = match rgb.fmt() {
                    ImageFormat::ARGB => I420ToARGB,
                    ImageFormat::ABGR => I420ToABGR,
                    _ => bail!("unsupported format: {:?} -> {:?}", frame.pixfmt, rgb.fmt()),
                };
                call_yuv!(f(
                    frame.data[0].as_ptr(),
                    frame.linesize[0],
                    frame.data[1].as_ptr(),
                    frame.linesize[1],
                    frame.data[2].as_ptr(),
                    frame.linesize[2],
                    rgb.raw.as_mut_ptr(),
                    bytes_per_row as _,
                    width,
                    height,
                ));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "android")]
fn get_mime_type(codec: DataFormat) -> &'static str {
    match codec {
        DataFormat::VP8 => "video/x-vnd.on2.vp8",
        DataFormat::VP9 => "video/x-vnd.on2.vp9",
        DataFormat::AV1 => "video/av01",
        DataFormat::H264 => "video/avc",
        DataFormat::H265 => "video/hevc",
    }
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct HwCodecConfig {
    #[serde(default)]
    pub signature: u64,
    #[serde(default)]
    pub ram_encode: Vec<CodecInfo>,
    #[serde(default)]
    pub ram_decode: Vec<CodecInfo>,
    #[cfg(feature = "vram")]
    #[serde(default)]
    pub vram_encode: Vec<hwcodec::vram::FeatureContext>,
    #[cfg(feature = "vram")]
    #[serde(default)]
    pub vram_decode: Vec<hwcodec::vram::DecodeContext>,
}

// HwCodecConfig2 is used to store the config in json format,
// confy can't serde HwCodecConfig successfully if the non-first struct Vec is empty due to old toml version.
// struct T { a: Vec<A>, b: Vec<String>} will fail if b is empty, but struct T { a: Vec<String>, b: Vec<String>} is ok.
#[derive(Debug, Default, Serialize, Deserialize, Clone)]
struct HwCodecConfig2 {
    #[serde(default)]
    pub config: String,
}

// ipc server process start check process once, other process get from ipc server once
// install: --server start check process, check process send to --server,  ui get from --server
// portable: ui start check process, check process send to ui
// sciter and unilink: get from ipc server
impl HwCodecConfig {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn set(config: String) {
        let config = serde_json::from_str(&config).unwrap_or_default();
        log::info!("set hwcodec config");
        log::debug!("{config:?}");
        #[cfg(any(windows, target_os = "macos"))]
        hbb_common::config::common_store(
            &HwCodecConfig2 {
                config: serde_json::to_string_pretty(&config).unwrap_or_default(),
            },
            "_hwcodec",
        );
        *CONFIG.lock().unwrap() = Some(config);
        *CONFIG_SET_BY_IPC.lock().unwrap() = true;
    }

    pub fn get() -> HwCodecConfig {
        #[cfg(target_os = "android")]
        {
            let info = crate::android::ffi::get_codec_info();
            log::info!("all codec info: {info:?}");
            struct T {
                name_prefix: &'static str,
                data_format: DataFormat,
            }
            let ts = vec![
                T {
                    name_prefix: "h264",
                    data_format: DataFormat::H264,
                },
                T {
                    name_prefix: "hevc",
                    data_format: DataFormat::H265,
                },
            ];
            let mut e = vec![];
            if let Some(info) = info {
                ts.iter().for_each(|t| {
                    let codecs: Vec<_> = info
                        .codecs
                        .iter()
                        .filter(|c| {
                            c.is_encoder
                                && c.mime_type.as_str() == get_mime_type(t.data_format)
                                && c.nv12
                                && c.hw == Some(true) //only use hardware codec
                        })
                        .collect();
                    let screen_wh = std::cmp::max(info.w, info.h);
                    let mut best = None;
                    if let Some(codec) = codecs
                        .iter()
                        .find(|c| c.max_width >= screen_wh && c.max_height >= screen_wh)
                    {
                        best = Some(codec.name.clone());
                    } else {
                        // find the max resolution
                        let mut max_area = 0;
                        for codec in codecs.iter() {
                            if codec.max_width * codec.max_height > max_area {
                                best = Some(codec.name.clone());
                                max_area = codec.max_width * codec.max_height;
                            }
                        }
                    }
                    if let Some(best) = best {
                        e.push(CodecInfo {
                            name: format!("{}_mediacodec", t.name_prefix),
                            mc_name: Some(best),
                            format: t.data_format,
                            hwdevice: hwcodec::ffmpeg::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE,
                            priority: 0,
                        });
                    }
                });
            }
            log::debug!("e: {e:?}");
            HwCodecConfig {
                ram_encode: e,
                ..Default::default()
            }
        }
        #[cfg(any(windows, target_os = "macos"))]
        {
            let config = CONFIG.lock().unwrap().clone();
            match config {
                Some(c) => c,
                None => {
                    log::info!("try load cached hwcodec config");
                    let c = hbb_common::config::common_load::<HwCodecConfig2>("_hwcodec");
                    let c: HwCodecConfig = serde_json::from_str(&c.config).unwrap_or_default();
                    let new_signature = hwcodec::common::get_gpu_signature();
                    if c.signature == new_signature {
                        log::debug!("load cached hwcodec config: {c:?}");
                        *CONFIG.lock().unwrap() = Some(c.clone());
                        c
                    } else {
                        log::info!(
                            "gpu signature changed, {} -> {}",
                            c.signature,
                            new_signature
                        );
                        HwCodecConfig::default()
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            CONFIG.lock().unwrap().clone().unwrap_or_default()
        }
        #[cfg(target_os = "ios")]
        {
            HwCodecConfig::default()
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn get_set_value() -> Option<HwCodecConfig> {
        let set = CONFIG_SET_BY_IPC.lock().unwrap().clone();
        if set {
            CONFIG.lock().unwrap().clone()
        } else {
            None
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn already_set() -> bool {
        CONFIG_SET_BY_IPC.lock().unwrap().clone()
    }

    pub fn clear(vram: bool, encode: bool) {
        log::info!("clear hwcodec config, vram: {vram}, encode: {encode}");
        #[cfg(target_os = "android")]
        crate::android::ffi::clear_codec_info();
        #[cfg(not(target_os = "android"))]
        {
            let mut c = CONFIG.lock().unwrap();
            if let Some(c) = c.as_mut() {
                if vram {
                    #[cfg(feature = "vram")]
                    if encode {
                        c.vram_encode = vec![];
                    } else {
                        c.vram_decode = vec![];
                    }
                } else {
                    if encode {
                        c.ram_encode = vec![];
                    } else {
                        c.ram_decode = vec![];
                    }
                }
            }
        }
        crate::codec::Encoder::update(crate::codec::EncodingUpdate::Check);
    }
}

pub fn check_available_hwcodec() -> String {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    hwcodec::common::setup_parent_death_signal();
    let ctx = EncodeContext {
        name: String::from(""),
        mc_name: None,
        width: 1280,
        height: 720,
        pixfmt: DEFAULT_PIXFMT,
        align: HW_STRIDE_ALIGN as _,
        kbs: 1000,
        fps: DEFAULT_FPS,
        gop: DEFAULT_GOP,
        quality: DEFAULT_HW_QUALITY,
        rc: RC_CBR,
        q: -1,
        thread_count: 4,
    };
    #[cfg(feature = "vram")]
    let vram = crate::vram::check_available_vram();
    #[cfg(feature = "vram")]
    let vram_string = vram.2;
    #[cfg(not(feature = "vram"))]
    let vram_string = "".to_owned();
    let c = HwCodecConfig {
        ram_encode: Encoder::available_encoders(ctx, Some(vram_string)),
        ram_decode: Decoder::available_decoders(),
        #[cfg(feature = "vram")]
        vram_encode: vram.0,
        #[cfg(feature = "vram")]
        vram_decode: vram.1,
        signature: hwcodec::common::get_gpu_signature(),
    };
    log::debug!("{c:?}");
    serde_json::to_string(&c).unwrap_or_default()
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn start_check_process() {
    if !enable_hwcodec_option() || HwCodecConfig::already_set() {
        return;
    }
    use hbb_common::allow_err;
    use std::sync::Once;
    let f = || {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(_) = exe.file_name().to_owned() {
                let arg = "--check-hwcodec-config";
                if let Ok(mut child) = std::process::Command::new(exe).arg(arg).spawn() {
                    #[cfg(windows)]
                    hwcodec::common::child_exit_when_parent_exit(child.id());
                    // wait up to 30 seconds, it maybe slow on windows startup for poorly performing machines
                    for _ in 0..30 {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        if let Ok(Some(_)) = child.try_wait() {
                            break;
                        }
                    }
                    allow_err!(child.kill());
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            log::info!("Check hwcodec config, exit with: {status}")
                        }
                        Ok(None) => {
                            log::info!(
                                "Check hwcodec config, status not ready yet, let's really wait"
                            );
                            let res = child.wait();
                            log::info!("Check hwcodec config, wait result: {res:?}");
                        }
                        Err(e) => {
                            log::error!("Check hwcodec config, error attempting to wait: {e}")
                        }
                    }
                }
            }
        };
    };
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(f);
    });
}
