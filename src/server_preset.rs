//! Server configuration preset for branded builds.
//!
//! Empty by default, so an ordinary build behaves exactly like upstream.
//! A branded build (`.deploy/builder/build_job.py` or
//! `.deploy/package/client/package.ps1`) rewrites `src/server_preset.txt` with
//! the real config string *before* compiling.

use hbb_common::config::Config;

/// The preset config string, read from `src/server_preset.txt`.
///
/// Same encoding as the client's `ServerConfig.encode()`:
/// `base64url(json{host,relay,api,key})` with the whole string reversed.
pub const PRESET_SERVER_CONFIG: &str = include_str!("server_preset.txt");

/// Writes the preset into the local config, but only while the option is
/// still empty.
///
/// A user who later points the client at another server keeps that choice;
/// the preset never overrides it.
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

    // base64url({"host":"1.2.3.4","relay":"","api":"","key":"k"}), reversed
    const ENCODED: &str = "9JyaiojI5V2aiwiIiojIpBXYiwiIiojI5FGblJnIsICNuMjLy4SMiojI0N3boJye";

    // The preset and the client must agree on the encoding, otherwise the
    // preset decodes into the wrong fields.
    #[test]
    fn preset_uses_the_client_config_encoding() {
        let cfg = crate::custom_server::get_custom_server_from_config_string(ENCODED).unwrap();
        assert_eq!(cfg.host, "1.2.3.4");
        assert_eq!(cfg.key, "k");
        assert!(cfg.relay.is_empty());
        assert!(cfg.api.is_empty());
    }
}
