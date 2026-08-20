use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::state::AppState;

#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/web/dist"]
struct Assets;

pub async fn serve(State(state): State<Arc<AppState>>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path == "index.html" {
        return index_response(&state);
    }
    if let Some(file) = Assets::get(path) {
        return file_response(path, file, path.starts_with("assets/"));
    }
    // SPA fallback: /share (and anything else non-API) renders index.html.
    index_response(&state)
}

/// index.html with the client id injected so the Discord SDK can be
/// constructed synchronously at module scope — the handshake must fire as
/// early as possible or the client may never answer with READY.
fn index_response(state: &AppState) -> Response {
    let Some(file) = Assets::get("index.html") else {
        return (StatusCode::NOT_FOUND, "frontend not embedded").into_response();
    };
    let html = String::from_utf8_lossy(&file.data).into_owned();
    let tag = format!(
        "<head><script>window.__NMPJ_CLIENT_ID={:?}</script>",
        state.cfg.discord.client_id
    );
    let html = html.replacen("<head>", &tag, 1);
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CACHE_CONTROL, "no-cache".to_string()),
        ],
        html,
    )
        .into_response()
}

fn file_response(path: &str, file: rust_embed::EmbeddedFile, immutable: bool) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // Vite content-hashes everything under assets/, so those can cache forever.
    let cache = if immutable { "public, max-age=31536000, immutable" } else { "no-cache" };
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
        ],
        file.data.into_owned(),
    )
        .into_response()
}
