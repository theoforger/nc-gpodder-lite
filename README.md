# nc-gpodder-lite

A standalone podcast sync server that speaks the same API as the
[nextcloud-gpodder](https://github.com/thrillfall/nextcloud-gpodder) Nextcloud app (v3.18),
without needing Nextcloud. Podcast apps that support "Nextcloud gpodder sync" (AntennaPod,
Kasts, gPodder, …) connect to it as if it were a Nextcloud server.

It's a single binary backed by one SQLite file.

## Quick start

```sh
cargo build --release
./target/release/nc-gpodder-lite user add alice          # prompts for a password
./target/release/nc-gpodder-lite serve --public-url https://pods.example.com
```

Then open `https://pods.example.com/` to log in and see your subscriptions.

With Docker Compose, configured through a `.env` file:

```sh
cp .env.example .env        # then edit: NCGL_PORT, NCGL_PUBLIC_URL, …
docker compose up -d --build
docker compose exec nc-gpodder-lite nc-gpodder-lite user add alice
```

All settings in `.env` are optional. `NCGL_PORT` is the host port (default `8080`); the
others are the environment variables listed under [Configuration](#configuration). `.env`
is ignored by git, so your settings stay local.

With plain Docker:

```sh
docker build -t nc-gpodder-lite .
docker run -d --name nc-gpodder-lite -p 8080:8080 -v ncgl-data:/data \
  -e NCGL_PUBLIC_URL=https://pods.example.com nc-gpodder-lite
docker exec -it nc-gpodder-lite nc-gpodder-lite user add alice
```

Put it behind a TLS-terminating reverse proxy for anything beyond local testing. Clients send
credentials with HTTP Basic auth.

## Configuration

| Flag | Env | Default | |
|---|---|---|---|
| `--db` | `NCGL_DB` | `nc-gpodder-lite.db` | SQLite database path |
| `--listen` | `NCGL_LISTEN` | `0.0.0.0:8080` | Listen address |
| `--public-url` | `NCGL_PUBLIC_URL` | derived from `Host` / `X-Forwarded-Proto` / `X-Forwarded-Host` | Base URL returned to clients by Login Flow v2 |
| `--allow-local-remote-servers` | `NCGL_ALLOW_LOCAL_REMOTE_SERVERS` | off | Allow fetching feeds from loopback/private IPs (like Nextcloud's `allow_local_remote_servers`). Without it, `HTTP(S)_PROXY` is also ignored for feed fetches |
| `--trusted-proxies` | `NCGL_TRUSTED_PROXIES` | none | Comma-separated IPs/CIDR ranges of reverse proxies whose `X-Forwarded-For` gives the real client IP, e.g. `172.16.0.0/12` |

Log verbosity is controlled with `RUST_LOG` (e.g. `RUST_LOG=debug`).

User management: `nc-gpodder-lite user add|passwd|delete|list`. When stdin is piped,
the password is read from its first line.

## Connecting clients

- **AntennaPod:** Settings → Synchronization → Choose a provider → Nextcloud. Enter the
  server address. AntennaPod opens a browser page where you log in and grant access
  (Nextcloud Login Flow v2). An app password is created automatically.
- **Apps that ask for a username and password** (e.g. Kasts): use your account password,
  or create an app password on the web page and use that.

App passwords are listed on the web page, where they can be revoked.

## API

Every route works both with and without the `/index.php` prefix, as on Nextcloud.

| Method | Path | |
|---|---|---|
| GET | `/index.php/apps/gpoddersync/subscriptions?since=` | `{"add":[...],"remove":[...],"timestamp":n}` |
| POST | `/index.php/apps/gpoddersync/subscription_change/create` | body `{"add":[...],"remove":[...]}` |
| GET | `/index.php/apps/gpoddersync/episode_action?since=&allActions=` | only `PLAY` actions unless `allActions=true` |
| POST | `/index.php/apps/gpoddersync/episode_action/create` | body: JSON list of actions |
| GET | `/index.php/apps/gpoddersync/personal_settings/metrics` | per-subscription listening metrics |
| GET | `/index.php/apps/gpoddersync/personal_settings/podcast_data?url=` | feed title/author/image etc. |
| POST | `/index.php/login/v2`, `/index.php/login/v2/poll` | Nextcloud Login Flow v2 |
| GET | `/status.php` | Nextcloud status probe |

Behavior follows upstream, including:

- action names are uppercased;
- timestamps without an offset are treated as UTC and returned as `2009-12-12T09:00:00+00:00`;
- `since` uses a strict greater-than comparison against the action's own timestamp;
- an action is upserted by guid, or else by episode URL, for each action type;
- invalid subscription URLs are silently dropped;
- JSON is escaped like PHP's `json_encode` (`\/`, `\uXXXX`).

Deliberate differences from upstream:

- Edge cases where upstream crashes with HTTP 500 are handled gracefully. For example, when an
  episode-action upsert can't find the row it conflicts with, that entry is skipped.
- An episode-action upload is validated in full before anything is saved, so a bad entry
  doesn't leave a partially saved batch.
- `podcast_data` checks that the user knows the podcast before returning cached data, and
  returns a generic error message when a feed can't be fetched.
- Results are returned in insertion order.

## Security

- **Passwords:** account passwords are stored as Argon2id hashes. App passwords and session
  tokens are random and stored as SHA-256 hashes.
- **What app passwords can do:** they only work for the sync API (HTTP Basic auth). Logging
  in to the web UI, granting a login flow, and changing the password all require the
  account password.
- **Brute-force protection:** after 10 failed password attempts for a username, or 30 from
  one client IP, within 15 minutes, further password attempts get HTTP 429 until the window
  passes. App passwords keep working, so an attack on a username can't lock out that user's
  devices. Behind a reverse proxy, set `--trusted-proxies` so the real client IP is used;
  otherwise every client appears to come from the proxy's address.
- **CSRF protection:** browser requests that change state must come from the server's own
  origin (checked via `Origin`/`Referer`). Set `--public-url` so the check knows the exact
  origin.
- **Login Flow v2:** granting access always asks for the account password, and the page
  warns against granting access via links sent by someone else.
- **Browser hardening:** pages are sent with a strict Content-Security-Policy and can't be
  framed.
- **Feed fetching:** like Nextcloud, feeds and images aren't fetched from loopback or private
  addresses. Fetched podcast data is cached for 24 hours, with the cache capped at 128 MB.
