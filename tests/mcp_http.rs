// `runner mcp --http`: the MCP tools over Streamable HTTP, as claude.ai
// reaches them through Funnel or `tailscale serve`, each request with a
// bearer token from `runner mcp-http token`. The server runs on loopback on a
// port the kernel picks, and says which on stderr. The fleet test puts
// kvm-host-2 behind tests/common's fake ssh, as tests/mcp_hosts.rs does.
mod common;

use common::{PEER_SSH, PeerHost, me, script};
use reqwest::StatusCode;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// The public base URL, as RUNNER_MCP_URL gives it.
const URL: &str = "https://kvm-host.example.ts.net";

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn runner(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", root)
        .args(args)
        .output()
        .unwrap()
}

// A new token, as `runner mcp-http token` prints it.
fn token(root: &Path) -> String {
    let out = runner(root, &["mcp-http", "token"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let token = text(&out.stdout).trim().to_string();
    assert!(token.len() >= 32, "{token:?}");
    token
}

fn revoke(root: &Path) {
    let out = runner(root, &["mcp-http", "revoke"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
}

// One host's runner, with base and dev, and a fake systemd on PATH.
fn host(dir: &Path) -> PeerHost {
    PeerHost::new(dir.join("here"), &["dev"], 2 << 30)
}

struct Server {
    child: Child,
    /// Where MCP is, like http://127.0.0.1:41234/mcp.
    mcp: String,
}

impl Server {
    fn start(host: &PeerHost, env: &[(&str, &str)]) -> Server {
        let path = format!("{}:{}", host.sd.path(), std::env::var("PATH").unwrap());
        let mut child = Command::new(env!("CARGO_BIN_EXE_runner"))
            .env("RUNNER_ROOT", host.root())
            .env("RUNNER_MEMINFO", host.dir.join("meminfo"))
            .env("RUNNER_USER", me())
            .env("RUNNER_MCP_URL", URL)
            .env("PATH", path)
            .envs(env.iter().copied())
            .args(["mcp", "--http", "127.0.0.1:0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let mut mcp = None;
        for line in lines.by_ref() {
            let line = line.unwrap();
            if let Some(at) = line.split_whitespace().find(|w| w.starts_with("http://")) {
                mcp = Some(at.to_string());
                break;
            }
        }
        let mcp = mcp.expect("the server says where it serves on stderr");
        // The rest of stderr, drained so the server never blocks on it.
        std::thread::spawn(move || lines.for_each(drop));
        Server { child, mcp }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn connect(mcp: &str, token: &str) -> RunningService<RoleClient, ()> {
    let config = StreamableHttpClientTransportConfig::with_uri(mcp).auth_header(token);
    let transport = StreamableHttpClientTransport::with_client(reqwest::Client::new(), config);
    ().serve(transport).await.unwrap()
}

async fn call(client: &RunningService<RoleClient, ()>, tool: &str, args: Value) -> Value {
    let mut params = CallToolRequestParams::new(tool.to_string());
    params.arguments = args.as_object().cloned();
    let result = client.call_tool(params).await.unwrap();
    assert_ne!(result.is_error, Some(true), "{tool}: {result:?}");
    result.structured_content.unwrap()
}

// What stdio's `runner mcp` lists, each tool's name and description.
fn stdio_tools(root: &Path) -> BTreeSet<(String, String)> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_runner"))
        .env("RUNNER_ROOT", root)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let client = json!({"name": "test", "version": "0"});
    let init = json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client});
    for msg in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ] {
        writeln!(stdin, "{msg}").unwrap();
    }
    let lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut tools = None;
    for line in lines {
        let msg: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if msg["id"] == json!(2) {
            tools = Some(msg["result"]["tools"].clone());
            break;
        }
    }
    drop(stdin);
    let _ = child.wait();
    let tools = tools.expect("stdio's tools/list");
    (tools.as_array().unwrap().iter())
        .map(|t| {
            let name = t["name"].as_str().unwrap().to_string();
            (name, t["description"].as_str().unwrap_or("").to_string())
        })
        .collect()
}

fn names(vms: &Value) -> BTreeSet<(String, String)> {
    (vms["vms"].as_array().unwrap().iter())
        .map(|v| {
            let name = v["name"].as_str().unwrap().to_string();
            (name, v["host"].as_str().unwrap().to_string())
        })
        .collect()
}

#[test]
fn with_a_token_it_lists_the_same_tools_as_stdio_and_answers() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let token = token(&host.root());
    let server = Server::start(&host, &[]);
    let here = runner::hosts::hostname();
    runtime().block_on(async {
        let client = connect(&server.mcp, &token).await;
        let info = client.peer_info().unwrap();
        assert_eq!(info.server_info.as_ref().unwrap().name, "runner");
        let tools: BTreeSet<(String, String)> = (client.list_all_tools().await.unwrap())
            .into_iter()
            .map(|t| {
                let description = t.description.map(|d| d.to_string()).unwrap_or_default();
                (t.name.to_string(), description)
            })
            .collect();
        assert!(tools.len() > 20, "{tools:?}");
        assert_eq!(tools, stdio_tools(&host.root()));
        let vms = call(&client, "list_vms", json!({})).await;
        let want: BTreeSet<(String, String)> = (["base", "dev"].iter())
            .map(|v| (v.to_string(), here.clone()))
            .collect();
        assert_eq!(names(&vms), want);
        client.cancel().await.unwrap();
    });
}

// A raw JSON-RPC POST to /mcp, as any HTTP client would send it.
struct Post<'a> {
    token: Option<&'a str>,
    session: Option<&'a str>,
    origin: Option<&'a str>,
    host: Option<&'a str>,
}

impl Post<'_> {
    fn bare() -> Self {
        Post {
            token: None,
            session: None,
            origin: None,
            host: None,
        }
    }

    async fn send(&self, mcp: &str, body: Value) -> reqwest::Response {
        let mut req = reqwest::Client::new()
            .post(mcp)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-06-18")
            .body(body.to_string());
        if let Some(token) = self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(session) = self.session {
            req = req.header("Mcp-Session-Id", session);
        }
        if let Some(origin) = self.origin {
            req = req.header("Origin", origin);
        }
        if let Some(host) = self.host {
            req = req.header("Host", host);
        }
        req.send().await.unwrap()
    }
}

fn initialize() -> Value {
    let client = json!({"name": "curl", "version": "0"});
    let params = json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client});
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params})
}

fn delete(vm: &str) -> Value {
    let params = json!({"name": "delete_vm", "arguments": {"name": vm}});
    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": params})
}

// A session opened with `token`: its id, once the server knows the client
// is initialized.
async fn session(mcp: &str, token: &str) -> String {
    let with = Post {
        token: Some(token),
        ..Post::bare()
    };
    let res = with.send(mcp, initialize()).await;
    assert_eq!(res.status(), StatusCode::OK);
    let id = res.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    res.text().await.unwrap();
    let with = Post {
        token: Some(token),
        session: Some(&id),
        ..Post::bare()
    };
    let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let res = with.send(mcp, note).await;
    assert!(res.status().is_success(), "{}", res.status());
    id
}

fn challenge() -> String {
    format!("Bearer resource_metadata=\"{URL}/.well-known/oauth-protected-resource\"")
}

fn refused(res: &reqwest::Response, what: &str) {
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{what}");
    let header = res.headers().get("www-authenticate");
    let header = header.map(|h| h.to_str().unwrap().to_string());
    assert_eq!(header, Some(challenge()), "{what}");
}

#[test]
fn no_token_a_wrong_one_or_a_revoked_one_gets_401_and_no_tool_runs() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let good = token(&host.root());
    let server = Server::start(&host, &[]);
    let mcp = server.mcp.as_str();
    runtime().block_on(async {
        // Each without a session, as a client starts.
        let res = Post::bare().send(mcp, initialize()).await;
        refused(&res, "no token");
        let wrong = Post {
            token: Some("not-a-token"),
            ..Post::bare()
        };
        refused(&wrong.send(mcp, initialize()).await, "a wrong token");
        let mangled = good.replacen(&good[..1], if &good[..1] == "a" { "b" } else { "a" }, 1);
        let wrong = Post {
            token: Some(&mangled),
            ..Post::bare()
        };
        refused(&wrong.send(mcp, initialize()).await, "a token one off");

        // A session a good token opened can't call a tool without one.
        let id = session(mcp, &good).await;
        for token in [None, Some("not-a-token")] {
            let with = Post {
                token,
                session: Some(&id),
                ..Post::bare()
            };
            refused(&with.send(mcp, delete("dev")).await, "a call in a session");
        }
        assert!(host.has("dev"), "a refused call deleted dev");

        // Nor once its token is revoked.
        revoke(&host.root());
        let with = Post {
            token: Some(&good),
            session: Some(&id),
            ..Post::bare()
        };
        refused(&with.send(mcp, delete("dev")).await, "a revoked token");
        assert!(host.has("dev"), "a revoked token's call deleted dev");

        // A new token works in a new session: the call goes through.
        let fresh = token(&host.root());
        let id = session(mcp, &fresh).await;
        let with = Post {
            token: Some(&fresh),
            session: Some(&id),
            ..Post::bare()
        };
        let res = with.send(mcp, delete("dev")).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.text().await.unwrap();
        assert!(body.contains(r#""isError":false"#), "{body}");
        assert!(!host.has("dev"));
    });
}

#[test]
fn a_browser_origin_but_claude_s_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let good = token(&host.root());
    let server = Server::start(&host, &[]);
    let mcp = server.mcp.as_str();
    runtime().block_on(async {
        for origin in [
            "https://evil.example",
            "http://claude.ai",
            "https://claude.ai.evil.example",
        ] {
            let with = Post {
                token: Some(&good),
                origin: Some(origin),
                ..Post::bare()
            };
            let res = with.send(mcp, initialize()).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{origin}");
        }
        for origin in [None, Some("https://claude.ai"), Some("https://claude.com")] {
            let with = Post {
                token: Some(&good),
                origin,
                ..Post::bare()
            };
            let res = with.send(mcp, initialize()).await;
            assert_eq!(res.status(), StatusCode::OK, "{origin:?}");
        }
    });
}

// Behind Funnel the Host is the public one; any other is refused, against
// DNS rebinding.
#[test]
fn the_public_host_is_let_in_and_a_stranger_isn_t() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let good = token(&host.root());
    let server = Server::start(&host, &[]);
    let mcp = server.mcp.as_str();
    runtime().block_on(async {
        let public = URL.trim_start_matches("https://");
        for (name, want) in [
            (public, StatusCode::OK),
            ("evil.example", StatusCode::FORBIDDEN),
        ] {
            let with = Post {
                token: Some(&good),
                host: Some(name),
                ..Post::bare()
            };
            let res = with.send(mcp, initialize()).await;
            assert_eq!(res.status(), want, "{name}");
        }
    });
}

#[test]
fn only_mcp_is_served() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let good = token(&host.root());
    let server = Server::start(&host, &[]);
    let root = server.mcp.trim_end_matches("/mcp").to_string();
    runtime().block_on(async {
        let res = reqwest::Client::new()
            .get(format!("{root}/vms"))
            .header("Authorization", format!("Bearer {good}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    });
}

// Every file under <root>/mcp-http: (path, mode, contents).
fn store_files(root: &Path) -> Vec<(PathBuf, u32, Vec<u8>)> {
    let dir = root.join("mcp-http");
    (fs::read_dir(&dir).unwrap())
        .map(|e| {
            let path = e.unwrap().path();
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            let bytes = fs::read(&path).unwrap();
            (path, mode, bytes)
        })
        .collect()
}

#[test]
fn the_token_store_holds_only_hashes_at_0600() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let tokens = [token(&root), token(&root)];
    assert_ne!(tokens[0], tokens[1]);
    let mode = fs::metadata(root.join("mcp-http"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
    let files = store_files(&root);
    assert!(!files.is_empty());
    let all: Vec<u8> = files.iter().flat_map(|(_, _, b)| b.clone()).collect();
    let all = text(&all);
    for (path, mode, _) in &files {
        assert_eq!(*mode, 0o600, "{}", path.display());
    }
    for token in &tokens {
        assert!(!all.contains(token.as_str()), "a token in the store");
        let hash: String = (Sha256::digest(token.as_bytes()).iter())
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(all.contains(&hash), "no hash of a token in the store");
    }
    // Revoke drops them all.
    revoke(&root);
    let all: Vec<u8> = (store_files(&root).iter())
        .flat_map(|(_, _, b)| b.clone())
        .collect();
    let all = text(&all);
    assert!(all.split_whitespace().all(|w| w.len() != 64), "{all}");
}

#[test]
fn an_address_that_isn_t_loopback_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    for addr in ["0.0.0.0:8740", "[::]:8740", "192.0.2.1:8740"] {
        let started = Instant::now();
        let out = runner(dir.path(), &["mcp", "--http", addr]);
        assert!(!out.status.success(), "{addr}");
        assert!(
            text(&out.stderr).contains("loopback"),
            "{addr}: {}",
            text(&out.stderr)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

// The service has no laptop to pass --peer: it reads <root>/peers at each
// call, the first line this host as the laptop lists it, so a host listed
// after it started is reached without a restart.
#[test]
fn a_peer_written_after_the_server_started_is_in_the_next_list() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    script(&bin.join("ssh"), PEER_SSH);
    let fleet = dir.path().join("fleet");
    let here = host(dir.path());
    let _two = PeerHost::new(fleet.join("kvm-host-2"), &["web"], 4 << 30);
    let good = token(&here.root());
    let sys = std::env::var("PATH").unwrap();
    let path = format!("{}:{}:{sys}", bin.display(), here.sd.path());
    let fleet_dir = fleet.display().to_string();
    let server = Server::start(
        &here,
        &[
            ("PATH", &path),
            ("SYS_PATH", &sys),
            ("FLEET", &fleet_dir),
            ("RUNNER", env!("CARGO_BIN_EXE_runner")),
        ],
    );
    let kernel = runner::hosts::hostname();
    runtime().block_on(async {
        let client = connect(&server.mcp, &good).await;
        let vms = call(&client, "list_vms", json!({})).await;
        let want: BTreeSet<(String, String)> = (["base", "dev"].iter())
            .map(|v| (v.to_string(), kernel.clone()))
            .collect();
        assert_eq!(names(&vms), want);

        fs::write(
            here.root().join("peers"),
            "runner@kvm-host\nrunner@kvm-host-2\n",
        )
        .unwrap();
        let vms = call(&client, "list_vms", json!({})).await;
        let want: BTreeSet<(String, String)> = [
            ("base", "kvm-host"),
            ("dev", "kvm-host"),
            ("base", "kvm-host-2"),
            ("web", "kvm-host-2"),
        ]
        .iter()
        .map(|(v, h)| (v.to_string(), h.to_string()))
        .collect();
        assert_eq!(names(&vms), want);
        // A call for web goes to its host.
        let status = call(&client, "vm_status", json!({"name": "web"})).await;
        assert_eq!(status["name"], json!("web"), "{status}");

        // And one taken off the list is no longer asked.
        fs::write(here.root().join("peers"), "runner@kvm-host\n").unwrap();
        let vms = call(&client, "list_vms", json!({})).await;
        let want: BTreeSet<(String, String)> = (["base", "dev"].iter())
            .map(|v| (v.to_string(), "kvm-host".to_string()))
            .collect();
        assert_eq!(names(&vms), want);
        client.cancel().await.unwrap();
    });
}

// run_output waits for an event while a run goes on with nothing new: past
// SSE's 15-second keep-alive here, and up to 45 seconds, with no timeout of
// the server's cutting it.
#[test]
fn run_output_s_wait_holds_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let good = token(&host.root());
    // A run of dev's that's going: something holds the run lock.
    let runs = host.root().join("vms/dev/runs");
    let run = runs.join("1");
    fs::create_dir_all(&run).unwrap();
    let ask = json!({"prompt": "say hi", "dir": "/home/agent/work", "resume": null, "user": "agent", "boots": true, "started": "2026-11-05T00:00:00Z"});
    fs::write(run.join("ask.json"), ask.to_string()).unwrap();
    let event = "{\"type\":\"assistant\",\"n\":1}\n";
    fs::write(run.join("stream"), event).unwrap();
    let lock = fs::File::create(runs.join("lock")).unwrap();
    lock.lock().unwrap();
    let server = Server::start(&host, &[]);
    runtime().block_on(async {
        let client = connect(&server.mcp, &good).await;
        let started = Instant::now();
        let args = json!({"run": "dev.1", "cursor": event.len(), "wait_secs": 20});
        let got = call(&client, "run_output", args).await;
        let waited = started.elapsed();
        assert!(waited >= Duration::from_secs(19), "{waited:?}");
        assert_eq!(got["run"]["state"], json!("running"), "{got}");
        assert_eq!(got["events"], json!([]), "{got}");
        client.cancel().await.unwrap();
    });
    drop(lock);
}

// The server runs as the root's owner, runner, and reads the store as it: a
// token made as anyone else, root above all, is refused before anything is
// made, so the store can't end up one the server can't read.
#[test]
fn a_token_is_made_only_as_the_root_s_owner() {
    if !nix::unistd::geteuid().is_root() {
        eprintln!("skipped: only root can give the root to another user");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let nobody = nix::unistd::Uid::from_raw(65534);
    nix::unistd::chown(&root, Some(nobody), None).unwrap();
    for args in [["mcp-http", "token"], ["mcp-http", "revoke"]] {
        let out = runner(&root, &args);
        assert!(!out.status.success(), "{args:?}");
        let err = text(&out.stderr);
        assert!(err.contains("sudo -u"), "{args:?}: {err}");
        assert!(!root.join("mcp-http").exists(), "{args:?}");
    }
}

#[test]
fn a_url_that_isn_t_plain_http_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    for url in [
        "kvm-host.tailnet.ts.net",
        "ftp://kvm-host.tailnet.ts.net",
        "https://kvm-host\".ts.net",
        "https://kvm host.ts.net",
        "https://",
    ] {
        let started = Instant::now();
        let out = runner(dir.path(), &["mcp", "--http", "127.0.0.1:0", "--url", url]);
        assert!(!out.status.success(), "{url}");
        assert!(
            text(&out.stderr).contains("RUNNER_MCP_URL"),
            "{url}: {}",
            text(&out.stderr)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
