//! 品牌定制构建的服务端配置预设。
//!
//! 默认为空，因此普通构建的行为与上游完全一致。
//! 品牌定制构建（`.deploy/builder/build_job.py` 或
//! `.deploy/package/client/package.ps1`）会用真实的配置字符串重写 `src/server_preset.txt`，
//! 这一步在*编译之前*完成。

use hbb_common::config::Config;

/// 预设的配置字符串，从 `src/server_preset.txt` 读取。
///
/// 与客户端的 `ServerConfig.encode()` 采用相同编码：
/// `base64url(json{host,relay,api,key})`，并把整个字符串反转。
pub const PRESET_SERVER_CONFIG: &str = include_str!("server_preset.txt");

/// 把预设写入本地配置，但仅当该选项
/// 仍为空时。
///
/// 之后把客户端指向另一台服务器的用户会保留那个选择；
/// 预设永远不会覆盖它。
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

    // base64url({"host":"1.2.3.4","relay":"","api":"","key":"k"})，反转
    const ENCODED: &str = "9JyaiojI5V2aiwiIiojIpBXYiwiIiojI5FGblJnIsICNuMjLy4SMiojI0N3boJye";

    // 预设与客户端必须在编码上一致，否则
    // 预设会被解析到错误的字段。
    #[test]
    fn preset_uses_the_client_config_encoding() {
        let cfg = crate::custom_server::get_custom_server_from_config_string(ENCODED).unwrap();
        assert_eq!(cfg.host, "1.2.3.4");
        assert_eq!(cfg.key, "k");
        assert!(cfg.relay.is_empty());
        assert!(cfg.api.is_empty());
    }
}
