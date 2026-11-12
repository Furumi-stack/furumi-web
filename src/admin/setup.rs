use crate::auth;
use crate::i18n::{I18n, Translations};
use crate::user::User;
use cot::Template;
use cot::db::Database;
use cot::form::{Form, FormResult};
use cot::html::Html;
use cot::request::extractors::RequestForm;
use cot::response::IntoResponse;
use cot::session::Session;

#[derive(Debug, Template)]
#[template(path = "admin/setup.html")]
struct SetupTemplate {
    t: &'static Translations,
    message: String,
}

pub async fn setup_page(i18n: I18n, message: String) -> cot::Result<Html> {
    let template = SetupTemplate { t: i18n.t, message };
    Ok(Html::new(template.render()?))
}

#[derive(Debug, Form)]
pub struct SetupForm {
    username: String,
    password: String,
    confirm_password: String,
}

pub async fn setup_submit(
    i18n: I18n,
    db: &Database,
    session: &Session,
    form: RequestForm<SetupForm>,
) -> cot::Result<cot::response::Response> {
    let RequestForm(result) = form;
    let data = match result {
        FormResult::Ok(data) => data,
        FormResult::ValidationError(_) => {
            return setup_page(i18n, String::new()).await?.into_response();
        }
    };

    if data.password != data.confirm_password {
        let msg = i18n.t.setup_mismatch.to_owned();
        return setup_page(i18n, msg).await?.into_response();
    }

    let user = User::create(db, &data.username, None, None, &data.password, "admin")
        .await
        .map_err(|e| cot::Error::internal(format!("failed to create admin: {e}")))?;

    auth::login(session, user.id_val()).await?;
    Ok(auth::redirect("/admin/"))
}
