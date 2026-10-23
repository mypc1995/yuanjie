//! 品牌化构建时预置的服务器配置。
//!
//! 默认是空的 —— 普通编译出来的客户端跟上游没有区别。
//! 品牌化构建 (`.deploy/builder/build_job.py` 或 `.deploy/package/client/package.ps1`)
//! 会在编译**之前**把 `src/server_preset.txt` 改写成真正的配置串。

use hbb_common::config::Config;

/// 预置的服务器配置串, 内容来自 `src/server_preset.txt`。
///
/// 格式与客户端 `ServerConfig.encode()` 完全一致:
/// `base64url(json{host,relay,api,key})` 之后**整体反转**。
pub const PRESET_SERVER_CONFIG: &str = include_str!("server_preset.txt");

/// 把预置配置写进本地配置, 只在该选项**还是空**的时候写。
///
/// 用户之后在设置里换成别的服务器, 不会被预置值顶回去。
pub fn apply() {
    let preset = PRESET_SERVER_CONFIG.trim();
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

    // base64url({"host":"1.2.3.4","relay":"","api":"","key":"k"}) 整体反转
    const ENCODED: &str = "9JyaiojI5V2aiwiIiojIpBXYiwiIiojI5FGblJnIsICNuMjLy4SMiojI0N3boJye";

    #[test]
    fn preset_uses_the_client_config_encoding() {
        let cfg = crate::custom_server::get_custom_server_from_config_string(ENCODED).unwrap();
        assert_eq!(cfg.host, "1.2.3.4");
        assert_eq!(cfg.key, "k");
        assert!(cfg.relay.is_empty());
        assert!(cfg.api.is_empty());
    }

    #[test]
    fn empty_preset_does_nothing() {
        let preset = PRESET_SERVER_CONFIG.trim();
        if preset.is_empty() {
            // 空预置是默认状态, 不该写任何选项
            assert_eq!(preset, "");
        }
    }
}
