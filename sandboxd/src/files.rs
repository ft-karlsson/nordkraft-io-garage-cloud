// files.rs — stream files in and out of the sandbox.
//
//   PUT /v1/files?path=/abs/path[&mode=755]   body = file contents (any size)
//   GET /v1/files?path=/abs/path               body = file contents
//
// Uploads land in a temp file next to the target and are renamed into place,
// so a reader never sees a half-written file. Parent directories are created.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::TryStreamExt;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio_util::io::{ReaderStream, StreamReader};

use crate::server::AppState;

#[derive(Debug, Deserialize)]
pub struct FileQuery {
    pub path: String,
    /// Octal permission bits for PUT, e.g. "755".
    pub mode: Option<String>,
}

fn absolute(path: &str) -> Result<PathBuf, (StatusCode, &'static str)> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err((StatusCode::BAD_REQUEST, "path must be absolute"));
    }
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err((StatusCode::BAD_REQUEST, "path must not contain .."));
    }
    Ok(p.to_path_buf())
}

pub async fn put_file(
    State(_state): State<AppState>,
    Query(q): Query<FileQuery>,
    body: Body,
) -> Response {
    let path = match absolute(&q.path) {
        Ok(p) => p,
        Err(r) => return r.into_response(),
    };
    let mode = match q.mode.as_deref().map(|m| u32::from_str_radix(m, 8)) {
        None => None,
        Some(Ok(m)) if m <= 0o7777 => Some(m),
        Some(_) => {
            return (StatusCode::BAD_REQUEST, "mode must be octal, e.g. 755").into_response()
        }
    };
    let dir = path.parent().unwrap_or(Path::new("/"));
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("mkdir {}: {e}", dir.display()),
        )
            .into_response();
    }
    let tmp = dir.join(format!(
        ".{}.nk-upload-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    let mut file = match tokio::fs::File::create(&tmp).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("create {}: {e}", tmp.display()),
            )
                .into_response()
        }
    };
    let stream = body.into_data_stream().map_err(std::io::Error::other);
    let mut reader = StreamReader::new(stream);
    let written = match tokio::io::copy(&mut reader, &mut file).await {
        Ok(n) => n,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return (StatusCode::BAD_REQUEST, format!("upload interrupted: {e}")).into_response();
        }
    };
    if let Err(e) = file.flush().await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("flush: {e}")).into_response();
    }
    drop(file);
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m)).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("chmod: {e}")).into_response();
        }
    }
    if let Err(e) = tokio::fs::rename(&tmp, &path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("rename into {}: {e}", path.display()),
        )
            .into_response();
    }
    axum::Json(serde_json::json!({ "path": path, "bytes": written })).into_response()
}

pub async fn get_file(State(_state): State<AppState>, Query(q): Query<FileQuery>) -> Response {
    let path = match absolute(&q.path) {
        Ok(p) => p,
        Err(r) => return r.into_response(),
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (StatusCode::NOT_FOUND, "no such file").into_response()
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("open: {e}")).into_response(),
    };
    let len = file.metadata().await.ok().map(|m| m.len());
    let mut resp = Body::from_stream(ReaderStream::new(file)).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    if let Some(len) = len {
        if let Ok(v) = header::HeaderValue::from_str(&len.to_string()) {
            resp.headers_mut().insert(header::CONTENT_LENGTH, v);
        }
    }
    resp
}
