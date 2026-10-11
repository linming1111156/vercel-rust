use serde_json::json;

use crate::config::Config;

const ADD_URL_API: &str = "https://oooo.serv00.net/add-url";

/// 自动保活：把订阅地址登记到第三方保活服务（对应 index.js 的 addAccessTask）。
pub async fn register(config: &Config, client: &reqwest::Client) {
    if !config.auto_access || config.domain.is_empty() {
        return;
    }

    let full_url = format!("https://{}/{}", config.domain, config.sub_path);
    let response = client
        .post(ADD_URL_API)
        .json(&json!({ "url": full_url }))
        .send()
        .await
        .and_then(|response| response.error_for_status());

    match response {
        Ok(_) => tracing::info!("Automatic Access Task added successfully"),
        Err(error) => tracing::debug!("[Access] 保活任务添加失败: {error}"),
    }
}
