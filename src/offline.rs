use cot::Body;
use cot::db::Database;
use cot::http::{HeaderValue, Response, StatusCode};
use cot::session::Session;

pub(crate) const CATALOG: &str = include_str!("player/offline-catalog.js");

fn asset(content_type: &'static str, body: &'static str) -> cot::Result<Response<Body>> {
    Ok(Response::builder()
        .header("Content-Type", content_type)
        .header("Cache-Control", "no-cache")
        .header("X-Content-Type-Options", "nosniff")
        .body(Body::fixed(body))
        .expect("valid offline asset"))
}

pub(crate) fn identify(response: &mut Response<Body>, account_id: i64) {
    response.headers_mut().insert(
        "X-Furumusic-Player-Account",
        HeaderValue::from_str(&account_id.to_string()).expect("numeric account id"),
    );
}

pub(crate) async fn worker() -> cot::Result<Response<Body>> {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("player/offline-worker.js"),
    )
}

pub(crate) async fn catalog() -> cot::Result<Response<Body>> {
    asset("text/javascript; charset=utf-8", CATALOG)
}

pub(crate) async fn manifest() -> cot::Result<Response<Body>> {
    asset(
        "application/manifest+json",
        include_str!("player/player.webmanifest"),
    )
}

pub(crate) async fn icon() -> cot::Result<Response<Body>> {
    asset("image/svg+xml", include_str!("player/player-icon.svg"))
}

pub(crate) async fn probe(session: Session, db: Database) -> cot::Result<Response<Body>> {
    let identity = crate::auth::checked_session_identity(&session, &db).await?;
    let user = identity.filter(|user| user.is_active());
    let mut response = Response::builder()
        .header("Cache-Control", "private, no-store")
        .status(if user.is_some() {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::UNAUTHORIZED
        })
        .body(Body::empty())
        .expect("valid reachability response");
    identify(&mut response, user.map_or(0, |u| u.id_val()));
    Ok(response)
}
