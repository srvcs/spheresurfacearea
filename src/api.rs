use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use utoipa::{OpenApi, ToSchema};

use crate::client::{self, DepError};

pub const SERVICE: &str = "srvcs-spheresurfacearea";
pub const CONCERN: &str = "geometry: surface area of a sphere";
pub const DEPENDS_ON: &[&str] = &["srvcs-pi", "srvcs-floatmultiply"];

/// Dependency endpoints, injected as router state so tests can point them at
/// mock services.
#[derive(Clone)]
pub struct Deps {
    pub pi_url: String,
    pub floatmultiply_url: String,
}

#[derive(Serialize, ToSchema)]
pub struct Info {
    pub service: &'static str,
    pub concern: &'static str,
    pub depends_on: Vec<&'static str>,
}

/// `GET /` — service identity (srvcs service standard).
#[utoipa::path(get, path = "/", responses((status = 200, body = Info)))]
pub async fn index() -> Json<Info> {
    Json(Info {
        service: SERVICE,
        concern: CONCERN,
        depends_on: DEPENDS_ON.to_vec(),
    })
}

#[derive(Deserialize, ToSchema)]
pub struct EvalRequest {
    /// The radius of the sphere.
    #[schema(value_type = Object)]
    pub radius: Value,
}

#[derive(Serialize, ToSchema)]
pub struct SphereSurfaceAreaResponse {
    #[schema(value_type = Object)]
    pub radius: Value,
    pub result: f64,
}

fn ok(radius: Value, result: f64) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "radius": radius, "result": result })),
    )
        .into_response()
}

fn degraded(dependency: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "dependency unavailable", "dependency": dependency })),
    )
        .into_response()
}

fn forward(status: u16, body: Value) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    (code, Json(body)).into_response()
}

/// A reachable dependency answered `200` but its body lacked a numeric
/// `result`. That is a contract violation we cannot recover from, so surface a
/// `500` rather than guessing.
fn malformed(dependency: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(
            json!({ "error": "dependency returned a malformed result", "dependency": dependency }),
        ),
    )
        .into_response()
}

/// Call one dependency at `url` with `body`, mapping its outcome to either the
/// numeric `result` it returned (on `200`) or an early-return `Response` the
/// caller should surface verbatim:
///
/// - unreachable / non-`200`/`422` -> `503` degraded
/// - `422` -> forwarded `422` (the dependency rejected the input)
/// - `200` without a numeric `result` -> `500` malformed
async fn ask(url: &str, body: &Value, dependency: &str) -> Result<f64, Response> {
    match client::call(url, body).await {
        Err(DepError::Unreachable) => Err(degraded(dependency)),
        Ok((200, body)) => match body.get("result").and_then(Value::as_f64) {
            Some(v) => Ok(v),
            None => Err(malformed(dependency)),
        },
        Ok((422, body)) => Err(forward(422, body)),
        Ok(_) => Err(degraded(dependency)),
    }
}

/// `POST /` — compute the surface area of a sphere, `A = 4 * pi * r^2`.
///
/// This service owns the *control flow* but delegates every arithmetic step to
/// its dependencies, exactly as specified:
///
/// 1. ask `srvcs-pi` (with an empty body, since it is a constant service) for
///    `p = pi`;
/// 2. ask `srvcs-floatmultiply` for `r2 = radius * radius`;
/// 3. ask `srvcs-floatmultiply` for `t = p * r2`;
/// 4. ask `srvcs-floatmultiply` for `result = 4 * t`.
///
/// This service never validates `radius` itself; if a dependency rejects the
/// input it forwards the `422`, and if a dependency is unreachable it reports
/// itself degraded (`503`).
#[utoipa::path(
    post,
    path = "/",
    request_body = EvalRequest,
    responses(
        (status = 200, body = SphereSurfaceAreaResponse),
        (status = 422, description = "a dependency rejected the input (forwarded)"),
        (status = 500, description = "a dependency returned a malformed result"),
        (status = 503, description = "a dependency is unavailable")
    )
)]
pub async fn evaluate(State(deps): State<Deps>, Json(req): Json<EvalRequest>) -> Response {
    let radius = req.radius;

    // 1. p = pi (constant service: call with an empty body).
    let p = match ask(&deps.pi_url, &json!({}), "srvcs-pi").await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // 2. r2 = radius * radius.
    let r2 = match ask(
        &deps.floatmultiply_url,
        &json!({ "a": radius, "b": radius }),
        "srvcs-floatmultiply",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // 3. t = p * r2.
    let t = match ask(
        &deps.floatmultiply_url,
        &json!({ "a": p, "b": r2 }),
        "srvcs-floatmultiply",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // 4. result = 4 * t.
    let result = match ask(
        &deps.floatmultiply_url,
        &json!({ "a": 4, "b": t }),
        "srvcs-floatmultiply",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    ok(radius, result)
}

#[derive(OpenApi)]
#[openapi(
    paths(index, evaluate),
    components(schemas(Info, EvalRequest, SphereSurfaceAreaResponse))
)]
pub struct ApiDoc;

/// Serve OpenAPI document
pub async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_documents_routes() {
        let doc = ApiDoc::openapi();
        let root = doc.paths.paths.get("/").expect("path / present");
        assert!(root.get.is_some());
        assert!(root.post.is_some());
    }

    #[tokio::test]
    async fn index_reports_all_dependencies() {
        let Json(info) = index().await;
        assert_eq!(info.service, "srvcs-spheresurfacearea");
        assert_eq!(info.concern, "geometry: surface area of a sphere");
        assert_eq!(info.depends_on, vec!["srvcs-pi", "srvcs-floatmultiply"]);
    }
}
