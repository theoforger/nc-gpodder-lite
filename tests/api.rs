use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use http_body_util::BodyExt;
use nc_gpodder_lite::db::Db;
use nc_gpodder_lite::{AppState, Config, app, users};
use serde_json::{Value, json};
use tower::ServiceExt;

const API: &str = "/index.php/apps/gpoddersync";
const ORIGIN: &str = "https://pods.example.com";

struct TestServer {
    app: Router,
    state: Arc<AppState>,
    auth: String,
}

fn basic(user: &str, pass: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
    )
}

fn server() -> TestServer {
    let db = Db::open_in_memory().unwrap();
    db.with(|conn| users::create_user(conn, "alice", "secret-pw"))
        .unwrap();
    db.with(|conn| users::create_user(conn, "bob", "bob-pw"))
        .unwrap();
    let state = AppState::new(
        db,
        Config {
            public_url: Some("https://pods.example.com".into()),
            allow_local_remote_servers: false,
            trusted_proxies: Vec::new(),
        },
    );
    let state = Arc::new(state);
    TestServer {
        app: app(state.clone()),
        state,
        auth: basic("alice", "secret-pw"),
    }
}

struct Resp {
    status: StatusCode,
    text: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|e| panic!("{e}: {}", self.text))
    }
}

impl TestServer {
    async fn send(&self, req: Request<Body>) -> Resp {
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Resp {
            status,
            text: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn get_as(&self, auth: &str, path: &str) -> Resp {
        self.send(
            Request::get(path)
                .header(header::AUTHORIZATION, auth)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn get(&self, path: &str) -> Resp {
        self.get_as(&self.auth.clone(), path).await
    }

    async fn post_json_as(&self, auth: &str, path: &str, body: Value) -> Resp {
        self.send(
            Request::post(path)
                .header(header::AUTHORIZATION, auth)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    async fn post_json(&self, path: &str, body: Value) -> Resp {
        self.post_json_as(&self.auth.clone(), path, body).await
    }

    async fn post_form(&self, path: &str, body: &str, cookie: Option<&str>) -> Resp {
        let mut req = Request::post(path)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::USER_AGENT, "AntennaPod/3.5");
        if let Some(cookie) = cookie {
            // Browsers send Origin with form POSTs; cookie-authenticated POSTs need a same-origin one.
            req = req
                .header(header::COOKIE, cookie)
                .header(header::ORIGIN, ORIGIN);
        }
        self.send(req.body(Body::from(body.to_owned())).unwrap())
            .await
    }

    /// Logs into the web UI and returns the session cookie (`name=value`).
    async fn web_login(&self, username: &str, password: &str) -> Result<String, StatusCode> {
        let res = self
            .app
            .clone()
            .oneshot(
                Request::post("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "username={username}&password={password}"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        if res.status() != StatusCode::SEE_OTHER {
            return Err(res.status());
        }
        let cookie = res.headers()[header::SET_COOKIE].to_str().unwrap();
        Ok(cookie.split(';').next().unwrap().to_owned())
    }

    /// Completes Login Flow v2 as `alice` and returns the issued app password.
    async fn app_password_via_login_flow(&self) -> String {
        let init = self.post_form("/index.php/login/v2", "", None).await.json();
        let poll_token = init["poll"]["token"].as_str().unwrap().to_owned();
        let flow_token = init["login"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .to_owned();
        let res = self
            .post_form(
                "/index.php/login/v2/grant",
                &format!("flow_token={flow_token}&username=alice&password=secret-pw"),
                None,
            )
            .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text);
        let res = self
            .post_form(
                "/index.php/login/v2/poll",
                &format!("token={poll_token}"),
                None,
            )
            .await;
        res.json()["appPassword"].as_str().unwrap().to_owned()
    }

    async fn actions(&self, query: &str) -> Vec<Value> {
        let res = self.get(&format!("{API}/episode_action{query}")).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text);
        res.json()["actions"].as_array().unwrap().clone()
    }
}

fn action(episode: &str, action: &str, timestamp: &str, position: i64) -> Value {
    json!({
        "podcast": "https://example.com/feed.xml",
        "episode": episode,
        "action": action,
        "timestamp": timestamp,
        "started": 0,
        "position": position,
        "total": 3600,
    })
}

#[tokio::test]
async fn requires_authentication() {
    let s = server();
    let res = s
        .send(
            Request::get(format!("{API}/subscriptions"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert_eq!(res.text, r#"{"message":"Current user is not logged in"}"#);

    let res = s
        .get_as(&basic("alice", "wrong"), &format!("{API}/subscriptions"))
        .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn subscriptions_roundtrip() {
    let s = server();
    let res = s
        .post_json(
            &format!("{API}/subscription_change/create"),
            json!({"add": ["https://a.example/feed", "https://b.example/feed", "not a url"], "remove": []}),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text);
    let created_at = res.json()["timestamp"].as_i64().unwrap();
    assert!(res.text.starts_with(r#"{"timestamp":"#));

    let res = s.get(&format!("{API}/subscriptions")).await;
    let body = res.json();
    assert_eq!(
        body["add"],
        json!(["https://a.example/feed", "https://b.example/feed"])
    );
    assert_eq!(body["remove"], json!([]));
    // URLs are escaped like PHP's json_encode.
    assert!(res.text.contains(r#""https:\/\/a.example\/feed""#));

    // Strict `updated > since`: nothing newer than the creation second.
    let res = s
        .get(&format!("{API}/subscriptions?since={created_at}"))
        .await;
    assert_eq!(res.json()["add"], json!([]));

    // Add and remove in one request: remove wins.
    s.post_json(
        &format!("{API}/subscription_change/create"),
        json!({"add": ["https://c.example/feed"], "remove": ["https://a.example/feed", "https://c.example/feed"]}),
    )
    .await;
    let body = s.get(&format!("{API}/subscriptions?since=0")).await.json();
    assert_eq!(body["add"], json!(["https://b.example/feed"]));
    assert_eq!(
        body["remove"],
        json!(["https://a.example/feed", "https://c.example/feed"])
    );

    // Same path without the /index.php prefix.
    let res = s.get("/apps/gpoddersync/subscriptions").await;
    assert_eq!(res.status, StatusCode::OK);

    // Data is per user.
    let res = s
        .get_as(&basic("bob", "bob-pw"), &format!("{API}/subscriptions"))
        .await;
    assert_eq!(res.json()["add"], json!([]));
}

#[tokio::test]
async fn subscription_create_requires_both_lists() {
    let s = server();
    let res = s
        .post_json(
            &format!("{API}/subscription_change/create"),
            json!({"add": []}),
        )
        .await;
    assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR);
    let res = s
        .post_json(
            &format!("{API}/subscription_change/create"),
            json!({"add": [1], "remove": []}),
        )
        .await;
    assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn episode_actions_roundtrip() {
    let s = server();
    let res = s
        .post_json(
            &format!("{API}/episode_action/create"),
            json!([
                {
                    "podcast": "https://example.com/feed.xml",
                    "episode": "https://example.com/ep1.mp3",
                    "guid": "ep1-guid",
                    "action": "play",
                    "timestamp": "2009-12-12T09:00:00",
                    "started": 15,
                    "position": 120,
                    "total": 500
                },
                {
                    "podcast": "https://example.com/feed.xml",
                    "episode": "https://example.com/ep1.mp3",
                    "action": "download",
                    "timestamp": "2009-12-12T09:05:00"
                }
            ]),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text);

    let res = s.get(&format!("{API}/episode_action")).await;
    assert_eq!(
        res.text
            .rsplit_once(r#","timestamp":"#)
            .unwrap()
            .0
            .to_owned()
            + "}",
        concat!(
            r#"{"actions":[{"podcast":"https:\/\/example.com\/feed.xml","episode":"https:\/\/example.com\/ep1.mp3","#,
            r#""timestamp":"2009-12-12T09:00:00+00:00","guid":"ep1-guid","position":120,"started":15,"total":500,"action":"PLAY"}]}"#
        )
    );

    // allActions includes non-play actions, with defaults of -1 and a null guid.
    let all = s.actions("?allActions=true").await;
    assert_eq!(all.len(), 2);
    assert_eq!(all[1]["action"], "DOWNLOAD");
    assert_eq!(all[1]["guid"], Value::Null);
    assert_eq!(all[1]["position"], -1);
    assert_eq!(all[1]["started"], -1);
    assert_eq!(all[1]["total"], -1);

    // `since` filters on the action's own timestamp, strictly.
    assert_eq!(s.actions("?since=1260608400&allActions=1").await.len(), 1);
    assert_eq!(s.actions("?since=1260608700&allActions=1").await.len(), 0);
    let res = s.get(&format!("{API}/episode_action?since=abc")).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn episode_action_upserts() {
    let s = server();
    let url = format!("{API}/episode_action/create");

    // Same episode + action: updated in place.
    s.post_json(
        &url,
        json!([action("https://e/1.mp3", "play", "2020-01-01T00:00:00", 10)]),
    )
    .await;
    s.post_json(
        &url,
        json!([action(
            "https://e/1.mp3",
            "PLAY",
            "2020-01-01T00:10:00",
            600
        )]),
    )
    .await;
    let actions = s.actions("").await;
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["position"], 600);
    assert_eq!(actions[0]["timestamp"], "2020-01-01T00:10:00+00:00");

    // A guid is matched even when the episode URL changed.
    let mut with_guid = action("https://e/2.mp3", "play", "2020-01-02T00:00:00", 5);
    with_guid["guid"] = json!("guid-2");
    s.post_json(&url, json!([with_guid])).await;
    let mut moved = action("https://e/2-moved.mp3", "play", "2020-01-02T00:01:00", 50);
    moved["guid"] = json!("guid-2");
    s.post_json(&url, json!([moved])).await;
    let actions = s.actions("").await;
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[1]["episode"], "https://e/2-moved.mp3");
    assert_eq!(actions[1]["position"], 50);

    // A null guid doesn't erase the stored guid.
    s.post_json(
        &url,
        json!([action(
            "https://e/2-moved.mp3",
            "play",
            "2020-01-02T00:02:00",
            70
        )]),
    )
    .await;
    let actions = s.actions("").await;
    assert_eq!(actions[1]["guid"], "guid-2");
    assert_eq!(actions[1]["position"], 70);

    // Guid match whose new episode URL collides with another row: the other row is replaced.
    let mut conflicting = action("https://e/1.mp3", "play", "2020-01-03T00:00:00", 99);
    conflicting["guid"] = json!("guid-2");
    s.post_json(&url, json!([conflicting])).await;
    let actions = s.actions("").await;
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(actions[0]["episode"], "https://e/1.mp3");
    assert_eq!(actions[0]["guid"], "guid-2");
    assert_eq!(actions[0]["position"], 99);
}

#[tokio::test]
async fn episode_action_validation() {
    let s = server();
    let url = format!("{API}/episode_action/create");
    let missing = json!([{"podcast": "p", "episode": "e", "action": "play"}]);
    assert_eq!(
        s.post_json(&url, missing).await.status,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let bad_time = json!([action("e", "play", "not a date", 1)]);
    assert_eq!(
        s.post_json(&url, bad_time).await.status,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let mut float_position = action("e", "play", "2020-01-01T00:00:00", 1);
    float_position["position"] = json!(1.5);
    assert_eq!(
        s.post_json(&url, json!([float_position])).await.status,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    // A non-list body carries no actions and is accepted.
    assert_eq!(
        s.post_json(&url, json!({"foo": "bar"})).await.status,
        StatusCode::OK
    );
    assert!(s.actions("?allActions=true").await.is_empty());
}

#[tokio::test]
async fn metrics() {
    let s = server();
    s.post_json(
        &format!("{API}/subscription_change/create"),
        json!({"add": ["https://example.com/feed.xml", "https://other.example/feed"], "remove": []}),
    )
    .await;
    s.post_json(
        &format!("{API}/episode_action/create"),
        json!([
            action("https://e/1.mp3", "play", "2020-01-01T00:00:00", 100),
            action("https://e/2.mp3", "play", "2020-01-01T00:00:00", 50),
            action("https://e/3.mp3", "play", "2020-01-01T00:00:00", -1),
            action("https://e/1.mp3", "download", "2020-01-01T00:00:00", 0),
            action("https://e/1.mp3", "delete", "2020-01-01T00:00:00", 0),
        ]),
    )
    .await;
    let res = s.get(&format!("{API}/personal_settings/metrics")).await;
    assert_eq!(
        res.json(),
        json!({"subscriptions": [
            {"url": "https://example.com/feed.xml", "listenedSeconds": 150,
             "actionCounts": {"delete": 1, "download": 1, "flattr": 0, "new": 0, "play": 3}},
            {"url": "https://other.example/feed", "listenedSeconds": 0,
             "actionCounts": {"delete": 0, "download": 0, "flattr": 0, "new": 0, "play": 0}},
        ]})
    );
}

#[tokio::test]
async fn podcast_data_checks() {
    let s = server();
    let res = s
        .get(&format!("{API}/personal_settings/podcast_data"))
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        res.text,
        r#"{"message":"Missing query parameter 'url'.","data":null}"#
    );

    let res = s
        .get(&format!(
            "{API}/personal_settings/podcast_data?url=https%3A%2F%2Fexample.com%2Ffeed"
        ))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.text, r#"{"data":null}"#);
}

#[tokio::test]
async fn status_php() {
    let s = server();
    let res = s
        .send(Request::get("/status.php").body(Body::empty()).unwrap())
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.json()["installed"], true);
}

#[tokio::test]
async fn login_flow_v2() {
    let s = server();
    let init = s.post_form("/index.php/login/v2", "", None).await;
    assert_eq!(init.status, StatusCode::OK, "{}", init.text);
    let init = init.json();
    let poll_token = init["poll"]["token"].as_str().unwrap().to_owned();
    assert_eq!(
        init["poll"]["endpoint"],
        "https://pods.example.com/index.php/login/v2/poll"
    );
    let login = init["login"].as_str().unwrap();
    let flow_path = login.strip_prefix("https://pods.example.com").unwrap();
    let flow_token = flow_path.rsplit('/').next().unwrap().to_owned();

    // Pending: 404 with an empty array.
    let poll_body = format!("token={poll_token}");
    let res = s
        .post_form("/index.php/login/v2/poll", &poll_body, None)
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert_eq!(res.text, "[]");

    // The browser page shows the requesting client.
    let page = s
        .send(Request::get(flow_path).body(Body::empty()).unwrap())
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text.contains("AntennaPod/3.5"));

    // Wrong password is rejected.
    let res = s
        .post_form(
            "/index.php/login/v2/grant",
            &format!("flow_token={flow_token}&username=alice&password=nope"),
            None,
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    let res = s
        .post_form(
            "/index.php/login/v2/grant",
            &format!("flow_token={flow_token}&username=alice&password=secret-pw"),
            None,
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text);

    let res = s
        .post_form("/index.php/login/v2/poll", &poll_body, None)
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.text);
    let creds = res.json();
    assert_eq!(creds["server"], "https://pods.example.com");
    assert_eq!(creds["loginName"], "alice");
    let app_password = creds["appPassword"].as_str().unwrap();
    assert_eq!(app_password.len(), 72);

    // Credentials are handed out only once.
    let res = s
        .post_form("/index.php/login/v2/poll", &poll_body, None)
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // The app password works for the sync API.
    let res = s
        .get_as(
            &basic("alice", app_password),
            &format!("{API}/subscriptions"),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    // ...but not for another user.
    let res = s
        .get_as(&basic("bob", app_password), &format!("{API}/subscriptions"))
        .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn web_login_and_settings() {
    let s = server();
    let res = s
        .send(Request::get("/settings").body(Body::empty()).unwrap())
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);

    let res = s
        .app
        .clone()
        .oneshot(
            Request::post("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("username=alice&password=secret-pw"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let cookie = res.headers()[header::SET_COOKIE].to_str().unwrap();
    let cookie = cookie.split(';').next().unwrap().to_owned();

    s.post_json(
        &format!("{API}/subscription_change/create"),
        json!({"add": ["https://example.com/feed.xml"], "remove": []}),
    )
    .await;
    let page = s
        .send(
            Request::get("/settings")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text.contains("https://example.com/feed.xml"));

    // The session cookie also authenticates the API (used by the page's JS).
    let res = s
        .send(
            Request::get(format!("{API}/personal_settings/metrics"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);

    let res = s
        .post_form("/settings/app-passwords", "name=Kasts", Some(&cookie))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.text.contains("Kasts"));
}

// Security regression tests (see the security audit fixes).

#[tokio::test]
async fn app_password_cannot_unlock_web_ui_or_grant() {
    let s = server();
    let app_password = s.app_password_via_login_flow().await;

    // No web login with an app password...
    assert_eq!(
        s.web_login("alice", &app_password).await,
        Err(StatusCode::FORBIDDEN)
    );

    // ...no password change with one, even from a valid session...
    let cookie = s.web_login("alice", "secret-pw").await.unwrap();
    let res = s
        .post_form(
            "/settings/password",
            &format!("current={app_password}&new=attacker-pw-1"),
            Some(&cookie),
        )
        .await;
    assert!(res.text.contains("The current password is wrong."));
    assert_eq!(
        s.get_as(
            &basic("alice", "secret-pw"),
            &format!("{API}/subscriptions")
        )
        .await
        .status,
        StatusCode::OK
    );

    // ...and no granting a new login flow with one.
    let init = s.post_form("/index.php/login/v2", "", None).await.json();
    let flow_token = init["login"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let res = s
        .post_form(
            "/index.php/login/v2/grant",
            &format!("flow_token={flow_token}&username=alice&password={app_password}"),
            None,
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // The app password still works for syncing.
    assert_eq!(
        s.get_as(
            &basic("alice", &app_password),
            &format!("{API}/subscriptions")
        )
        .await
        .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn cookie_authenticated_writes_require_same_origin() {
    let s = server();
    let cookie = s.web_login("alice", "secret-pw").await.unwrap();
    let unsubscribe = |origin: Option<&str>| {
        let mut req = Request::post(format!("{API}/subscription_change/create"))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "text/plain");
        if let Some(origin) = origin {
            req = req.header(header::ORIGIN, origin);
        }
        req.body(Body::from(
            r#"{"add":[],"remove":["https://x.example/feed"]}"#,
        ))
        .unwrap()
    };
    assert_eq!(
        s.send(unsubscribe(Some("https://evil.example.com")))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        s.send(unsubscribe(None)).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        s.send(unsubscribe(Some(ORIGIN))).await.status,
        StatusCode::OK
    );

    // A Referer from this server is accepted when Origin is absent.
    let res = s
        .send(
            Request::post("/settings/app-passwords")
                .header(header::COOKIE, &cookie)
                .header(header::REFERER, format!("{ORIGIN}/settings"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("name=Laptop"))
                .unwrap(),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);

    // Cross-origin login (login CSRF) is rejected too.
    let res = s
        .send(
            Request::post("/login")
                .header(header::ORIGIN, "https://evil.example.com")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("username=alice&password=secret-pw"))
                .unwrap(),
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Podcast clients (Basic auth, no cookie, no Origin) are unaffected.
    let res = s
        .post_json(
            &format!("{API}/subscription_change/create"),
            json!({"add": ["https://a.example/feed"], "remove": []}),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
}

#[tokio::test]
async fn login_flow_grant_always_requires_password() {
    let s = server();
    let cookie = s.web_login("alice", "secret-pw").await.unwrap();
    let init = s.post_form("/index.php/login/v2", "", None).await.json();
    let poll_token = init["poll"]["token"].as_str().unwrap().to_owned();
    let flow_token = init["login"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();

    // A session alone (same origin, no password) can't grant access.
    let res = s
        .post_form(
            "/index.php/login/v2/grant",
            &format!("flow_token={flow_token}"),
            Some(&cookie),
        )
        .await;
    assert_ne!(res.status, StatusCode::OK);
    let res = s
        .post_form(
            "/index.php/login/v2/poll",
            &format!("token={poll_token}"),
            None,
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // A flow can only be granted once.
    let grant = format!("flow_token={flow_token}&username=alice&password=secret-pw");
    assert_eq!(
        s.post_form("/index.php/login/v2/grant", &grant, None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        s.post_form("/index.php/login/v2/grant", &grant, None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn pages_cannot_be_framed_and_have_csp() {
    let s = server();
    let init = s.post_form("/index.php/login/v2", "", None).await.json();
    let flow_path = init["login"]
        .as_str()
        .unwrap()
        .strip_prefix(ORIGIN)
        .unwrap()
        .to_owned();
    let res = s
        .app
        .clone()
        .oneshot(Request::get(&flow_path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let headers = res.headers();
    assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    // Not `no-referrer`: browsers would then send `Origin: null` on the page's own
    // form posts, and the CSRF check would reject them.
    assert_eq!(headers[header::REFERRER_POLICY], "same-origin");
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(csp.contains("script-src 'nonce-"));
    let body =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(body.contains("Only continue if you just started connecting an app yourself."));
    assert!(body.contains(r#"<meta name="referrer" content="same-origin">"#));

    // API responses get the generic headers too.
    let res = s
        .app
        .clone()
        .oneshot(Request::get("/status.php").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
}

#[tokio::test]
async fn repeated_login_failures_are_throttled() {
    let s = server();
    let app_password = s.app_password_via_login_flow().await;
    let wrong = basic("alice", "wrong");
    let path = format!("{API}/subscriptions");
    // Concurrent attempts must not slip past the limit of 10 per username.
    let mut attempts = tokio::task::JoinSet::new();
    for _ in 0..15 {
        let request = Request::get(&path)
            .header(header::AUTHORIZATION, &wrong)
            .body(Body::empty())
            .unwrap();
        attempts.spawn(s.app.clone().oneshot(request));
    }
    let mut statuses = Vec::new();
    while let Some(res) = attempts.join_next().await {
        statuses.push(res.unwrap().unwrap().status());
    }
    let count = |status| statuses.iter().filter(|s| **s == status).count();
    assert_eq!(count(StatusCode::UNAUTHORIZED), 10, "{statuses:?}");
    assert_eq!(count(StatusCode::TOO_MANY_REQUESTS), 5, "{statuses:?}");

    // Further attempts are refused, even with the right password...
    let res = s.get(&path).await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        s.web_login("alice", "secret-pw").await,
        Err(StatusCode::TOO_MANY_REQUESTS)
    );
    // ...but app passwords keep working, so synced devices aren't locked out.
    assert_eq!(
        s.get_as(&basic("alice", &app_password), &path).await.status,
        StatusCode::OK
    );
    // Other users are unaffected.
    assert_eq!(
        s.get_as(&basic("bob", "bob-pw"), &path).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn password_change_logs_out_other_sessions() {
    let s = server();
    let current = s.web_login("alice", "secret-pw").await.unwrap();
    let other = s.web_login("alice", "secret-pw").await.unwrap();
    let res = s
        .post_form(
            "/settings/password",
            "current=secret-pw&new=new-password-1",
            Some(&current),
        )
        .await;
    assert!(res.text.contains("Password changed."), "{}", res.text);

    let settings = |cookie: &str| {
        Request::get("/settings")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(s.send(settings(&current)).await.status, StatusCode::OK);
    assert_eq!(s.send(settings(&other)).await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn large_bodies_only_allowed_for_sync_uploads() {
    let s = server();
    let big = "a".repeat(3 * 1024 * 1024);
    let res = s
        .send(
            Request::post("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("username=alice&password={big}")))
                .unwrap(),
        )
        .await;
    assert_eq!(res.status, StatusCode::PAYLOAD_TOO_LARGE);

    let urls: Vec<String> = (0..40_000)
        .map(|i| format!("https://example.com/podcast/{i:08}/feed.xml"))
        .collect();
    let res = s
        .post_json(
            &format!("{API}/subscription_change/create"),
            json!({"add": urls, "remove": []}),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
}

#[tokio::test]
async fn podcast_data_fetch_errors_are_generic() {
    let s = server();
    s.post_json(
        &format!("{API}/subscription_change/create"),
        json!({"add": ["http://127.0.0.1:9/feed.xml"], "remove": []}),
    )
    .await;
    let res = s
        .get(&format!(
            "{API}/personal_settings/podcast_data?url=http%3A%2F%2F127.0.0.1%3A9%2Ffeed.xml"
        ))
        .await;
    assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(res.text, r#"{"message":"Failed to fetch podcast data"}"#);
}

#[tokio::test]
async fn remembered_passwords_stop_working_after_a_password_change() {
    let s = server();
    let path = format!("{API}/subscriptions");
    // The first request verifies the password with Argon2; the second is served
    // from the verified-password cache.
    assert_eq!(s.get(&path).await.status, StatusCode::OK);
    assert_eq!(s.get(&path).await.status, StatusCode::OK);

    // Change the password directly in the database, as `user passwd` does from
    // another process while the server is running.
    s.state
        .db
        .with(|conn| users::set_password(conn, "alice", "changed-pw-1"))
        .unwrap();
    assert_eq!(s.get(&path).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        s.get_as(&basic("alice", "changed-pw-1"), &path)
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn gzip_is_used_only_when_requested() {
    let s = server();
    let request = |encoding: Option<&str>| {
        let mut req = Request::get(format!("{API}/episode_action?since=0"))
            .header(header::AUTHORIZATION, basic("alice", "secret-pw"));
        if let Some(encoding) = encoding {
            req = req.header(header::ACCEPT_ENCODING, encoding);
        }
        req.body(Body::empty()).unwrap()
    };
    let plain = s.app.clone().oneshot(request(None)).await.unwrap();
    assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
    let gzipped = s.app.clone().oneshot(request(Some("gzip"))).await.unwrap();
    assert_eq!(gzipped.headers()[header::CONTENT_ENCODING], "gzip");
}

#[tokio::test]
async fn overview_page_shows_only_the_users_data() {
    let s = server();
    let page = |cookie: Option<&str>| {
        let mut req = Request::get("/dashboard");
        if let Some(cookie) = cookie {
            req = req.header(header::COOKIE, cookie);
        }
        req.body(Body::empty()).unwrap()
    };
    assert_eq!(s.send(page(None)).await.status, StatusCode::SEE_OTHER);

    // A feed URL that tries to close the embedded <script> element.
    let hostile = "https://evil.example/</script><script>alert(1)</script>";
    s.post_json(
        &format!("{API}/subscription_change/create"),
        json!({"add": ["https://alice.example/feed", hostile], "remove": []}),
    )
    .await;
    s.post_json(
        &format!("{API}/episode_action/create"),
        json!([action(
            "https://alice.example/ep1.mp3",
            "play",
            "2026-01-01T00:00:00",
            600
        )]),
    )
    .await;
    s.post_json_as(
        &basic("bob", "bob-pw"),
        &format!("{API}/subscription_change/create"),
        json!({"add": ["https://bob.example/feed"], "remove": []}),
    )
    .await;

    let cookie = s.web_login("alice", "secret-pw").await.unwrap();
    let res = s.send(page(Some(&cookie))).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.text.contains(r#"https:\/\/alice.example\/feed"#));
    assert!(!res.text.contains("bob.example"));
    assert!(!res.text.contains("</script><script>alert"));
    assert!(
        res.text
            .contains(r#"\u003C\/script\u003E\u003Cscript\u003Ealert(1)"#)
    );

    let start = res.text.find(r#"id="overview-data">"#).unwrap() + r#"id="overview-data">"#.len();
    let end = start + res.text[start..].find("</script>").unwrap();
    let data: Value = serde_json::from_str(&res.text[start..end]).unwrap();
    assert_eq!(data["subscribed"], 2);
    assert_eq!(data["episodes_played"], 1);
    assert_eq!(data["listened_seconds"], 600);
    assert_eq!(data["weeks"].as_array().unwrap().len(), 52);
    assert_eq!(
        data["top_podcasts"][0]["url"],
        "https://example.com/feed.xml"
    );
}
