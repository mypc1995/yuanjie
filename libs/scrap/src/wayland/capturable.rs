use std::boxed::Box;
use std::error::Error;

pub enum PixelProvider<'a> {
    // 每种颜色 8 位
    RGB(usize, usize, &'a [u8]),
    RGB0(usize, usize, &'a [u8]),
    BGR0(usize, usize, &'a [u8]),
    // width、height、stride
    BGR0S(usize, usize, usize, &'a [u8]),
    NONE,
}

impl<'a> PixelProvider<'a> {
    pub fn size(&self) -> (usize, usize) {
        match self {
            PixelProvider::RGB(w, h, _) => (*w, *h),
            PixelProvider::RGB0(w, h, _) => (*w, *h),
            PixelProvider::BGR0(w, h, _) => (*w, *h),
            PixelProvider::BGR0S(w, h, _, _) => (*w, *h),
            PixelProvider::NONE => (0, 0),
        }
    }
}

pub trait Recorder {
    fn capture(&mut self, timeout_ms: u64) -> Result<PixelProvider<'_>, Box<dyn Error>>;
}

pub trait BoxCloneCapturable {
    fn box_clone(&self) -> Box<dyn Capturable>;
}

impl<T> BoxCloneCapturable for T
where
    T: Clone + Capturable + 'static,
{
    fn box_clone(&self) -> Box<dyn Capturable> {
        Box::new(self.clone())
    }
}

pub trait Capturable: Send + BoxCloneCapturable {
    /// Capturable 的名称，例如它是窗口时的窗口标题。
    fn name(&self) -> String;
    /// 以相对于屏幕绝对尺寸的浮点数返回 Capturable 的 x、y、width、height。
    /// 例如 x=0.5、y=0.0、width=0.5、height=1.0 表示屏幕的右半部分。
    fn geometry_relative(&self) -> Result<(f64, f64, f64, f64), Box<dyn Error>>;
    /// 在模拟输入之前调用的回调。
    /// 用于在有输入时把窗口置前。
    fn before_input(&mut self) -> Result<(), Box<dyn Error>>;
    /// 返回一个可以录制当前 capturable 的 Recorder。
    fn recorder(&self, capture_cursor: bool) -> Result<Box<dyn Recorder>, Box<dyn Error>>;
}

impl Clone for Box<dyn Capturable> {
    fn clone(&self) -> Self {
        self.box_clone()
    }
}
