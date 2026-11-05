//! Minimal web UI: login, the personal-settings page (subscriptions with metrics
//! and podcast info, like upstream's Nextcloud settings section), app passwords,
//! and password change.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use chrono::DateTime;
use serde::Deserialize;

use super::personal_settings::podcast_metrics;
use crate::AppState;
use crate::auth::{
    AuthUser, Credentials, LoginError, SESSION_COOKIE, SESSION_LIFETIME_SECS, authenticate, cookie,
    random_token, session_user, sha256_hex,
};
use crate::db::now;
use crate::json::ApiError;
use crate::security::ClientIp;
use crate::users;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(|| async { Redirect::to("/dashboard") }))
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

/// The user logged in to the web UI; requests without a session are redirected
/// to the login page.
pub(super) struct WebUser(pub AuthUser);

impl FromRequestParts<Arc<AppState>> for WebUser {
    type Rejection = Redirect;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        session_user(state, &parts.headers)
            .map(WebUser)
            .ok_or_else(|| Redirect::to("/login"))
    }
}

/// Site-wide styles. Colors are roles (CSS custom properties) with separately
/// chosen light and dark values from the data-viz reference palette.
const STYLE: &str = r#"
:root {
  color-scheme: light;
  --page: #f9f9f7; --surface: #fcfcfb; --ink: #0b0b0b; --ink-2: #52514e; --muted: #898781;
  --grid: #e1e0d9; --axis: #c3c2b7; --ring: rgba(11,11,11,0.10); --wash: #f0efec;
  --series-1: #2a78d6; --series-2: #eb6834; --series-3: #1baf7a; --series-4: #eda100;
  --other: #898781; --track: #cde2fb; --critical: #d03b3b;
  --on-series-1: #fff; --on-series-2: #fff; --on-series-3: #fff; --on-series-4: #0b0b0b; --on-other: #fff;
}
@media (prefers-color-scheme: dark) {
  :root {
    color-scheme: dark;
    --page: #0d0d0d; --surface: #1a1a19; --ink: #f0efec; --ink-2: #c3c2b7; --muted: #898781;
    --grid: #2c2c2a; --axis: #383835; --ring: rgba(255,255,255,0.10); --wash: #2c2c2a;
    --series-1: #3987e5; --series-2: #d95926; --series-3: #199e70; --series-4: #c98500;
    --other: #6b6a66; --track: #104281; --critical: #e66767;
    --on-series-4: #fff;
  }
}
body { font-family: system-ui, -apple-system, "Segoe UI", sans-serif; max-width: 64rem; margin: 2rem auto;
  padding: 0 1rem; color: var(--ink); background: var(--page); }
h1 { font-size: 1.5rem; } h2 { font-size: 1.15rem; margin: 2rem 0 0.75rem; }
a { color: var(--series-1); }
form { margin: 0.5rem 0; }
label { display: block; margin: 0.5rem 0; }
input { padding: 0.3rem; font: inherit; }
button { padding: 0.3rem 0.8rem; font: inherit; cursor: pointer; }
table { border-collapse: collapse; width: 100%; }
th, td { text-align: left; padding: 0.4rem; border-bottom: 1px solid var(--grid); vertical-align: top; }
th { color: var(--ink-2); font-weight: 600; }
td.num, th.num { text-align: right; font-variant-numeric: tabular-nums; }
.podcast { display: flex; gap: 0.75rem; align-items: flex-start; }
.podcast img { width: 3rem; height: 3rem; object-fit: cover; border-radius: 4px; }
.muted { color: var(--ink-2); font-size: 0.9em; overflow-wrap: anywhere; }
.error { color: var(--critical); } .notice { background: var(--wash); padding: 0.75rem; border-radius: 4px; }
code { background: var(--wash); padding: 0.1rem 0.3rem; border-radius: 3px; overflow-wrap: anywhere; }
header { display: flex; justify-content: space-between; align-items: center; gap: 1rem; margin-bottom: 1rem; }
nav.site { display: flex; gap: 1.25rem; align-items: center; }
nav.site a { text-decoration: none; color: var(--ink-2); padding-bottom: 2px; }
nav.site a[aria-current="page"] { color: var(--ink); border-bottom: 2px solid var(--series-1); }
.account { display: flex; gap: 0.75rem; align-items: center; }
.account form { margin: 0; }

/* Overview page */
.kpis { display: grid; grid-template-columns: repeat(auto-fit, minmax(10rem, 1fr)); gap: 0.75rem; }
.tile, .card { background: var(--surface); border: 1px solid var(--ring); border-radius: 8px; }
.tile { padding: 0.9rem 1rem; }
.tile .label { color: var(--ink-2); font-size: 0.85rem; }
.tile .value { font-size: 1.6rem; font-weight: 600; margin-top: 0.2rem; }
.tile .sub { color: var(--ink-2); font-size: 0.8rem; margin-top: 0.15rem; }
.card { padding: 1rem 1.25rem; margin-top: 0.75rem; }
.card h2 { margin: 0; font-size: 1rem; }
.card .caption { color: var(--ink-2); font-size: 0.85rem; margin: 0.2rem 0 0.75rem; }
.card details { margin-top: 0.75rem; }
.card summary { color: var(--ink-2); font-size: 0.85rem; cursor: pointer; }
.chart svg { display: block; width: 100%; overflow: visible; }
.chart svg:focus-visible, .seg-block:focus-visible, .hbar-row:focus-visible { outline: 2px solid var(--series-1); outline-offset: 2px; }
.chart text { fill: var(--muted); font-size: 11px; font-variant-numeric: tabular-nums; }
.seg { display: flex; gap: 2px; height: 28px; }
.seg-block { border-radius: 4px; display: flex; align-items: center; padding: 0 6px; min-width: 0;
  font-size: 0.8rem; font-weight: 600; white-space: nowrap; }
.seg-block:hover, .hbar-row:hover .hbar { filter: brightness(1.12); }
.legend { display: flex; flex-wrap: wrap; gap: 0.4rem 1rem; margin-top: 0.6rem; font-size: 0.85rem; color: var(--ink-2); }
.legend span::before { content: ""; display: inline-block; width: 10px; height: 10px; border-radius: 2px;
  margin-right: 0.35rem; background: var(--swatch); vertical-align: -1px; }
.hbar-row { display: grid; grid-template-columns: minmax(0, 1fr); gap: 0.2rem; padding: 0.35rem 0; }
.hbar-name { font-size: 0.9rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.hbar-line { display: flex; align-items: center; gap: 0.5rem; }
.hbar { height: 12px; background: var(--series-1); border-radius: 0 4px 4px 0; min-width: 2px; }
.hbar-value { font-size: 0.85rem; color: var(--ink-2); white-space: nowrap; font-variant-numeric: tabular-nums; }
.meter { width: 6rem; height: 4px; border-radius: 2px; background: var(--track); overflow: hidden; display: inline-block;
  vertical-align: middle; margin-right: 0.5rem; }
.meter > i { display: block; height: 100%; background: var(--series-1); border-radius: 2px; }
.dot::before { content: ""; display: inline-block; width: 8px; height: 8px; border-radius: 50%;
  margin-right: 0.4rem; background: var(--swatch); }
.tooltip { position: fixed; pointer-events: none; z-index: 10; background: var(--surface); color: var(--ink);
  border: 1px solid var(--ring); border-radius: 6px; padding: 0.4rem 0.6rem; font-size: 0.85rem;
  box-shadow: 0 2px 8px rgba(0,0,0,0.12); }
.tooltip strong { display: block; font-size: 0.95rem; }
.tooltip span { color: var(--ink-2); }
.empty { color: var(--ink-2); padding: 1rem 0; }
.scroll { overflow-x: auto; }
td.nowrap { white-space: nowrap; }
td.episode { white-space: nowrap; max-width: 16rem; overflow: hidden; text-overflow: ellipsis; }
"#;

/// Navigation shown on pages for logged-in users.
pub fn user_nav(user: &AuthUser, active: &str) -> String {
    let link = |href: &str, label: &str| {
        let current = if href == active {
            r#" aria-current="page""#
        } else {
            ""
        };
        format!(r#"<a href="{href}"{current}>{label}</a>"#)
    };
    format!(
        r#"<header><nav class="site">{overview}{settings}</nav>
<div class="account"><span class="muted">{username}</span>
<form method="post" action="/logout"><button type="submit">Log out</button></form></div></header>"#,
        overview = link("/dashboard", "Overview"),
        settings = link("/settings", "Settings"),
        username = esc(&user.username),
    )
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
<style nonce="{nonce}">{STYLE}</style>
</head>
<body>
<h1>{title}</h1>
{body}
{script}
</body>
</html>"#,
        title = esc(title),
        STYLE = STYLE,
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

fn login_error_page(err: &LoginError) -> Response {
    let (status, message) = login_error_status(err);
    (status, login_form(Some(message))).into_response()
}

async fn login_page(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if session_user(&state, &headers).is_some() {
        return Redirect::to("/dashboard").into_response();
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
    ClientIp(ip): ClientIp,
    Form(form): Form<LoginForm>,
) -> Result<Response, ApiError> {
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
        Err(err) => return Ok(login_error_page(&err)),
    };
    let token = random_token(64);
    state.db.with(|conn| {
        conn.execute(
            "DELETE FROM sessions WHERE created_at < ?1",
            [now() - SESSION_LIFETIME_SECS],
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
        SESSION_LIFETIME_SECS
    );
    let mut res = Redirect::to("/dashboard").into_response();
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
    WebUser(user): WebUser,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    render_settings(&state, &headers, &user, None)
}

fn render_settings(
    state: &AppState,
    headers: &HeaderMap,
    user: &AuthUser,
    notice: Option<String>,
) -> Result<Response, ApiError> {
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
        r#"{nav}
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
        nav = user_nav(user, "/settings"),
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
    WebUser(user): WebUser,
    headers: HeaderMap,
    Form(form): Form<NewAppPasswordForm>,
) -> Result<Response, ApiError> {
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
    render_settings(&state, &headers, &user, Some(notice))
}

async fn delete_app_password(
    State(state): State<Arc<AppState>>,
    WebUser(user): WebUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
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
    WebUser(user): WebUser,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Form(form): Form<ChangePasswordForm>,
) -> Result<Response, ApiError> {
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
    render_settings(&state, &headers, &user, Some(notice.to_owned()))
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
