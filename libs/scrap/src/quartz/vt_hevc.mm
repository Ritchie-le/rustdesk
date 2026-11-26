#import <Foundation/Foundation.h>
#import <CoreFoundation/CoreFoundation.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <VideoToolbox/VideoToolbox.h>

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <vector>

typedef void (*VtHevcOutputCallback)(
    void *user_data,
    const uint8_t *data,
    size_t len,
    int64_t pts_ms,
    int32_t is_key
);

namespace {

constexpr uint8_t kAnnexBStartCode[4] = {0x00, 0x00, 0x00, 0x01};

struct VtHevcEncodeInvocation {
    VtHevcOutputCallback callback = nullptr;
    void *user_data = nullptr;
    int64_t pts_ms = 0;
    int32_t emitted = 0;
    OSStatus status = noErr;
};

struct VtHevcEncoderBox {
    VTCompressionSessionRef session = nullptr;
    CVPixelBufferPoolRef pixel_pool = nullptr;
    int32_t width = 0;
    int32_t height = 0;
    int32_t kbs = 0;
    int32_t fps = 30;
    bool low_latency_rc = false;
    std::vector<uint8_t> annexb_buf;
    VtHevcEncodeInvocation current;
};

static bool set_session_bool(VTCompressionSessionRef session, CFStringRef key, bool value) {
    return VTSessionSetProperty(session, key, value ? kCFBooleanTrue : kCFBooleanFalse) == noErr;
}

static bool set_session_i32(VTCompressionSessionRef session, CFStringRef key, int32_t value) {
    CFNumberRef num = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &value);
    if (!num) {
        return false;
    }
    OSStatus st = VTSessionSetProperty(session, key, num);
    CFRelease(num);
    return st == noErr;
}

static bool set_session_f64(VTCompressionSessionRef session, CFStringRef key, double value) {
    CFNumberRef num = CFNumberCreate(kCFAllocatorDefault, kCFNumberFloat64Type, &value);
    if (!num) {
        return false;
    }
    OSStatus st = VTSessionSetProperty(session, key, num);
    CFRelease(num);
    return st == noErr;
}

static void apply_rate_control_properties(VTCompressionSessionRef session, int32_t kbs, int32_t fps) {
    int32_t clamped_fps = std::clamp(fps, 10, 120);
    int64_t avg_bps = static_cast<int64_t>(std::max(kbs, 500)) * 1000LL;
    // Cap 1-second burst rate tightly at 1.25x average bitrate (UU Remote-style peak cap)
    // so sudden full-screen window switches never dump an unconstrained reservoir burst
    // into WebRTC SCTP and trigger 1000ms uplink bufferbloat.
    double max_bytes_per_sec = static_cast<double>(avg_bps) * 1.25 / 8.0;
    double window_sec = 1.0;

    set_session_f64(session, kVTCompressionPropertyKey_ExpectedFrameRate, static_cast<double>(clamped_fps));

    CFNumberRef avg_num = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt64Type, &avg_bps);
    if (avg_num) {
        VTSessionSetProperty(session, kVTCompressionPropertyKey_AverageBitRate, avg_num);
        CFRelease(avg_num);
    }

    CFNumberRef limit_bytes = CFNumberCreate(kCFAllocatorDefault, kCFNumberFloat64Type, &max_bytes_per_sec);
    CFNumberRef limit_secs = CFNumberCreate(kCFAllocatorDefault, kCFNumberFloat64Type, &window_sec);
    if (limit_bytes && limit_secs) {
        const void *vals[2] = {limit_bytes, limit_secs};
        CFArrayRef limits = CFArrayCreate(
            kCFAllocatorDefault,
            vals,
            2,
            &kCFTypeArrayCallBacks
        );
        if (limits) {
            VTSessionSetProperty(session, kVTCompressionPropertyKey_DataRateLimits, limits);
            CFRelease(limits);
        }
    }
    if (limit_bytes) CFRelease(limit_bytes);
    if (limit_secs) CFRelease(limit_secs);
}

static void vt_hevc_output_callback(
    void *outputCallbackRefCon,
    void *sourceFrameRefCon,
    OSStatus status,
    VTEncodeInfoFlags infoFlags,
    CMSampleBufferRef sampleBuffer
) {
    (void)sourceFrameRefCon;
    auto *box = static_cast<VtHevcEncoderBox *>(outputCallbackRefCon);
    if (!box) {
        return;
    }
    if (status != noErr) {
        box->current.status = status;
        return;
    }
    if ((infoFlags & kVTEncodeInfo_FrameDropped) != 0 || !sampleBuffer || !CMSampleBufferDataIsReady(sampleBuffer)) {
        return;
    }

    bool is_key = true;
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, false);
    if (attachments && CFArrayGetCount(attachments) > 0) {
        CFDictionaryRef dict = static_cast<CFDictionaryRef>(CFArrayGetValueAtIndex(attachments, 0));
        if (dict) {
            CFBooleanRef not_sync = static_cast<CFBooleanRef>(
                CFDictionaryGetValue(dict, kCMSampleAttachmentKey_NotSync)
            );
            if (not_sync && CFBooleanGetValue(not_sync)) {
                is_key = false;
            }
        }
    }

    box->annexb_buf.clear();

    CMVideoFormatDescriptionRef fmt = CMSampleBufferGetFormatDescription(sampleBuffer);
    int nalu_len_size = 4;
    if (fmt) {
        size_t param_count = 0;
        OSStatus p_st = CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
            fmt, 0, nullptr, nullptr, &param_count, &nalu_len_size
        );
        if (p_st != noErr) {
            nalu_len_size = 4;
            param_count = 0;
        }
        if (is_key && param_count > 0) {
            for (size_t i = 0; i < param_count; ++i) {
                const uint8_t *ps_ptr = nullptr;
                size_t ps_size = 0;
                if (CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                        fmt, i, &ps_ptr, &ps_size, nullptr, nullptr) == noErr &&
                    ps_ptr && ps_size > 0) {
                    box->annexb_buf.insert(
                        box->annexb_buf.end(), kAnnexBStartCode, kAnnexBStartCode + 4
                    );
                    box->annexb_buf.insert(box->annexb_buf.end(), ps_ptr, ps_ptr + ps_size);
                }
            }
        }
    }
    if (nalu_len_size != 1 && nalu_len_size != 2 && nalu_len_size != 4) {
        nalu_len_size = 4;
    }

    CMBlockBufferRef block_buf = CMSampleBufferGetDataBuffer(sampleBuffer);
    if (!block_buf) {
        return;
    }
    size_t total_len = CMBlockBufferGetDataLength(block_buf);
    if (total_len == 0) {
        return;
    }

    // Read contiguous pointer if available; otherwise copy into a temporary buffer.
    size_t len_at_offset = 0;
    char *raw_ptr = nullptr;
    std::vector<uint8_t> contig_copy;
    const uint8_t *src = nullptr;

    OSStatus ptr_st = CMBlockBufferGetDataPointer(block_buf, 0, &len_at_offset, &total_len, &raw_ptr);
    if (ptr_st == kCMBlockBufferNoErr && raw_ptr && len_at_offset >= total_len) {
        src = reinterpret_cast<const uint8_t *>(raw_ptr);
    } else {
        contig_copy.resize(total_len);
        if (CMBlockBufferCopyDataBytes(block_buf, 0, total_len, contig_copy.data()) != kCMBlockBufferNoErr) {
            return;
        }
        src = contig_copy.data();
    }

    size_t offset = 0;
    while (offset + static_cast<size_t>(nalu_len_size) <= total_len) {
        uint32_t nalu_len = 0;
        for (int i = 0; i < nalu_len_size; ++i) {
            nalu_len = (nalu_len << 8) | static_cast<uint32_t>(src[offset + i]);
        }
        offset += static_cast<size_t>(nalu_len_size);
        if (nalu_len == 0 || offset + nalu_len > total_len) {
            break;
        }
        box->annexb_buf.insert(box->annexb_buf.end(), kAnnexBStartCode, kAnnexBStartCode + 4);
        box->annexb_buf.insert(box->annexb_buf.end(), src + offset, src + offset + nalu_len);
        offset += nalu_len;
    }

    if (!box->annexb_buf.empty() && box->current.callback) {
        box->current.emitted += 1;
        box->current.callback(
            box->current.user_data,
            box->annexb_buf.data(),
            box->annexb_buf.size(),
            box->current.pts_ms,
            is_key ? 1 : 0
        );
    }
}

static VTCompressionSessionRef create_vt_hevc_session(
    int32_t width,
    int32_t height,
    bool enable_low_latency_rc,
    VtHevcEncoderBox *box
) {
    NSMutableDictionary *encoderSpec = [NSMutableDictionary dictionary];
    encoderSpec[(id)kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder] = @YES;
    encoderSpec[(id)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder] = @YES;
    if (enable_low_latency_rc) {
        if (@available(macOS 11.3, *)) {
            encoderSpec[(id)kVTVideoEncoderSpecification_EnableLowLatencyRateControl] = @YES;
        }
    }

    NSDictionary *srcAttrs = @{
        (id)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
        (id)kCVPixelBufferWidthKey: @(width),
        (id)kCVPixelBufferHeightKey: @(height),
        (id)kCVPixelBufferIOSurfacePropertiesKey: @{},
    };

    VTCompressionSessionRef session = nullptr;
    OSStatus st = VTCompressionSessionCreate(
        kCFAllocatorDefault,
        width,
        height,
        kCMVideoCodecType_HEVC,
        (__bridge CFDictionaryRef)encoderSpec,
        (__bridge CFDictionaryRef)srcAttrs,
        kCFAllocatorDefault,
        vt_hevc_output_callback,
        box,
        &session
    );
    if (st != noErr || !session) {
        return nullptr;
    }
    return session;
}

} // namespace

extern "C" void *rustdesk_vt_hevc_create(
    int32_t width,
    int32_t height,
    int32_t kbs,
    int32_t fps,
    int32_t gop,
    bool *out_low_latency_rc
) {
    if (width < 2 || height < 2 || (width & 1) != 0 || (height & 1) != 0) {
        return nullptr;
    }
    @autoreleasepool {
        auto *box = new VtHevcEncoderBox();
        box->width = width;
        box->height = height;
        box->kbs = std::max(kbs, 500);
        box->fps = std::clamp(fps, 10, 120);
        box->annexb_buf.reserve(256 * 1024);

        // Try Apple Silicon hardware low-latency rate control first (UU Remote approach),
        // falling back cleanly if the current hardware/OS rejects it.
        box->session = create_vt_hevc_session(width, height, true, box);
        if (box->session) {
            box->low_latency_rc = true;
        } else {
            box->session = create_vt_hevc_session(width, height, false, box);
            box->low_latency_rc = false;
        }
        if (!box->session) {
            delete box;
            return nullptr;
        }
        if (out_low_latency_rc) {
            *out_low_latency_rc = box->low_latency_rc;
        }

        // Core zero-latency / desktop-clarity properties matching UU Remote's VideoToolbox pipeline.
        set_session_bool(box->session, kVTCompressionPropertyKey_RealTime, true);
        set_session_bool(box->session, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, true);
        set_session_bool(box->session, kVTCompressionPropertyKey_AllowFrameReordering, false);
        set_session_bool(box->session, kVTCompressionPropertyKey_AllowOpenGOP, false);
        set_session_bool(box->session, kVTCompressionPropertyKey_AllowTemporalCompression, true);
        set_session_i32(box->session, kVTCompressionPropertyKey_MaxFrameDelayCount, 0);
        VTSessionSetProperty(
            box->session,
            kVTCompressionPropertyKey_ProfileLevel,
            kVTProfileLevel_HEVC_Main_AutoLevel
        );
        // Match sRGB/BT.601 NV12 capture matrix from sck.mm & RustDesk client decoder.
        VTSessionSetProperty(
            box->session,
            kVTCompressionPropertyKey_ColorPrimaries,
            kCVImageBufferColorPrimaries_SMPTE_C
        );
        VTSessionSetProperty(
            box->session,
            kVTCompressionPropertyKey_TransferFunction,
            kCVImageBufferTransferFunction_ITU_R_709_2
        );
        VTSessionSetProperty(
            box->session,
            kVTCompressionPropertyKey_YCbCrMatrix,
            kCVImageBufferYCbCrMatrix_ITU_R_601_4
        );

        if (gop > 0 && gop < 60000) {
            set_session_i32(box->session, kVTCompressionPropertyKey_MaxKeyFrameInterval, gop);
        }

        apply_rate_control_properties(box->session, box->kbs, box->fps);

        if (VTCompressionSessionPrepareToEncodeFrames(box->session) != noErr) {
            VTCompressionSessionInvalidate(box->session);
            CFRelease(box->session);
            delete box;
            return nullptr;
        }

        NSDictionary *poolAttrs = @{
            (id)kCVPixelBufferPoolMinimumBufferCountKey: @(3),
        };
        NSDictionary *pbAttrs = @{
            (id)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
            (id)kCVPixelBufferWidthKey: @(width),
            (id)kCVPixelBufferHeightKey: @(height),
            (id)kCVPixelBufferBytesPerRowAlignmentKey: @(64),
            (id)kCVPixelBufferIOSurfacePropertiesKey: @{},
        };
        if (CVPixelBufferPoolCreate(
                kCFAllocatorDefault,
                (__bridge CFDictionaryRef)poolAttrs,
                (__bridge CFDictionaryRef)pbAttrs,
                &box->pixel_pool) != kCVReturnSuccess || !box->pixel_pool) {
            VTCompressionSessionInvalidate(box->session);
            CFRelease(box->session);
            delete box;
            return nullptr;
        }

        return static_cast<void *>(box);
    }
}

extern "C" int32_t rustdesk_vt_hevc_update(void *handle, int32_t kbs, int32_t fps) {
    if (!handle) {
        return -1;
    }
    auto *box = static_cast<VtHevcEncoderBox *>(handle);
    if (!box->session) {
        return -1;
    }
    @autoreleasepool {
        box->kbs = std::max(kbs, 500);
        box->fps = std::clamp(fps, 10, 120);
        apply_rate_control_properties(box->session, box->kbs, box->fps);
    }
    return 0;
}

extern "C" int32_t rustdesk_vt_hevc_encode(
    void *handle,
    const uint8_t *yuv_data,
    size_t yuv_len,
    size_t src_y_stride,
    size_t src_uv_offset,
    size_t src_uv_stride,
    int64_t pts_ms,
    bool force_key,
    VtHevcOutputCallback callback,
    void *user_data
) {
    if (!handle || !yuv_data || !callback) {
        return -1;
    }
    auto *box = static_cast<VtHevcEncoderBox *>(handle);
    if (!box->session || !box->pixel_pool) {
        return -1;
    }
    const size_t w = static_cast<size_t>(box->width);
    const size_t h = static_cast<size_t>(box->height);
    const size_t uv_h = h / 2;
    if (src_y_stride < w || src_uv_stride < w) {
        return -2;
    }
    if (src_y_stride * h > yuv_len || src_uv_offset + src_uv_stride * uv_h > yuv_len) {
        return -3;
    }

    @autoreleasepool {
        CVPixelBufferRef pb = nullptr;
        if (CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, box->pixel_pool, &pb) != kCVReturnSuccess || !pb) {
            return -4;
        }
        if (CVPixelBufferLockBaseAddress(pb, 0) != kCVReturnSuccess) {
            CVPixelBufferRelease(pb);
            return -5;
        }

        uint8_t *dst_y = static_cast<uint8_t *>(CVPixelBufferGetBaseAddressOfPlane(pb, 0));
        size_t dst_y_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
        uint8_t *dst_uv = static_cast<uint8_t *>(CVPixelBufferGetBaseAddressOfPlane(pb, 1));
        size_t dst_uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);

        if (dst_y && dst_uv && dst_y_stride >= w && dst_uv_stride >= w) {
            if (dst_y_stride == src_y_stride) {
                std::memcpy(dst_y, yuv_data, dst_y_stride * h);
            } else {
                for (size_t r = 0; r < h; ++r) {
                    std::memcpy(dst_y + r * dst_y_stride, yuv_data + r * src_y_stride, w);
                }
            }
            const uint8_t *src_uv = yuv_data + src_uv_offset;
            if (dst_uv_stride == src_uv_stride) {
                std::memcpy(dst_uv, src_uv, dst_uv_stride * uv_h);
            } else {
                for (size_t r = 0; r < uv_h; ++r) {
                    std::memcpy(dst_uv + r * dst_uv_stride, src_uv + r * src_uv_stride, w);
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, 0);

        box->current.callback = callback;
        box->current.user_data = user_data;
        box->current.pts_ms = pts_ms;
        box->current.emitted = 0;
        box->current.status = noErr;

        NSDictionary *frameProps = force_key
            ? @{(id)kVTEncodeFrameOptionKey_ForceKeyFrame: @YES}
            : nil;

        CMTime pts = CMTimeMake(pts_ms, 1000);
        OSStatus st = VTCompressionSessionEncodeFrame(
            box->session,
            pb,
            pts,
            kCMTimeInvalid,
            (__bridge CFDictionaryRef)frameProps,
            nullptr,
            nullptr
        );
        CVPixelBufferRelease(pb);
        if (st != noErr) {
            return static_cast<int32_t>(st);
        }

        // Complete synchronously so MaxFrameDelayCount=0 emits immediately (true latency_free=true).
        st = VTCompressionSessionCompleteFrames(box->session, pts);
        if (st != noErr) {
            return static_cast<int32_t>(st);
        }
        if (box->current.status != noErr) {
            return static_cast<int32_t>(box->current.status);
        }
        return box->current.emitted > 0 ? 0 : -6;
    }
}

extern "C" void rustdesk_vt_hevc_destroy(void *handle) {
    if (!handle) {
        return;
    }
    auto *box = static_cast<VtHevcEncoderBox *>(handle);
    @autoreleasepool {
        if (box->session) {
            VTCompressionSessionCompleteFrames(box->session, kCMTimeInvalid);
            VTCompressionSessionInvalidate(box->session);
            CFRelease(box->session);
            box->session = nullptr;
        }
        if (box->pixel_pool) {
            CVPixelBufferPoolRelease(box->pixel_pool);
            box->pixel_pool = nullptr;
        }
    }
    delete box;
}
