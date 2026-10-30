//! Nextcloud Login Flow v2, which clients like AntennaPod use to obtain an app password:
//! https://docs.nextcloud.com/server/latest/developer_manual/client_apis/LoginFlow/index.html#login-flow-v2

use std::sync::Arc;

use axum::Form;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{Extensions, HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

use super::web::{esc, layout, login_error_status};
use crate::AppState;
use crate::auth::{Credentials, authenticate, random_token, session_user};
use crate::db::now;
use crate::json::{ApiError, PhpJson};
use crate::security::{client_ip, peer_addr};
use crate::users::create_app_password;

/// Nextcloud expires login flows after 20 minutes.
const FLOW_LIFETIME_SECS: i64 = 20 * 60;

/// Deletes expired flows, including granted-but-never-polled ones, which still
/// hold a plaintext app password.
pub fn purge_expired_login_flows(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM login_flows WHERE created_at < ?1",
        [now() - FLOW_LIFETIME_SECS],
    )
}

pub async fn init(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<PhpJson, ApiError> {
    let poll_token = random_token(128);
    let flow_token = random_token(128);
    let client_name = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .unwrap_or("unknown client")
        .to_owned();
    state.db.with(|conn| -> rusqlite::Result<()> {
        purge_expired_login_flows(conn)?;
        conn.execute(
            "INSERT INTO login_flows (poll_token, flow_token, client_name, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![poll_token, flow_token, client_name, now()],
        )?;
        Ok(())
    })?;

    let base = state.base_url(&headers);
    Ok(PhpJson::ok(json!({
        "poll": {
            "token": poll_token,
            "endpoint": format!("{base}/index.php/login/v2/poll"),
        },
        "login": format!("{base}/index.php/login/v2/flow/{flow_token}"),
    })))
}

/// Returns the credentials once the user has granted access (exactly once),
/// and 404 with an empty JSON array until then.
pub async fn poll(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Result<PhpJson, ApiError> {
    let token = poll_token(query.as_deref(), &body);
    let granted = state.db.with(|conn| -> rusqlite::Result<_> {
        purge_expired_login_flows(conn)?;
        let found: Option<(String, String)> = conn
            .query_row(
                "SELECT login_name, app_password FROM login_flows
                 WHERE poll_token = ?1 AND created_at >= ?2 AND app_password IS NOT NULL",
                params![token, now() - FLOW_LIFETIME_SECS],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if found.is_some() {
            conn.execute("DELETE FROM login_flows WHERE poll_token = ?1", [&token])?;
        }
        Ok(found)
    })?;

    match granted {
        Some((login_name, app_password)) => Ok(PhpJson::ok(json!({
            "server": state.base_url(&headers),
            "loginName": login_name,
            "appPassword": app_password,
        }))),
        None => Ok(PhpJson(StatusCode::NOT_FOUND, json!([]))),
    }
}

/// The poll token may arrive form-encoded, as JSON, or in the query string.
fn poll_token(query: Option<&str>, body: &[u8]) -> String {
    if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body)
        && let Some(Value::String(token)) = map.get("token")
    {
        return token.clone();
    }
    url::form_urlencoded::parse(body)
        .chain(url::form_urlencoded::parse(
            query.unwrap_or_default().as_bytes(),
        ))
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn pending_flow_client(state: &AppState, flow_token: &str) -> Result<Option<String>, ApiError> {
    Ok(state.db.with(|conn| {
        conn.query_row(
            "SELECT client_name FROM login_flows
             WHERE flow_token = ?1 AND created_at >= ?2 AND app_password IS NULL",
            params![flow_token, now() - FLOW_LIFETIME_SECS],
            |r| r.get(0),
        )
        .optional()
    })?)
}

fn invalid_flow_page() -> Response {
    (
        StatusCode::NOT_FOUND,
        layout(
            "Login link expired",
            "<p>This login link is invalid, expired, or already used. Start the login again from your app.</p>",
        ),
    )
        .into_response()
}

fn grant_form(
    flow_token: &str,
    client: &str,
    username: Option<&str>,
    error: Option<&str>,
) -> Response {
    let error = error
        .map(|e| format!(r#"<p class="error">{}</p>"#, esc(e)))
        .unwrap_or_default();
    let username = username.map(esc).unwrap_or_default();
    layout(
        "Connect an app to your account",
        &format!(
            r#"<p>An app that identifies itself as <strong>{client}</strong> is asking for access to your podcast subscriptions and listening history.</p>
<p class="error"><strong>Only continue if you just started connecting an app yourself.</strong> If someone sent you this link, close this page: granting access would give them your sync data.</p>
<p class="muted">Access is granted through a new app password, which you can revoke later on the settings page.</p>
{error}
<form method="post" action="../grant">
<input type="hidden" name="flow_token" value="{token}">
<label>Username <input name="username" value="{username}" autocomplete="username" required></label>
<label>Password <input name="password" type="password" autocomplete="current-password" required autofocus></label>
<button type="submit">Grant access</button>
</form>"#,
            client = esc(client),
            token = esc(flow_token),
        ),
    )
}

pub async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(flow_token): Path<String>,
) -> Result<Response, ApiError> {
    let Some(client) = pending_flow_client(&state, &flow_token)? else {
        return Ok(invalid_flow_page());
    };
    let session = session_user(&state, &headers);
    let username = session.as_ref().map(|u| u.username.as_str());
    Ok(grant_form(&flow_token, &client, username, None))
}

#[derive(Deserialize)]
pub struct GrantForm {
    flow_token: String,
    username: String,
    password: String,
}

/// Grants access. The account password is always required, even with a session:
/// a one-click grant could be triggered by clickjacking or a same-site forged
/// request, handing an app password to whoever started the flow.
pub async fn grant(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    extensions: Extensions,
    Form(form): Form<GrantForm>,
) -> Result<Response, ApiError> {
    let Some(client) = pending_flow_client(&state, &form.flow_token)? else {
        return Ok(invalid_flow_page());
    };

    let ip = client_ip(&state, &headers, peer_addr(&extensions));
    let user = match authenticate(
        &state,
        ip,
        &form.username,
        &form.password,
        Credentials::AccountPasswordOnly,
    )
    .await
    {
        Ok(user) => user,
        Err(err) => {
            let (status, message) = login_error_status(&err);
            let page = grant_form(
                &form.flow_token,
                &client,
                Some(&form.username),
                Some(message),
            );
            return Ok((status, page).into_response());
        }
    };

    let granted = state.db.with(|conn| -> rusqlite::Result<bool> {
        let tx = conn.transaction()?;
        let app_password = create_app_password(&tx, user.id, &client)?;
        let granted = tx.execute(
            "UPDATE login_flows SET login_name = ?1, app_password = ?2
             WHERE flow_token = ?3 AND app_password IS NULL",
            params![user.username, app_password, form.flow_token],
        )? > 0;
        if granted {
            tx.commit()?;
        }
        Ok(granted)
    })?;
    if !granted {
        return Ok(invalid_flow_page());
    }

    Ok(layout(
        "Account connected",
        &format!(
            "<p><strong>{}</strong> is now connected to your account. You can close this window and return to the app.</p>",
            esc(&client)
        ),
    ))
}
