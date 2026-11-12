use super::v2::{json_error, require_admin_json};
use crate::{auth::Role, user::User};
use cot::{db::Database, http::StatusCode, json::Json, response::IntoResponse, session::Session};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(super) struct UserRequest {
    action: String,
    id: Option<i64>,
    #[serde(default)]
    username: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    role: String,
}

pub(super) async fn manage_user(
    session: Session,
    db: Database,
    Json(body): Json<UserRequest>,
) -> cot::Result<cot::response::Response> {
    let admin = match require_admin_json(&session, &db).await {
        Ok(u) => u,
        Err(r) => return Ok(r),
    };
    if body.action == "delete" {
        let Some(id) = body.id.filter(|id| *id > 0) else {
            return Ok(json_error(StatusCode::BAD_REQUEST, "Invalid user"));
        };
        if id == admin.id {
            return Ok(json_error(
                StatusCode::BAD_REQUEST,
                "You cannot delete your own account",
            ));
        }
        User::delete_by_id(&db, id).await?;
    } else {
        if !["create", "update"].contains(&body.action.as_str())
            || body.username.trim().is_empty()
            || body.username.chars().count() > 255
            || Role::from_code(&body.role).is_none()
            || (body.action == "create" && body.password.is_empty())
        {
            return Ok(json_error(
                StatusCode::BAD_REQUEST,
                "Provide a username, valid role, and a password for new accounts",
            ));
        }
        let optional = |s: String| if s.trim().is_empty() { None } else { Some(s) };
        let email = optional(body.email);
        let display_name = optional(body.display_name);
        if body.action == "create" {
            if User::get_by_username(&db, body.username.trim())
                .await?
                .is_some()
            {
                return Ok(json_error(StatusCode::CONFLICT, "Username already exists"));
            }
            User::create(
                &db,
                body.username.trim(),
                email.as_deref(),
                display_name.as_deref(),
                &body.password,
                &body.role,
            )
            .await?;
        } else {
            let Some(id) = body.id.filter(|id| *id > 0) else {
                return Ok(json_error(StatusCode::BAD_REQUEST, "Invalid user"));
            };
            if id == admin.id && body.role != "admin" {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    "You cannot remove your own administrator role",
                ));
            }
            let Some(mut user) = User::get_by_id(&db, id).await? else {
                return Ok(json_error(StatusCode::NOT_FOUND, "User not found"));
            };
            if User::get_by_username(&db, body.username.trim())
                .await?
                .is_some_and(|u| u.id_val() != id)
            {
                return Ok(json_error(StatusCode::CONFLICT, "Username already exists"));
            }
            user.update_fields(
                &db,
                body.username.trim(),
                email.as_deref(),
                display_name.as_deref(),
                (!body.password.is_empty()).then_some(body.password.as_str()),
                &body.role,
            )
            .await?;
        }
    }
    Json(serde_json::json!({"ok": true})).into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct ScheduleRequest {
    cron: String,
}

pub(super) async fn schedule(
    session: Session,
    db: Database,
    handle: &std::sync::Arc<
        tokio::sync::OnceCell<std::sync::Arc<crate::scheduler::SchedulerHandle>>,
    >,
    name: &str,
    Json(body): Json<ScheduleRequest>,
) -> cot::Result<cot::response::Response> {
    if let Err(response) = require_admin_json(&session, &db).await {
        return Ok(response);
    }
    let cron = body.cron.trim();
    if !valid_schedule(cron) {
        return Ok(json_error(
            StatusCode::BAD_REQUEST,
            "Invalid cron schedule: include the seconds field",
        ));
    }
    if crate::scheduler::ScheduledJob::get_by_name(&db, name)
        .await?
        .is_none()
    {
        return Ok(json_error(StatusCode::NOT_FOUND, "Job not found"));
    }
    let Some(handle) = handle.get() else {
        return Ok(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Scheduler is not ready",
        ));
    };
    handle
        .reschedule_job(name, cron)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Json(serde_json::json!({"ok": true})).into_response()
}

fn valid_schedule(cron: &str) -> bool {
    cron.len() <= 100
        && croner::parser::CronParser::builder()
            .seconds(croner::parser::Seconds::Required)
            .build()
            .parse(cron)
            .is_ok()
        && crate::scheduler::compute_next_run(cron).is_some()
}

#[cfg(test)]
mod tests {
    #[test]
    fn schedules_use_the_same_seconds_field_as_the_scheduler() {
        assert!(super::valid_schedule("0 17 3 * * *"));
        assert!(!super::valid_schedule("17 3 * * *"));
        assert!(!super::valid_schedule("invalid"));
    }
}
