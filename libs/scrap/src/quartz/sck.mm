#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <IOSurface/IOSurface.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>

#include <atomic>
#include <stdint.h>

typedef void (*SckFrameCallback)(void *user_data, IOSurfaceRef surface);

API_AVAILABLE(macos(12.3))
@interface RustDeskSckOutput : NSObject <SCStreamOutput, SCStreamDelegate> {
@public
    std::atomic<bool> stopped;
    std::atomic<bool> failed;
    SckFrameCallback callback;
    void *user_data;
}
@end

@implementation RustDeskSckOutput

- (instancetype)initWithCallback:(SckFrameCallback)cb userData:(void *)ud {
    self = [super init];
    if (self) {
        stopped.store(false, std::memory_order_relaxed);
        failed.store(false, std::memory_order_relaxed);
        callback = cb;
        user_data = ud;
    }
    return self;
}

- (void)stream:(SCStream *)stream
    didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
                   ofType:(SCStreamOutputType)type {
    (void)stream;
    if (type != SCStreamOutputTypeScreen) {
        return;
    }
    if (stopped.load(std::memory_order_acquire)) {
        return;
    }
    if (!sampleBuffer || !CMSampleBufferIsValid(sampleBuffer)) {
        return;
    }

    CFArrayRef attachmentsArray = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, false);
    if (!attachmentsArray || CFArrayGetCount(attachmentsArray) == 0) {
        return;
    }
    NSDictionary *attachments = (__bridge NSDictionary *)CFArrayGetValueAtIndex(attachmentsArray, 0);
    if (!attachments) {
        return;
    }

    NSNumber *statusNum = attachments[SCStreamFrameInfoStatus];
    if (!statusNum || statusNum.integerValue != SCFrameStatusComplete) {
        return;
    }

    NSArray *dirtyRects = attachments[SCStreamFrameInfoDirtyRects];
    if (dirtyRects && dirtyRects.count == 0) {
        return;
    }

    CVImageBufferRef imageBuffer = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (!imageBuffer) {
        return;
    }

    IOSurfaceRef surface = CVPixelBufferGetIOSurface(imageBuffer);
    if (!surface) {
        return;
    }

    if (!stopped.load(std::memory_order_acquire) && callback != nullptr) {
        callback(user_data, surface);
    }
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    (void)stream;
    if (error != nil) {
        failed.store(true, std::memory_order_release);
    }
}

@end

struct SckCapturerBox {
    void *stream_ref;
    void *output_ref;
    dispatch_queue_t sample_queue;
};

extern "C" void *rustdesk_sck_capturer_create(
    uint32_t display_id,
    size_t width,
    size_t height,
    bool show_cursor,
    bool use_bgra,
    double min_frame_interval_sec,
    int32_t queue_depth,
    SckFrameCallback callback,
    void *user_data
) {
    if (@available(macOS 12.3, *)) {
        if ([SCShareableContent class] == nil || [SCStream class] == nil) {
            return nullptr;
        }
        @autoreleasepool {
            __block SCShareableContent *content = nil;
            __block NSError *shareError = nil;
            dispatch_semaphore_t shareSem = dispatch_semaphore_create(0);

            [SCShareableContent getShareableContentExcludingDesktopWindows:NO
                                                       onScreenWindowsOnly:YES
                                                         completionHandler:^(SCShareableContent * _Nullable shareableContent,
                                                                             NSError * _Nullable error) {
                content = shareableContent;
                shareError = error;
                dispatch_semaphore_signal(shareSem);
            }];

            long waitShare = dispatch_semaphore_wait(
                shareSem,
                dispatch_time(DISPATCH_TIME_NOW, 3LL * NSEC_PER_SEC)
            );
            if (waitShare != 0 || shareError != nil || content == nil) {
                return nullptr;
            }

            SCDisplay *targetDisplay = nil;
            for (SCDisplay *d in content.displays) {
                if (d.displayID == display_id) {
                    targetDisplay = d;
                    break;
                }
            }
            if (targetDisplay == nil) {
                return nullptr;
            }

            SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:targetDisplay
                                                              excludingWindows:@[]];
            SCStreamConfiguration *cfg = [[SCStreamConfiguration alloc] init];
            cfg.width = width;
            cfg.height = height;
            if (use_bgra) {
                // Full-resolution 32-bit BGRA for true 4:4:4 (I444) chroma fidelity.
                cfg.pixelFormat = kCVPixelFormatType_32BGRA;
            } else {
                cfg.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
                // Match hwcodec (AVCOL_SPC_SMPTE170M) and libyuv NV12/I420 decode matrix.
                cfg.colorMatrix = kCGDisplayStreamYCbCrMatrix_ITU_R_601_4;
            }
#if defined(__MAC_14_0)
            if (@available(macOS 14.0, *)) {
                // Force ScreenCaptureKit to sample from the highest native HiDPI backing store
                // rather than falling back to 1x nominal capture when output matches logical size.
                cfg.captureResolution = SCCaptureResolutionBest;
            }
#endif
            cfg.showsCursor = show_cursor ? YES : NO;
            cfg.scalesToFit = YES;
            cfg.colorSpaceName = kCGColorSpaceSRGB;
            cfg.queueDepth = (queue_depth >= 3 && queue_depth <= 8) ? queue_depth : 5;

            if (min_frame_interval_sec > 0.0) {
                int32_t fps = (int32_t)(1.0 / min_frame_interval_sec + 0.5);
                if (fps < 1) fps = 1;
                if (fps > 120) fps = 120;
                cfg.minimumFrameInterval = CMTimeMake(1, fps);
            } else {
                cfg.minimumFrameInterval = CMTimeMake(1, 60);
            }

            RustDeskSckOutput *output = [[RustDeskSckOutput alloc] initWithCallback:callback
                                                                           userData:user_data];
            SCStream *stream = [[SCStream alloc] initWithFilter:filter
                                                  configuration:cfg
                                                       delegate:output];
            if (stream == nil) {
                return nullptr;
            }

            dispatch_queue_attr_t attr = dispatch_queue_attr_make_with_qos_class(
                DISPATCH_QUEUE_SERIAL,
                QOS_CLASS_USER_INTERACTIVE,
                0
            );
            dispatch_queue_t queue = dispatch_queue_create("com.rustdesk.sck.capture", attr);

            NSError *addOutputErr = nil;
            BOOL added = [stream addStreamOutput:output
                                            type:SCStreamOutputTypeScreen
                              sampleHandlerQueue:queue
                                           error:&addOutputErr];
            if (!added || addOutputErr != nil) {
                return nullptr;
            }

            __block NSError *startError = nil;
            dispatch_semaphore_t startSem = dispatch_semaphore_create(0);
            [stream startCaptureWithCompletionHandler:^(NSError * _Nullable error) {
                startError = error;
                dispatch_semaphore_signal(startSem);
            }];

            long waitStart = dispatch_semaphore_wait(
                startSem,
                dispatch_time(DISPATCH_TIME_NOW, 3LL * NSEC_PER_SEC)
            );
            if (waitStart != 0 || startError != nil) {
                output->stopped.store(true, std::memory_order_release);
                return nullptr;
            }

            SckCapturerBox *box = new SckCapturerBox();
            box->stream_ref = (__bridge_retained void *)stream;
            box->output_ref = (__bridge_retained void *)output;
            box->sample_queue = queue;
            return box;
        }
    }
    return nullptr;
}

extern "C" bool rustdesk_sck_capturer_has_error(void *handle) {
    if (!handle) {
        return true;
    }
    if (@available(macOS 12.3, *)) {
        SckCapturerBox *box = static_cast<SckCapturerBox *>(handle);
        RustDeskSckOutput *output = (__bridge RustDeskSckOutput *)box->output_ref;
        return output->failed.load(std::memory_order_acquire);
    }
    return true;
}

extern "C" void rustdesk_sck_capturer_destroy(void *handle) {
    if (!handle) {
        return;
    }
    if (@available(macOS 12.3, *)) {
        @autoreleasepool {
            SckCapturerBox *box = static_cast<SckCapturerBox *>(handle);
            RustDeskSckOutput *output = (__bridge_transfer RustDeskSckOutput *)box->output_ref;
            SCStream *stream = (__bridge_transfer SCStream *)box->stream_ref;

            output->stopped.store(true, std::memory_order_release);

            dispatch_semaphore_t stopSem = dispatch_semaphore_create(0);
            [stream stopCaptureWithCompletionHandler:^(NSError * _Nullable error) {
                (void)error;
                dispatch_semaphore_signal(stopSem);
            }];
            (void)dispatch_semaphore_wait(
                stopSem,
                dispatch_time(DISPATCH_TIME_NOW, 500LL * NSEC_PER_MSEC)
            );

            // Drain any in-flight callback on the serial sample queue before freeing user_data.
            if (box->sample_queue != nil) {
                dispatch_sync(box->sample_queue, ^{
                    output->callback = nullptr;
                    output->user_data = nullptr;
                });
            }
            box->sample_queue = nil;
            delete box;
        }
    }
}
