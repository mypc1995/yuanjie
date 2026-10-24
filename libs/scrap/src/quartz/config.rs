use std::ptr;

use hbb_common::libc::c_void;

use super::ffi::*;

//TODO: Color space, YCbCr matrix.
pub struct Config {
    /// 光标是否可见。
    pub cursor: bool,
    /// 是加黑边还是拉伸。
    pub letterbox: bool,
    /// 每帧的最少秒数。
    pub throttle: f64,
    /// 分配多少帧。
    /// 推荐值为 3。
    /// 最大值为 8。
    pub queue_length: i8,
}

impl Config {
    /// 别忘了对它调用 CFRelease！
    pub fn build(self) -> CFDictionaryRef {
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

            let res = CFDictionaryCreate(
                ptr::null_mut(),
                keys.as_ptr(),
                values.as_ptr(),
                4,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );

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
            throttle: 0.0,
            queue_length: 3,
        }
    }
}
