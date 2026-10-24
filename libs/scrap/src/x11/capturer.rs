use super::ffi::*;
use super::Display;
use hbb_common::libc;
use std::{io, ptr, slice};

pub struct Capturer {
    display: Display,
    shmid: i32,
    xcbid: u32,
    buffer: *const u8,

    size: usize,
    saved_raw_data: Vec<u8>, // for faster compare and copy
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        // 计算尺寸。

        let pixel_width = display.pixfmt().bytes_per_pixel();
        let rect = display.rect();
        let size = (rect.w as usize) * (rect.h as usize) * pixel_width;

        // 创建共享内存段。

        let shmid = unsafe {
            libc::shmget(
                libc::IPC_PRIVATE,
                size,
                // 任何人都可以做任何事。
                libc::IPC_CREAT | 0o777,
            )
        };

        if shmid == -1 {
            return Err(io::Error::last_os_error());
        }

        // 把该内存段映射到一个可读地址。

        let buffer = unsafe { libc::shmat(shmid, ptr::null(), libc::SHM_RDONLY) } as *mut u8;

        if buffer as isize == -1 {
            return Err(io::Error::last_os_error());
        }

        // 把该内存段挂接到 XCB。

        let server = display.server().raw();
        let xcbid = unsafe { xcb_generate_id(server) };
        unsafe {
            xcb_shm_attach(
                server,
                xcbid,
                shmid as u32,
                0, // False, i.e. not read-only.
            );
        }

        let c = Capturer {
            display,
            shmid,
            xcbid,
            buffer,
            size,
            saved_raw_data: Vec::new(),
        };
        Ok(c)
    }

    pub fn display(&self) -> &Display {
        &self.display
    }

    fn get_image(&self) {
        let rect = self.display.rect();
        unsafe {
            let request = xcb_shm_get_image_unchecked(
                self.display.server().raw(),
                self.display.root(),
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                !0,
                XCB_IMAGE_FORMAT_Z_PIXMAP,
                self.xcbid,
                0,
            );
            let response =
                xcb_shm_get_image_reply(self.display.server().raw(), request, ptr::null_mut());
            libc::free(response as *mut _);
        }
    }

    pub fn frame<'b>(&'b mut self) -> std::io::Result<&'b [u8]> {
        self.get_image();
        let result = unsafe { slice::from_raw_parts(self.buffer, self.size) };
        crate::would_block_if_equal(&mut self.saved_raw_data, result)?;
        Ok(result)
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        unsafe {
            // 把该内存段从 XCB 上摘下。
            xcb_shm_detach(self.display.server().raw(), self.xcbid);
            // 把该内存段从我们的地址空间摘下。
            libc::shmdt(self.buffer as *mut _);
            // 销毁该共享内存段。
            libc::shmctl(self.shmid, libc::IPC_RMID, ptr::null_mut());
        }
    }
}
