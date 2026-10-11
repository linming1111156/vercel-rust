use std::sync::Arc;

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequest, Request, State};
use axum::http::{StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::sink::SinkExt;

use crate::app::AppState;
use crate::proxy;
use crate::subscription::subscription_body;

/// 伪装页，构建时内嵌；运行时同目录下的 index.html 优先。
const INDEX_HTML: &str = include_str!("../index.html");
const FALLBACK_PAGE: &str = "Hello world!";
const MAX_WS_MESSAGE: usize = 4 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    let sub_route = format!("/{}", state.config.sub_path);
    let mut router = Router::new()
        .route("/", get(index_handler))
        .fallback(fallback_handler);
    if sub_route != "/" {
        router = router.route(&sub_route, get(subscription_handler));
    }
    router.with_state(state)
}

async fn index_handler(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if let Some(response) = close_upgrade(request, &state).await {
        return response;
    }
    let content = std::env::current_dir()
        .ok()
        .and_then(|dir| std::fs::read_to_string(dir.join("index.html")).ok())
        .unwrap_or_else(|| INDEX_HTML.to_string());
    let content = if content.is_empty() {
        FALLBACK_PAGE.to_string()
    } else {
        content
    };
    ([(CONTENT_TYPE, "text/html; charset=utf-8")], content).into_response()
}

async fn subscription_handler(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if let Some(response) = close_upgrade(request, &state).await {
        return response;
    }
    subscription_response(&state).await
}

async fn subscription_response(state: &Arc<AppState>) -> Response {
    let body = subscription_body(&state.config, &state.http).await;
    ([(CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

/// 未匹配的路径：WebSocket 升级走代理，其余返回 404（与 index.js 的路由顺序一致）。
async fn fallback_handler(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let path = request.uri().path().to_string();
    if !is_websocket_upgrade(request.headers()) {
        return not_found();
    }

    let Ok(upgrade) = WebSocketUpgrade::from_request(request, &state).await else {
        return not_found();
    };

    let ws_path = format!("/{}", state.config.ws_path);
    if path.starts_with(&ws_path) {
        return upgrade
            .max_message_size(MAX_WS_MESSAGE)
            .on_upgrade(move |socket| proxy::handle_socket(socket, state));
    }

    // 路径不匹配时按 index.js 的行为：接受连接后立即关闭。
    close_upgrade_response(upgrade)
}

/// 非代理路径上的 WebSocket 升级：接受后立即关闭（与 index.js 的 wss 行为一致）。
async fn close_upgrade(request: Request, state: &Arc<AppState>) -> Option<Response> {
    if !is_websocket_upgrade(request.headers()) {
        return None;
    }
    match WebSocketUpgrade::from_request(request, state).await {
        Ok(upgrade) => Some(close_upgrade_response(upgrade)),
        Err(_) => Some(not_found()),
    }
}

fn close_upgrade_response(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |mut socket| async move {
        let _ = socket.close().await;
    })
}

fn is_websocket_upgrade(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Not Found\n",
    )
        .into_response()
}
