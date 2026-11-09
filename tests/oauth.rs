// OAuth for claude.ai (src/oauth.rs): a client registers, is sent to the
// approval page, the code `runner mcp-http pair` printed is typed there, and
// the authorization code it's sent back with buys a token MCP takes. The
// server runs in this process on a fake clock, so a code can be made to
// outlive its minute, and a reqwest client plays claude.ai. The CLI runs on
// the same root, as it would on the host.
mod common;

use common::me;
use reqwest::StatusCode;
use rmcp::ServiceExt;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

const CALLBACK: &str = "https://claude.ai/api/mcp/auth_callback";

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut out = form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        out.append_pair(k, v);
    }
    out.finish()
}

// A query's or a form's pairs.
fn pairs(query: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn param(query: &str, name: &str) -> Option<String> {
    pairs(query)
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

/// The runner's root, served in this process.
struct Host {
    _dir: tempfile::TempDir,
    root: PathBuf,
    /// http://127.0.0.1:<port>: the issuer, as no RUNNER_MCP_URL is given.
    base: String,
    rt: tokio::runtime::Runtime,
    /// How far the server's clock is ahead of the real one, in seconds.
    ahead: Arc<AtomicI64>,
    http: reqwest::Client,
}

fn real_now() -> i64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since.unwrap().as_secs() as i64
}

impl Host {
    fn new() -> Host {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let ahead = Arc::new(AtomicI64::new(0));
        let clock = {
            let ahead = ahead.clone();
            runner::oauth::Clock::new(move || real_now() + ahead.load(Ordering::SeqCst))
        };
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let store = runner::store::Store::new(&root);
        rt.spawn(async move {
            runner::mcp::serve_on(listener, store, me(), None, clock)
                .await
                .unwrap();
        });
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Host {
            _dir: dir,
            root,
            base,
            rt,
            ahead,
            http,
        }
    }

    fn later(&self, secs: i64) {
        self.ahead.fetch_add(secs, Ordering::SeqCst);
    }

    fn runner(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_runner"))
            .env("RUNNER_ROOT", &self.root)
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.runner(args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }

    /// A new pairing code, as `runner mcp-http pair` prints it.
    fn pair(&self) -> String {
        let code = self.ok(&["mcp-http", "pair"]).trim().to_string();
        assert_eq!(code.len(), 8, "{code:?}");
        code
    }

    fn get(&self, path: &str) -> Reply {
        self.rt.block_on(async {
            let res = self.http.get(self.url(path)).send().await.unwrap();
            Reply::of(res).await
        })
    }

    fn post(&self, path: &str, body: String, headers: &[(&str, &str)]) -> Reply {
        self.rt.block_on(async {
            let mut req = self
                .http
                .post(self.url(path))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(body);
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            Reply::of(req.send().await.unwrap()).await
        })
    }

    fn post_json(&self, path: &str, body: Value) -> Reply {
        self.rt.block_on(async {
            let req = (self.http.post(self.url(path)))
                .header("Content-Type", "application/json")
                .body(body.to_string());
            Reply::of(req.send().await.unwrap()).await
        })
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{path}", self.base)
        }
    }

    fn mcp(&self) -> String {
        format!("{}/mcp", self.base)
    }

    // Registers a client: with no auth method, one with a secret.
    fn register(&self, method: Option<&str>) -> Registered {
        let mut req = json!({
            "client_name": "Claude",
            "redirect_uris": [CALLBACK],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        });
        if let Some(m) = method {
            req["token_endpoint_auth_method"] = json!(m);
        }
        let res = self.post_json("/register", req);
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        let body: Value = serde_json::from_str(&res.body).unwrap();
        Registered {
            id: body["client_id"].as_str().unwrap().to_string(),
            secret: body["client_secret"].as_str().map(String::from),
            basic: method == Some("client_secret_basic"),
        }
    }

    // The approval page for `client`, as claude.ai sends the browser to it.
    fn page(&self, client: &Registered, flow: &Flow) -> Reply {
        let query = form(&[
            ("response_type", "code"),
            ("client_id", &client.id),
            ("redirect_uri", &flow.redirect),
            ("code_challenge", &flow.challenge),
            ("code_challenge_method", "S256"),
            ("state", "st4te"),
            ("resource", &flow.resource),
        ]);
        self.get(&format!("/authorize?{query}"))
    }

    // The page's form posted with `code` typed in.
    fn approve(&self, page: &Reply, code: &str) -> Reply {
        let csrf = page.csrf();
        let cookie = page.cookie();
        let cookie = format!("runner_csrf={cookie}");
        let body = form(&[("csrf", &csrf), ("code", code), ("action", "allow")]);
        self.post("/authorize", body, &[("Cookie", &cookie)])
    }

    // Through the page with the right pairing code: the authorization code
    // the redirect carries.
    fn authorize(&self, client: &Registered, flow: &Flow) -> String {
        let code = self.pair();
        let page = self.page(client, flow);
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        let back = self.approve(&page, &code);
        back.code().unwrap_or_else(|| panic!("no code: {back:?}"))
    }

    fn exchange(
        &self,
        client: &Registered,
        code: &str,
        verifier: &str,
        extra: &[(&str, &str)],
    ) -> Reply {
        let mut p = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", CALLBACK),
            ("code_verifier", verifier),
        ];
        p.extend_from_slice(extra);
        self.token_request(client, p)
    }

    fn refresh(&self, client: &Registered, refresh: &str) -> Reply {
        let p = vec![("grant_type", "refresh_token"), ("refresh_token", refresh)];
        self.token_request(client, p)
    }

    fn token_request<'a>(&self, client: &'a Registered, mut p: Vec<(&'a str, &'a str)>) -> Reply {
        let mut headers = Vec::new();
        let basic;
        match (&client.secret, client.basic) {
            (Some(secret), true) => {
                use base64::Engine;
                let pair = format!("{}:{secret}", client.id);
                let enc = base64::engine::general_purpose::STANDARD.encode(pair);
                basic = format!("Basic {enc}");
                headers.push(("Authorization", basic.as_str()));
            }
            (Some(secret), false) => {
                p.push(("client_id", &client.id));
                p.push(("client_secret", secret));
            }
            (None, _) => p.push(("client_id", &client.id)),
        }
        self.post("/token", form(&p), &headers)
    }

    // A token pair, all the way from a fresh flow.
    fn sign_in(&self, client: &Registered) -> Tokens {
        let flow = Flow::new(self);
        let code = self.authorize(client, &flow);
        let res = self.exchange(client, &code, &flow.verifier, &[]);
        res.tokens()
    }

    // Whether MCP takes `token`: list_vms over rmcp's HTTP client, or
    // the status of an initialize a refused token gets.
    fn mcp_works(&self, token: &str) -> bool {
        let status = self.rt.block_on(async {
            let res = (self.http.post(self.mcp()))
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .header("Authorization", format!("Bearer {token}"))
                .body(initialize().to_string())
                .send()
                .await
                .unwrap();
            res.status()
        });
        if status == StatusCode::UNAUTHORIZED {
            return false;
        }
        assert_eq!(status, StatusCode::OK);
        self.rt.block_on(async {
            let config =
                StreamableHttpClientTransportConfig::with_uri(self.mcp()).auth_header(token);
            let transport =
                StreamableHttpClientTransport::with_client(reqwest::Client::new(), config);
            let client = ().serve(transport).await.unwrap();
            let params = rmcp::model::CallToolRequestParams::new("list_vms".to_string());
            let result = client.call_tool(params).await.unwrap();
            assert_ne!(result.is_error, Some(true), "{result:?}");
            let vms = result.structured_content.unwrap();
            assert_eq!(vms["vms"], json!([]), "{vms}");
            client.cancel().await.unwrap();
        });
        true
    }
}

fn initialize() -> Value {
    let client = json!({"name": "curl", "version": "0"});
    let params = json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client});
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params})
}

struct Registered {
    id: String,
    secret: Option<String>,
    /// Sends its secret as HTTP Basic, not in the form.
    basic: bool,
}

/// One authorization's PKCE pair, where it goes back to, and what for.
struct Flow {
    verifier: String,
    challenge: String,
    redirect: String,
    resource: String,
}

impl Flow {
    fn new(host: &Host) -> Flow {
        let verifier = b64url(&rand_bytes());
        let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
        Flow {
            verifier,
            challenge,
            redirect: CALLBACK.to_string(),
            resource: host.mcp(),
        }
    }
}

fn rand_bytes() -> [u8; 32] {
    let mut b = [0u8; 32];
    use std::io::Read;
    fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut b)
        .unwrap();
    b
}

#[derive(Debug)]
struct Reply {
    status: StatusCode,
    location: Option<String>,
    cookie: Option<String>,
    headers: reqwest::header::HeaderMap,
    body: String,
}

struct Tokens {
    access: String,
    refresh: String,
}

impl Reply {
    async fn of(res: reqwest::Response) -> Reply {
        let status = res.status();
        let headers = res.headers().clone();
        let location = (headers.get("location")).map(|v| v.to_str().unwrap().to_string());
        let cookie = (headers.get_all("set-cookie").iter())
            .map(|v| v.to_str().unwrap().to_string())
            .find(|c| c.starts_with("runner_csrf="));
        let body = res.text().await.unwrap();
        Reply {
            status,
            location,
            cookie,
            headers,
            body,
        }
    }

    // The CSRF token in the page's form.
    fn csrf(&self) -> String {
        let at = self
            .body
            .find(r#"name="csrf" value=""#)
            .expect("a csrf field");
        let rest = &self.body[at + r#"name="csrf" value=""#.len()..];
        rest[..rest.find('"').unwrap()].to_string()
    }

    fn cookie(&self) -> String {
        let c = self.cookie.as_deref().expect("a csrf cookie");
        let v = c.trim_start_matches("runner_csrf=");
        v.split(';').next().unwrap().to_string()
    }

    // The authorization code a redirect to the callback carries.
    fn code(&self) -> Option<String> {
        if self.status != StatusCode::SEE_OTHER {
            return None;
        }
        let at = self.location.as_deref()?;
        let (to, query) = at.split_once('?')?;
        assert_eq!(to, CALLBACK);
        assert_eq!(param(query, "state").as_deref(), Some("st4te"));
        param(query, "code")
    }

    // The error a redirect to the callback carries.
    fn redirect_error(&self) -> Option<String> {
        let at = self.location.as_deref()?;
        let (to, query) = at.split_once('?')?;
        assert_eq!(to, CALLBACK);
        assert!(param(query, "code").is_none(), "{at}");
        param(query, "error")
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    fn tokens(&self) -> Tokens {
        assert_eq!(self.status, StatusCode::OK, "{}", self.body);
        let body = self.json();
        assert_eq!(body["token_type"], "Bearer");
        assert_eq!(body["expires_in"], 3600);
        assert_eq!(
            self.headers.get("cache-control").unwrap().to_str().unwrap(),
            "no-store"
        );
        Tokens {
            access: body["access_token"].as_str().unwrap().to_string(),
            refresh: body["refresh_token"].as_str().unwrap().to_string(),
        }
    }

    // A /token refusal with `error`, and no token in it.
    fn refused(&self, error: &str) {
        assert!(
            self.status.is_client_error(),
            "{}: {}",
            self.status,
            self.body
        );
        let body = self.json();
        assert_eq!(body["error"], error, "{}", self.body);
        assert!(body.get("access_token").is_none(), "{}", self.body);
    }
}

#[test]
fn a_client_registers_pairs_and_its_token_calls_mcp() {
    let host = Host::new();
    for method in [
        None,
        Some("none"),
        Some("client_secret_post"),
        Some("client_secret_basic"),
    ] {
        let client = host.register(method);
        assert_eq!(
            client.secret.is_some(),
            method != Some("none"),
            "{method:?}"
        );
        let tokens = host.sign_in(&client);
        assert!(host.mcp_works(&tokens.access), "{method:?}");
        assert!(
            !host.mcp_works(&tokens.refresh),
            "a refresh token as a bearer"
        );
    }
}

#[test]
fn the_page_names_the_client_and_escapes_it() {
    let host = Host::new();
    let res = host.post_json(
        "/register",
        json!({"client_name": "<b>Claude</b>", "redirect_uris": [CALLBACK], "token_endpoint_auth_method": "none"}),
    );
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let client = Registered {
        id: res.json()["client_id"].as_str().unwrap().to_string(),
        secret: None,
        basic: false,
    };
    let page = host.page(&client, &Flow::new(&host));
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains("&lt;b&gt;Claude&lt;/b&gt;"),
        "{}",
        page.body
    );
    assert!(!page.body.contains("<b>Claude"), "{}", page.body);
    assert!(page.body.contains("claude.ai"), "{}", page.body);
    let csp = page.headers.get("content-security-policy").unwrap();
    assert!(csp.to_str().unwrap().contains("frame-ancestors 'none'"));
    let cookie = page.cookie.unwrap();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"),
        "{cookie}"
    );
}

#[test]
fn refresh_rotates_and_a_replayed_one_revokes_the_client() {
    let host = Host::new();
    let client = host.register(Some("none"));
    let first = host.sign_in(&client);
    let second = host.refresh(&client, &first.refresh).tokens();
    assert_ne!(second.access, first.access);
    assert_ne!(second.refresh, first.refresh);
    assert!(host.mcp_works(&second.access));
    let third = host.refresh(&client, &second.refresh).tokens();
    assert!(host.mcp_works(&third.access));

    // Another client isn't touched by this one's replay.
    let other = host.register(Some("none"));
    let others = host.sign_in(&other);

    // The first refresh token again: refused, and every token of the
    // client's is gone, the newest included.
    host.refresh(&client, &first.refresh)
        .refused("invalid_grant");
    assert!(!host.mcp_works(&third.access));
    host.refresh(&client, &third.refresh)
        .refused("invalid_grant");
    assert!(host.mcp_works(&others.access));

    // Nor does another client's refresh token work for this one.
    host.refresh(&client, &others.refresh)
        .refused("invalid_grant");
    assert!(host.mcp_works(&others.access));
}

#[test]
fn an_access_token_lasts_an_hour_and_a_refresh_token_thirty_days() {
    let host = Host::new();
    let client = host.register(Some("none"));
    let tokens = host.sign_in(&client);
    host.later(3600 - 5);
    assert!(host.mcp_works(&tokens.access));
    host.later(10);
    assert!(!host.mcp_works(&tokens.access));
    let next = host.refresh(&client, &tokens.refresh).tokens();
    assert!(host.mcp_works(&next.access));
    host.later(30 * 24 * 3600 + 5);
    host.refresh(&client, &next.refresh)
        .refused("invalid_grant");
}

#[test]
fn a_wrong_pkce_verifier_gets_no_token() {
    let host = Host::new();
    let client = host.register(Some("none"));
    let flow = Flow::new(&host);
    let code = host.authorize(&client, &flow);
    let wrong = Flow::new(&host).verifier;
    host.exchange(&client, &code, &wrong, &[])
        .refused("invalid_grant");
    // And the code is spent: the right verifier is too late.
    host.exchange(&client, &code, &flow.verifier, &[])
        .refused("invalid_grant");
}

#[test]
fn a_reused_code_gets_no_token_and_revokes_what_it_bought() {
    let host = Host::new();
    let client = host.register(Some("none"));
    let flow = Flow::new(&host);
    let code = host.authorize(&client, &flow);
    let tokens = host.exchange(&client, &code, &flow.verifier, &[]).tokens();
    host.exchange(&client, &code, &flow.verifier, &[])
        .refused("invalid_grant");
    assert!(!host.mcp_works(&tokens.access));
}

#[test]
fn a_code_past_sixty_seconds_gets_no_token() {
    let host = Host::new();
    let client = host.register(Some("none"));
    let flow = Flow::new(&host);
    let code = host.authorize(&client, &flow);
    host.later(61);
    host.exchange(&client, &code, &flow.verifier, &[])
        .refused("invalid_grant");

    // Just inside the minute it's good.
    let flow = Flow::new(&host);
    let code = host.authorize(&client, &flow);
    host.later(55);
    host.exchange(&client, &code, &flow.verifier, &[]).tokens();
}

#[test]
fn a_code_is_the_client_s_and_its_redirect_s() {
    let host = Host::new();
    let client = host.register(Some("none"));
    let other = host.register(Some("none"));
    let flow = Flow::new(&host);
    let code = host.authorize(&client, &flow);
    host.exchange(&other, &code, &flow.verifier, &[])
        .refused("invalid_grant");

    let flow = Flow::new(&host);
    let code = host.authorize(&client, &flow);
    let p = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", "https://claude.com/api/mcp/auth_callback"),
        ("code_verifier", flow.verifier.as_str()),
    ];
    host.token_request(&client, p).refused("invalid_grant");
}

#[test]
fn a_secret_client_needs_its_secret() {
    let host = Host::new();
    let client = host.register(Some("client_secret_post"));
    let flow = Flow::new(&host);
    let code = host.authorize(&client, &flow);
    let wrong = Registered {
        id: client.id.clone(),
        secret: Some("0".repeat(64)),
        basic: false,
    };
    let res = host.exchange(&wrong, &code, &flow.verifier, &[]);
    assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}", res.body);
    res.refused("invalid_client");
    let bare = Registered {
        id: client.id.clone(),
        secret: None,
        basic: false,
    };
    host.exchange(&bare, &code, &flow.verifier, &[])
        .refused("invalid_client");
}

#[test]
fn a_redirect_off_the_list_is_refused_at_register_and_at_authorize() {
    let host = Host::new();
    for uris in [
        json!(["https://evil.example/cb"]),
        json!([CALLBACK, "https://evil.example/cb"]),
        json!(["https://claude.ai/api/mcp/auth_callback/x"]),
        json!(["http://claude.ai/api/mcp/auth_callback"]),
        json!([]),
    ] {
        let res = host.post_json(
            "/register",
            json!({"client_name": "x", "redirect_uris": uris, "token_endpoint_auth_method": "none"}),
        );
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{uris}");
        assert_eq!(res.json()["error"], "invalid_redirect_uri", "{uris}");
        assert!(res.json().get("client_id").is_none());
    }
    // Registered for claude.ai's callback only, it can't be sent elsewhere,
    // nor to claude.com's.
    let client = host.register(Some("none"));
    host.pair();
    for to in [
        "https://evil.example/cb",
        "https://claude.com/api/mcp/auth_callback",
    ] {
        let mut flow = Flow::new(&host);
        flow.redirect = to.to_string();
        let page = host.page(&client, &flow);
        assert_eq!(page.status, StatusCode::BAD_REQUEST, "{to}");
        assert!(page.location.is_none(), "{to}");
        assert!(page.cookie.is_none(), "{to}");
    }
    // An unknown client gets no page either.
    let ghost = Registered {
        id: "0123456789abcdef".to_string(),
        secret: None,
        basic: false,
    };
    let page = host.page(&ghost, &Flow::new(&host));
    assert_eq!(page.status, StatusCode::BAD_REQUEST);
    assert!(page.location.is_none());
}

#[test]
fn authorize_wants_pkce_s256_and_a_code_response() {
    let host = Host::new();
    let client = host.register(Some("none"));
    let flow = Flow::new(&host);
    for (drop, set) in [
        (
            "code_challenge_method",
            Some(("code_challenge_method", "plain")),
        ),
        ("code_challenge_method", None),
        ("code_challenge", None),
        ("response_type", Some(("response_type", "token"))),
    ] {
        let mut p = vec![
            ("response_type", "code"),
            ("client_id", client.id.as_str()),
            ("redirect_uri", CALLBACK),
            ("code_challenge", flow.challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", "st4te"),
        ];
        p.retain(|(k, _)| *k != drop);
        p.extend(set);
        let res = host.get(&format!("/authorize?{}", form(&p)));
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{drop} {set:?}");
        let error = res.redirect_error();
        let want = if drop == "response_type" {
            "unsupported_response_type"
        } else {
            "invalid_request"
        };
        assert_eq!(error.as_deref(), Some(want), "{drop} {set:?}");
    }
}

#[test]
fn five_wrong_pairing_codes_void_it() {
    let host = Host::new();
    let client = host.register(Some("none"));
    let code = host.pair();
    let flow = Flow::new(&host);
    let mut page = host.page(&client, &flow);
    let wrong = if code == "AAAAAAAA" {
        "BBBBBBBB"
    } else {
        "AAAAAAAA"
    };
    for left in (0..5).rev() {
        page = host.approve(&page, wrong);
        assert_eq!(page.status, StatusCode::BAD_REQUEST, "{}", page.body);
        assert!(page.code().is_none());
        if left > 0 {
            assert!(
                page.body.contains(&format!("{left} tr")),
                "{left}: {}",
                page.body
            );
        }
    }
    assert!(page.body.contains("runner mcp-http pair"), "{}", page.body);
    let res = host.approve(&page, &code);
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    assert!(res.code().is_none(), "the right code after five wrong ones");

    // A new one works.
    let code = host.pair();
    let page = host.page(&client, &flow);
    assert!(host.approve(&page, &code).code().is_some());
}

#[test]
fn a_pairing_code_is_single_use_lives_ten_minutes_and_types_loosely() {
    let host = Host::new();
    let client = host.register(Some("none"));
    let flow = Flow::new(&host);

    // Typed in lower case with a dash, it's still the code.
    let code = host.pair();
    assert!(
        code.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
        "{code}"
    );
    let loose = format!("{}-{}", &code[..4], &code[4..]).to_lowercase();
    let page = host.page(&client, &flow);
    assert!(host.approve(&page, &loose).code().is_some());
    // Used once, it's gone.
    let page = host.page(&client, &flow);
    let res = host.approve(&page, &code);
    assert!(res.code().is_none());
    assert!(res.body.contains("runner mcp-http pair"), "{}", res.body);

    // A new pair replaces the old.
    let old = host.pair();
    let new = host.pair();
    let page = host.page(&client, &flow);
    if old != new {
        assert!(host.approve(&page, &old).code().is_none());
    }
    let page = host.page(&client, &flow);
    assert!(host.approve(&page, &new).code().is_some());

    // Ten minutes on, it's void: last, as `pair` runs on the real clock.
    let code = host.pair();
    host.later(10 * 60 + 1);
    let page = host.page(&client, &flow);
    assert!(host.approve(&page, &code).code().is_none());
}

#[test]
fn a_post_without_its_csrf_token_or_cookie_gets_no_code() {
    let host = Host::new();
    let client = host.register(Some("none"));
    let flow = Flow::new(&host);
    let code = host.pair();
    let page = host.page(&client, &flow);
    let csrf = page.csrf();
    let cookie = format!("runner_csrf={}", page.cookie());

    // No CSRF token in the form.
    let body = form(&[("code", &code), ("action", "allow")]);
    let res = host.post("/authorize", body, &[("Cookie", &cookie)]);
    assert!(res.status.is_client_error(), "{}", res.status);
    assert!(res.code().is_none());
    // A made-up one.
    let made_up = "f".repeat(csrf.len());
    let body = form(&[("csrf", &made_up), ("code", &code), ("action", "allow")]);
    let res = host.post(
        "/authorize",
        body,
        &[("Cookie", &format!("runner_csrf={made_up}"))],
    );
    assert!(res.status.is_client_error(), "{}", res.status);
    assert!(res.code().is_none());
    // The page's, but without its cookie, as from another site's form.
    let body = form(&[("csrf", &csrf), ("code", &code), ("action", "allow")]);
    let res = host.post("/authorize", body, &[]);
    assert!(res.status.is_client_error(), "{}", res.status);
    assert!(res.code().is_none());
    // None of these spent a try or the code: a fresh page still works.
    let page = host.page(&client, &flow);
    assert!(host.approve(&page, &code).code().is_some());
}

#[test]
fn deny_sends_access_denied_back() {
    let host = Host::new();
    let client = host.register(Some("none"));
    host.pair();
    let page = host.page(&client, &Flow::new(&host));
    let cookie = format!("runner_csrf={}", page.cookie());
    let body = form(&[("csrf", &page.csrf()), ("code", ""), ("action", "deny")]);
    let res = host.post("/authorize", body, &[("Cookie", &cookie)]);
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.redirect_error().as_deref(), Some("access_denied"));
}

#[test]
fn a_token_for_another_resource_is_refused() {
    let host = Host::new();
    let client = host.register(Some("none"));
    host.pair();
    // At authorize, back to the client with invalid_target.
    let mut flow = Flow::new(&host);
    flow.resource = "https://evil.example/mcp".to_string();
    let res = host.page(&client, &flow);
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.redirect_error().as_deref(), Some("invalid_target"));

    // At token, for a code that was asked for this server.
    let flow = Flow::new(&host);
    let code = host.authorize(&client, &flow);
    let other = [("resource", "https://evil.example/mcp")];
    host.exchange(&client, &code, &flow.verifier, &other)
        .refused("invalid_target");

    // At refresh too.
    let tokens = host.sign_in(&client);
    let p = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", tokens.refresh.as_str()),
        ("resource", "https://evil.example/mcp"),
    ];
    host.token_request(&client, p).refused("invalid_target");

    // This server, as its base or its /mcp, is fine, and so is none.
    for resource in [host.base.clone(), host.mcp(), format!("{}/", host.mcp())] {
        let mut flow = Flow::new(&host);
        flow.resource = resource.clone();
        let code = host.authorize(&client, &flow);
        let at = [("resource", resource.as_str())];
        host.exchange(&client, &code, &flow.verifier, &at).tokens();
    }
}

#[test]
fn revoking_a_client_drops_its_tokens_alone() {
    let host = Host::new();
    let one = host.register(Some("none"));
    let two = host.register(Some("client_secret_post"));
    let ones = host.sign_in(&one);
    let twos = host.sign_in(&two);
    let cli = host.ok(&["mcp-http", "token"]).trim().to_string();
    assert!(host.mcp_works(&ones.access));
    assert!(host.mcp_works(&twos.access));

    let said = host.ok(&["mcp-http", "revoke", &one.id]);
    assert!(said.contains(&one.id), "{said}");
    assert!(!host.mcp_works(&ones.access));
    host.refresh(&one, &ones.refresh).refused("invalid_grant");
    assert!(host.mcp_works(&twos.access));
    assert!(host.mcp_works(&cli));

    // It can pair again without registering again.
    let again = host.sign_in(&one);
    assert!(host.mcp_works(&again.access));

    // An unknown client is an error.
    let out = host.runner(&["mcp-http", "revoke", "0123456789abcdef"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("mcp-http list"),
        "{}",
        text(&out.stderr)
    );

    // Plain revoke drops every token, and the waiting pairing code.
    let code = host.pair();
    host.ok(&["mcp-http", "revoke"]);
    for token in [&again.access, &twos.access, &cli] {
        assert!(!host.mcp_works(token));
    }
    host.refresh(&two, &twos.refresh).refused("invalid_grant");
    let page = host.page(&one, &Flow::new(&host));
    assert!(host.approve(&page, &code).code().is_none());
}

#[test]
fn list_shows_each_client_and_when_it_last_called() {
    let host = Host::new();
    let out = host.ok(&["mcp-http", "list"]);
    assert!(out.contains("no clients"), "{out}");
    let one = host.register(Some("none"));
    let two = host.register(Some("none"));
    let tokens = host.sign_in(&one);
    assert!(host.mcp_works(&tokens.access));
    let out = host.ok(&["mcp-http", "list"]);
    let line = |id: &str| -> String {
        let found = out.lines().find(|l| l.starts_with(id));
        found.unwrap_or_else(|| panic!("{id}: {out}")).to_string()
    };
    let l1 = line(&one.id);
    assert!(l1.contains("Claude"), "{l1}");
    assert!(!l1.contains("never"), "{l1}");
    assert!(l1.contains("tokens 2"), "{l1}");
    let l2 = line(&two.id);
    assert!(l2.contains("called never"), "{l2}");
    assert!(l2.contains("no tokens"), "{l2}");
}

#[test]
fn the_metadata_carries_what_the_rfcs_require() {
    let host = Host::new();
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let res = host.get(path);
        assert_eq!(res.status, StatusCode::OK, "{path}");
        let doc = res.json();
        // RFC 9728: resource is required, and is the URL the client called.
        assert_eq!(doc["resource"], json!(host.mcp()), "{path}");
        assert_eq!(doc["authorization_servers"], json!([host.base]), "{path}");
        assert_eq!(doc["bearer_methods_supported"], json!(["header"]), "{path}");
    }
    let res = host.get("/.well-known/oauth-authorization-server");
    assert_eq!(res.status, StatusCode::OK);
    let doc = res.json();
    // RFC 8414: issuer, the endpoints and response types are required.
    assert_eq!(doc["issuer"], json!(host.base));
    assert_eq!(
        doc["authorization_endpoint"],
        json!(format!("{}/authorize", host.base))
    );
    assert_eq!(doc["token_endpoint"], json!(format!("{}/token", host.base)));
    assert_eq!(
        doc["registration_endpoint"],
        json!(format!("{}/register", host.base))
    );
    assert_eq!(doc["response_types_supported"], json!(["code"]));
    assert_eq!(doc["code_challenge_methods_supported"], json!(["S256"]));
    let grants = doc["grant_types_supported"].as_array().unwrap();
    assert!(grants.contains(&json!("authorization_code")));
    assert!(grants.contains(&json!("refresh_token")));
    let methods = doc["token_endpoint_auth_methods_supported"]
        .as_array()
        .unwrap();
    for m in ["none", "client_secret_post", "client_secret_basic"] {
        assert!(methods.contains(&json!(m)), "{m}");
    }
    // And a 401 from /mcp points at the first.
    let res = host.rt.block_on(async {
        let res = (host.http.post(host.mcp()))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(initialize().to_string())
            .send()
            .await
            .unwrap();
        Reply::of(res).await
    });
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    let challenge = res
        .headers
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap();
    let want = format!("{}/.well-known/oauth-protected-resource", host.base);
    assert!(challenge.contains(&want), "{challenge}");
}

#[test]
fn the_metadata_names_the_public_url() {
    // With RUNNER_MCP_URL, the binary's documents name it, not loopback.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let url = "https://kvm-host.example.ts.net";
    let mut child = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", &root)
        .env("RUNNER_MCP_URL", url)
        .args(["mcp", "--http", "127.0.0.1:0"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::BufRead;
    let mut lines = std::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let at = lines
        .by_ref()
        .map(|l| l.unwrap())
        .find_map(|l| {
            l.split_whitespace()
                .find(|w| w.starts_with("http://"))
                .map(String::from)
        })
        .expect("where it serves");
    std::thread::spawn(move || lines.for_each(drop));
    let local = at.trim_end_matches("/mcp").to_string();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let get = |path: &str| -> Value {
        rt.block_on(async {
            let res = reqwest::get(format!("{local}{path}")).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            serde_json::from_str(&res.text().await.unwrap()).unwrap()
        })
    };
    let pr = get("/.well-known/oauth-protected-resource");
    assert_eq!(pr["resource"], json!(format!("{url}/mcp")));
    assert_eq!(pr["authorization_servers"], json!([url]));
    let asm = get("/.well-known/oauth-authorization-server");
    assert_eq!(asm["issuer"], json!(url));
    assert_eq!(asm["token_endpoint"], json!(format!("{url}/token")));
    let _ = child.kill();
    let _ = child.wait();
}

// Every file under <root>/mcp-http: (path, mode, contents).
fn store_files(root: &Path) -> Vec<(PathBuf, u32, String)> {
    let dir = root.join("mcp-http");
    (fs::read_dir(&dir).unwrap())
        .map(|e| {
            let path = e.unwrap().path();
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            let bytes = fs::read(&path).unwrap();
            (path, mode, text(&bytes))
        })
        .collect()
}

#[test]
fn the_store_keeps_only_hashes_at_0600() {
    let host = Host::new();
    let secret = host.register(Some("client_secret_post"));
    let tokens = host.sign_in(&secret);
    let code = host.pair();
    let files = store_files(&host.root);
    let all: String = files.iter().map(|(_, _, t)| t.as_str()).collect();
    for (path, mode, _) in &files {
        assert_eq!(*mode, 0o600, "{}", path.display());
    }
    let secret_text = secret.secret.unwrap();
    for (what, plain) in [
        ("access token", tokens.access.as_str()),
        ("refresh token", tokens.refresh.as_str()),
        ("client secret", secret_text.as_str()),
        ("pairing code", code.as_str()),
    ] {
        assert!(!all.contains(plain), "the {what} itself is in the store");
    }
    for (what, plain) in [
        ("access token", tokens.access.as_str()),
        ("refresh token", tokens.refresh.as_str()),
        ("client secret", secret_text.as_str()),
    ] {
        let hash: String = (Sha256::digest(plain.as_bytes()).iter())
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(all.contains(&hash), "no hash of the {what} in the store");
    }
}

#[test]
fn registration_keeps_only_a_few_clients_that_never_paired() {
    let host = Host::new();
    let paired = host.register(Some("none"));
    let tokens = host.sign_in(&paired);
    let mut ids = Vec::new();
    for _ in 0..30 {
        ids.push(host.register(Some("none")).id);
    }
    let out = host.ok(&["mcp-http", "list"]);
    assert!(out.contains(&paired.id), "{out}");
    assert_eq!(out.lines().count(), 21, "{out}");
    // The newest are kept, the oldest went.
    assert!(out.contains(ids.last().unwrap()));
    assert!(!out.contains(&ids[0]));
    assert!(host.mcp_works(&tokens.access));
}
