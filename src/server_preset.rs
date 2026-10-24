//! 品牌化构建时预置的应用名与服务器配置。
//!
//! 内容来自 `src/server_preset.txt`, 默认是空的 —— 普通编译出来的客户端跟上游
//! 没有区别。品牌化构建 (`.deploy/builder/build_job.py` 或
//! `.deploy/package/client/package.ps1`) 会在编译**之前**把这个文件改写成
//! 真正的值。
//!
//! 文件格式 (三行, 任意一行都可以为空):
//!   第 1 行: 显示名 —— 界面、文案里出现的品牌名, 可以是中文
//!   第 2 行: 内部名 —— 服务名、安装目录、注册表键、提权脚本里用的名字,
//!             **必须是纯 ASCII**。上游会强制校验, 因为这些名字会被插值进
//!             特权脚本和路径里 (见 `src/platform/mod.rs`
//!             的 `validate_install_app_name`)。
//!   第 3 行: 服务器配置串 —— 空表示不预置服务器
//!
//! 配置串格式与客户端 `ServerConfig.encode()` 完全一致:
//! `base64url(json{host,relay,api,key})` 之后**整体反转**。

use hbb_common::config::Config;

/// 预置内容的原始文本, 读自 `src/server_preset.txt`。
const PRESET_RAW: &str = include_str!("server_preset.txt");

/// 拆成 (显示名, 内部名, 服务器配置串) 三项, 都是去掉首尾空白后的值。
fn preset_lines() -> (&'static str, &'static str, &'static str) {
    let mut it = PRESET_RAW.lines();
    let display = it.next().unwrap_or("").trim();
    let internal = it.next().unwrap_or("").trim();
    let server = it.next().unwrap_or("").trim();
    (display, internal, server)
}

/// 界面与文案里显示的品牌名。没预置就回退到内部名。
pub fn display_app_name() -> String {
    let (display, internal, _) = preset_lines();
    if display.is_empty() {
        crate::get_app_name()
    } else {
        display.to_owned()
    }
}

/// 安装、服务、路径用的内部名。没预置就回退到当前的应用名。
///
/// 必须是纯 ASCII —— 安装流程会把它插值进提权脚本、注册表键和路径。
pub fn install_app_name() -> String {
    let (_, internal, _) = preset_lines();
    if internal.is_empty() {
        crate::get_app_name()
    } else {
        internal.to_owned()
    }
}

/// 把预置值写进本地配置, 只在该选项**还是空**的时候写。
///
/// 用户之后在设置里换成别的服务器, 不会被预置值顶回去。
pub fn apply() {
    let (_, internal, preset) = preset_lines();

    // 内部名要先设好: 配置目录、日志文件名、IPC 目录、服务名都按它算,
    // 晚一步就会写到用旧名字算出来的目录里去。
    if !internal.is_empty() {
        *hbb_common::config::APP_NAME.write().unwrap() = internal.to_owned();
    }

    if preset.is_empty() {
        return;
    }
    let Ok(cfg) = crate::custom_server::get_custom_server_from_config_string(preset) else {
        hbb_common::log::error!("Failed to parse preset server config");
        return;
    };
    set_if_empty("custom-rendezvous-server", &cfg.host);
    set_if_empty("relay-server", &cfg.relay);
    set_if_empty("api-server", &cfg.api);
    set_if_empty("key", &cfg.key);
}

fn set_if_empty(key: &str, value: &str) {
    if value.is_empty() || !Config::get_option(key).is_empty() {
        return;
    }
    Config::set_option(key.to_owned(), value.to_owned());
}

#[cfg(test)]
mod test {
    use super::*;

    // base64url({"host":"1.2.3.4","relay":"","api":"","key":"k"}), 整体反转
    const ENCODED: &str = "9JyaiojI5V2aiwiIiojIpBXYiwiIiojI5FGblJnIsICNuMjLy4SMiojI0N3boJye";

    // 预置和客户端必须用同一套编码, 否则预置会解到错误的字段上。
    #[test]
    fn preset_uses_the_client_config_encoding() {
        let cfg = crate::custom_server::get_custom_server_from_config_string(ENCODED).unwrap();
        assert_eq!(cfg.host, "1.2.3.4");
        assert_eq!(cfg.key, "k");
        assert!(cfg.relay.is_empty());
        assert!(cfg.api.is_empty());
    }

    // 内部名会被插值进特权脚本和路径, 必须是纯 ASCII。
    #[test]
    fn install_name_must_be_ascii() {
        let name = install_app_name();
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "内部名必须是纯 ASCII: {name}"
        );
    }
}
