use std::time::Duration;

use scrap::{Frame, TraitPixelBuffer};

extern crate scrap;

fn main() {
    use scrap::{Capturer, Display, TraitCapturer};
    use std::io::ErrorKind::WouldBlock;
    use std::io::Write;
    use std::process::{Command, Stdio};

    let d = Display::primary().unwrap();
    let (w, h) = (d.width(), d.height());

    let child = Command::new("ffplay")
        .args(&[
            "-f",
            "rawvideo",
            "-pixel_format",
            "bgr0",
            "-video_size",
            &format!("{}x{}", w, h),
            "-framerate",
            "60",
            "-",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .expect("This example requires ffplay.");

    let mut capturer = Capturer::new(d).unwrap();
    let mut out = child.stdin.unwrap();

    loop {
        match capturer.frame(Duration::from_millis(0)) {
            Ok(frame) => {
                // 写出该帧，并去掉行尾填充。
                let Frame::PixelBuffer(frame) = frame else {
                    return;
                };
                let stride = frame.stride()[0];
                let rowlen = 4 * w;
                for row in frame.data().chunks(stride) {
                    let row = &row[..rowlen];
                    out.write_all(row).unwrap();
                }
            }
            Err(ref e) if e.kind() == WouldBlock => {
                // 等待该帧。
            }
            Err(_) => {
                // 这里就结束了。
                break;
            }
        }
    }
}
