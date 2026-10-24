use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

// 每条流各自拥有自己的标志，这样迟到的回调就不会重启一个替换实例。
#[derive(Clone, Default)]
pub(super) struct CaptureErrorHandler {
    interrupted: Arc<AtomicBool>,
}

impl CaptureErrorHandler {
    pub(super) fn handle(&self, error: cpal::StreamError) {
        if matches!(
            error,
            cpal::StreamError::StreamInterrupted { .. } | cpal::StreamError::DeviceNotAvailable
        ) {
            // ScreenCaptureKit 可能在远程会话仍然打开时停止采集。
            // 观察到的 -3821 错误并未指出其根本触发原因。
            // https://developer.apple.com/documentation/screencapturekit/scstreamdelegate/stream(_:didstopwitherror:)
            hbb_common::log::error!("Audio capture stream interrupted: {error}");
            self.interrupted.store(true, Ordering::Relaxed);
        } else {
            // 频繁出现的 sample buffer 错误保持现有的 trace 级别。
            hbb_common::log::trace!("an error occurred on stream: {error}");
        }
    }

    pub(super) fn process_frame(&self, process: impl FnOnce() -> hbb_common::ResultType<()>) {
        // 防御性恢复：在采集输入有效的情况下持续出现的处理失败
        // 尚未复现。在流被替换之前停止使用已出错的处理器。
        if self.needs_restart() {
            return;
        }
        if let Err(error) = process() {
            self.interrupted.store(true, Ordering::Relaxed);
            hbb_common::log::error!(
                "Failed to process captured audio frame; requesting stream restart: {error:#}"
            );
        }
    }

    pub(super) fn needs_restart(&self) -> bool {
        self.interrupted.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::{BackendSpecificError, StreamError};

    fn system_interruption() -> StreamError {
        StreamError::StreamInterrupted {
            err: BackendSpecificError {
                description: "ScreenCaptureKit system-stopped capture (-3821)".to_owned(),
            },
        }
    }

    #[test]
    fn system_interruption_requests_recreation_of_the_active_stream() {
        let errors = CaptureErrorHandler::default();
        let callback = errors.clone();
        assert!(!errors.needs_restart());
        callback.handle(system_interruption());
        assert!(errors.needs_restart());
    }

    #[test]
    fn other_backend_errors_do_not_request_recreation() {
        let errors = CaptureErrorHandler::default();
        errors.handle(StreamError::BackendSpecific {
            err: BackendSpecificError {
                description: "A sample buffer could not be read".to_owned(),
            },
        });
        assert!(!errors.needs_restart());
    }
}
