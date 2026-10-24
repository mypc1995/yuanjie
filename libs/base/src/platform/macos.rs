use hbb_common::ResultType;
use osascript;
use serde_derive::{Deserialize, Serialize};

#[derive(Serialize)]
struct AlertParams {
    title: String,
    message: String,
    alert_type: String,
    buttons: Vec<String>,
}

#[derive(Deserialize)]
struct AlertResult {
    #[serde(rename = "buttonReturned")]
    button: String,
}

/// 先运行指定的 app，然后弹出一个对话框。返回被点击按钮的值。
///
/// # 参数
///
/// * `app` - 要执行脚本的 app。
/// * `alert_type` - 警告类型。 . informational、warning、critical
/// * `title` - 警告标题。
/// * `message` - 警告内容。
/// * `buttons` - 要显示的按钮。
pub fn alert(
    app: String,
    alert_type: String,
    title: String,
    message: String,
    buttons: Vec<String>,
) -> ResultType<String> {
    let script = osascript::JavaScript::new(&format!(
        "
    var App = Application('{}');
    App.includeStandardAdditions = true;
    return App.displayAlert($params.title, {{
        message: $params.message,
        'as': $params.alert_type,
        buttons: $params.buttons,
    }});
    ",
        app
    ));

    let result: AlertResult = script.execute_with_params(AlertParams {
        title,
        message,
        alert_type,
        buttons,
    })?;
    Ok(result.button)
}
