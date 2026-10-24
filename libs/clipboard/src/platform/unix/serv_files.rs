use crate::{
    platform::unix::{
        filetype::file_list_id,
        local_file::{construct_file_list, LocalFile},
    },
    ClipboardFile, CliprdrError,
};
use hbb_common::{
    bytes::{BufMut, BytesMut},
    log,
};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{atomic::Ordering, Arc},
    time::SystemTime,
    usize,
};

// 我们的客户端每个区间最多请求这么多：macOS 为 4 MiB，FUSE 更少，而 Windows
// 会拆分更大的读取（WF_CLIPRDR_MAX_RANGE_READ）。更大的读取会被拒绝，而不是被截短：
// 在 Windows 上，对 IStream 调用方而言，短响应会被读作文件结束。
const MAX_RANGE_READ: u64 = 16 * 1024 * 1024;

lazy_static::lazy_static! {
    // 本地文件是缓存的，复制文件时不应改变这个值
    // 因为 `CliprdrFileContentsRequest` 只包含该文件在列表中的索引。
    // 我们需要让文件列表与远端保持相同的顺序。
    // 将来我们可能会给 `CliprdrFileContentsRequest` 增加一个 `FileId` 字段。
    static ref CLIP_FILES: Arc<Mutex<ClipFiles>> = Default::default();
    // 在对端收到某个列表之后，该列表被较新的副本替换，按最旧优先的顺序。对端的
    // 数据流仍然读取它们原本所属的那个列表。在 `CLIP_FILES` 之后加锁。
    static ref RETIRED_CLIP_FILES: Mutex<VecDeque<ClipFiles>> = Default::default();
}

const MAX_RETIRED_CLIP_FILES: usize = 4;

// 第一个描述符的 `clsid`，位于每个对端都会跳过的 32 个保留字节之内。一个随机的
// nonce 放在那里，可以让每次复制的列表 id 互不相同，即使两份副本具有完全相同的
// 名称、大小和时间（同一个文件位于两个文件夹中）。
const LIST_NONCE: std::ops::Range<usize> = 8..24;

#[derive(Debug)]
enum FileContentsRequest {
    Size {
        stream_id: i32,
        file_idx: usize,
    },

    Range {
        stream_id: i32,
        file_idx: usize,
        offset: u64,
        length: u64,
    },
}

// 某个顶层选中项的廉价指纹。大小/修改时间的变化 ——
// 或选区中出现目录 —— 都会强制 sync_files() 重建（见下文）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct FileSig {
    size: u64,
    mtime: Option<SystemTime>,
    is_dir: bool,
}

// 只 stat 顶层的选中路径（不递归），顺序与 `files` 相同。
fn fingerprint(files: &[String]) -> Vec<FileSig> {
    files
        .iter()
        .map(|s| match std::fs::metadata(s) {
            Ok(mt) => FileSig {
                size: mt.len(),
                mtime: mt.modified().ok(),
                is_dir: mt.is_dir(),
            },
            Err(_) => FileSig::default(),
        })
        .collect()
}

#[derive(Default)]
struct ClipFiles {
    files: Vec<String>,
    // `files` 的指纹（长度/顺序相同）；用于在重新复制时检测原地修改。
    sigs: Vec<FileSig>,
    file_list: Vec<LocalFile>,
    first_file_index: usize,
    files_pdu: Vec<u8>,
    // `file_list_id(files_pdu)`，对端会把它作为 `clip_data_id` 回传。
    id: i32,
    // 已被发送 `files_pdu` 的连接。
    served_to: Vec<i32>,
}

impl ClipFiles {
    fn clear(&mut self) {
        self.files.clear();
        self.sigs.clear();
        self.file_list.clear();
        self.first_file_index = usize::MAX;
        self.files_pdu.clear();
        self.id = 0;
        self.served_to.clear();
    }

    fn sync_files(
        &mut self,
        clipboard_files: &[String],
        sigs: Vec<FileSig>,
    ) -> Result<(), CliprdrError> {
        let clipboard_paths = clipboard_files
            .iter()
            .map(|s| PathBuf::from(s))
            .collect::<Vec<_>>();
        self.file_list = construct_file_list(&clipboard_paths)?;
        self.first_file_index = self
            .file_list
            .iter()
            .position(|f| !f.path.is_dir())
            .unwrap_or(usize::MAX);
        self.files = clipboard_files.to_vec();
        self.sigs = sigs;
        Ok(())
    }

    fn build_file_list_pdu(&mut self) -> Result<(), CliprdrError> {
        let mut data = BytesMut::with_capacity(4 + 592 * self.file_list.len());
        data.put_u32_le(self.file_list.len() as u32);
        for file in self.file_list.iter() {
            data.put(file.as_bin()?.as_slice());
        }
        self.files_pdu = data.to_vec();
        if let Some(nonce) = self.files_pdu.get_mut(LIST_NONCE) {
            nonce.copy_from_slice(&rand::random::<[u8; 16]>());
        }
        self.id = file_list_id(&self.files_pdu);
        Ok(())
    }

    // 关闭那些当前没有被读取的文件，例如预加载的下一个文件；`read_exact_at`
    // 会在下次读取时重新打开它们。传输到一半的文件会保留其句柄和偏移量，因此
    // 即使期间它的路径被替换，剩余字节仍来自同一个文件。
    fn release_handles(&mut self) {
        for file in self.file_list.iter_mut() {
            if file.offset.load(Ordering::Relaxed) == 0 {
                file.handle = None;
            }
        }
    }

    // 某个已退役列表的文件在句柄关闭后通过路径重新打开，而此时该路径可能已经
    // 指向了另一个文件。要在读取将要使用的句柄上检查大小和修改时间，并拒绝
    // 与列表不再匹配的文件。
    fn check_retired_file(&mut self, request: &FileContentsRequest) -> Result<(), CliprdrError> {
        let FileContentsRequest::Range { file_idx, .. } = request else {
            return Ok(());
        };
        let Some(file) = self.file_list.get_mut(*file_idx) else {
            return Ok(());
        };
        if file.is_dir {
            return Ok(());
        }
        file.load_handle()?;
        let unchanged = file
            .handle
            .as_ref()
            .and_then(|handle| handle.get_ref().metadata().ok())
            .map_or(false, |md| {
                md.len() == file.size && md.modified().ok() == Some(file.last_write_time)
            });
        if unchanged {
            Ok(())
        } else {
            Err(CliprdrError::InvalidRequest {
                description: format!("file {} changed since its list was sent", file.name),
            })
        }
    }

    fn get_files_for_audit(&self, request: &FileContentsRequest) -> Option<ClipboardFile> {
        if let FileContentsRequest::Range {
            file_idx, offset, ..
        } = request
        {
            if *file_idx == self.first_file_index && *offset == 0 {
                let files: Vec<(String, u64)> = self
                    .file_list
                    .iter()
                    .filter_map(|f| {
                        if f.path.is_file() {
                            Some((f.path.to_string_lossy().to_string(), f.size))
                        } else {
                            None
                        }
                    })
                    .collect::<_>();
                if files.is_empty() {
                    return None;
                } else {
                    return Some(ClipboardFile::Files { files });
                }
            }
        }
        None
    }

    fn serve_file_contents(
        &mut self,
        conn_id: i32,
        request: FileContentsRequest,
    ) -> Result<ClipboardFile, CliprdrError> {
        let (file_idx, file_contents_resp) = match request {
            FileContentsRequest::Size {
                stream_id,
                file_idx,
            } => {
                log::debug!("file contents (size) requested from conn: {}", conn_id);
                let Some(file) = self.file_list.get(file_idx) else {
                    log::error!(
                        "invalid file index {} requested from conn: {}",
                        file_idx,
                        conn_id
                    );
                    return Err(CliprdrError::InvalidRequest {
                        description: format!(
                            "invalid file index {} requested from conn: {}",
                            file_idx, conn_id
                        ),
                    });
                };

                log::debug!(
                    "conn {} requested file-{}: {}",
                    conn_id,
                    file_idx,
                    file.name
                );

                let size = file.size;
                (
                    file_idx,
                    ClipboardFile::FileContentsResponse {
                        msg_flags: 0x1,
                        stream_id,
                        requested_data: size.to_le_bytes().to_vec(),
                    },
                )
            }
            FileContentsRequest::Range {
                stream_id,
                file_idx,
                offset,
                length,
            } => {
                log::debug!(
                    "file contents (range from {} length {}) request from conn: {}",
                    offset,
                    length,
                    conn_id
                );
                let Some(file) = self.file_list.get_mut(file_idx) else {
                    log::error!(
                        "invalid file index {} requested from conn: {}",
                        file_idx,
                        conn_id
                    );
                    return Err(CliprdrError::InvalidRequest {
                        description: format!(
                            "invalid file index {} requested from conn: {}",
                            file_idx, conn_id
                        ),
                    });
                };
                log::debug!(
                    "conn {} requested file-{}: {}",
                    conn_id,
                    file_idx,
                    file.name
                );

                if offset > file.size {
                    log::error!("invalid reading offset requested from conn: {}", conn_id);
                    return Err(CliprdrError::InvalidRequest {
                        description: format!(
                            "invalid reading offset requested from conn: {}",
                            conn_id
                        ),
                    });
                }
                let read_size = if length > file.size - offset {
                    file.size - offset
                } else {
                    length
                };

                if read_size > MAX_RANGE_READ {
                    return Err(CliprdrError::InvalidRequest {
                        description: format!(
                            "file contents request of {} bytes exceeds the {} byte limit, conn: {}",
                            read_size, MAX_RANGE_READ, conn_id
                        ),
                    });
                }

                let mut buf = vec![0u8; read_size as usize];

                file.read_exact_at(&mut buf, offset)?;

                (
                    file_idx,
                    ClipboardFile::FileContentsResponse {
                        msg_flags: 0x1,
                        stream_id,
                        requested_data: buf,
                    },
                )
            }
        };

        log::debug!("file contents sent to conn: {}", conn_id);
        // 热重载下一个文件
        for next_file in self.file_list.iter_mut().skip(file_idx + 1) {
            if !next_file.is_dir {
                // 尽力而为：如果仍无法打开，它自己的请求会报告这一点。
                if let Err(e) = next_file.load_handle() {
                    log::trace!("failed to preload the next file: {:?}", e);
                }
                break;
            }
        }
        Ok(file_contents_resp)
    }
}

#[inline]
pub fn clear_files() {
    CLIP_FILES.lock().clear();
    RETIRED_CLIP_FILES.lock().clear();
}

// 某个请求读取的列表：其 `clip_data_id` 所指明的那一个，否则就是最后发送给
// 该连接的那一个。被指明的列表若已消失则返回 `None`。既未发送 id 又未曾
// 收到过列表的对端，仍像以前一样读取当前列表。
fn select_clip_files<'a>(
    current: &'a mut ClipFiles,
    retired: &'a mut VecDeque<ClipFiles>,
    conn_id: i32,
    clip_data_id: Option<i32>,
) -> Option<&'a mut ClipFiles> {
    match clip_data_id {
        Some(id) if current.id == id => Some(current),
        Some(id) => retired
            .iter_mut()
            .rev()
            .find(|files| files.id == id && files.served_to.contains(&conn_id)),
        None if current.served_to.contains(&conn_id) => Some(current),
        None => retired
            .iter_mut()
            .rev()
            .find(|files| files.served_to.contains(&conn_id))
            .or(Some(current)),
    }
}

// 保留已发送给某个对端的那个列表，这样它的数据流不会在相同索引处读到新副本。
fn retire_if_served(mut replaced: ClipFiles) {
    if replaced.served_to.is_empty() {
        return;
    }
    replaced.release_handles();
    let mut retired = RETIRED_CLIP_FILES.lock();
    if retired.len() == MAX_RETIRED_CLIP_FILES {
        retired.pop_front();
    }
    retired.push_back(replaced);
}

pub fn read_file_contents(
    conn_id: i32,
    stream_id: i32,
    list_index: i32,
    dw_flags: i32,
    n_position_low: i32,
    n_position_high: i32,
    cb_requested: i32,
    clip_data_id: Option<i32>,
) -> Vec<Result<ClipboardFile, CliprdrError>> {
    let fcr = if dw_flags == 0x1 {
        FileContentsRequest::Size {
            stream_id,
            file_idx: list_index as usize,
        }
    } else if dw_flags == 0x2 {
        // nPositionLow 和 cbRequested 是承载在 i32 字段中的 UINT32。对它们做符号扩展
        // 会拒绝低位字 >= 2 GiB 的偏移，并把负的长度
        // 变成接近 u64::MAX 的读取。
        let offset = (n_position_high as u64) << 32 | n_position_low as u32 as u64;
        let length = cb_requested as u32 as u64;

        FileContentsRequest::Range {
            stream_id,
            file_idx: list_index as usize,
            offset,
            length,
        }
    } else {
        return vec![Err(CliprdrError::InvalidRequest {
            description: format!("got invalid FileContentsRequest, dw_flats: {dw_flags}"),
        })];
    };

    let mut current = CLIP_FILES.lock();
    let mut retired = RETIRED_CLIP_FILES.lock();
    let current_ptr: *const ClipFiles = &*current;
    let Some(clip_files) = select_clip_files(&mut current, &mut retired, conn_id, clip_data_id)
    else {
        return vec![Err(CliprdrError::InvalidRequest {
            description: format!(
                "file list {:?} is not available to conn: {}",
                clip_data_id, conn_id
            ),
        })];
    };
    if !std::ptr::eq(&*clip_files, current_ptr) {
        if let Err(e) = clip_files.check_retired_file(&fcr) {
            return vec![Err(e)];
        }
    }
    let mut res = vec![];
    if let Some(files_res) = clip_files.get_files_for_audit(&fcr) {
        res.push(Ok(files_res));
    }
    res.push(clip_files.serve_file_contents(conn_id, fcr));
    res
}

pub fn sync_files(files: &[String]) -> Result<(), CliprdrError> {
    // 去重：只有当路径 + 大小 + 修改时间都匹配，且选区中没有目录时，才跳过重建
    // （目录自身的 mtime 不会因为其内部文件被编辑而改变）。
    let current = fingerprint(files);
    let mut files_lock = CLIP_FILES.lock();
    if files_lock.files == files
        && files_lock.sigs == current
        && !current.iter().any(|sig| sig.is_dir)
    {
        return Ok(());
    }
    // 构建放在 aside 中，这样失败时当前列表仍然保留。
    let mut next = ClipFiles::default();
    next.sync_files(files, current)?;
    next.build_file_list_pdu()?;
    let replaced = std::mem::replace(&mut *files_lock, next);
    retire_if_served(replaced);
    Ok(())
}

pub fn get_file_list_pdu(conn_id: i32) -> Vec<u8> {
    let mut clip_files = CLIP_FILES.lock();
    if !clip_files.files_pdu.is_empty() && !clip_files.served_to.contains(&conn_id) {
        clip_files.served_to.push(conn_id);
    }
    clip_files.files_pdu.clone()
}

#[cfg(test)]
mod sig_test {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    // 系统临时目录下的唯一临时目录；drop 时被删除（不引入 dev-dependency）。
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let mut dir = std::env::temp_dir();
            dir.push(format!("rustdesk_sig_test_{}_{}", tag, nanos));
            fs::create_dir_all(&dir).unwrap();
            TmpDir(dir)
        }
        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn path_str(p: &PathBuf) -> String {
        p.to_string_lossy().to_string()
    }

    // 驱动全局 CLIP_FILES 的测试之间不得交错执行。
    static CLIP_FILES_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_clip_files() -> std::sync::MutexGuard<'static, ()> {
        CLIP_FILES_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // 稀疏文件，因此数 GiB 的文件不占用磁盘空间。
    fn write_at(path: &PathBuf, offset: u64, data: &[u8]) {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = fs::File::create(path).unwrap();
        f.seek(SeekFrom::Start(offset)).unwrap();
        f.write_all(data).unwrap();
    }

    fn range_data(res: Vec<Result<ClipboardFile, CliprdrError>>) -> Vec<u8> {
        match res.into_iter().last() {
            Some(Ok(ClipboardFile::FileContentsResponse { requested_data, .. })) => requested_data,
            other => panic!("unexpected response: {:?}", other),
        }
    }

    #[test]
    fn recopy_after_edit_refreshes_cached_size() {
        let tmp = TmpDir::new("recopy");
        let file = tmp.join("doc.bin");
        fs::write(&file, b"v1").unwrap(); // 2 bytes
        let files = vec![path_str(&file)];

        // 在全局 CLIP_FILES 上驱动公开且带保护的 `sync_files`；
        // 先重置。
        let _guard = lock_clip_files();
        clear_files();

        sync_files(&files).unwrap();
        {
            let cache = CLIP_FILES.lock();
            let idx = cache.first_file_index;
            assert_eq!(cache.file_list[idx].size, 2);
        }

        // 原地编辑会让文件变大；重新复制时必须重建。修复之前，
        // 只检查路径的保护会提前返回，导致缓存的大小停留在 2。
        fs::write(&file, b"v2 is bigger").unwrap(); // 12 bytes
        sync_files(&files).unwrap();
        {
            let cache = CLIP_FILES.lock();
            let idx = cache.first_file_index;
            assert_eq!(cache.file_list[idx].size, 12);
        }

        clear_files(); // leave the global clean for other tests
    }

    #[test]
    fn range_request_decodes_wire_fields_as_u32() {
        let tmp = TmpDir::new("decode");
        let small = tmp.join("small.bin");
        fs::write(&small, b"0123456789").unwrap();
        let large = tmp.join("large.bin");
        write_at(&large, 0x8000_0000, b"tail");

        let _guard = lock_clip_files();
        clear_files();
        sync_files(&[path_str(&small), path_str(&large)]).unwrap();
        let small_idx = CLIP_FILES.lock().first_file_index as i32;
        let large_idx = small_idx + 1;

        // 负的 cbRequested 是 UINT32 长度，会被钳制到文件的剩余长度。
        let res = read_file_contents(0, 0, small_idx, 0x2, 1, 0, -1, None);
        assert_eq!(range_data(res), b"123456789");

        // nPositionLow 的第 31 位被置位表示偏移超过 2 GiB，而不是一个巨大的值。
        let res = read_file_contents(0, 0, large_idx, 0x2, 0x8000_0000u32 as i32, 0, 4, None);
        assert_eq!(range_data(res), b"tail");

        clear_files();
    }

    #[test]
    fn range_request_over_limit_is_rejected() {
        let tmp = TmpDir::new("limit");
        let file = tmp.join("big.bin");
        let size = MAX_RANGE_READ + 1;
        write_at(&file, size - 1, b"x");
        let files = vec![path_str(&file)];

        let mut clip = ClipFiles::default();
        clip.sync_files(&files, fingerprint(&files)).unwrap();
        let file_idx = clip.first_file_index;

        let res = clip.serve_file_contents(
            0,
            FileContentsRequest::Range {
                stream_id: 0,
                file_idx,
                offset: 0,
                length: u32::MAX as u64,
            },
        );
        assert!(matches!(res, Err(CliprdrError::InvalidRequest { .. })));
    }

    #[test]
    fn host_copy_during_transfer_keeps_serving_the_sent_list() {
        let tmp = TmpDir::new("recopy_in_flight");
        let first = tmp.join("first.bin");
        fs::write(&first, b"AAAAAAAA").unwrap();
        let second = tmp.join("second.bin");
        fs::write(&second, b"BBBBBBBB").unwrap();

        let _guard = lock_clip_files();
        clear_files();
        sync_files(&[path_str(&first)]).unwrap();
        get_file_list_pdu(1);
        let idx = CLIP_FILES.lock().first_file_index as i32;
        let read = |offset| range_data(read_file_contents(1, 7, idx, 0x2, offset, 0, 4, None));
        assert_eq!(read(0), b"AAAA");

        // 在这一侧复制另一个文件，绝不能把对端的数据流改道。
        sync_files(&[path_str(&second)]).unwrap();
        assert_eq!(read(4), b"AAAA");

        clear_files();
    }

    #[test]
    fn stream_keeps_its_list_after_a_second_paste() {
        let tmp = TmpDir::new("repaste_in_flight");
        let first = tmp.join("first.bin");
        fs::write(&first, b"AAAAAAAA").unwrap();
        let second = tmp.join("second.bin");
        fs::write(&second, b"BBBBBBBB").unwrap();

        let _guard = lock_clip_files();
        clear_files();
        sync_files(&[path_str(&first)]).unwrap();
        let first_id = file_list_id(&get_file_list_pdu(1));
        let idx = CLIP_FILES.lock().first_file_index as i32;

        // 对端在这一侧复制了第二个文件并粘贴，而此时第一个
        // 传输仍在进行中。
        sync_files(&[path_str(&second)]).unwrap();
        let second_id = file_list_id(&get_file_list_pdu(1));
        assert_ne!(first_id, second_id);

        let read = |id| range_data(read_file_contents(1, 7, idx, 0x2, 4, 0, 4, Some(id)));
        assert_eq!(read(first_id), b"AAAA");
        assert_eq!(read(second_id), b"BBBB");

        clear_files();
    }

    #[test]
    fn named_list_must_have_been_sent_to_the_conn() {
        let tmp = TmpDir::new("foreign_list");
        let first = tmp.join("first.bin");
        fs::write(&first, b"AAAAAAAA").unwrap();
        let second = tmp.join("second.bin");
        fs::write(&second, b"BBBBBBBB").unwrap();

        let _guard = lock_clip_files();
        clear_files();
        sync_files(&[path_str(&first)]).unwrap();
        let first_id = file_list_id(&get_file_list_pdu(1));
        let idx = CLIP_FILES.lock().first_file_index as i32;
        sync_files(&[path_str(&second)]).unwrap();

        let refused = |conn_id, id| {
            matches!(
                read_file_contents(conn_id, 7, idx, 0x2, 0, 0, 4, Some(id)).last(),
                Some(Err(CliprdrError::InvalidRequest { .. }))
            )
        };
        // 已退役的列表，被一个从未收到过它的连接询问。
        assert!(refused(2, first_id));
        // 这一侧从未发送过的列表 id。
        assert!(refused(1, first_id ^ 1));

        clear_files();
    }

    fn list_files(selection: &[&PathBuf]) -> ClipFiles {
        let files: Vec<String> = selection.iter().map(|p| path_str(p)).collect();
        let mut clip = ClipFiles::default();
        clip.sync_files(&files, fingerprint(&files)).unwrap();
        clip
    }

    fn read_head(clip: &mut ClipFiles, file_idx: usize) -> Result<Vec<u8>, CliprdrError> {
        let request = FileContentsRequest::Range {
            stream_id: 0,
            file_idx,
            offset: 0,
            length: 4,
        };
        match clip.serve_file_contents(0, request)? {
            ClipboardFile::FileContentsResponse { requested_data, .. } => Ok(requested_data),
            other => panic!("unexpected response: {:?}", other),
        }
    }

    // 预加载被请求文件之后的那个文件属于尽力而为：某个已无法
    // 打开的文件只会让自己的请求失败，而不会影响它前一个文件的请求。
    fn assert_only_its_own_requests_fail(clip: &mut ClipFiles, broken: &PathBuf) {
        let first_idx = clip.first_file_index;
        let broken_idx = clip
            .file_list
            .iter()
            .position(|f| &f.path == broken)
            .unwrap();
        let size = FileContentsRequest::Size {
            stream_id: 0,
            file_idx: first_idx,
        };
        clip.serve_file_contents(0, size).unwrap();
        assert_eq!(read_head(clip, first_idx).unwrap(), b"AAAA");
        assert!(matches!(
            read_head(clip, broken_idx),
            Err(CliprdrError::FileError { .. })
        ));
    }

    #[test]
    fn a_deleted_next_file_fails_only_its_own_requests() {
        let tmp = TmpDir::new("next_deleted");
        let (first, second) = (tmp.join("first.bin"), tmp.join("second.bin"));
        fs::write(&first, b"AAAAAAAA").unwrap();
        fs::write(&second, b"BBBBBBBB").unwrap();
        let mut clip = list_files(&[&first, &second]);

        fs::remove_file(&second).unwrap();
        assert_only_its_own_requests_fail(&mut clip, &second);
    }

    #[test]
    fn the_next_file_is_preloaded_past_a_directory() {
        let tmp = TmpDir::new("next_preloaded");
        let first = tmp.join("first.bin");
        fs::write(&first, b"AAAAAAAA").unwrap();
        let dir = tmp.join("dir");
        fs::create_dir(&dir).unwrap();
        let inner = dir.join("inner.bin");
        fs::write(&inner, b"BBBBBBBB").unwrap();
        // 列出为 first.bin、dir、dir/inner.bin。
        let mut clip = list_files(&[&first, &dir]);
        let first_idx = clip.first_file_index;
        let inner_idx = clip.file_list.len() - 1;
        assert_eq!(clip.file_list[inner_idx].path, inner);

        assert_eq!(read_head(&mut clip, first_idx).unwrap(), b"AAAA");
        assert!(clip.file_list[inner_idx].handle.is_some());
        // 最后一个文件没有下一个文件可预加载。
        assert_eq!(read_head(&mut clip, inner_idx).unwrap(), b"BBBB");
    }
}
