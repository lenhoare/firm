use crate::{
    app::App,
    config::Allowances,
    snapshots::{Bundle, MAX_BUNDLE, RecipeEdit},
    state::ProviderSettings,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as UrlPath, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;
fn done(result: anyhow::Result<()>) -> ApiResult {
    result
        .map(|()| Json(json!({"ok":true})))
        .map_err(|e| (StatusCode::CONFLICT, Json(json!({"error":e.to_string()}))))
}
pub fn router(app: App) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../static/index.html")) }),
        )
        .route("/api/state", get(status))
        .route(
            "/api/usage",
            get(|State(app): State<App>| async move { Json(app.account_usage().await) }),
        )
        .route(
            "/usage.js",
            get(|| async {
                (
                    [("content-type", "text/javascript; charset=utf-8")],
                    include_str!("../static/usage.js"),
                )
            }),
        )
        .route(
            "/meetings",
            get(|| async { Html(include_str!("../static/meetings.html")) }),
        )
        .route(
            "/team",
            get(|| async { Html(include_str!("../static/team.html")) }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [("content-type", "text/css; charset=utf-8")],
                    include_str!("../static/style.css"),
                )
            }),
        )
        .route("/api/meetings", get(meetings).post(meeting_create))
        .route("/api/meetings/{id}", get(meeting_read))
        .route("/api/meetings/{id}/ask", post(meeting_ask))
        .route("/api/meetings/{id}/stop", post(meeting_stop))
        .route("/api/history", get(history))
        .route("/api/objective", post(objective))
        .route("/api/start", post(start))
        .route("/api/pause", post(pause))
        .route("/api/stop", post(stop))
        .route("/api/allowances", post(allowances))
        .route("/api/manager", post(manager_selection))
        .route("/api/providers/{id}", post(provider_settings))
        .route("/api/reconcile", post(reconcile))
        .route("/api/snapshots", get(snapshot_list).post(snapshot_capture))
        .route(
            "/api/snapshots/import",
            post(snapshot_import).layer(DefaultBodyLimit::max(MAX_BUNDLE)),
        )
        .route("/api/snapshots/{id}", get(snapshot_read))
        .route("/api/snapshots/{id}/export", get(snapshot_export))
        .route(
            "/api/snapshots/{id}/fork",
            post(snapshot_fork).layer(DefaultBodyLimit::max(3 * 1024 * 1024)),
        )
        .route("/api/snapshots/{id}/evaluate", post(snapshot_evaluate))
        .route("/api/snapshots/{id}/artifact", get(snapshot_artifact))
        .route("/api/snapshots/{id}/diff", get(snapshot_diff))
        .layer(DefaultBodyLimit::max(32000))
        .layer(middleware::from_fn_with_state(app.clone(), local_request))
        .with_state(app)
}

async fn local_request(
    State(app): State<App>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let allowed = [
        app.config.listen.to_string(),
        format!("localhost:{}", app.config.listen.port()),
    ];
    if !allowed.iter().any(|h| h == host) {
        return (StatusCode::FORBIDDEN, "Unknown host").into_response();
    }
    if let Some(origin) = headers.get("origin")
        && origin.to_str().ok() != Some(format!("http://{host}").as_str())
    {
        return (
            StatusCode::FORBIDDEN,
            "Cross-origin requests are not allowed",
        )
            .into_response();
    }
    if request.method() != axum::http::Method::GET
        && headers.get("x-firm-control").and_then(|v| v.to_str().ok()) != Some("1")
    {
        return (StatusCode::FORBIDDEN, "Missing controller request header").into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("x-content-type-options", "nosniff".parse().unwrap());
    response.headers_mut().insert("content-security-policy", "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().unwrap());
    response
}

async fn status(State(app): State<App>) -> Json<Value> {
    Json(app.snapshot().await)
}
async fn meetings(State(app): State<App>) -> ApiResult {
    app.meeting_overview()
        .await
        .map(Json)
        .map_err(snapshot_error)
}
#[derive(Deserialize)]
struct MeetingTitle {
    title: String,
}
async fn meeting_create(State(app): State<App>, Json(input): Json<MeetingTitle>) -> ApiResult {
    app.new_meeting(input.title)
        .await
        .map(|id| Json(json!({"id":id})))
        .map_err(snapshot_error)
}
async fn meeting_read(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    app.core
        .lock()
        .await
        .store
        .meeting(&id)
        .map(|m| Json(json!(m)))
        .map_err(snapshot_error)
}
async fn meeting_ask(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Json(input): Json<crate::meetings::Question>,
) -> ApiResult {
    done(app.ask_meeting(&id, input).await)
}
async fn meeting_stop(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    done(app.stop_meeting(&id).await)
}
async fn history(State(app): State<App>) -> ApiResult {
    app.core
        .lock()
        .await
        .store
        .history()
        .map(|rows| Json(json!(rows)))
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":error.to_string()})),
            )
        })
}
#[derive(Deserialize)]
struct Objective {
    objective: String,
}
async fn objective(State(app): State<App>, Json(input): Json<Objective>) -> ApiResult {
    done(app.new_experiment(input.objective).await)
}
async fn start(State(app): State<App>) -> ApiResult {
    done(app.resume().await)
}
async fn pause(State(app): State<App>) -> ApiResult {
    done(app.pause().await)
}
async fn stop(State(app): State<App>) -> ApiResult {
    done(app.stop().await)
}
async fn reconcile(State(app): State<App>) -> ApiResult {
    done(app.reconcile().await)
}
async fn allowances(State(app): State<App>, Json(input): Json<Allowances>) -> ApiResult {
    done(app.allowances(input).await)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagerSelection {
    provider: String,
}
async fn manager_selection(
    State(app): State<App>,
    Json(input): Json<ManagerSelection>,
) -> ApiResult {
    done(app.select_manager(&input.provider).await)
}
async fn provider_settings(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Json(input): Json<ProviderUpdate>,
) -> ApiResult {
    done(
        app.provider_update(
            &id,
            ProviderSettings {
                enabled: input.enabled,
                max_runs: input.max_runs,
            },
            input.description,
        )
        .await,
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderUpdate {
    enabled: bool,
    max_runs: usize,
    #[serde(default)]
    description: Option<String>,
}

fn snapshot_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":error.to_string()})),
    )
}
async fn snapshot_list(State(app): State<App>) -> ApiResult {
    app.archive_action(|archive| archive.list())
        .await
        .map(|v| Json(json!(v)))
        .map_err(snapshot_error)
}
#[derive(Deserialize)]
struct SnapshotLabel {
    label: String,
}
async fn snapshot_capture(State(app): State<App>, Json(input): Json<SnapshotLabel>) -> ApiResult {
    app.capture_manual(input.label)
        .await
        .map(|id| Json(json!({"id":id})))
        .map_err(snapshot_error)
}
async fn snapshot_read(State(app): State<App>, UrlPath(id): UrlPath<String>) -> ApiResult {
    app.archive_action(move |archive| Ok(json!({"id":id,"manifest":archive.get(&id)?,"recipe_edit":archive.recipe_editor(&id).ok()}))).await.map(Json).map_err(snapshot_error)
}
async fn snapshot_export(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let bundle = app
        .archive_action(move |archive| archive.export(&id))
        .await
        .map_err(snapshot_error)?;
    let filename = format!(
        "attachment; filename=firm-snapshot-{}.json",
        &bundle.root[..12]
    );
    Ok(([("content-disposition", filename)], Json(bundle)).into_response())
}
async fn snapshot_import(State(app): State<App>, Json(bundle): Json<Bundle>) -> ApiResult {
    app.archive_action(move |archive| archive.import(bundle))
        .await
        .map(|id| Json(json!({"id":id,"activated":false})))
        .map_err(snapshot_error)
}
async fn snapshot_fork(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Json(edit): Json<RecipeEdit>,
) -> ApiResult {
    app.archive_action(move |archive| archive.fork(&id, edit))
        .await
        .map(|id| Json(json!({"id":id,"activated":false})))
        .map_err(snapshot_error)
}
#[derive(Deserialize)]
struct Evaluation {
    note: String,
    verdict: String,
}
async fn snapshot_evaluate(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Json(evaluation): Json<Evaluation>,
) -> ApiResult {
    app.archive_action(move |archive| archive.evaluate(&id, &evaluation.note, &evaluation.verdict))
        .await
        .map(|id| Json(json!({"id":id})))
        .map_err(snapshot_error)
}
#[derive(Deserialize)]
struct ArtifactQuery {
    name: String,
}
async fn snapshot_artifact(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<ArtifactQuery>,
) -> ApiResult {
    app.archive_action(move |archive| archive.preview(&id, &query.name))
        .await
        .map(Json)
        .map_err(snapshot_error)
}
#[derive(Deserialize)]
struct DiffQuery {
    against: Option<String>,
}
async fn snapshot_diff(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<DiffQuery>,
) -> ApiResult {
    app.archive_action(move |archive| archive.diff(&id, query.against))
        .await
        .map(Json)
        .map_err(snapshot_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    #[tokio::test]
    async fn shared_usage_assets_and_cached_endpoint_are_served() {
        let (_dir, app) = crate::app::tests::fixture();
        let router = router(app);
        for path in ["/", "/team", "/meetings", "/usage.js", "/api/usage"] {
            let request = Request::builder()
                .uri(path)
                .header("host", "127.0.0.1:7433")
                .body(Body::empty())
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert!(
                response.headers()["content-security-policy"]
                    .to_str()
                    .unwrap()
                    .contains("script-src 'self'")
            );
            if path == "/usage.js" {
                assert_eq!(
                    response.headers()["content-type"],
                    "text/javascript; charset=utf-8"
                );
            }
        }
    }
    #[tokio::test]
    async fn manager_endpoint_validates_selection() {
        let (_dir, app) = crate::app::tests::fixture();
        let router = router(app.clone());
        for (body, expected) in [
            (json!({"provider":"muse"}), 200),
            (json!({"provider":"missing"}), 409),
            (json!({"provider":"codex","command":"unexpected"}), 422),
        ] {
            let request = Request::builder()
                .method("POST")
                .uri("/api/manager")
                .header("host", "127.0.0.1:7433")
                .header("x-firm-control", "1")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            assert_eq!(
                router
                    .clone()
                    .oneshot(request)
                    .await
                    .unwrap()
                    .status()
                    .as_u16(),
                expected
            );
        }
        assert_eq!(app.core.lock().await.state.manager_provider, "muse");
        assert!(app.core.lock().await.state.manager_starts.is_empty());
    }
    #[tokio::test]
    async fn provider_endpoint_validates_settings_and_requires_paused_idle_control() {
        let (_dir, app) = crate::app::tests::fixture();
        let router = router(app.clone());
        for (id, body, expected) in [
            (
                "muse",
                json!({"enabled":false,"max_runs":1,"description":"Design critic"}),
                200,
            ),
            ("missing", json!({"enabled":true,"max_runs":1}), 409),
            ("muse", json!({"enabled":true,"max_runs":-1}), 422),
            (
                "muse",
                json!({"enabled":true,"max_runs":1,"command":"unexpected"}),
                422,
            ),
        ] {
            let request = Request::builder()
                .method("POST")
                .uri(format!("/api/providers/{id}"))
                .header("host", "127.0.0.1:7433")
                .header("x-firm-control", "1")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            assert_eq!(
                router
                    .clone()
                    .oneshot(request)
                    .await
                    .unwrap()
                    .status()
                    .as_u16(),
                expected
            );
        }
        assert!(!app.core.lock().await.state.provider_settings["muse"].enabled);
        assert_eq!(
            app.core.lock().await.state.provider_descriptions["muse"],
            "Design critic"
        );
        app.core.lock().await.active = true;
        let request = Request::builder()
            .method("POST")
            .uri("/api/providers/muse")
            .header("host", "127.0.0.1:7433")
            .header("x-firm-control", "1")
            .header("content-type", "application/json")
            .body(Body::from(json!({"enabled":true,"max_runs":3}).to_string()))
            .unwrap();
        assert_eq!(
            router.oneshot(request).await.unwrap().status(),
            StatusCode::CONFLICT
        );
        assert!(!app.core.lock().await.state.provider_settings["muse"].enabled);
    }
    #[tokio::test]
    async fn snapshot_export_import_preserves_live_state_and_accepts_large_bundles() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::read(std::path::Path::new("firm.toml")).unwrap();
        config.state_dir = dir.path().join("state");
        config.workspace = dir.path().join("project");
        std::fs::create_dir_all(&config.workspace).unwrap();
        std::fs::write(
            config.workspace.join("context.txt"),
            "context ".repeat(10000),
        )
        .unwrap();
        let (store, mut state) =
            crate::state::Store::open(&dir.path().join("test.db"), true, config.allowances.clone())
                .unwrap();
        state.manager_starts = vec![123];
        state.worker_starts = vec![124];
        let app = App::new(config, store, state, None);
        let router = router(app.clone());
        let request = |method: &str, path: &str, body: String| {
            Request::builder()
                .method(method)
                .uri(path)
                .header("host", "127.0.0.1:7433")
                .header("x-firm-control", "1")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        let response = router
            .clone()
            .oneshot(request(
                "POST",
                "/api/snapshots",
                json!({"label":"HTTP baseline"}).to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let data: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), MAX_BUNDLE)
                .await
                .unwrap(),
        )
        .unwrap();
        let id = data["id"].as_str().unwrap();
        let response = router
            .clone()
            .oneshot(request(
                "GET",
                &format!("/api/snapshots/{id}/export"),
                String::new(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("content-disposition"));
        let bundle = axum::body::to_bytes(response.into_body(), MAX_BUNDLE)
            .await
            .unwrap();
        assert!(bundle.len() > 32000);
        let before = serde_json::to_value(&app.core.lock().await.state).unwrap();
        let response = router
            .oneshot(request(
                "POST",
                "/api/snapshots/import",
                String::from_utf8(bundle.to_vec()).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            serde_json::to_value(&app.core.lock().await.state).unwrap(),
            before
        );
    }
    #[tokio::test]
    async fn dashboard_rejects_cross_origin_and_rebinding_requests() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::Config::read(std::path::Path::new("firm.toml")).unwrap();
        let (store, state) =
            crate::state::Store::open(&dir.path().join("test.db"), true, config.allowances.clone())
                .unwrap();
        let router = router(App::new(config, store, state, None));
        for (host, origin, control, expected) in [
            ("127.0.0.1:7433", "http://evil.test", true, 403),
            ("evil.test:7433", "http://evil.test:7433", true, 403),
            ("127.0.0.1:7433", "http://127.0.0.1:7433", false, 403),
            ("127.0.0.1:7433", "http://127.0.0.1:7433", true, 200),
        ] {
            let mut request = Request::builder()
                .method("POST")
                .uri("/api/pause")
                .header("host", host)
                .header("origin", origin);
            if control {
                request = request.header("x-firm-control", "1");
            }
            let response = router
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), expected);
        }
    }
}
