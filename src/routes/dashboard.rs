//! Overview page: charts and tables of everything the logged-in user has synced.
//!
//! The server aggregates in SQL and embeds the result as JSON; a small script
//! renders it. Raw action history never goes to the browser beyond the most
//! recent entries.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use rusqlite::{Connection, params};
use serde::Serialize;

use super::web::{WebUser, page, user_nav};
use crate::AppState;
use crate::db::now;
use crate::json::{ApiError, to_php_json};

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/dashboard", get(dashboard))
}

const WEEK_SECS: i64 = 7 * 24 * 60 * 60;
/// 1970-01-05, the first Monday after the epoch: weeks start on Monday (UTC).
const FIRST_MONDAY: i64 = 4 * 24 * 60 * 60;
const WEEKS_SHOWN: i64 = 52;
const TOP_PODCASTS: i64 = 10;
const RECENT_ACTIONS: i64 = 50;

#[derive(Serialize)]
struct Overview {
    now: i64,
    subscribed: i64,
    unsubscribed: i64,
    episodes_played: i64,
    listened_seconds: i64,
    apps: i64,
    apps_last_used: Option<i64>,
    latest_activity: Option<i64>,
    /// Action counts by type, most frequent first.
    actions: Vec<ActionCount>,
    /// Episodes by the week (Monday 00:00 UTC) of their latest play, oldest first.
    weeks: Vec<Week>,
    top_podcasts: Vec<TopPodcast>,
    recent: Vec<RecentAction>,
    subscriptions: Vec<Subscription>,
    /// Feed titles already known from the podcast data cache, by URL.
    titles: HashMap<String, String>,
}

#[derive(Serialize)]
struct ActionCount {
    action: String,
    count: i64,
}

#[derive(Serialize)]
struct Week {
    start: i64,
    plays: i64,
}

#[derive(Serialize)]
struct TopPodcast {
    url: String,
    listened_seconds: i64,
    plays: i64,
}

#[derive(Serialize)]
struct RecentAction {
    podcast: String,
    episode: String,
    action: String,
    timestamp: i64,
    position: i64,
    total: i64,
}

#[derive(Serialize)]
struct Subscription {
    url: String,
    subscribed: bool,
    updated: i64,
}

/// Seconds listened per play action, counted like upstream's metrics: the playback
/// position, ignoring 0 and the -1 "unknown" marker.
const LISTENED: &str =
    "CASE WHEN action = 'PLAY' AND position NOT IN (0, -1) THEN position ELSE 0 END";

fn overview(conn: &Connection, user_id: i64, username: &str) -> rusqlite::Result<Overview> {
    let now = now();

    let (subscribed, unsubscribed, subscriptions_updated) = conn.query_row(
        "SELECT COALESCE(SUM(subscribed = 1), 0), COALESCE(SUM(subscribed = 0), 0), MAX(updated)
         FROM gpodder_subscriptions WHERE user_id = ?1",
        [username],
        |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<i64>>(2)?)),
    )?;
    let (apps, apps_last_used) = conn.query_row(
        "SELECT COUNT(*), MAX(last_used) FROM app_passwords WHERE user_id = ?1",
        [user_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    let mut actions = Vec::new();
    let (mut episodes_played, mut listened_seconds) = (0, 0);
    let mut latest_action: Option<i64> = None;
    let mut stmt = conn.prepare(&format!(
        "SELECT action, COUNT(*), SUM({LISTENED}), MAX(timestamp_epoch)
         FROM gpodder_episode_action WHERE user_id = ?1 AND timestamp_epoch > 0
         GROUP BY action ORDER BY COUNT(*) DESC, action"
    ))?;
    let rows = stmt.query_map([username], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (action, count, listened, latest) = row?;
        if action == "PLAY" {
            episodes_played = count;
            listened_seconds = listened;
        }
        latest_action = latest_action.max(Some(latest));
        actions.push(ActionCount { action, count });
    }

    let current_week = (now - FIRST_MONDAY).div_euclid(WEEK_SECS);
    let first_week = current_week - WEEKS_SHOWN + 1;
    let mut plays_by_week = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT (timestamp_epoch - ?3) / ?4 AS week, COUNT(*) FROM gpodder_episode_action
         WHERE user_id = ?1 AND action = 'PLAY' AND timestamp_epoch >= ?2
         GROUP BY week",
    )?;
    let rows = stmt.query_map(
        params![
            username,
            FIRST_MONDAY + first_week * WEEK_SECS,
            FIRST_MONDAY,
            WEEK_SECS
        ],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?;
    for row in rows {
        let (week, plays) = row?;
        plays_by_week.insert(week, plays);
    }
    let weeks = (first_week..=current_week)
        .map(|week| Week {
            start: FIRST_MONDAY + week * WEEK_SECS,
            plays: plays_by_week.get(&week).copied().unwrap_or(0),
        })
        .collect();

    let top_podcasts = conn
        .prepare(&format!(
            "SELECT podcast, SUM({LISTENED}) AS listened, COUNT(*) AS plays
             FROM gpodder_episode_action
             WHERE user_id = ?1 AND action = 'PLAY' AND timestamp_epoch > 0
             GROUP BY podcast ORDER BY listened DESC, plays DESC, podcast LIMIT ?2"
        ))?
        .query_map(params![username, TOP_PODCASTS], |r| {
            Ok(TopPodcast {
                url: r.get(0)?,
                listened_seconds: r.get(1)?,
                plays: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let recent = conn
        .prepare(
            "SELECT podcast, episode, action, timestamp_epoch, position, total
             FROM gpodder_episode_action WHERE user_id = ?1
             ORDER BY timestamp_epoch DESC, id DESC LIMIT ?2",
        )?
        .query_map(params![username, RECENT_ACTIONS], |r| {
            Ok(RecentAction {
                podcast: r.get(0)?,
                episode: r.get(1)?,
                action: r.get(2)?,
                timestamp: r.get(3)?,
                position: r.get(4)?,
                total: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let subscriptions = conn
        .prepare(
            "SELECT url, subscribed, updated FROM gpodder_subscriptions WHERE user_id = ?1
             ORDER BY subscribed DESC, updated DESC, url",
        )?
        .query_map([username], |r| {
            Ok(Subscription {
                url: r.get(0)?,
                subscribed: r.get::<_, Option<bool>>(1)?.unwrap_or(false),
                updated: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(Overview {
        now,
        subscribed,
        unsubscribed,
        episodes_played,
        listened_seconds,
        apps,
        apps_last_used,
        latest_activity: latest_action.max(subscriptions_updated),
        actions,
        weeks,
        top_podcasts,
        recent,
        subscriptions,
        titles: HashMap::new(),
    })
}

async fn dashboard(
    State(state): State<Arc<AppState>>,
    WebUser(user): WebUser,
) -> Result<Response, ApiError> {
    let mut data = state
        .db
        .with(|conn| overview(conn, user.id, &user.username))?;
    let urls = data
        .top_podcasts
        .iter()
        .map(|p| &p.url)
        .chain(data.recent.iter().map(|a| &a.podcast))
        .chain(data.subscriptions.iter().map(|s| &s.url));
    for url in urls {
        if let Some(title) = state.podcast_data.cached(url).and_then(|d| d.title) {
            data.titles.insert(url.clone(), title);
        }
    }

    // `to_php_json` escapes `<`, `>` and `/`, so the data can't close the script element.
    let body = format!(
        r#"{nav}
<div id="overview"></div>
<script type="application/json" id="overview-data">{json}</script>"#,
        nav = user_nav(&user, "/dashboard"),
        json = to_php_json(&data),
    );
    Ok(page("Overview", &body, Some(SCRIPT)))
}

/// Renders the overview from the embedded JSON. Every piece of synced data is
/// inserted with `textContent`, never as HTML.
const SCRIPT: &str = r##"
"use strict";
const data = JSON.parse(document.getElementById("overview-data").textContent);
const root = document.getElementById("overview");
const api = "/index.php/apps/gpoddersync/";

// ---------- helpers ----------
const el = (tag, attrs = {}, ...children) => {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") node.className = v;
    else if (k === "text") node.textContent = v;
    else if (k === "style") node.style.cssText = v;
    else node.setAttribute(k, v);
  }
  for (const c of children) if (c != null) node.append(c);
  return node;
};
const svg = (tag, attrs = {}) => {
  const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
  return node;
};
const nf = new Intl.NumberFormat();
const compact = new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 });
const count = (n) => (n >= 10000 ? compact.format(n) : nf.format(n));
const dateFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });
const dateTimeFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
const monthFmt = new Intl.DateTimeFormat(undefined, { month: "short", timeZone: "UTC" });
const weekFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeZone: "UTC" });
const plural = (n, one, many) => `${nf.format(n)} ${n === 1 ? one : many}`;
function duration(seconds) {
  const h = Math.floor(seconds / 3600), m = Math.floor((seconds % 3600) / 60);
  if (h >= 100) return `${nf.format(h)} h`;
  return h > 0 ? `${h} h ${m} min` : `${m} min`;
}
function clock(seconds) {
  const h = Math.floor(seconds / 3600), m = Math.floor((seconds % 3600) / 60), s = seconds % 60;
  const mm = String(m).padStart(h ? 2 : 1, "0"), ss = String(s).padStart(2, "0");
  return h ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
}
function relative(epoch) {
  const diff = data.now - epoch;
  if (diff < 60) return "just now";
  const units = [[86400 * 365, " yr"], [86400 * 30, " mo"], [86400, " d"], [3600, " h"], [60, " min"]];
  for (const [secs, unit] of units) if (diff >= secs) return `${Math.floor(diff / secs)}${unit} ago`;
}
const title = (url) => data.titles[url] || prettyUrl(url);
function prettyUrl(url) {
  try { const u = new URL(url); return u.host + (u.pathname === "/" ? "" : u.pathname); } catch { return url; }
}
function episodeName(url) {
  try {
    const parts = new URL(url).pathname.split("/").filter(Boolean);
    return decodeURIComponent(parts[parts.length - 1] || url);
  } catch { return url; }
}

// Action types: four categorical slots in fixed order; the rest fold into "Other".
const ACTIONS = [
  { key: "PLAY", label: "Play", slot: 1 },
  { key: "DOWNLOAD", label: "Download", slot: 2 },
  { key: "NEW", label: "New", slot: 3 },
  { key: "DELETE", label: "Delete", slot: 4 },
];
const actionStyle = (key) => {
  const a = ACTIONS.find((x) => x.key === key);
  return a
    ? { label: a.label, color: `var(--series-${a.slot})`, ink: `var(--on-series-${a.slot})` }
    : { label: key.charAt(0) + key.slice(1).toLowerCase(), color: "var(--other)", ink: "var(--on-other)" };
};

// ---------- tooltip (one for the page; hover and keyboard focus) ----------
const tip = el("div", { class: "tooltip", role: "status", hidden: "" });
document.body.append(tip);
function showTip(value, label, x, y) {
  tip.replaceChildren(el("strong", { text: value }), el("span", { text: label }));
  tip.hidden = false;
  const r = tip.getBoundingClientRect();
  const left = Math.min(Math.max(8, x - r.width / 2), innerWidth - r.width - 8);
  const top = y - r.height - 10 < 8 ? y + 16 : y - r.height - 10;
  tip.style.left = `${left}px`;
  tip.style.top = `${top}px`;
}
const hideTip = () => { tip.hidden = true; };
function tipOn(node, value, label) {
  node.tabIndex = 0;
  node.setAttribute("aria-label", `${label}: ${value}`);
  node.addEventListener("pointermove", (e) => showTip(value, label, e.clientX, e.clientY));
  node.addEventListener("pointerleave", hideTip);
  node.addEventListener("focus", () => {
    const r = node.getBoundingClientRect();
    showTip(value, label, r.left + r.width / 2, r.top);
  });
  node.addEventListener("blur", hideTip);
}

function card(heading, caption, ...content) {
  return el("section", { class: "card" }, el("h2", { text: heading }),
    caption ? el("p", { class: "caption", text: caption }) : null, ...content);
}
function tableView(summary, head, rows) {
  const thead = el("thead", {}, el("tr", {}, ...head.map(([text, num]) => el("th", { class: num ? "num" : "", text }))));
  const tbody = el("tbody", {}, ...rows.map((row) =>
    el("tr", {}, ...row.map((cell, i) => el("td", { class: head[i][1] ? "num" : "", text: cell })))));
  return el("details", {}, el("summary", { text: summary }), el("div", { class: "scroll" }, el("table", {}, thead, tbody)));
}

// ---------- KPI row ----------
function tile(label, value, sub) {
  return el("div", { class: "tile" }, el("div", { class: "label", text: label }),
    el("div", { class: "value", text: value }), sub ? el("div", { class: "sub", text: sub }) : null);
}
function kpis() {
  return el("div", { class: "kpis" },
    tile("Subscriptions", nf.format(data.subscribed),
      data.unsubscribed ? `${nf.format(data.unsubscribed)} unsubscribed` : null),
    tile("Episodes played", nf.format(data.episodes_played)),
    tile("Listening time", duration(data.listened_seconds), "sum of playback positions"),
    tile("Connected apps", nf.format(data.apps),
      data.apps_last_used ? `last used ${relative(data.apps_last_used)}` : null),
    tile("Latest activity", data.latest_activity ? relative(data.latest_activity) : "none",
      data.latest_activity ? dateFmt.format(new Date(data.latest_activity * 1000)) : null));
}

// ---------- episodes played per week: column chart ----------
function niceStep(max, targetTicks) {
  const raw = Math.max(max, 1) / targetTicks;
  const pow = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 5, 10].map((m) => m * pow).find((s) => s >= raw);
  return Math.max(1, step);
}
function weeklyChart() {
  const weeks = data.weeks;
  const container = el("div", { class: "chart" });
  const total = weeks.reduce((sum, w) => sum + w.plays, 0);
  const caption = "Each episode counts once, in the week of its latest play. Last 52 weeks, weeks starting Monday (UTC).";
  let active = weeks.length - 1;

  function render() {
    const width = Math.max(320, container.clientWidth);
    const plotH = 160, top = 8, axisH = 22, left = 40, right = 4;
    const height = top + plotH + axisH;
    const max = Math.max(...weeks.map((w) => w.plays));
    const step = niceStep(max, 4);
    const yMax = Math.max(step, Math.ceil(max / step) * step);
    const y = (v) => top + plotH - (v / yMax) * plotH;
    const slot = (width - left - right) / weeks.length;
    const barW = Math.min(24, Math.max(2, slot - 2));

    const chart = svg("svg", { viewBox: `0 0 ${width} ${height}`, height, role: "img", tabindex: "0",
      "aria-label": `Episodes played per week. ${plural(total, "episode", "episodes")} in the last 52 weeks. Use the left and right arrow keys to read each week.` });
    for (let v = 0; v <= yMax; v += step) {
      chart.append(svg("line", { x1: left, x2: width - right, y1: y(v), y2: y(v),
        stroke: v === 0 ? "var(--axis)" : "var(--grid)", "stroke-width": 1, "shape-rendering": "crispEdges" }));
      const label = svg("text", { x: left - 6, y: y(v) + 4, "text-anchor": "end" });
      label.textContent = count(v);
      chart.append(label);
    }
    let lastLabelX = -Infinity, lastMonth = null;
    const bars = weeks.map((w, i) => {
      const x = left + i * slot + (slot - barW) / 2;
      const month = monthFmt.format(new Date(w.start * 1000));
      if (month !== lastMonth && x - lastLabelX > 32) {
        const label = svg("text", { x: x, y: top + plotH + 16 });
        label.textContent = month;
        chart.append(label);
        lastLabelX = x;
      }
      lastMonth = month;
      if (w.plays === 0) return null;
      const h = plotH - (y(w.plays) - top), r = Math.min(4, h, barW / 2), y0 = top + plotH;
      const path = svg("path", { fill: "var(--series-1)",
        d: `M${x},${y0} V${y0 - h + r} Q${x},${y0 - h} ${x + r},${y0 - h} H${x + barW - r} Q${x + barW},${y0 - h} ${x + barW},${y0 - h + r} V${y0} Z` });
      chart.append(path);
      return path;
    });
    // Hit targets span each week's full slot, larger than the bar.
    const describe = (i) => [plural(weeks[i].plays, "episode", "episodes"),
      `Week of ${weekFmt.format(new Date(weeks[i].start * 1000))}`];
    const highlight = (i) => bars.forEach((b, j) => b && b.setAttribute("opacity", j === i ? "0.75" : "1"));
    weeks.forEach((w, i) => {
      const hit = svg("rect", { x: left + i * slot, y: top, width: slot, height: plotH, fill: "transparent" });
      hit.addEventListener("pointermove", (e) => { highlight(i); showTip(...describe(i), e.clientX, e.clientY); });
      hit.addEventListener("pointerleave", () => { highlight(-1); hideTip(); });
      chart.append(hit);
    });
    const showActive = () => {
      highlight(active);
      const r = chart.getBoundingClientRect(), scale = r.width / width;
      showTip(...describe(active), r.left + (left + (active + 0.5) * slot) * scale, r.top + top * scale);
    };
    chart.addEventListener("focus", showActive);
    chart.addEventListener("blur", () => { highlight(-1); hideTip(); });
    chart.addEventListener("keydown", (e) => {
      if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
      e.preventDefault();
      active = Math.min(weeks.length - 1, Math.max(0, active + (e.key === "ArrowRight" ? 1 : -1)));
      showActive();
    });
    container.replaceChildren(chart);
  }
  new ResizeObserver(() => render()).observe(container);
  const rows = weeks.filter((w) => w.plays > 0).reverse()
    .map((w) => [weekFmt.format(new Date(w.start * 1000)), nf.format(w.plays)]);
  return card("Episodes played per week", caption, container,
    tableView("Show as table", [["Week of", false], ["Episodes", true]], rows));
}

// ---------- actions by type: tall segmented bar ----------
function actionsBar() {
  const total = data.actions.reduce((sum, a) => sum + a.count, 0);
  const known = ACTIONS.map((a) => data.actions.find((x) => x.action === a.key)).filter(Boolean);
  const other = data.actions.filter((x) => !ACTIONS.some((a) => a.key === x.action));
  const parts = known.map((a) => ({ key: a.action, count: a.count }));
  const otherCount = other.reduce((sum, a) => sum + a.count, 0);
  if (otherCount) parts.push({ key: "OTHER", count: otherCount, names: other.map((a) => actionStyle(a.action).label) });

  const bar = el("div", { class: "seg", role: "list", "aria-label": `Episode actions by type, ${plural(total, "action", "actions")}` });
  const legend = el("div", { class: "legend" });
  for (const p of parts) {
    const style = p.key === "OTHER" ? { label: "Other", color: "var(--other)", ink: "var(--on-other)" } : actionStyle(p.key);
    const share = p.count / total;
    const pct = `${Math.round(share * 100)}%`;
    const block = el("div", { class: "seg-block", role: "listitem",
      style: `flex: ${p.count} 1 0; background: ${style.color}; color: ${style.ink}` });
    const text = el("span", { text: pct });
    block.append(text);
    tipOn(block, `${nf.format(p.count)} (${pct})`, p.names ? `Other: ${p.names.join(", ")}` : style.label);
    bar.append(block);
    legend.append(el("span", { style: `--swatch: ${style.color}`, text: style.label }));
  }
  // Measure after layout: a share that doesn't fit its block is left to the tooltip and table.
  requestAnimationFrame(() => {
    for (const block of bar.children) {
      const text = block.firstChild;
      text.style.visibility = text.scrollWidth + 12 > block.clientWidth ? "hidden" : "visible";
    }
  });
  const rows = parts.map((p) => [p.key === "OTHER" ? `Other (${p.names.join(", ")})` : actionStyle(p.key).label,
    nf.format(p.count), `${(p.count / total * 100).toFixed(1)}%`]);
  return card("Episode actions by type", `${plural(total, "action", "actions")} synced from your apps.`, bar, legend,
    tableView("Show as table", [["Action", false], ["Count", true], ["Share", true]], rows));
}

// ---------- top podcasts: ranked horizontal bars ----------
function topPodcasts() {
  const top = data.top_podcasts;
  const max = Math.max(...top.map((p) => p.listened_seconds), 1);
  const list = el("div", { role: "list" });
  const names = new Map();
  for (const p of top) {
    const name = el("div", { class: "hbar-name", text: title(p.url), title: p.url });
    names.set(p.url, name);
    const row = el("div", { class: "hbar-row", role: "listitem" }, name,
      el("div", { class: "hbar-line" },
        el("div", { class: "hbar", style: `width: ${Math.max(0.5, (p.listened_seconds / max) * 85)}%` }),
        el("span", { class: "hbar-value", text: duration(p.listened_seconds) })));
    tipOn(row, duration(p.listened_seconds), `${title(p.url)} · ${plural(p.plays, "episode", "episodes")} played`);
    list.append(row);
  }
  // Fill in feed titles the server hasn't fetched yet.
  for (const p of top) {
    if (data.titles[p.url]) continue;
    fetch(`${api}personal_settings/podcast_data?url=${encodeURIComponent(p.url)}`)
      .then((res) => (res.ok ? res.json() : null))
      .then((json) => {
        const t = json && json.data && json.data.title;
        if (t) { data.titles[p.url] = t; names.get(p.url).textContent = t; }
      })
      .catch(() => {});
  }
  const rows = top.map((p) => [title(p.url), duration(p.listened_seconds), nf.format(p.plays)]);
  return card("Top podcasts by listening time", "Sum of playback positions of played episodes.", list,
    tableView("Show as table", [["Podcast", false], ["Listening time", true], ["Episodes played", true]], rows));
}

// ---------- recent activity table ----------
function recentActivity() {
  const head = el("thead", {}, el("tr", {}, ...["When", "Action", "Episode", "Podcast", "Progress"].map((t) => el("th", { text: t }))));
  const body = el("tbody");
  for (const a of data.recent) {
    const style = actionStyle(a.action);
    let progress = el("span", { class: "muted", text: "–" });
    if (a.total > 0 && a.position >= 0) {
      const share = Math.min(1, a.position / a.total);
      progress = el("span", {},
        el("span", { class: "meter", role: "img", "aria-label": `${Math.round(share * 100)}% played` },
          el("i", { style: `width: ${share * 100}%` })),
        el("span", { class: "muted", text: `${clock(a.position)} / ${clock(a.total)}` }));
    } else if (a.position > 0) {
      progress = el("span", { class: "muted", text: clock(a.position) });
    }
    body.append(el("tr", {},
      el("td", { class: "nowrap", text: dateTimeFmt.format(new Date(a.timestamp * 1000)) }),
      el("td", { class: "nowrap" }, el("span", { class: "dot", style: `--swatch: ${style.color}`, text: style.label })),
      el("td", { class: "episode", text: episodeName(a.episode), title: a.episode }),
      el("td", { text: title(a.podcast), title: a.podcast }),
      el("td", { class: "nowrap" }, progress)));
  }
  return card("Recent activity", `The latest ${plural(data.recent.length, "episode action", "episode actions")}, newest first.`,
    el("div", { class: "scroll" }, el("table", {}, head, body)));
}

// ---------- subscriptions table ----------
function subscriptions() {
  const rows = data.subscriptions.map((s) => [title(s.url), s.subscribed ? "Subscribed" : "Unsubscribed",
    dateTimeFmt.format(new Date(s.updated * 1000))]);
  const details = tableView(`Show all ${plural(data.subscriptions.length, "subscription", "subscriptions")}`,
    [["Podcast", false], ["Status", false], ["Last changed", false]], rows);
  details.open = data.subscriptions.length <= 10;
  return card("Subscriptions", `${nf.format(data.subscribed)} subscribed, ${nf.format(data.unsubscribed)} unsubscribed.`, details);
}

// ---------- page ----------
const hasActions = data.actions.length > 0;
if (!hasActions && data.subscriptions.length === 0) {
  root.append(kpis(), card("Nothing synced yet",
    "Connect a podcast app on the Settings page; its subscriptions and listening history will show up here."));
} else {
  root.append(kpis());
  if (hasActions) root.append(weeklyChart(), actionsBar());
  if (data.top_podcasts.length) root.append(topPodcasts());
  if (data.recent.length) root.append(recentActivity());
  if (data.subscriptions.length) root.append(subscriptions());
}
"##;
