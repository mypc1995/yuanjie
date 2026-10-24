use std::{
    alloc::{alloc, dealloc, handle_alloc_error, Layout},
    ops::Deref,
    ptr::NonNull,
};

/// 一个拥有所有权的字节缓冲区，分配时的对齐方式由调用方指定。
pub struct AlignedU8Vec {
    ptr: NonNull<u8>,
    len: usize,
    capacity: usize,
    layout: Layout,
}

// 安全性：AlignedU8Vec 独占其内存分配，并且只
// 通过独占访问暴露可变能力，因此移动或共享它与 Vec<u8> 一样安全。
unsafe impl Send for AlignedU8Vec {}
unsafe impl Sync for AlignedU8Vec {}

impl AlignedU8Vec {
    /// 追加字节，且不超过分配时申请的容量。
    pub fn extend_from_slice(&mut self, data: &[u8]) {
        assert!(
            data.len() <= self.capacity - self.len,
            "extend beyond fixed capacity"
        );
        unsafe {
            self.ptr
                .as_ptr()
                .add(self.len)
                .copy_from_nonoverlapping(data.as_ptr(), data.len());
        }
        self.len += data.len();
    }
}

impl Deref for AlignedU8Vec {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedU8Vec {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

/// 按请求的容量和对齐方式分配一个空的字节缓冲区。
pub fn aligned_u8_vec(cap: usize, align: usize) -> AlignedU8Vec {
    let layout = Layout::from_size_align(cap.max(1), align)
        .expect("invalid aligned value, must be power of 2");
    unsafe {
        let ptr = NonNull::new(alloc(layout)).unwrap_or_else(|| handle_alloc_error(layout));
        AlignedU8Vec {
            ptr,
            len: 0,
            capacity: cap,
            layout,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn is_send_and_sync() {
        assert_send_sync::<AlignedU8Vec>();
    }

    #[test]
    fn preserves_alignment_and_contents() {
        let mut data = aligned_u8_vec(10, 4096);
        data.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(data.as_ptr() as usize % 4096, 0);
        assert_eq!(&*data, &[1, 2, 3, 4]);
    }

    #[test]
    #[should_panic(expected = "extend beyond fixed capacity")]
    fn rejects_extension_when_capacity_is_zero() {
        let mut data = aligned_u8_vec(0, 4);
        data.extend_from_slice(&[1]);
    }

    #[test]
    fn accumulates_across_multiple_extends() {
        let mut data = aligned_u8_vec(8, 4);
        data.extend_from_slice(&[1, 2, 3]);
        data.extend_from_slice(&[4, 5]);
        assert_eq!(&*data, &[1, 2, 3, 4, 5]);
    }

    #[test]
    #[should_panic(expected = "extend beyond fixed capacity")]
    fn rejects_extension_past_partial_fill() {
        let mut data = aligned_u8_vec(4, 4);
        data.extend_from_slice(&[1, 2, 3]);
        data.extend_from_slice(&[4, 5]);
    }
}
