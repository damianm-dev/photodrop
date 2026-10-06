use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use axum::Router;
use axum::extract::multipart::MultipartError;
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{get, post};
use tokio::io::AsyncWriteExt;

/// Builds the application router; `dir` is where uploads will be saved.
fn app(dir: PathBuf) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/upload", post(upload).layer(DefaultBodyLimit::disable()))
        .with_state(dir)
}

/// Returns the on-disk name for an upload's `attempt`-th collision candidate.
///
/// Uses the part of `client_name` after the last `/` or `\`, case kept.
/// Attempt 0 is the name itself; attempt `n` inserts ` (n)` before the last
/// `.` (a leading dot does not start an extension). Falls back to
/// `upload.bin` (suffixed the same way) if the name is missing, empty, `.`, `..`, contains control
/// characters, or the result would exceed 255 bytes.
fn save_name(client_name: Option<&str>, attempt: u32) -> String {
    client_name
        .map(|name| name.rsplit_once(['/', '\\']).map_or(name, |(_, base)| base))
        .filter(|base| !matches!(*base, "" | "." | "..") && !base.chars().any(char::is_control))
        .map(|base| with_suffix(base, attempt))
        .filter(|name| name.len() <= 255)
        .unwrap_or_else(|| with_suffix("upload.bin", attempt))
}

/// Inserts ` (attempt)` before the extension of `name`; attempt 0 is `name`.
fn with_suffix(name: &str, attempt: u32) -> String {
    if attempt == 0 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({attempt}).{ext}"),
        _ => format!("{name} ({attempt})"),
    }
}

/// Serves the upload page.
async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

/// Streams the first `file` field of a multipart upload into `dir`.
async fn upload(State(dir): State<PathBuf>, multipart: Multipart) -> (StatusCode, String) {
    match save_upload(&dir, multipart).await {
        Ok(Some(name)) => {
            println!("saved {name}");
            (StatusCode::OK, format!("saved as {name}"))
        }
        Ok(None) => (StatusCode::BAD_REQUEST, "missing file field".to_string()),
        Err(err) => {
            eprintln!("upload failed: {err:#}");
            let status = err
                .downcast_ref::<MultipartError>()
                .map_or(StatusCode::INTERNAL_SERVER_ERROR, MultipartError::status);
            (status, "upload failed".to_string())
        }
    }
}

/// Writes the first `file` field to a new file in `dir` chunk by chunk.
///
/// If the name is taken, tries `name (1)`, `name (2)`, ... until one is free.
///
/// Returns the saved name, or `None` if the body has no `file` field.
async fn save_upload(dir: &Path, mut multipart: Multipart) -> anyhow::Result<Option<String>> {
    while let Some(mut field) = multipart.next_field().await? {
        if field.name() != Some("file") {
            continue;
        }
        let mut attempt = 0;
        let (name, path, mut file) = loop {
            let name = save_name(field.file_name(), attempt);
            let path = dir.join(&name);
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
            {
                Ok(file) => break (name, path, file),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => attempt += 1,
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to create {}", path.display()));
                }
            }
        };
        while let Some(chunk) = field.chunk().await? {
            file.write_all(&chunk)
                .await
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
        file.flush()
            .await
            .with_context(|| format!("failed to flush {}", path.display()))?;
        return Ok(Some(name));
    }
    Ok(None)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from("inbox");
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("failed to create {}", dir.display()))?;

    let addr = "0.0.0.0:8000";
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    println!("listening on http://{addr}");

    axum::serve(listener, app(dir))
        .await
        .context("server error")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn index_serves_upload_form() {
        let tmp = tempfile::tempdir().unwrap();
        let response = app(tmp.path().to_path_buf())
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response.headers()[header::CONTENT_TYPE].to_str().unwrap();
        assert!(content_type.starts_with("text/html"), "{content_type}");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(r#"action="/upload""#));
        assert!(body.contains(r#"enctype="multipart/form-data""#));
        assert!(body.contains(r#"name="file""#));
        assert!(body.contains(r#"name="file" multiple"#));
        assert!(body.contains(r#"id="retry""#));
    }

    #[tokio::test]
    async fn unknown_route_is_404() {
        let tmp = tempfile::tempdir().unwrap();
        let response = app(tmp.path().to_path_buf())
            .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    const BOUNDARY: &str = "X-PHOTODROP-BOUNDARY";

    /// Builds a multipart body with one part per `(field, filename, data)`.
    fn multipart_body(parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (field, filename, data) in parts {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            let disposition = match filename {
                Some(f) => format!("form-data; name=\"{field}\"; filename=\"{f}\""),
                None => format!("form-data; name=\"{field}\""),
            };
            body.extend_from_slice(
                format!("Content-Disposition: {disposition}\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    async fn post_upload(dir: &Path, body: Vec<u8>) -> (StatusCode, String) {
        let request = Request::post("/upload")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .unwrap();
        let response = app(dir.to_path_buf()).oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    fn dir_entries(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    #[tokio::test]
    async fn upload_saves_exact_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..10 * 1024).map(|i| (i % 251) as u8).collect();
        let body = multipart_body(&[
            ("note", None, b"ignored"),
            ("file", Some("photo.JPG"), &data),
        ]);
        let (status, text) = post_upload(tmp.path(), body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let files = dir_entries(tmp.path());
        assert_eq!(files.len(), 1, "{files:?}");
        let name = files[0].file_name().unwrap().to_str().unwrap();
        assert_eq!(name, "photo.JPG");
        assert_eq!(text, format!("saved as {name}"));
        assert_eq!(std::fs::read(&files[0]).unwrap(), data);
    }

    #[tokio::test]
    async fn upload_accepts_body_over_default_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let body = multipart_body(&[("file", Some("big.jpg"), &data)]);
        let (status, text) = post_upload(tmp.path(), body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let files = dir_entries(tmp.path());
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(std::fs::read(&files[0]).unwrap(), data);
    }

    #[tokio::test]
    async fn upload_without_file_field_is_400() {
        let tmp = tempfile::tempdir().unwrap();
        let body = multipart_body(&[("other", Some("a.jpg"), b"data")]);
        let (status, _) = post_upload(tmp.path(), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(dir_entries(tmp.path()).is_empty());
    }

    #[tokio::test]
    async fn upload_truncated_body_is_400() {
        let tmp = tempfile::tempdir().unwrap();
        let mut body = multipart_body(&[("file", Some("a.jpg"), b"partial data")]);
        let closing = format!("\r\n--{BOUNDARY}--\r\n");
        body.truncate(body.len() - closing.len());
        let (status, text) = post_upload(tmp.path(), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(text, "upload failed");
    }

    #[tokio::test]
    async fn upload_ignores_path_in_filename() {
        let outer = tempfile::tempdir().unwrap();
        let inbox = outer.path().join("inbox");
        std::fs::create_dir(&inbox).unwrap();
        let body = multipart_body(&[("file", Some("../../evil.jpg"), b"evil")]);
        let (status, text) = post_upload(&inbox, body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let files = dir_entries(&inbox);
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(std::fs::read(&files[0]).unwrap(), b"evil");
        assert_eq!(dir_entries(outer.path()), vec![inbox]);
        assert!(!outer.path().parent().unwrap().join("evil.jpg").exists());
    }

    #[tokio::test]
    async fn upload_collisions_get_numbered_names() {
        let tmp = tempfile::tempdir().unwrap();
        let uploads: [(&str, &[u8]); 3] = [
            ("a.jpg", b"first"),
            ("a (1).jpg", b"second"),
            ("a (2).jpg", b"third"),
        ];
        for (expected, data) in uploads {
            let body = multipart_body(&[("file", Some("a.jpg"), data)]);
            let (status, text) = post_upload(tmp.path(), body).await;
            assert_eq!(status, StatusCode::OK, "{text}");
            assert_eq!(text, format!("saved as {expected}"));
        }
        assert_eq!(dir_entries(tmp.path()).len(), 3);
        for (name, data) in uploads {
            assert_eq!(std::fs::read(tmp.path().join(name)).unwrap(), data);
        }
    }

    #[tokio::test]
    async fn upload_does_not_overwrite_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.jpg"), b"existing").unwrap();
        let body = multipart_body(&[("file", Some("a.jpg"), b"new")]);
        let (status, text) = post_upload(tmp.path(), body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(text, "saved as a (1).jpg");
        assert_eq!(
            std::fs::read(tmp.path().join("a.jpg")).unwrap(),
            b"existing"
        );
        assert_eq!(std::fs::read(tmp.path().join("a (1).jpg")).unwrap(), b"new");
    }

    #[tokio::test]
    async fn upload_keeps_utf8_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let body = multipart_body(&[("file", Some("фото.jpg"), b"photo")]);
        let (status, text) = post_upload(tmp.path(), body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(text, "saved as фото.jpg");
        assert_eq!(
            std::fs::read(tmp.path().join("фото.jpg")).unwrap(),
            b"photo"
        );
    }

    #[tokio::test]
    async fn upload_into_missing_dir_is_500() {
        let tmp = tempfile::tempdir().unwrap();
        let body = multipart_body(&[("file", Some("a.jpg"), b"photo")]);
        let (status, text) = post_upload(&tmp.path().join("missing"), body).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(text, "upload failed");
    }

    fn checked_save_name(client_name: Option<&str>, attempt: u32) -> String {
        let name = save_name(client_name, attempt);
        assert!(!name.contains('/') && !name.contains('\\'), "{name}");
        assert!(name != "." && name != "..", "{name}");
        name
    }

    #[test]
    fn save_name_keeps_plain_name() {
        assert_eq!(checked_save_name(Some("IMG_1.HEIC"), 0), "IMG_1.HEIC");
    }

    #[test]
    fn save_name_adds_suffix_on_collision() {
        assert_eq!(checked_save_name(Some("IMG_1.HEIC"), 1), "IMG_1 (1).HEIC");
        assert_eq!(checked_save_name(Some("IMG_1.HEIC"), 2), "IMG_1 (2).HEIC");
    }

    #[test]
    fn save_name_suffix_goes_before_last_extension() {
        assert_eq!(checked_save_name(Some("a.tar.gz"), 1), "a.tar (1).gz");
    }

    #[test]
    fn save_name_suffix_without_extension() {
        assert_eq!(checked_save_name(Some("a"), 0), "a");
        assert_eq!(checked_save_name(Some("a"), 1), "a (1)");
    }

    #[test]
    fn save_name_strips_path() {
        assert_eq!(checked_save_name(Some("../../etc/passwd"), 0), "passwd");
        assert_eq!(checked_save_name(Some("a\\b.jpg"), 0), "b.jpg");
    }

    #[test]
    fn save_name_falls_back_for_unusable_names() {
        for name in [
            None,
            Some(""),
            Some("."),
            Some(".."),
            Some("a/"),
            Some("a\nb.jpg"),
        ] {
            assert_eq!(checked_save_name(name, 0), "upload.bin", "{name:?}");
            assert_eq!(checked_save_name(name, 1), "upload (1).bin", "{name:?}");
        }
    }

    #[test]
    fn save_name_falls_back_for_long_name() {
        let max = format!("{}.jpg", "a".repeat(251));
        assert_eq!(checked_save_name(Some(&max), 0), max);
        assert_eq!(checked_save_name(Some(&max), 1), "upload (1).bin");
        let long = format!("{}.jpg", "a".repeat(252));
        assert_eq!(checked_save_name(Some(&long), 0), "upload.bin");
        let fits_suffix = format!("{}.jpg", "a".repeat(247));
        let expected = format!("{} (1).jpg", "a".repeat(247));
        assert_eq!(checked_save_name(Some(&fits_suffix), 1), expected);
    }

    #[test]
    fn save_name_limit_counts_bytes_not_chars() {
        let fits = format!("{}.jpg", "ф".repeat(125));
        assert_eq!(checked_save_name(Some(&fits), 0), fits);
        let too_long = format!("{}.jpg", "ф".repeat(126));
        assert_eq!(checked_save_name(Some(&too_long), 0), "upload.bin");
    }

    #[test]
    fn save_name_keeps_utf8() {
        assert_eq!(checked_save_name(Some("фото.jpg"), 1), "фото (1).jpg");
    }

    #[test]
    fn save_name_leading_dot_has_no_extension() {
        assert_eq!(checked_save_name(Some(".heic"), 0), ".heic");
        assert_eq!(checked_save_name(Some(".foo"), 1), ".foo (1)");
    }
}
