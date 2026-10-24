use dbus::blocking::Connection;
use gtk::{
    gio::{self, prelude::SettingsExt},
    glib::variant::FromVariant,
};
use hbb_common::{anyhow::anyhow, bail, ResultType};
use std::{error::Error, ffi::CString, fmt, time::Duration};

mod ibus;

pub(super) const DBUS_TIMEOUT: Duration = Duration::from_millis(250);
const GNOME_INPUT_SCHEMA: &str = "org.gnome.desktop.input-sources";
const GNOME_DEFAULT_LAYOUT: &str = "us";

#[derive(Debug)]
pub(super) struct UnreliableSource(pub &'static str);

impl fmt::Display for UnreliableSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for UnreliableSource {}

#[derive(PartialEq, Eq)]
pub(super) enum Source {
    Gnome(Names),
    Wayland { keymap: Vec<u8>, group: Option<u32> },
    Xwayland { keymap: Vec<u8>, group: Option<u32> },
}

#[derive(PartialEq, Eq)]
pub(super) struct Names {
    pub model: CString,
    pub layout: CString,
    pub variant: CString,
    pub options: CString,
}

impl Source {
    pub fn read() -> ResultType<Self> {
        let connection = Connection::new_session()?;
        if name_has_owner(&connection, "org.gnome.Shell")? {
            if let Some(names) = gnome_layout()? {
                return Ok(Self::Gnome(names));
            }
            return Ok(Self::Wayland {
                keymap: super::wayland::read_keymap()?,
                group: None,
            });
        }
        let kde_layouts = name_has_owner(&connection, "org.kde.keyboard")?;
        // KWin 5 不会为单布局映射发布布局服务。
        if kde_layouts || name_has_owner(&connection, "org.kde.KWin")? {
            return Ok(Self::Wayland {
                keymap: super::wayland::read_keymap()?,
                group: if kde_layouts {
                    Some(kde_layout_group(&connection)?)
                } else {
                    None
                },
            });
        }
        // 其他合成器需要我们 uinput 设备的有效键盘映射和活动组，
        // 包括按设备的选项以及自定义键盘映射覆盖。
        // wl_keyboard 暴露的 seat 映射可能属于某个物理键盘。
        // 相同的布局名/组索引并不能证明映射是同一个：Caps 在一个映射中
        // 可能充当 AltGr，而在另一个映射中却是切换 CapsLock。
        //
        // Sway 的 GET_INPUTS/GET_SEATS 只暴露名称和索引，不暴露设备映射。
        // GET_CONFIG 返回的是已保存的配置文本，而不是运行时有效的输入
        // 设置；解析它无法可靠地涵盖运行时的覆盖项。
        // Hyprland 的 devices reply 会报告 RMLVO（rules/model/layout/variant/options），
        // 但 kb_file 可以覆盖它。仅凭这些字段无法重建
        // 已加载的映射。但这并不排除存在其他合成器专有的 API。
        //
        // 可靠的原生发现需要一个合成器查询，把设备的实际映射（或经过验证的有效配置）
        // 与其组关联起来。
        // uinput 只提供键码；它无法报告合成器的 XKB 策略。
        // Xwayland 仍然是尽力而为；其时效性限制参见 xwayland::read。
        // zwp_virtual_keyboard_manager_v1 需要单独的注入后端，并且
        // 并非普遍支持；它也无法查询已有的 uinput 映射。
        super::xwayland::read()
    }
}

fn name_has_owner(connection: &Connection, name: &str) -> ResultType<bool> {
    let proxy = connection.with_proxy(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        DBUS_TIMEOUT,
    );
    let (owned,): (bool,) = proxy.method_call("org.freedesktop.DBus", "NameHasOwner", (name,))?;
    Ok(owned)
}

fn kde_layout_group(connection: &Connection) -> ResultType<u32> {
    let proxy = connection.with_proxy("org.kde.keyboard", "/Layouts", DBUS_TIMEOUT);
    let (group,): (u32,) = proxy.method_call("org.kde.KeyboardLayouts", "getLayout", ())?;
    Ok(group)
}

fn gnome_layout() -> ResultType<Option<Names>> {
    let schemas = gio::SettingsSchemaSource::default()
        .ok_or_else(|| anyhow!("GSettings schemas are unavailable"))?;
    let schema = schemas
        .lookup(GNOME_INPUT_SCHEMA, true)
        .ok_or_else(|| anyhow!("GNOME input source schema is unavailable"))?;
    let settings = gio::Settings::new_full(&schema, gio::SettingsBackend::NONE, None);
    let sources: Vec<(String, String)> = required_setting(&settings, "sources")?;
    // 焦点变化会恢复窗口的输入源，但不会持久化 MRU。把它标记为
    // 不可靠，这样刷新也会丢弃此前已接受的 MRU 映射。
    if sources.len() > 1 && required_setting::<bool>(&settings, "per-window")? {
        return Err(UnreliableSource(
            "GNOME per-window input sources cannot be determined from persisted MRU",
        )
        .into());
    }
    let recent: Vec<(String, String)> = required_setting(&settings, "mru-sources")?;
    // MRU 为空时，GNOME 初始会选择第一个已配置的输入源。
    // IBus 读取器在接受持久化的 MRU 之前会校验当前引擎。
    let selected = recent
        .iter()
        .find(|source| sources.contains(source))
        .or(sources.first());
    let Some((layout, variant)) = gnome_source_layout(selected)? else {
        // 没有 XKB 覆盖的 IBus 引擎使用合成器的键盘映射。
        return Ok(None);
    };
    if layout.is_empty() {
        bail!("GNOME input source has an empty layout");
    }
    // 与 GNOME Shell 的会话级选项保持一致；它不会把 IBus 的
    // 引擎布局选项元数据合并进合成器键盘映射。
    let options: Vec<String> = required_setting(&settings, "xkb-options")?;
    // 较旧的 GNOME 版本使用默认模型，并且没有 xkb-model 设置。
    let model: String = if schema.has_key("xkb-model") {
        required_setting(&settings, "xkb-model")?
    } else {
        String::new()
    };
    Ok(Some(Names {
        model: CString::new(model)?,
        layout: CString::new(layout)?,
        variant: CString::new(variant)?,
        options: CString::new(options.join(","))?,
    }))
}

fn gnome_source_layout(
    selected: Option<&(String, String)>,
) -> ResultType<Option<(String, String)>> {
    let Some((kind, id)) = selected else {
        // 未配置任何输入源时，GNOME Shell 会选择该布局。
        return Ok(Some((GNOME_DEFAULT_LAYOUT.to_owned(), String::new())));
    };
    match kind.as_str() {
        "xkb" => {
            let (layout, variant) = id.split_once('+').unwrap_or((id.as_str(), ""));
            Ok(Some((layout.to_owned(), variant.to_owned())))
        }
        "ibus" => ibus::layout(id),
        _ => bail!("Unsupported GNOME input source type: {}", kind),
    }
}

fn required_setting<T: FromVariant>(settings: &gio::Settings, key: &str) -> ResultType<T> {
    if !settings
        .settings_schema()
        .is_some_and(|schema| schema.has_key(key))
    {
        bail!("Missing GNOME setting {}", key);
    }
    settings
        .value(key)
        .get()
        .ok_or_else(|| anyhow!("Invalid GNOME setting {}", key))
}
