#![allow(non_snake_case)]

use hbb_common::{bail, ResultType};
use std::{io, ptr::null_mut};
use winapi::{
    shared::{
        minwindef::{BOOL, DWORD, FALSE, LPBYTE, LPDWORD},
        ntdef::{LPCWSTR, LPWSTR},
    },
    um::winbase::{lstrcmpiW, lstrlenW},
};
use windows_strings::PCWSTR;

mod driver;
mod port;
pub(crate) mod printer;
pub(crate) mod setup;

#[inline]
pub fn is_rd_printer_installed(app_name: &str) -> ResultType<bool> {
    let printer_name = crate::get_printer_name(app_name);
    let rd_printer_name = PCWSTR::from_raw(printer_name.as_ptr());
    printer::is_printer_added(&rd_printer_name)
}

fn get_wstr_bytes(p: LPWSTR) -> Vec<u16> {
    let mut vec_bytes = vec![];
    unsafe {
        let len: isize = lstrlenW(p) as _;
        if len > 0 {
            for i in 0..len + 1 {
                vec_bytes.push(*p.offset(i));
            }
        }
    }
    vec_bytes
}

fn is_name_equal(name: &PCWSTR, name_from_api: LPCWSTR) -> bool {
    // https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-lstrcmpiw
    // 对某些区域设置，lstrcmpi 函数可能不够用。
    // 如果出现这种情况，请使用 `CompareStringEx` 以确保比较正确。
    // 例如在日本，应带上 NORM_IGNORECASE、NORM_IGNOREKANATYPE 和 NORM_IGNOREWIDTH 这几个值来调用，以获得最合适的非精确字符串比较。
    // 请注意，指定这些值会降低性能，因此只在必要时使用。
    //
    //  目前无需考虑 `CompareStringEx`。
    unsafe { lstrcmpiW(name.as_ptr(), name_from_api) == 0 }
}

fn common_enum<T, R: Sized>(
    enum_name: &str,
    enum_fn: fn(
        Level: DWORD,
        pDriverInfo: LPBYTE,
        cbBuf: DWORD,
        pcbNeeded: LPDWORD,
        pcReturned: LPDWORD,
    ) -> BOOL,
    level: DWORD,
    on_data: impl Fn(&T) -> Option<R>,
    on_no_data: impl Fn() -> Option<R>,
) -> ResultType<Option<R>> {
    let mut needed = 0;
    let mut returned = 0;
    enum_fn(level, null_mut(), 0, &mut needed, &mut returned);
    if needed == 0 {
        return Ok(on_no_data());
    }

    let mut buffer = vec![0u8; needed as usize];
    if FALSE
        == enum_fn(
            level,
            buffer.as_mut_ptr(),
            needed,
            &mut needed,
            &mut returned,
        )
    {
        bail!(
            "Failed to call {}, error: {}",
            enum_name,
            io::Error::last_os_error()
        )
    }

    // 待办：如何释放 *const T 中的缓冲区？

    let p_enum_info = buffer.as_ptr() as *const T;
    unsafe {
        for i in 0..returned {
            let enum_info = p_enum_info.offset(i as isize);
            let r = on_data(&*enum_info);
            if r.is_some() {
                return Ok(r);
            }
        }
    }

    Ok(on_no_data())
}
