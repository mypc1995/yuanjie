use super::{common_enum, get_wstr_bytes, is_name_equal};
use hbb_common::{bail, log, ResultType};
use std::{io, ptr::null_mut, time::Duration};
use winapi::{
    shared::{
        minwindef::{BOOL, DWORD, FALSE, LPBYTE, LPDWORD, MAX_PATH},
        ntdef::{DWORDLONG, LPCWSTR},
        winerror::{ERROR_UNKNOWN_PRINTER_DRIVER, S_OK},
    },
    um::{
        winspool::{
            DeletePrinterDriverExW, DeletePrinterDriverPackageW, EnumPrinterDriversW,
            InstallPrinterDriverFromPackageW, UploadPrinterDriverPackageW, DPD_DELETE_ALL_FILES,
            DRIVER_INFO_6W, DRIVER_INFO_8W, IPDFP_COPY_ALL_FILES, UPDP_SILENT_UPLOAD,
            UPDP_UPLOAD_ALWAYS,
        },
        winuser::GetForegroundWindow,
    },
};
use windows_strings::PCWSTR;

const HRESULT_ERR_ELEMENT_NOT_FOUND: u32 = 0x80070490;

fn enum_printer_driver(
    level: DWORD,
    p_driver_info: LPBYTE,
    cb_buf: DWORD,
    pcb_needed: LPDWORD,
    pc_returned: LPDWORD,
) -> BOOL {
    unsafe {
        // https://learn.microsoft.com/en-us/windows/win32/printdocs/enumprinterdrivers
        // 这是一个阻塞或同步函数，可能不会立即返回。
        // 该函数返回的快慢取决于运行时的各种因素，
        // 例如网络状态、打印服务器配置，以及编写应用程序时难以预测的打印机驱动实现方面的因素。
        // 在管理用户界面交互的线程中调用此函数，可能会让应用程序看起来没有响应。
        EnumPrinterDriversW(
            null_mut(),
            null_mut(),
            level,
            p_driver_info,
            cb_buf,
            pcb_needed,
            pc_returned,
        )
    }
}

pub fn get_installed_driver_version(name: &PCWSTR) -> ResultType<Option<DWORDLONG>> {
    common_enum(
        "EnumPrinterDriversW",
        enum_printer_driver,
        6,
        |info: &DRIVER_INFO_6W| {
            if is_name_equal(name, info.pName) {
                Some(info.dwlDriverVersion)
            } else {
                None
            }
        },
        || None,
    )
}

fn find_inf(name: &PCWSTR) -> ResultType<Vec<u16>> {
    let r = common_enum(
        "EnumPrinterDriversW",
        enum_printer_driver,
        8,
        |info: &DRIVER_INFO_8W| {
            if is_name_equal(name, info.pName) {
                Some(get_wstr_bytes(info.pszInfPath))
            } else {
                None
            }
        },
        || None,
    )?;
    Ok(r.unwrap_or(vec![]))
}

fn delete_printer_driver(name: &PCWSTR) -> ResultType<()> {
    unsafe {
        // 如果在后台处理程序服务启动之后仍在使用该打印机。例如通过 RustDesk Printer 打印文档。
        // `DeletePrinterDriverExW()` 可能以 `ERROR_PRINTER_DRIVER_IN_USE`（3001, 0xBB9）失败。
        // 我们目前只能忽略这个错误。
        // 虽然重启后台处理程序服务是一个解决办法，但重启该服务并不是个好主意。
        //
        // 删除打印机之后再删除打印机驱动是常见做法。
        // 不清楚为什么用过一次打印机之后 `DeletePrinterDriverExW()` 会以 `ERROR_UNKNOWN_PRINTER_DRIVER` 失败。
        // https://github.com/ChromiumWebApps/chromium/blob/c7361d39be8abd1574e6ce8957c8dbddd4c6ccf7/cloud_print/virtual_driver/win/install/setup.cc#L422
        // AnyDesk 的打印机驱动以及最简单的打印机驱动也有同样的问题。
        if FALSE
            == DeletePrinterDriverExW(
                null_mut(),
                null_mut(),
                name.as_ptr() as _,
                DPD_DELETE_ALL_FILES,
                0,
            )
        {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_UNKNOWN_PRINTER_DRIVER as _) {
                return Ok(());
            } else {
                bail!("Failed to delete the printer driver, {}", err)
            }
        }
    }
    Ok(())
}

// https://github.com/dvalter/chromium-android-ext-dev/blob/dab74f7d5bc5a8adf303090ee25c611b4d54e2db/cloud_print/virtual_driver/win/install/setup.cc#L190
fn delete_printer_driver_package(inf: Vec<u16>) -> ResultType<()> {
    if inf.is_empty() {
        return Ok(());
    }
    let slen = if inf[inf.len() - 1] == 0 {
        inf.len() - 1
    } else {
        inf.len()
    };
    let inf_path = String::from_utf16_lossy(&inf[..slen]);
    if !std::path::Path::new(&inf_path).exists() {
        return Ok(());
    }

    let mut retries = 3;
    loop {
        unsafe {
            let res = DeletePrinterDriverPackageW(null_mut(), inf.as_ptr(), null_mut());
            if res == S_OK || res == HRESULT_ERR_ELEMENT_NOT_FOUND as i32 {
                return Ok(());
            }
            log::error!("Failed to delete the printer driver, result: {}", res);
        }
        retries -= 1;
        if retries <= 0 {
            bail!("Failed to delete the printer driver");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

pub fn uninstall_driver(name: &PCWSTR) -> ResultType<()> {
    // 注意：必须在 `delete_printer_driver()` 之前找到 inf。
    let inf = find_inf(name)?;
    delete_printer_driver(name)?;
    delete_printer_driver_package(inf)
}

pub fn install_driver(name: &PCWSTR, inf: LPCWSTR) -> ResultType<()> {
    let mut size = (MAX_PATH * 10) as u32;
    let mut package_path = [0u16; MAX_PATH * 10];
    unsafe {
        let mut res = UploadPrinterDriverPackageW(
            null_mut(),
            inf,
            null_mut(),
            UPDP_SILENT_UPLOAD | UPDP_UPLOAD_ALWAYS,
            null_mut(),
            package_path.as_mut_ptr(),
            &mut size as _,
        );
        if res != S_OK {
            log::error!(
                "Failed to upload the printer driver package to the driver cache silently, {}. Will try with user UI.",
                res
            );

            res = UploadPrinterDriverPackageW(
                null_mut(),
                inf,
                null_mut(),
                UPDP_UPLOAD_ALWAYS,
                GetForegroundWindow(),
                package_path.as_mut_ptr(),
                &mut size as _,
            );
            if res != S_OK {
                bail!(
                    "Failed to upload the printer driver package to the driver cache with UI, {}",
                    res
                );
            }
        }

        // https://learn.microsoft.com/en-us/windows/win32/printdocs/installprinterdriverfrompackage
        res = InstallPrinterDriverFromPackageW(
            null_mut(),
            package_path.as_ptr(),
            name.as_ptr(),
            null_mut(),
            IPDFP_COPY_ALL_FILES,
        );
        if res != S_OK {
            bail!("Failed to install the printer driver from package, {}", res);
        }
    }

    Ok(())
}
