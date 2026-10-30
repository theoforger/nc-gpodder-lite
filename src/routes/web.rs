//! Minimal web UI: login, the personal-settings page (subscriptions with metrics
//! and podcast info, like upstream's Nextcloud settings section), app passwords,
//! and password change.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use chrono::DateTime;
use serde::Deserialize;

use super::personal_settings::podcast_metrics;
use crate::AppState;
use crate::auth::{
    Credentials, LoginError, SESSION_COOKIE, authenticate, cookie, random_token, session_user,
    sha256_hex,
};
use crate::db::now;
use crate::json::ApiError;
use crate::security::{client_ip, peer_addr};
use crate::users;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(|| async { Redirect::to("/settings") }))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/settings", get(settings_page))
        .route("/settings/app-passwords", post(create_app_password))
        .route(
            "/settings/app-passwords/{id}/delete",
            post(delete_app_password),
        )
        .route("/settings/password", post(change_password))
}

pub fn esc(s: &str) -> String {
    html_escape::encode_double_quoted_attribute(s).into_owned()
}

pub fn layout(title: &str, body: &str) -> Response {
    page(title, body, None)
}

/// Renders an HTML page with a strict Content-Security-Policy. Inline style and
/// script are allowed only through a per-response nonce.
pub fn page(title: &str, body: &str, script: Option<&str>) -> Response {
    let nonce = random_token(32);
    let script = script
        .map(|s| format!(r#"<script nonce="{nonce}">{s}</script>"#))
        .unwrap_or_default();
    let csp = format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; \
         img-src data:; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; \
         base-uri 'none'"
    );
    let html = Html(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="same-origin">
<title>{title} · nc-gpodder-lite</title>
<style nonce="{nonce}">
body {{ font-family: system-ui, sans-serif; max-width: 60rem; margin: 2rem auto; padding: 0 1rem; color: #222; }}
h1 {{ font-size: 1.5rem; }} h2 {{ font-size: 1.2rem; margin-top: 2rem; }}
form {{ margin: 0.5rem 0; }}
label {{ display: block; margin: 0.5rem 0; }}
input {{ padding: 0.3rem; font: inherit; }}
button {{ padding: 0.3rem 0.8rem; font: inherit; cursor: pointer; }}
table {{ border-collapse: collapse; width: 100%; }}
th, td {{ text-align: left; padding: 0.4rem; border-bottom: 1px solid #ddd; vertical-align: top; }}
.podcast {{ display: flex; gap: 0.75rem; align-items: flex-start; }}
.podcast img {{ width: 3rem; height: 3rem; object-fit: cover; border-radius: 4px; }}
.muted {{ color: #666; font-size: 0.9em; overflow-wrap: anywhere; }}
.error {{ color: #b00020; }} .notice {{ background: #eef6ee; padding: 0.75rem; border-radius: 4px; }}
code {{ background: #f3f3f3; padding: 0.1rem 0.3rem; border-radius: 3px; overflow-wrap: anywhere; }}
header {{ display: flex; justify-content: space-between; align-items: center; }}
</style>
</head>
<body>
<h1>{title}</h1>
{body}
{script}
</body>
</html>"#,
        title = esc(title),
    ));
    let csp = HeaderValue::from_str(&csp).expect("CSP is a valid header value");
    ([(header::CONTENT_SECURITY_POLICY, csp)], html).into_response()
}

fn login_form(error: Option<&str>) -> Response {
    let error = error
        .map(|e| format!(r#"<p class="error">{}</p>"#, esc(e)))
        .unwrap_or_default();
    layout(
        "Log in",
        &format!(
            r#"{error}<form method="post" action="/login">
<label>Username <input name="username" autocomplete="username" required autofocus></label>
<label>Password <input name="password" type="password" autocomplete="current-password" required></label>
<button type="submit">Log in</button>
</form>"#
        ),
    )
}

pub fn login_error_status(err: &LoginError) -> (StatusCode, &'static str) {
    match err {
        LoginError::Invalid => (StatusCode::FORBIDDEN, "Wrong username or password."),
        LoginError::Throttled => (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed login attempts. Try again later.",
        ),
    }
}

fn login_error_page(err: LoginError) -> Response {
    let (status, message) = login_error_status(&err);
    (status, login_form(Some(message))).into_response()
}

async fn login_page(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if session_user(&state, &headers).is_some() {
        return Redirect::to("/settings").into_response();
    }
    login_form(None).into_response()
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    extensions: Extensions,
    Form(form): Form<LoginForm>,
) -> Result<Response, ApiError> {
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
        Err(err) => return Ok(login_error_page(err)),
    };
    let token = random_token(64);
    state.db.with(|conn| {
        conn.execute(
            "DELETE FROM sessions WHERE created_at < ?1",
            [now() - crate::auth::SESSION_LIFETIME_SECS],
        )?;
        conn.execute(
            "INSERT INTO sessions (token_hash, user_id, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![sha256_hex(&token), user.id, now()],
        )
    })?;
    let secure = if state.is_https(&headers) {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        crate::auth::SESSION_LIFETIME_SECS
    );
    let mut res = Redirect::to("/settings").into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    Ok(res)
}

async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = cookie(&headers, SESSION_COOKIE) {
        state.db.with(|conn| {
            conn.execute(
                "DELETE FROM sessions WHERE token_hash = ?1",
                [sha256_hex(token)],
            )
        })?;
    }
    let mut res = Redirect::to("/login").into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"
        ))
        .unwrap(),
    );
    Ok(res)
}

fn format_date(epoch: i64) -> String {
    DateTime::from_timestamp(epoch, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

fn format_duration(seconds: i64) -> String {
    let (h, m) = (seconds / 3600, (seconds % 3600) / 60);
    if h > 0 {
        format!("{h} h {m} min")
    } else {
        format!("{m} min")
    }
}

async fn settings_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    render_settings(&state, &headers, None).await
}

async fn render_settings(
    state: &AppState,
    headers: &HeaderMap,
    notice: Option<String>,
) -> Result<Response, ApiError> {
    let Some(user) = session_user(state, headers) else {
        return Ok(Redirect::to("/login").into_response());
    };
    let (metrics, app_passwords) = state.db.with(|conn| -> rusqlite::Result<_> {
        Ok((
            podcast_metrics(conn, &user.username)?,
            users::list_app_passwords(conn, user.id)?,
        ))
    })?;

    let mut subscriptions = String::new();
    for m in &metrics {
        let cached = state.podcast_data.cached(&m.url);
        let title = cached
            .as_ref()
            .and_then(|d| d.title.clone())
            .unwrap_or_else(|| m.url.clone());
        let image = cached
            .as_ref()
            .and_then(|d| d.image_blob.clone())
            .map(|src| format!(r#"<img alt="" src="{}">"#, esc(&src)))
            .unwrap_or_default();
        let c = &m.action_counts;
        subscriptions.push_str(&format!(
            r#"<tr data-url="{url}">
<td><div class="podcast"><span class="cover">{image}</span><div><strong class="title">{title}</strong><div class="muted">{url}</div></div></div></td>
<td>{listened}</td>
<td class="muted">play {play} · download {download} · new {new} · delete {delete} · flattr {flattr}</td>
<td><button type="button" class="unsubscribe">Unsubscribe</button></td>
</tr>"#,
            url = esc(&m.url),
            title = esc(&title),
            listened = format_duration(m.listened_seconds),
            play = c.play,
            download = c.download,
            new = c.new,
            delete = c.delete,
            flattr = c.flattr,
        ));
    }
    if metrics.is_empty() {
        subscriptions.push_str(
            r#"<tr><td colspan="4" class="muted">No subscriptions synced yet.</td></tr>"#,
        );
    }

    let mut passwords = String::new();
    for p in &app_passwords {
        passwords.push_str(&format!(
            r#"<tr><td>{name}</td><td>{created}</td><td>{used}</td>
<td><form method="post" action="/settings/app-passwords/{id}/delete"><button type="submit">Revoke</button></form></td></tr>"#,
            name = esc(&p.name),
            created = format_date(p.created_at),
            used = p.last_used.map(format_date).unwrap_or_else(|| "never".into()),
            id = p.id,
        ));
    }
    if app_passwords.is_empty() {
        passwords.push_str(r#"<tr><td colspan="4" class="muted">No app passwords.</td></tr>"#);
    }

    let notice = notice
        .map(|n| format!(r#"<div class="notice">{n}</div>"#))
        .unwrap_or_default();
    let body = format!(
        r#"<header><span>Logged in as <strong>{username}</strong></span>
<form method="post" action="/logout"><button type="submit">Log out</button></form></header>
{notice}
<p class="muted">Server address for podcast apps (Nextcloud sync): <code>{base}</code></p>

<h2>Subscriptions</h2>
<table>
<thead><tr><th>Podcast</th><th>Listened</th><th>Actions</th><th></th></tr></thead>
<tbody>{subscriptions}</tbody>
</table>

<h2>App passwords</h2>
<p class="muted">Apps that use the Nextcloud login flow create these automatically. You can also create one for apps that ask for a username and password.</p>
<table>
<thead><tr><th>Name</th><th>Created</th><th>Last used</th><th></th></tr></thead>
<tbody>{passwords}</tbody>
</table>
<form method="post" action="/settings/app-passwords">
<label>New app password name <input name="name" required placeholder="e.g. Kasts on laptop"></label>
<button type="submit">Create app password</button>
</form>

<h2>Change password</h2>
<form method="post" action="/settings/password">
<label>Current password <input name="current" type="password" autocomplete="current-password" required></label>
<label>New password <input name="new" type="password" autocomplete="new-password" required minlength="8"></label>
<button type="submit">Change password</button>
</form>"#,
        username = esc(&user.username),
        base = esc(&state.base_url(headers)),
    );
    Ok(page("Podcast sync", &body, Some(SETTINGS_SCRIPT)))
}

#[derive(Deserialize)]
struct NewAppPasswordForm {
    name: String,
}

async fn create_app_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<NewAppPasswordForm>,
) -> Result<Response, ApiError> {
    let Some(user) = session_user(&state, &headers) else {
        return Ok(Redirect::to("/login").into_response());
    };
    let name = form.name.trim();
    let name = if name.is_empty() {
        "App password"
    } else {
        name
    };
    let token = state
        .db
        .with(|conn| users::create_app_password(conn, user.id, name))?;
    let notice = format!(
        "App password <strong>{}</strong> created. Use it with username <code>{}</code>. It won't be shown again:<br><code>{}</code>",
        esc(name),
        esc(&user.username),
        esc(&token)
    );
    render_settings(&state, &headers, Some(notice)).await
}

async fn delete_app_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let Some(user) = session_user(&state, &headers) else {
        return Ok(Redirect::to("/login").into_response());
    };
    state
        .db
        .with(|conn| users::delete_app_password(conn, user.id, id))?;
    Ok(Redirect::to("/settings").into_response())
}

#[derive(Deserialize)]
struct ChangePasswordForm {
    current: String,
    new: String,
}

async fn change_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    extensions: Extensions,
    Form(form): Form<ChangePasswordForm>,
) -> Result<Response, ApiError> {
    let Some(user) = session_user(&state, &headers) else {
        return Ok(Redirect::to("/login").into_response());
    };
    let ip = client_ip(&state, &headers, peer_addr(&extensions));
    let notice = if form.new.chars().count() < 8 {
        "The new password must be at least 8 characters long."
    } else if let Err(err) = authenticate(
        &state,
        ip,
        &user.username,
        &form.current,
        Credentials::AccountPasswordOnly,
    )
    .await
    {
        match err {
            LoginError::Invalid => "The current password is wrong.",
            LoginError::Throttled => "Too many failed attempts. Try again later.",
        }
    } else {
        let new = form.new.clone();
        let username = user.username.clone();
        // Argon2 hashing is CPU-heavy; keep it off the async runtime.
        let hash = tokio::task::spawn_blocking(move || crate::auth::hash_password(&new))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?;
        let current_session = cookie(&headers, SESSION_COOKIE).map(sha256_hex);
        state.db.with(|conn| -> rusqlite::Result<()> {
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE users SET password_hash = ?1 WHERE username = ?2",
                rusqlite::params![hash, username],
            )?;
            // Log out every other browser session; app passwords stay valid, as in Nextcloud.
            tx.execute(
                "DELETE FROM sessions WHERE user_id = ?1 AND token_hash IS NOT ?2",
                rusqlite::params![user.id, current_session],
            )?;
            tx.commit()
        })?;
        "Password changed. Other browser sessions have been logged out."
    };
    render_settings(&state, &headers, Some(notice.to_owned())).await
}

/// Unsubscribe buttons and lazy loading of podcast titles and covers.
const SETTINGS_SCRIPT: &str = r#"
const api = "/index.php/apps/gpoddersync/";
for (const row of document.querySelectorAll("tr[data-url]")) {
  const url = row.dataset.url;
  row.querySelector(".unsubscribe").addEventListener("click", async (ev) => {
    if (!confirm("Unsubscribe from " + url + "?")) return;
    ev.target.disabled = true;
    const res = await fetch(api + "subscription_change/create", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ add: [], remove: [url] }),
    });
    if (res.ok) row.remove(); else ev.target.disabled = false;
  });
  if (row.querySelector(".cover img")) continue;
  fetch(api + "personal_settings/podcast_data?url=" + encodeURIComponent(url))
    .then((res) => res.ok ? res.json() : null)
    .then((json) => {
      const data = json && json.data;
      if (!data) return;
      if (data.title) row.querySelector(".title").textContent = data.title;
      if (data.imageBlob) {
        const img = document.createElement("img");
        img.alt = "";
        img.src = data.imageBlob;
        row.querySelector(".cover").replaceChildren(img);
      }
    })
    .catch(() => {});
}
"#;
