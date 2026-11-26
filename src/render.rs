use crate::models::MonitorSnapshot;

const HTML_TEMPLATE: &str = include_str!("../templates/dashboard.html");

/// Render a self-contained HTML dashboard with safely escaped JSON payload.
pub fn render_html(snapshot: &MonitorSnapshot) -> Result<String, serde_json::Error> {
    let payload = serde_json::to_string(snapshot)?.replace("</", "<\\/");
    Ok(HTML_TEMPLATE.replace("__DATA__", &payload))
}
