// An OAuth 2.1 authorization server for one user: how claude.ai, added as a
// custom connector, gets a token for `runner mcp --http` (src/mcp.rs), with
// Nguyen at the host approving it. It's served beside /mcp, under
// RUNNER_MCP_URL, which is its issuer:
//
// - /.well-known/oauth-protected-resource (RFC 9728), where a 401 from /mcp
//   points, names the resource, RUNNER_MCP_URL/mcp, and this server, whose
//   own metadata is at /.well-known/oauth-authorization-server (RFC 8414).
// - POST /register (RFC 7591) takes anyone's registration, since claude.ai
//   registers itself, but only with redirect URIs on REDIRECTS: a code can
//   only ever be sent to claude.ai, so a client someone else registers gets
//   nothing it can use. At most UNPAIRED clients that were never approved
//   are kept, the oldest dropped first, so registering can't fill the disk.
// - GET /authorize checks the client and that the redirect URI is one it
//   registered before anything else, since until then there's nowhere safe
//   to send an error. It wants PKCE with S256 (plain would let whoever saw
//   the request redeem the code). A `resource` (RFC 8707) has to be this
//   server, here and at /token: it issues tokens for nothing else, so one
//   left out means this one too. Then it shows a page naming the client, with a box for the
//   pairing code. The form carries only a CSRF token, naming the checked
//   request this server holds, and the same token is a SameSite=Strict
//   cookie: a form posted from another site has neither, and nothing in the
//   form can change what's approved.
// - POST /authorize takes the code `runner mcp-http pair` printed. That code
//   is the approval: it's on the host's terminal, so only someone with a
//   shell there has it. One at a time, single-use, ten minutes, five wrong
//   tries for it and it's void, 40 bits: a guess has no real chance. The
//   right one gets a redirect with an authorization code that's single-use,
//   lives a minute and is bound to the client, the redirect URI and the
//   PKCE challenge; the resource it's for is this server, the only one.
// - POST /token trades the code, with its PKCE verifier and the client's
//   secret if it has one, for an access token that lives an hour and a
//   refresh token that lives 30 days. A refresh token is rotated at each
//   use, and the spent one is kept until it'd have expired: one used twice
//   was stolen, or the client's broken, so every token of the client's is
//   dropped. So is every token of a client that tries its code twice.
//
// Tokens go in src/tokens.rs's file as hashes, as `runner mcp-http token`'s
// do, so #176's check takes them, and its plain `revoke` drops them. The
// clients are in <root>/mcp-http/clients, their secrets hashed; the pairing
// code's hash in <root>/mcp-http/pairing. Pages and authorization codes are
// held in memory, at most HELD of each: a restart drops them, and claude.ai
// starts over. `scope` is ignored, as a token is for every tool.
use crate::tokens::{self, Entry, Kind};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Where a code may be sent back to: claude.ai's connector callbacks.
pub const REDIRECTS: [&str; 2] = [
    "https://claude.ai/api/mcp/auth_callback",
    "https://claude.com/api/mcp/auth_callback",
];
/// How long an access token lives.
pub const ACCESS_SECS: i64 = 60 * 60;
/// How long a refresh token lives.
pub const REFRESH_SECS: i64 = 30 * 24 * 60 * 60;
/// How long an authorization code lives.
pub const CODE_SECS: i64 = 60;
/// How long a pairing code lives.
pub const PAIR_SECS: i64 = 10 * 60;
/// Wrong tries a pairing code takes before it's void.
pub const PAIR_TRIES: u32 = 5;
// How long an approval page's form stays good.
const PAGE_SECS: i64 = 10 * 60;
// How long a spent code is remembered, so a second try is caught.
const SPENT_SECS: i64 = 10 * 60;
// Clients never approved that are kept.
const UNPAIRED: usize = 20;
// Pages, codes and spent codes held at once, each.
const HELD: usize = 64;
const CLIENTS: &str = "clients";
const PAIRING: &str = "pairing";
const COOKIE: &str = "runner_csrf";
// Crockford's base32: no I, L, O or U, so nothing reads as another.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The time, in seconds since the epoch, as the server sees it.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> i64 + Send + Sync>);

impl Clock {
    pub fn new(now: impl Fn() -> i64 + Send + Sync + 'static) -> Clock {
        Clock(Arc::new(now))
    }

    pub fn system() -> Clock {
        Clock::new(|| chrono::Utc::now().timestamp())
    }

    pub fn now(&self) -> i64 {
        (self.0)()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] tokens::Error),
    #[error("no client {0}: `runner mcp-http list` shows them")]
    NoClient(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A registered client.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Client {
    pub id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    /// Its secret's SHA-256, in hex; none for a public client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
    pub registered: i64,
    /// When the pairing code was first typed for it.
    #[serde(default)]
    pub paired: Option<i64>,
    /// When it last called MCP, to the minute.
    #[serde(default)]
    pub called: Option<i64>,
}

fn load(root: &Path) -> tokens::Result<Vec<Client>> {
    let text = tokens::read(root, CLIENTS)?;
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&text).map_err(|e| {
        let path = tokens::dir(root).join(CLIENTS);
        let e = std::io::Error::new(std::io::ErrorKind::InvalidData, e);
        tokens::Error::Io(format!("reading {}", path.display()), e)
    })
}

// Held under tokens::locked.
fn save(root: &Path, clients: &[Client]) -> tokens::Result<()> {
    let text = serde_json::to_string_pretty(clients).expect("clients serialize");
    tokens::write(root, CLIENTS, &(text + "\n"))
}

/// Every registered client, oldest first.
pub fn clients(root: &Path) -> Result<Vec<Client>> {
    if !tokens::dir(root).exists() {
        return Ok(Vec::new());
    }
    Ok(load(root)?)
}

// Changes the clients, locked.
fn change<T>(root: &Path, f: impl FnOnce(&mut Vec<Client>) -> T) -> Result<T> {
    Ok(tokens::locked(root, || {
        let mut clients = load(root)?;
        let out = f(&mut clients);
        save(root, &clients)?;
        Ok(out)
    })?)
}

// What a client may call itself: printable, one line, not too long.
fn tidy(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let name: String = words
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    if name.is_empty() {
        "an unnamed client".to_string()
    } else {
        name
    }
}

// A pairing code as typed: any case, spaces and dashes left out, and the
// letters that read as digits taken for them.
fn typed(code: &str) -> String {
    (code.chars())
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

/// A new pairing code, replacing any waiting: 8 characters from the
/// kernel's random source, only its hash kept.
pub fn pair(root: &Path, now: i64) -> Result<String> {
    tokens::mine(root)?;
    let bytes = tokens::random::<8>()?;
    // 32 divides 256, so each character is as likely as the next.
    let code: String = (bytes.iter())
        .map(|b| ALPHABET[usize::from(b % 32)] as char)
        .collect();
    let line = format!(
        "{} {} {PAIR_TRIES}\n",
        tokens::hex(&tokens::hash(&code)),
        now + PAIR_SECS
    );
    tokens::locked(root, || tokens::write(root, PAIRING, &line))?;
    Ok(code)
}

/// What typing a pairing code came to.
#[derive(Debug, PartialEq, Eq)]
enum Paired {
    Right,
    /// Wrong, with this many tries left: 0 voided it.
    Wrong(u32),
    /// None is waiting, or it's past its time.
    None,
}

fn try_pair(root: &Path, code: &str, now: i64) -> Result<Paired> {
    let presented = tokens::hash(&typed(code));
    Ok(tokens::locked(root, || {
        let text = tokens::read(root, PAIRING)?;
        let mut words = text.split_whitespace();
        let kept = words.next().and_then(tokens::unhex);
        let until: Option<i64> = words.next().and_then(|w| w.parse().ok());
        let left: Option<u32> = words.next().and_then(|w| w.parse().ok());
        let (Some(kept), Some(until), Some(left)) = (kept, until, left) else {
            tokens::remove(root, PAIRING)?;
            return Ok(Paired::None);
        };
        if now >= until || left == 0 {
            tokens::remove(root, PAIRING)?;
            return Ok(Paired::None);
        }
        if tokens::same(&kept, &presented) {
            tokens::remove(root, PAIRING)?;
            return Ok(Paired::Right);
        }
        let left = left - 1;
        if left == 0 {
            tokens::remove(root, PAIRING)?;
        } else {
            let line = format!("{} {until} {left}\n", tokens::hex(&kept));
            tokens::write(root, PAIRING, &line)?;
        }
        Ok(Paired::Wrong(left))
    })?)
}

/// A new access and refresh token.
struct Pair {
    access: String,
    refresh: String,
}

// A pair for `client`, and the lines that keep their hashes.
fn new_pair(client: &str, now: i64) -> Result<(Pair, [String; 2])> {
    let pair = Pair {
        access: tokens::hex(&tokens::random::<32>()?),
        refresh: tokens::hex(&tokens::random::<32>()?),
    };
    let made = tokens::stamp();
    let line = |token: &str, kind: &str, until: i64| {
        let hash = tokens::hex(&tokens::hash(token));
        format!("{hash} {made} oauth client={client} kind={kind} until={until}")
    };
    let lines = [
        line(&pair.access, "access", now + ACCESS_SECS),
        line(&pair.refresh, "refresh", now + REFRESH_SECS),
    ];
    Ok((pair, lines))
}

// Drops the lines past their time.
fn prune(lines: &mut Vec<String>, now: i64) {
    lines.retain(|l| Entry::parse(l).is_none_or(|e| e.live(now)));
}

// Drops every line of `client`'s, and says how many.
fn drop_client(lines: &mut Vec<String>, client: &str) -> usize {
    let before = lines.len();
    lines.retain(|l| Entry::parse(l).is_none_or(|e| e.client != Some(client)));
    before - lines.len()
}

fn issue(root: &Path, client: &str, now: i64) -> Result<Pair> {
    let (pair, new) = new_pair(client, now)?;
    tokens::edit(root, |lines| {
        prune(lines, now);
        lines.extend(new);
    })?;
    Ok(pair)
}

enum Refreshed {
    New(Pair),
    /// Not a live refresh token of this client's.
    Refused,
    /// A spent one: every token of the client's is gone.
    Replayed,
}

fn refresh(root: &Path, client: &str, token: &str, now: i64) -> Result<Refreshed> {
    let presented = tokens::hash(token);
    let (pair, new) = new_pair(client, now)?;
    Ok(tokens::edit(root, |lines| {
        prune(lines, now);
        let mut at = None;
        for (i, line) in lines.iter().enumerate() {
            if Entry::parse(line).is_some_and(|e| tokens::same(&e.hash, &presented)) {
                at = Some(i);
            }
        }
        let Some(i) = at else {
            return Refreshed::Refused;
        };
        let (kind, owner, spent) = {
            let e = Entry::parse(&lines[i]).expect("parsed above");
            (e.kind, e.client.map(String::from), e.spent)
        };
        if kind != Kind::Refresh || owner.as_deref() != Some(client) {
            return Refreshed::Refused;
        }
        if spent {
            drop_client(lines, client);
            return Refreshed::Replayed;
        }
        lines[i].push_str(" spent");
        lines.extend(new);
        Refreshed::New(pair)
    })?)
}

/// Drops `id`'s tokens, keeping it registered, so it can pair again. Says
/// how many there were, and the client's name.
pub fn revoke_client(root: &Path, id: &str) -> Result<(usize, String)> {
    tokens::mine(root)?;
    let Some(client) = clients(root)?.into_iter().find(|c| c.id == id) else {
        return Err(Error::NoClient(id.to_string()));
    };
    let n = tokens::edit(root, |lines| drop_client(lines, id))?;
    Ok((n, client.name))
}

/// #176's revoke, which drops every token, and the waiting pairing code
/// too, so nobody gets a new one from it. Says how many tokens, and whether
/// a code was waiting.
pub fn revoke_all(root: &Path) -> Result<(usize, bool)> {
    let n = tokens::revoke(root)?;
    if !tokens::dir(root).exists() {
        return Ok((n, false));
    }
    let voided = tokens::locked(root, || tokens::remove(root, PAIRING))?;
    Ok((n, voided))
}

/// Notes that `id` called MCP at `now`.
pub fn called(root: &Path, id: &str, now: i64) -> Result<()> {
    change(root, |clients| {
        if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
            c.called = Some(now);
        }
    })
}

fn when(t: Option<i64>) -> String {
    (t.and_then(|t| chrono::DateTime::from_timestamp(t, 0))).map_or_else(
        || "never".to_string(),
        |t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
}

/// `runner mcp-http list`: a row per client, oldest first: its id, its live
/// tokens, when it last called, and its name.
pub fn list(root: &Path, now: i64) -> Result<Vec<String>> {
    tokens::mine(root)?;
    let clients = clients(root)?;
    let text = if clients.is_empty() {
        String::new()
    } else {
        tokens::read(root, "tokens")?
    };
    let rows = clients.iter().map(|c| {
        let n = (text.lines())
            .filter_map(Entry::parse)
            .filter(|e| e.client == Some(c.id.as_str()) && e.live(now) && !e.spent)
            .count();
        let held = if n == 0 {
            "no tokens".to_string()
        } else {
            format!("tokens {n}")
        };
        format!("{}  {held}  called {}  {}", c.id, when(c.called), c.name)
    });
    Ok(rows.collect())
}

/// An authorization request, checked.
#[derive(Clone)]
struct Request {
    client: String,
    name: String,
    redirect: String,
    challenge: String,
    state: Option<String>,
}

struct Held {
    request: Request,
    until: i64,
}

/// The OAuth endpoints' state.
pub struct Server {
    root: PathBuf,
    /// The public base URL: the issuer, and what /mcp hangs off.
    base: String,
    clock: Clock,
    /// Approval pages, by their CSRF token's hash.
    pages: Mutex<HashMap<[u8; 32], Held>>,
    /// Authorization codes, by hash.
    codes: Mutex<HashMap<[u8; 32], Held>>,
    /// Codes tried, by hash: whose, and until when it's remembered.
    spent: Mutex<HashMap<[u8; 32], (String, i64)>>,
}

// Keeps `value` in `map`, dropping what's past its time, and the soonest to
// go when it's full, so nobody can fill memory with requests.
fn hold<V>(
    map: &Mutex<HashMap<[u8; 32], V>>,
    key: [u8; 32],
    value: V,
    now: i64,
    until: fn(&V) -> i64,
) {
    let mut map = map.lock().unwrap();
    map.retain(|_, v| now < until(v));
    while map.len() >= HELD {
        let soonest = map.iter().min_by_key(|(_, v)| until(v)).map(|(k, _)| *k);
        map.remove(&soonest.expect("not empty"));
    }
    map.insert(key, value);
}

impl Server {
    pub fn new(root: PathBuf, base: String, clock: Clock) -> Server {
        Server {
            root,
            base: base.trim_end_matches('/').to_string(),
            clock,
            pages: Mutex::default(),
            codes: Mutex::default(),
            spent: Mutex::default(),
        }
    }

    fn resource(&self) -> String {
        format!("{}/mcp", self.base)
    }

    // Whether `resource` is this server: its base or its /mcp.
    fn ours(&self, resource: &str) -> bool {
        let r = resource.trim_end_matches('/').to_ascii_lowercase();
        let base = self.base.to_ascii_lowercase();
        r == base || r == format!("{base}/mcp")
    }

    fn secure(&self) -> bool {
        self.base.starts_with("https://")
    }
}

/// The OAuth endpoints, for the router beside /mcp.
pub fn routes(server: Arc<Server>) -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(server_metadata),
        )
        .route("/register", post(register))
        .route("/authorize", get(authorize).post(approve))
        .route("/token", post(token))
        .with_state(server)
}

fn json_reply(status: StatusCode, body: &Value) -> Response {
    let mut res = (status, body.to_string()).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    json_reply(
        status,
        &json!({"error": error, "error_description": description}),
    )
}

fn broken(e: impl std::fmt::Display) -> Response {
    eprintln!("runner mcp: oauth: {e}");
    let said = "the host couldn't read or write its OAuth store";
    oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", said)
}

// Runs a store call off the async threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> std::result::Result<T, Response> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(broken(e)),
        Err(e) => Err(broken(e)),
    }
}

fn pairs(bytes: &[u8]) -> Vec<(String, String)> {
    form_urlencoded::parse(bytes)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn find<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    (pairs.iter())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

async fn resource_metadata(State(s): State<Arc<Server>>) -> Response {
    json_reply(
        StatusCode::OK,
        &json!({
            "resource": s.resource(),
            "authorization_servers": [s.base],
            "bearer_methods_supported": ["header"],
            "resource_name": "ai-agent-runner",
        }),
    )
}

async fn server_metadata(State(s): State<Arc<Server>>) -> Response {
    let base = &s.base;
    json_reply(
        StatusCode::OK,
        &json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "registration_endpoint": format!("{base}/register"),
            "response_types_supported": ["code"],
            "response_modes_supported": ["query"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
            "authorization_response_iss_parameter_supported": true,
        }),
    )
}

#[derive(Deserialize)]
struct Registration {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    token_endpoint_auth_method: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
}

async fn register(State(s): State<Arc<Server>>, body: Bytes) -> Response {
    let bad = |what: &str| oauth_error(StatusCode::BAD_REQUEST, "invalid_client_metadata", what);
    let Ok(reg) = serde_json::from_slice::<Registration>(&body) else {
        return bad("the registration isn't the JSON RFC 7591 describes");
    };
    let uris = reg.redirect_uris;
    if uris.is_empty()
        || uris.len() > REDIRECTS.len()
        || !uris.iter().all(|u| REDIRECTS.contains(&u.as_str()))
    {
        let said = format!("redirect URIs are only {}", REDIRECTS.join(" and "));
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", &said);
    }
    let method = reg
        .token_endpoint_auth_method
        .as_deref()
        .unwrap_or("client_secret_basic");
    if !["none", "client_secret_post", "client_secret_basic"].contains(&method) {
        return bad(
            "token_endpoint_auth_method is none, client_secret_post or client_secret_basic",
        );
    }
    let grants = reg
        .grant_types
        .unwrap_or_else(|| vec!["authorization_code".to_string()]);
    if !grants
        .iter()
        .all(|g| g == "authorization_code" || g == "refresh_token")
    {
        return bad("grant_types are authorization_code and refresh_token");
    }
    let responses = reg
        .response_types
        .unwrap_or_else(|| vec!["code".to_string()]);
    if !responses.iter().all(|r| r == "code") {
        return bad("response_types is code");
    }
    let now = s.clock.now();
    let made = (|| -> Result<(Client, Option<String>)> {
        let id = tokens::hex(&tokens::random::<8>()?);
        let secret = if method == "none" {
            None
        } else {
            Some(tokens::hex(&tokens::random::<32>()?))
        };
        let client = Client {
            id,
            name: tidy(reg.client_name.as_deref().unwrap_or("")),
            redirect_uris: uris,
            secret: secret.as_deref().map(|t| tokens::hex(&tokens::hash(t))),
            registered: now,
            paired: None,
            called: None,
        };
        Ok((client, secret))
    })();
    let (client, secret) = match made {
        Ok(made) => made,
        Err(e) => return broken(e),
    };
    let root = s.root.clone();
    let kept = client.clone();
    let saved = blocking(move || {
        change(&root, |clients| {
            clients.push(kept);
            // The oldest never approved go first.
            let unpaired: Vec<String> = (clients.iter())
                .filter(|c| c.paired.is_none())
                .map(|c| c.id.clone())
                .collect();
            let over = unpaired.len().saturating_sub(UNPAIRED);
            let gone = &unpaired[..over];
            clients.retain(|c| !gone.contains(&c.id));
        })
    })
    .await;
    if let Err(res) = saved {
        return res;
    }
    let mut body = json!({
        "client_id": client.id,
        "client_id_issued_at": now,
        "client_name": client.name,
        "redirect_uris": client.redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": method,
    });
    if let Some(secret) = secret {
        body["client_secret"] = json!(secret);
        body["client_secret_expires_at"] = json!(0);
    }
    json_reply(StatusCode::CREATED, &body)
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

// A message for a page: escaped, with each `command` in it set as code.
fn message(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, part) in escape(text).split('`').enumerate() {
        if i % 2 == 1 {
            out.push_str("<code>");
            out.push_str(part);
            out.push_str("</code>");
        } else {
            out.push_str(part);
        }
    }
    out
}

// The page's headers: nothing but its own form, sent on to claude.ai, and
// never in another site's frame.
fn html(status: StatusCode, body: String) -> Response {
    let mut res = (status, body).into_response();
    let h = res.headers_mut();
    let csp = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self' https://claude.ai https://claude.com; frame-ancestors 'none'; base-uri 'none'";
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(csp),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

const STYLE: &str = "body{font:16px/1.5 system-ui,sans-serif;max-width:28rem;margin:2rem auto;padding:0 1rem}input{font:inherit;font-size:1.5rem;letter-spacing:.15em;width:100%;box-sizing:border-box;padding:.4rem;text-transform:uppercase}button{font:inherit;padding:.5rem 1.2rem;margin:.75rem .5rem 0 0}.said{color:#b00020}";

fn document(title: &str, inner: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>{title}</title><style>{STYLE}</style></head>\n<body>\n{inner}\n</body></html>\n"
    )
}

// A page with nothing to do but say why.
fn page_error(status: StatusCode, said: &str) -> Response {
    let inner = format!("<h1>Can't connect</h1>\n<p>{}</p>", message(said));
    html(status, document("Can't connect", &inner))
}

// Where a redirect URI sends you back, as the page names it.
fn host_of(uri: &str) -> &str {
    let rest = uri.split_once("://").map_or(uri, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

// The approval page for `request`, with a new CSRF token held for it.
fn approval(s: &Server, status: StatusCode, request: Request, said: Option<&str>) -> Response {
    let csrf = match tokens::random::<32>() {
        Ok(bytes) => tokens::hex(&bytes),
        Err(e) => return broken(e),
    };
    let now = s.clock.now();
    let name = escape(&request.name);
    let back = escape(host_of(&request.redirect));
    let said = said.map_or_else(String::new, |s| {
        format!("<p class=\"said\">{}</p>\n", message(s))
    });
    let inner = format!(
        "<h1>Connect {name}?</h1>\n\
         <p><b>{name}</b> wants to use this runner's MCP tools: every VM, on every host. Allowed, it's sent back to <b>{back}</b>.</p>\n\
         {said}<form method=\"post\" action=\"authorize\">\n\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\n\
         <label for=\"code\">The code <code>runner mcp-http pair</code> prints on the host:</label>\n\
         <input id=\"code\" name=\"code\" autocomplete=\"one-time-code\" autocapitalize=\"characters\" spellcheck=\"false\" maxlength=\"16\" autofocus>\n\
         <button name=\"action\" value=\"allow\">Allow</button><button name=\"action\" value=\"deny\" formnovalidate>Deny</button>\n\
         </form>"
    );
    let held = Held {
        request,
        until: now + PAGE_SECS,
    };
    hold(&s.pages, tokens::hash(&csrf), held, now, |h| h.until);
    let mut res = html(status, document("Connect to the runner", &inner));
    let secure = if s.secure() { "; Secure" } else { "" };
    let cookie =
        format!("{COOKIE}={csrf}; Path=/; HttpOnly; SameSite=Strict; Max-Age={PAGE_SECS}{secure}");
    if let Ok(cookie) = HeaderValue::from_str(&cookie) {
        res.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    res
}

// Back to the client's redirect URI with `params`, the state and the issuer
// (RFC 9207) added.
fn send_back(s: &Server, request: &Request, params: &[(&str, &str)]) -> Response {
    let mut query = form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        query.append_pair(k, v);
    }
    if let Some(state) = &request.state {
        query.append_pair("state", state);
    }
    query.append_pair("iss", &s.base);
    let to = format!("{}?{}", request.redirect, query.finish());
    let mut res = StatusCode::SEE_OTHER.into_response();
    let h = res.headers_mut();
    match HeaderValue::from_str(&to) {
        Ok(to) => h.insert(header::LOCATION, to),
        Err(e) => return broken(e),
    };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

// A PKCE S256 challenge: 43 characters of base64url, a SHA-256's.
fn challenge_ok(c: &str) -> bool {
    c.len() == 43
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// A PKCE verifier: 43 to 128 of RFC 7636's characters.
fn verifier_ok(v: &str) -> bool {
    (43..=128).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

async fn authorize(State(s): State<Arc<Server>>, uri: Uri) -> Response {
    let q = pairs(uri.query().unwrap_or("").as_bytes());
    let get = |name| find(&q, name);
    let root = s.root.clone();
    let clients = match blocking(move || clients(&root)).await {
        Ok(c) => c,
        Err(res) => return res,
    };
    let client = get("client_id").and_then(|id| clients.into_iter().find(|c| c.id == id));
    let Some(client) = client else {
        let said = "This client isn't registered here, or was dropped as never used: remove the connector in claude.ai and add it again.";
        return page_error(StatusCode::BAD_REQUEST, said);
    };
    let redirect = get("redirect_uri").filter(|r| client.redirect_uris.iter().any(|u| u == r));
    let Some(redirect) = redirect else {
        let said = "The address this asks to send you back to isn't one the client registered.";
        return page_error(StatusCode::BAD_REQUEST, said);
    };
    let request = Request {
        client: client.id,
        name: client.name,
        redirect: redirect.to_string(),
        challenge: get("code_challenge").unwrap_or("").to_string(),
        state: get("state").map(String::from),
    };
    if get("response_type") != Some("code") {
        let error = [
            ("error", "unsupported_response_type"),
            ("error_description", "only code"),
        ];
        return send_back(&s, &request, &error);
    }
    if get("code_challenge_method") != Some("S256") || !challenge_ok(&request.challenge) {
        let said = "PKCE with S256 is required";
        return send_back(
            &s,
            &request,
            &[("error", "invalid_request"), ("error_description", said)],
        );
    }
    if get("resource").is_some_and(|r| !s.ours(r)) {
        let said = format!("the only resource here is {}", s.resource());
        return send_back(
            &s,
            &request,
            &[("error", "invalid_target"), ("error_description", &said)],
        );
    }
    approval(&s, StatusCode::OK, request, None)
}

fn cookie(headers: &HeaderMap) -> Option<String> {
    let all = headers.get_all(header::COOKIE);
    for value in all.iter().filter_map(|v| v.to_str().ok()) {
        for pair in value.split(';') {
            if let Some((k, v)) = pair.trim().split_once('=')
                && k == COOKIE
            {
                return Some(v.to_string());
            }
        }
    }
    None
}

async fn approve(State(s): State<Arc<Server>>, headers: HeaderMap, body: Bytes) -> Response {
    let form = pairs(&body);
    let now = s.clock.now();
    let csrf = find(&form, "csrf").unwrap_or("");
    let held = if csrf.is_empty() {
        None
    } else {
        s.pages.lock().unwrap().remove(&tokens::hash(csrf))
    };
    let Some(held) = held.filter(|h| now < h.until) else {
        let said = "This page is out of date or was already used: start again from claude.ai.";
        return page_error(StatusCode::BAD_REQUEST, said);
    };
    let sent = cookie(&headers).unwrap_or_default();
    if !tokens::same(&tokens::hash(&sent), &tokens::hash(csrf)) {
        let said = "This form wasn't posted from the page this host sent: open the link from claude.ai again.";
        return page_error(StatusCode::FORBIDDEN, said);
    }
    let request = held.request;
    if find(&form, "action") == Some("deny") {
        return send_back(&s, &request, &[("error", "access_denied")]);
    }
    let code = find(&form, "code").unwrap_or("").to_string();
    // Allow with the box empty is no try.
    if typed(&code).is_empty() {
        let said = "Type the code `runner mcp-http pair` prints on the host.";
        return approval(&s, StatusCode::BAD_REQUEST, request, Some(said));
    }
    let root = s.root.clone();
    let paired = match blocking(move || try_pair(&root, &code, now)).await {
        Ok(p) => p,
        Err(res) => return res,
    };
    let said = match paired {
        Paired::Right => return granted(&s, request, now).await,
        Paired::Wrong(0) => "That code was wrong a fifth time, so it's void: run `runner mcp-http pair` on the host for a new one.".to_string(),
        Paired::Wrong(1) => "That code is wrong: 1 try left.".to_string(),
        Paired::Wrong(n) => format!("That code is wrong: {n} tries left."),
        Paired::None => "No pairing code is waiting, or it's past its 10 minutes: run `runner mcp-http pair` on the host, then type the code it prints.".to_string(),
    };
    approval(&s, StatusCode::BAD_REQUEST, request, Some(&said))
}

// The right pairing code: the client is approved, and goes back with a code.
async fn granted(s: &Server, request: Request, now: i64) -> Response {
    let root = s.root.clone();
    let id = request.client.clone();
    let noted = blocking(move || {
        change(&root, |clients| {
            if let Some(c) = clients.iter_mut().find(|c| c.id == id) {
                c.paired.get_or_insert(now);
            }
        })
    })
    .await;
    if let Err(res) = noted {
        return res;
    }
    let code = match tokens::random::<32>() {
        Ok(bytes) => tokens::hex(&bytes),
        Err(e) => return broken(e),
    };
    let held = Held {
        request: request.clone(),
        until: now + CODE_SECS,
    };
    hold(&s.codes, tokens::hash(&code), held, now, |h| h.until);
    send_back(s, &request, &[("code", &code)])
}

// The client a token request is from, as HTTP Basic or the form says.
fn presented(headers: &HeaderMap, form: &[(String, String)]) -> Option<(String, Option<String>)> {
    let basic = (headers.get(header::AUTHORIZATION))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("basic"))
        .and_then(|(_, b64)| STANDARD.decode(b64.trim()).ok())
        .and_then(|raw| String::from_utf8(raw).ok());
    if let Some(basic) = basic {
        // RFC 6749 2.3.1: each half is form-encoded first.
        let (id, secret) = basic.split_once(':')?;
        let decode = |t: &str| -> String {
            form_urlencoded::parse(format!("x={t}").as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        return Some((decode(id), Some(decode(secret))));
    }
    let id = find(form, "client_id")?.to_string();
    Some((id, find(form, "client_secret").map(String::from)))
}

async fn token(State(s): State<Arc<Server>>, headers: HeaderMap, body: Bytes) -> Response {
    let form = pairs(&body);
    let get = |name| find(&form, name);
    let now = s.clock.now();
    // RFC 6749 5.2: a 401 names how a client authenticates.
    let refused = |what: &str| {
        let mut res = oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", what);
        let how = HeaderValue::from_static("Basic realm=\"runner\"");
        res.headers_mut().insert(header::WWW_AUTHENTICATE, how);
        res
    };
    let Some((id, secret)) = presented(&headers, &form) else {
        return refused("no client_id");
    };
    if get("client_id").is_some_and(|i| i != id) {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "two client ids");
    }
    let root = s.root.clone();
    let clients = match blocking(move || clients(&root)).await {
        Ok(c) => c,
        Err(res) => return res,
    };
    let Some(client) = clients.into_iter().find(|c| c.id == id) else {
        return refused("no such client");
    };
    if let Some(kept) = &client.secret {
        let kept = tokens::unhex(kept).unwrap_or([0; 32]);
        let given = tokens::hash(secret.as_deref().unwrap_or(""));
        if secret.is_none() || !tokens::same(&kept, &given) {
            return refused("this client's secret is wrong or missing");
        }
    }
    if get("resource").is_some_and(|r| !s.ours(r)) {
        let said = format!("the only resource here is {}", s.resource());
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_target", &said);
    }
    let invalid = |what: &str| oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", what);
    let root = s.root.clone();
    let pair = match get("grant_type") {
        Some("authorization_code") => {
            let Some(code) = get("code") else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "no code");
            };
            let h = tokens::hash(code);
            let held = s.codes.lock().unwrap().remove(&h);
            let Some(held) = held else {
                let reused = s
                    .spent
                    .lock()
                    .unwrap()
                    .get(&h)
                    .filter(|(_, until)| now < *until)
                    .map(|(c, _)| c.clone());
                // Only the code's own client trying again revokes: anyone
                // else who saw it in a callback URL could otherwise cut
                // that client off.
                if let Some(owner) = reused.filter(|owner| *owner == client.id) {
                    let dropped = blocking(move || {
                        Ok(tokens::edit(&root, |lines| drop_client(lines, &owner))?)
                    })
                    .await;
                    if let Err(res) = dropped {
                        return res;
                    }
                    return invalid(
                        "this code was already tried: the client's tokens are revoked; connect again",
                    );
                }
                return invalid("no such code");
            };
            // Tried once, whatever comes of it.
            hold(
                &s.spent,
                h,
                (held.request.client.clone(), now + SPENT_SECS),
                now,
                |v| v.1,
            );
            let request = held.request;
            if request.client != client.id {
                return invalid("this code is another client's");
            }
            if now >= held.until {
                return invalid("this code is past its minute");
            }
            if get("redirect_uri") != Some(request.redirect.as_str()) {
                return invalid("redirect_uri isn't the one this code was sent to");
            }
            let verifier = get("code_verifier").unwrap_or("");
            let derived = URL_SAFE_NO_PAD.encode(tokens::hash(verifier));
            if !verifier_ok(verifier)
                || !tokens::same(&tokens::hash(&derived), &tokens::hash(&request.challenge))
            {
                return invalid("the PKCE verifier doesn't match the challenge");
            }
            let id = client.id.clone();
            match blocking(move || issue(&root, &id, now)).await {
                Ok(pair) => pair,
                Err(res) => return res,
            }
        }
        Some("refresh_token") => {
            let Some(token) = get("refresh_token").map(String::from) else {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "no refresh_token",
                );
            };
            let id = client.id.clone();
            match blocking(move || refresh(&root, &id, &token, now)).await {
                Ok(Refreshed::New(pair)) => pair,
                Ok(Refreshed::Refused) => {
                    return invalid("not a live refresh token of this client's");
                }
                Ok(Refreshed::Replayed) => {
                    return invalid(
                        "this refresh token was already used: the client's tokens are revoked; connect again",
                    );
                }
                Err(res) => return res,
            }
        }
        _ => {
            let said = "grant_type is authorization_code or refresh_token";
            return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", said);
        }
    };
    json_reply(
        StatusCode::OK,
        &json!({
            "access_token": pair.access,
            "token_type": "Bearer",
            "expires_in": ACCESS_SECS,
            "refresh_token": pair.refresh,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typed_code_reads_loosely() {
        assert_eq!(typed("ab0c-d1ef"), "AB0CD1EF");
        assert_eq!(typed(" oil 23 "), "01123");
    }

    #[test]
    fn a_name_is_one_printable_line() {
        assert_eq!(tidy("  Claude\n\tthe\x07 app "), "Claude the app");
        assert_eq!(tidy(""), "an unnamed client");
        assert_eq!(tidy(&"x".repeat(200)).len(), 64);
    }

    #[test]
    fn this_server_is_its_base_or_its_mcp() {
        let s = Server::new(
            PathBuf::from("/r"),
            "https://h.example/".to_string(),
            Clock::system(),
        );
        for r in [
            "https://h.example",
            "https://h.example/",
            "https://h.example/mcp",
            "https://H.example/mcp/",
        ] {
            assert!(s.ours(r), "{r}");
        }
        for r in [
            "https://h.example/other",
            "https://evil.example/mcp",
            "https://h.example.evil/mcp",
        ] {
            assert!(!s.ours(r), "{r}");
        }
    }

    #[test]
    fn rfc_7636_s_example_verifies() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verifier_ok(verifier));
        assert!(challenge_ok(challenge));
        assert_eq!(URL_SAFE_NO_PAD.encode(tokens::hash(verifier)), challenge);
    }
}
