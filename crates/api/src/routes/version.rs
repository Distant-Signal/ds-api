//! `/public/version`: the images this deployment runs and the source each
//! was built from. Distant Signal is AGPL-3.0-or-later, and §13 of the
//! licence asks a network service to offer its users the corresponding
//! source of what they use; this route and the site footer's "Source code"
//! link are how (repo split spec §6).
//!
//! The list is the file `API_SOURCE_IMAGES_FILE` names
//! (`config.source_images_file`), which the chart mounts from a config map
//! rendered from `sourceCode` and each first-party image reference
//! (`distant-signal.sourceImagesJson`). It is read per request: the chart
//! changes it on every build, and kubelet updates a mounted config map in
//! place, so a new revision needs no api restart. Unauthenticated and
//! read-only, like `/public/health`.

use std::path::Path;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::app::{App, Router};

pub fn router() -> Router {
    Router::new().route("/version", axum::routing::get(get_version))
}

/// One running image and the source it was built from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceImage {
    /// The service the image runs, e.g. `api`.
    pub service: String,
    /// The image reference the pods pull: `<repository>@<digest>` when the
    /// chart pins a digest, else `<repository>:<tag>`.
    pub image: String,
    /// The repository the image was built from.
    pub source: String,
    /// The commit the image was built from. `None` when the chart was not
    /// packaged by CI (an empty `sourceCode.revision`).
    #[serde(default)]
    pub revision: Option<String>,
}

/// Parses the source images file, a JSON array of [`SourceImage`]s. An
/// empty `revision` reads as none.
pub fn parse_source_images(text: &str) -> anyhow::Result<Vec<SourceImage>> {
    let mut images: Vec<SourceImage> = serde_json::from_str(text)?;
    for image in &mut images {
        if image.revision.as_deref().is_some_and(str::is_empty) {
            image.revision = None;
        }
    }
    Ok(images)
}

/// Reads and parses the source images file. A missing file is an empty
/// list (the api run outside the chart).
async fn read_source_images(path: &Path) -> anyhow::Result<Vec<SourceImage>> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => parse_source_images(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    }
}

/// The `/public/version` body.
#[derive(Debug, Serialize)]
pub struct Version {
    pub images: Vec<SourceImage>,
}

async fn get_version(State(app): State<App>) -> Result<Json<Version>, (StatusCode, String)> {
    let images = match &app.config.source_images_file {
        None => Vec::new(),
        Some(path) => read_source_images(path).await.map_err(|err| {
            tracing::error!(error = %err, path = %path.display(), "couldn't read the source images file");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "couldn't read the source images list".to_owned(),
            )
        })?,
    };
    Ok(Json(Version { images }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::json;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn parses_the_charts_json_and_drops_an_empty_revision() {
        let parsed = parse_source_images(
            r#"[{"image":"ghcr.io/x/api@sha256:aa","revision":"abc","service":"api","source":"https://example.com/ds"},
                {"image":"ghcr.io/x/notifier:0.1.0","revision":"","service":"notifier","source":"https://example.com/ds"}]"#,
        )
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].revision.as_deref(), Some("abc"));
        assert_eq!(parsed[1].revision, None);
        assert_eq!(parse_source_images("[]").unwrap(), []);
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_source_images("api").is_err());
        assert!(parse_source_images(r#"[{"service":"api"}]"#).is_err());
        assert!(
            parse_source_images(
                r#"[{"service":"api","image":"i","source":"s","revision":"r","extra":1}]"#
            )
            .is_err()
        );
    }

    /// A scratch file under the temp dir, removed on drop.
    struct ScratchFile(std::path::PathBuf);

    impl Drop for ScratchFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    async fn get(file: Option<std::path::PathBuf>) -> (StatusCode, serde_json::Value) {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();
        let app = crate::test_support::inert_app(pool);
        let mut state = std::sync::Arc::try_unwrap(app).unwrap_or_else(|_| unreachable!());
        state.config.source_images_file = file;
        let router: axum::Router = router().with_state(std::sync::Arc::new(state));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn lists_the_images_in_the_file_reading_it_per_request() {
        let file = ScratchFile(
            std::env::temp_dir().join(format!("ds-api-source-images-{}.json", std::process::id())),
        );
        std::fs::write(
            &file.0,
            r#"[{"service":"api","image":"ghcr.io/x/api@sha256:aa","source":"https://example.com/ds","revision":"abc"}]"#,
        )
        .unwrap();
        let (status, body) = get(Some(file.0.clone())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"images": [{
                "service": "api",
                "image": "ghcr.io/x/api@sha256:aa",
                "source": "https://example.com/ds",
                "revision": "abc",
            }]})
        );

        // A new chart build rewrites the mounted file; no restart needed.
        std::fs::write(&file.0, "[]").unwrap();
        assert_eq!(get(Some(file.0.clone())).await.1, json!({"images": []}));

        std::fs::write(&file.0, "not json").unwrap();
        assert_eq!(
            get(Some(file.0.clone())).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn no_file_lists_no_images() {
        assert_eq!(get(None).await, (StatusCode::OK, json!({"images": []})));
        let missing = std::env::temp_dir().join("ds-api-source-images-missing.json");
        assert_eq!(
            get(Some(missing)).await,
            (StatusCode::OK, json!({"images": []}))
        );
    }
}
