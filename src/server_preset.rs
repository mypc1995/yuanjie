//! 品牌化构建时预置的应用名与服务器配置。
//!
//! 内容来自 `src/server_preset.txt`, 默认是空的 —— 普通编译出来的客户端跟上游
//! 没有区别。品牌化构建 (`.deploy/builder/build_job.py` 或
//! `.deploy/package/client/package.ps1`) 会在编译**之前**把这个文件改写成
//! 真正的值。
//!
//! 文件格式 (两行, 任意一行都可以为空):
//!   第 1 行: 品牌化应用名, 空表示沿用默认的 "RustDesk"
//!   第 2 行: 服务器配置串, 空表示不预置服务器
//!
//! 配置串格式与客户端 `ServerConfig.encode()` 完全一致:
//! `base64url(json{host,relay,api,key})` 之后**整体反转**。

use hbb_common::config::Config;

/// 预置内容的原始文本, 读自 `src/server_preset.txt`。
const PRESET_RAW: &str = include_str!("server_preset.txt");

/// 拆出 (应用名, 服务器配置串) 两项, 都是去掉首尾空白后的值。
fn preset_parts() -> (&'static str, &'static str) {
    let mut lines = PRESET_RAW.lines();
    let app_name = lines.next().unwrap_or("").trim();
    let server = lines.next().unwrap_or("").trim();
    (app_name, server)
}

/// 把预置值写进本地配置, 只在该选项**还是空**的时候写。
///
/// 用户之后在设置里换成别的服务器, 不会被预置值顶回去。
pub fn apply() {
    let (app_name, preset) = preset_parts();

    // 应用名要最先设: 配置目录、日志文件名、IPC 目录都按应用名算,
    // 晚一步就会写到旧的目录里去。
    if !app_name.is_empty() {
        *hbb_common::config::APP_NAME.write().unwrap() = app_name.to_owned();
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

    #[test]
    fn preset_file_has_two_slots() {
        // 空文件 -> 两项都空 -> 不做任何预置
        let (name, server) = preset_parts();
        if PRESET_RAW.trim().is_empty() {
            assert!(name.is_empty());
            assert!(server.is_empty());
        }
    }
}
