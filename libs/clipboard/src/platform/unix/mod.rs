use dashmap::DashMap;
use lazy_static::lazy_static;

mod filetype;
pub use filetype::{FileDescription, FileType};
/// 在这些平台上使用 FUSE 实现文件粘贴
#[cfg(target_os = "linux")]
pub mod fuse;
#[cfg(target_os = "macos")]
pub mod macos;

pub mod local_file;
pub mod serv_files;

/// 具有有效的文件属性
pub const FLAGS_FD_ATTRIBUTES: u32 = 0x04;
/// 具有有效的文件大小
pub const FLAGS_FD_SIZE: u32 = 0x40;
/// 具有有效的最后写入时间
pub const FLAGS_FD_LAST_WRITE: u32 = 0x20;
/// 显示进度
pub const FLAGS_FD_PROGRESSUI: u32 = 0x4000;
/// 从 unix 传输而来，包含文件权限位
/// 附注：此标志在 windows 中不使用
pub const FLAGS_FD_UNIX_MODE: u32 = 0x08;

// 并非真正的格式 id，只是占位符
pub const FILEDESCRIPTOR_FORMAT_ID: i32 = 49334;
pub const FILEDESCRIPTORW_FORMAT_NAME: &str = "FileGroupDescriptorW";
// 并非真正的格式 id，只是占位符
pub const FILECONTENTS_FORMAT_ID: i32 = 49267;
pub const FILECONTENTS_FORMAT_NAME: &str = "FileContents";

/// fuse 的块大小，与我们通过 FileContentsRequest 使用的异步请求大小对齐。
pub(crate) const BLOCK_SIZE: u32 = 4 * 1024 * 1024;

/// `FILEDESCRIPTORW::cFileName` 的容量，包含末尾的 NUL 码元。
pub(super) const FILE_NAME_CODE_UNITS: usize = 260;
pub(super) const FILE_NAME_FIELD_SIZE: usize = FILE_NAME_CODE_UNITS * std::mem::size_of::<u16>();

// 微软所使用的纪元起点
// 1601-01-01 00:00:00 + LDAP_EPOCH_DELTA*(100 ns) = 1970-01-01 00:00:00
// 134_774 days (369 years, 89 leap days) * 86_400 s * 10_000_000 per s
const LDAP_EPOCH_DELTA: u64 = 116_444_736_000_000_000;

lazy_static! {
    static ref REMOTE_FORMAT_MAP: DashMap<i32, String> = DashMap::from_iter(
        [
            (
                FILEDESCRIPTOR_FORMAT_ID,
                FILEDESCRIPTORW_FORMAT_NAME.to_string()
            ),
            (FILECONTENTS_FORMAT_ID, FILECONTENTS_FORMAT_NAME.to_string())
        ]
        .iter()
        .cloned()
    );
}

#[inline]
pub fn get_local_format(remote_id: i32) -> Option<String> {
    REMOTE_FORMAT_MAP.get(&remote_id).map(|s| s.clone())
}
