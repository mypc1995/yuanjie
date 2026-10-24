use super::{
    driver::{get_installed_driver_version, install_driver, uninstall_driver},
    port::{check_add_local_port, check_delete_local_port},
    printer::{add_printer, delete_printer},
};
use hbb_common::{allow_err, bail, lazy_static, log, ResultType};
use std::{path::PathBuf, sync::Mutex};
use windows_strings::PCWSTR;

lazy_static::lazy_static!(
    static ref SETUP_MTX: Mutex<()> = Mutex::new(());
);

fn get_driver_inf_abs_path() -> ResultType<PathBuf> {
    use crate::RD_DRIVER_INF_PATH;

    let exe_file = std::env::current_exe()?;
    let abs_path = match exe_file.parent() {
        Some(parent) => parent.join(RD_DRIVER_INF_PATH),
        None => bail!(
            "Invalid exe parent for {}",
            exe_file.to_string_lossy().as_ref()
        ),
    };
    if !abs_path.exists() {
        bail!(
            "The driver inf file \"{}\" does not exists",
            RD_DRIVER_INF_PATH
        )
    }
    Ok(abs_path)
}

// 注意：此函数必须在单独的线程中调用。
// 因为本模块中的许多函数是阻塞或同步的。
// 在管理用户界面交互的线程中调用此函数，可能会让应用程序看起来没有响应。
// 步骤：
// 1. 添加本地端口。
// 2. 检查驱动是否已安装。
//    如果已安装，则卸载现有驱动。
//    我们不应该检查驱动版本，因为驱动是随应用程序一起部署的。
//    更好的做法是卸载现有驱动，然后安装应用程序自带的驱动。
// 3. 添加打印机。
pub fn install_update_printer(app_name: &str) -> ResultType<()> {
    let printer_name = crate::get_printer_name(app_name);
    let driver_name = crate::get_driver_name();
    let port = crate::get_port_name(app_name);
    let rd_printer_name = PCWSTR::from_raw(printer_name.as_ptr());
    let rd_printer_driver_name = PCWSTR::from_raw(driver_name.as_ptr());
    let rd_printer_port = PCWSTR::from_raw(port.as_ptr());

    let inf_file = get_driver_inf_abs_path()?;
    let inf_file: Vec<u16> = inf_file
        .to_string_lossy()
        .as_ref()
        .encode_utf16()
        .chain(Some(0).into_iter())
        .collect();
    let _lock = SETUP_MTX.lock().unwrap();

    check_add_local_port(&rd_printer_port)?;

    let should_install_driver = match get_installed_driver_version(&rd_printer_driver_name)? {
        Some(_version) => {
            delete_printer(&rd_printer_name)?;
            allow_err!(uninstall_driver(&rd_printer_driver_name));
            true
        }
        None => true,
    };

    if should_install_driver {
        allow_err!(install_driver(&rd_printer_driver_name, inf_file.as_ptr()));
    }

    add_printer(&rd_printer_name, &rd_printer_driver_name, &rd_printer_port)?;

    Ok(())
}

pub fn uninstall_printer(app_name: &str) {
    let printer_name = crate::get_printer_name(app_name);
    let driver_name = crate::get_driver_name();
    let port = crate::get_port_name(app_name);
    let rd_printer_name = PCWSTR::from_raw(printer_name.as_ptr());
    let rd_printer_driver_name = PCWSTR::from_raw(driver_name.as_ptr());
    let rd_printer_port = PCWSTR::from_raw(port.as_ptr());

    let _lock = SETUP_MTX.lock().unwrap();

    allow_err!(delete_printer(&rd_printer_name));
    allow_err!(uninstall_driver(&rd_printer_driver_name));
    allow_err!(check_delete_local_port(&rd_printer_port));
}
