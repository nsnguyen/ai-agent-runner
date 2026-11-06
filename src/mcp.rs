// The CLI's commands as MCP tools, served on stdio. Local Claude Code reaches
// it as `ssh -T kvm-host runner mcp`, so anything printed to stdout must be
// protocol: log to stderr only.
//
// With several hosts the laptop passes the rest as `--peer`s, and this one
// reaches them (docs/design.md, MCP across hosts): list_vms and list_hosts
// ask every host, create_vm places a VM as the laptop's `runner create`
// does, and a call naming a VM or a run goes, as it came, to the VM's host,
// through one `ssh <peer> runner mcp` each (peers::Remote).
use crate::best;
use crate::checkpoint;
use crate::claude;
use crate::creds;
use crate::doctor::{self, Report, System};
use crate::exec::{self, Captured};
use crate::hosts::{self, HostRow};
use crate::moves::{self, Moved};
use crate::peers::{Remote, structured};
use crate::proto::Request;
use crate::rebase::{self, Rebased};
use crate::runs::{self, AgentRun, Events};
use crate::services::{self, InVm, Service};
use crate::sessions::{self, Pane, Session};
use crate::store::{BASE, Checkpoint, Config, Store, Vm, parse_mem};
use crate::systemd::{self, VmStatus};
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct Tools {
    store: Store,
    /// Who exec runs commands as.
    user: String,
    /// What this host is called: as the laptop lists it, less its user@,
    /// else the kernel's name for it.
    name: String,
    /// It as the laptop lists it, like runner@kvm-host, if it said.
    listed: Option<String>,
    /// The other hosts, in the laptop's order. A peer's own MCP has none.
    peers: Arc<Vec<Remote>>,
    /// Which peer each VM was on when last listed, or made there since, so
    /// a VM on a peer that has stopped answering is named as there.
    seen: Arc<Mutex<HashMap<String, usize>>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CreateArgs {
    /// Name for the new VM: up to 32 of a-z, 0-9 and -.
    name: String,
    /// The VM whose current disk to fork. Defaults to base.
    from: Option<String>,
    /// vCPUs: 1 or an even number up to 32. Defaults to the source VM's, else 2.
    cpus: Option<u32>,
    /// Memory, like "4G" or "4096" (MiB). Defaults to the source VM's, else 2G.
    mem: Option<String>,
    /// An absolute path to a kernel for this VM, in place of the host's.
    kernel: Option<std::path::PathBuf>,
    /// How long it goes unused before it sleeps: minutes, like "10", or
    /// "90s", "10m", "2h", or "never". Defaults to the source VM's, else 10
    /// minutes.
    idle: Option<String>,
    /// The host to make it on, as list_hosts names it. Left out, it goes
    /// where the VM it forks is, else to the host with the most free memory.
    host: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct NameArg {
    /// The VM's name.
    name: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct VmList {
    vms: Vec<HostedVm>,
    /// The hosts that didn't answer, so their VMs aren't listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unreachable: Vec<Unreachable>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostedVm {
    #[serde(flatten)]
    vm: Vm,
    /// The host it's on, as list_hosts names it.
    host: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct Unreachable {
    /// The host, as list_hosts would name it.
    host: String,
    /// Why it didn't answer, like what ssh said.
    error: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct MoveArgs {
    /// The VM's name.
    name: String,
    /// The host to move it to, as list_hosts names it.
    host: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CheckpointArgs {
    /// The VM's name.
    name: String,
    /// A note to know the checkpoint by.
    comment: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RestoreArgs {
    /// The VM's name.
    name: String,
    /// The checkpoint's id, from list_checkpoints.
    id: u64,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct CheckpointList {
    checkpoints: Vec<Checkpoint>,
}

/// How long exec waits on a command unless told.
pub const EXEC_TIMEOUT_SECS: u64 = 120;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExecArgs {
    /// The VM's name. If it's stopped, it's started first.
    name: String,
    /// The program and its arguments, run as they are, not through a shell:
    /// ["sh", "-c", "..."] for shell syntax.
    argv: Vec<String>,
    /// Text for the command's stdin, which then closes. Left out, stdin is
    /// closed from the start.
    stdin: Option<String>,
    /// Seconds before the command is killed. Defaults to 120; at most 86400.
    timeout_secs: Option<u64>,
    /// Variables to set for it, like {"FOO": "bar"}, over a clean
    /// environment. CLAUDE_CODE_OAUTH_TOKEN, ANTHROPIC_API_KEY and GH_TOKEN
    /// are refused: credentials reach a VM only through `runner creds`.
    env: Option<BTreeMap<String, String>>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionList {
    sessions: Vec<Session>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionArgs {
    /// The VM's name.
    name: String,
    /// The tmux session's name.
    session: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct Killed {
    killed: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReadArgs {
    /// The VM's name.
    name: String,
    /// The tmux session's name.
    session: String,
    /// How many of the pane's last lines: 1 to 2000, default 100.
    lines: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AddServiceArgs {
    /// The VM's name. If it's stopped, it's started first.
    name: String,
    /// The service's name: up to 32 of A-Z, a-z, 0-9, _ and -.
    service: String,
    /// The program and its arguments, run as they are, not through a shell:
    /// ["sh", "-c", "..."] for shell syntax. A relative program with a / in
    /// it, like ./serve, is taken from dir.
    argv: Vec<String>,
    /// The directory it runs in: absolute, and there in the VM. Defaults to
    /// /home/agent/work.
    dir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ServiceArgs {
    /// The VM's name. If it's stopped, it's started first.
    name: String,
    /// The service's name.
    service: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ServiceList {
    services: Vec<Service>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct Removed {
    removed: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RunAgentArgs {
    /// The VM's name. A stopped VM is booted for the run and shut down after it.
    name: String,
    /// What to ask Claude Code.
    prompt: String,
    /// Where in the VM Claude Code runs; it has to exist. Defaults to /home/agent/work.
    dir: Option<String>,
    /// A Claude Code session to carry on: the session_id in an earlier run's init event.
    resume: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BestOfArgs {
    /// The VM to fork. It isn't touched: a running or sleeping one is
    /// checkpointed, and the forks copy that.
    name: String,
    /// What to ask Claude Code in each fork.
    prompt: String,
    /// How many forks: 2 to 4. Each run costs its own tokens.
    n: u32,
    /// Where in each fork Claude Code runs; it has to exist. Defaults to /home/agent/work.
    dir: Option<String>,
    /// A Claude Code session each fork carries on: the session_id in an earlier run's init event.
    resume: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RunArg {
    /// The run's id, from run_agent or list_runs, like dev.1.
    run: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RunOutputArgs {
    /// The run's id, from run_agent or list_runs, like dev.1.
    run: String,
    /// Where to read from: 0, the default, or the cursor the last reply gave.
    cursor: Option<u64>,
    /// How long to wait for an event while there's nothing new: 45 seconds,
    /// the default and the most.
    wait_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostList {
    hosts: Vec<HostRow>,
    /// The hosts that didn't answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unreachable: Vec<Unreachable>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct RunList {
    runs: Vec<AgentRun>,
}

// Named here, or clients see the SDK's own name.
#[tool_handler]
impl ServerHandler for Tools {
    fn get_info(&self) -> ServerConfig {
        let tools = ServerCapabilities::builder().enable_tools().build();
        let me = Implementation::new("runner", env!("CARGO_PKG_VERSION"));
        ServerConfig::new(tools).with_server_info(me)
    }

    // A call for a VM or run on a peer goes there; the rest run here. Each
    // holds this host up while it works, a peer's too, and the session
    // between calls doesn't (src/power.rs).
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let _busy = crate::power::Busy::take(self.store.root());
        if !self.peers.is_empty() {
            let there = match self.route(&request).await {
                Ok(None) => None,
                Ok(Some((i, request))) => {
                    let changes = self.changes(&request);
                    let moves_to = self.moves_to(&request);
                    let tries = tries_of(&request);
                    let answer = self.peers[i].call(request).await;
                    if let Ok(result) = &answer
                        && result.is_error != Some(true)
                    {
                        tries.iter().for_each(|vm| self.note(vm, i, true));
                    }
                    if let (Ok(result), Some((vm, made))) = (&answer, changes)
                        && result.is_error != Some(true)
                    {
                        self.note(&vm, i, made);
                        if let Some((vm, j)) = moves_to {
                            self.note(&vm, j, true);
                        }
                    }
                    Some(answer.map_err(|g| g.to_string()))
                }
                Err(why) => Some(Err(why)),
            };
            if let Some(answer) = there {
                let result = answer
                    .unwrap_or_else(|why| CallToolResult::error(vec![ContentBlock::text(why)]));
                return Ok(CallToolResponse::Complete(result));
            }
        }
        let call = ToolCallContext::new(self, request, context);
        Self::tool_router().call(call).await
    }
}

// Tool errors are strings, so a bad call becomes an error result the client
// can read, not a broken session.
#[tool_router]
impl Tools {
    #[tool(
        description = "List the VMs on every host this runner reaches, each with its host, its current disk generation, and the space its disks use (allocated blocks, not apparent size). A host that doesn't answer is named in `unreachable`, with why."
    )]
    async fn list_vms(&self) -> Result<Json<VmList>, String> {
        let here = self.store.list().map_err(|e| e.to_string())?;
        let mut vms: Vec<HostedVm> = (here.into_iter())
            .map(|vm| HostedVm {
                vm,
                host: self.name.clone(),
            })
            .collect();
        let mut unreachable = Vec::new();
        let all: Vec<usize> = (0..self.peers.len()).collect();
        for (peer, listed) in self.peers.iter().zip(self.survey(&all).await) {
            match listed {
                Ok(theirs) => vms.extend(theirs.into_iter().map(|vm| HostedVm {
                    vm,
                    host: peer.name.clone(),
                })),
                Err(error) => unreachable.push(Unreachable {
                    host: peer.name.clone(),
                    error,
                }),
            }
        }
        Ok(Json(VmList { vms, unreachable }))
    }

    #[tool(
        description = "Create a VM whose disk is a copy-on-write fork of another VM's current disk (base unless `from` names one). It keeps that VM's sizes and idle time unless told others. It sleeps once unused for its idle time, and any tool that needs its guest wakes it. It goes on `host` if given, else on the host of the VM it forks (base is on every host), else on the host with the most free memory. A name is one VM's across every host, base aside."
    )]
    fn create_vm(&self, Parameters(args): Parameters<CreateArgs>) -> Result<Json<Vm>, String> {
        // Another host's name took the call there (route).
        if let Some(host) = args.host.as_deref()
            && !self.is_me(host)
        {
            return Err(self.no_host(host));
        }
        let from = args.from.as_deref().unwrap_or(BASE);
        let mem_mib = args.mem.as_deref().map(parse_mem).transpose();
        let config = Config {
            cpus: args.cpus,
            mem_mib: mem_mib.map_err(|e| e.to_string())?,
            kernel: args.kernel,
            idle: args.idle.as_deref().map(str::parse).transpose()?,
            url: None,
        };
        self.store
            .fork_with(from, &args.name, &config)
            .map(Json)
            .map_err(|e| e.to_string())
    }

    #[tool(
        description = "Boot a VM from its current disk, or wake it if it's sleeping. Returns once the guest has started. The VM can't be deleted or undone while it runs or sleeps."
    )]
    async fn start_vm(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<VmStatus>, String> {
        let store = self.store.clone();
        blocking(move || systemd::start(&store, &name)).await
    }

    #[tool(
        description = "Shut a VM down with Ctrl-Alt-Del, killing it after 30 seconds. A clean shutdown keeps what the VM wrote to its disk; a kill rolls the disk back. A sleeping VM is woken first, so it shuts down cleanly."
    )]
    async fn stop_vm(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<VmStatus>, String> {
        let store = self.store.clone();
        blocking(move || systemd::stop(&store, &name)).await
    }

    #[tool(
        description = "Say whether a VM is stopped, booting, running, sleeping or stopping; while it's up, which credentials its guest took, by name; and its URL on the tailnet, which goes to port 8080 in the VM unless `runner url --port` set another, answers while the VM is up, and wakes it if it's asleep."
    )]
    fn vm_status(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<VmStatus>, String> {
        systemd::status(&self.store, &name)
            .map(Json)
            .map_err(|e| e.to_string())
    }

    #[tool(
        description = "Put a running VM to sleep: its memory and devices go to a snapshot on the host and its Firecracker stops, so it takes no CPU or RAM. wake_vm, start_vm or exec brings it back in about a second with every process as it was. Open shells and execs end; tmux sessions live on. Fails while a run is going, and before the guest is up."
    )]
    async fn sleep_vm(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<VmStatus>, String> {
        let store = self.store.clone();
        blocking(move || systemd::sleep(&store, &name)).await
    }

    #[tool(
        description = "Wake a sleeping VM where it was, and set its clock, which stood still while it slept. A VM awake already stays so."
    )]
    async fn wake_vm(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<VmStatus>, String> {
        let store = self.store.clone();
        blocking(move || systemd::wake(&store, &name)).await
    }

    #[tool(
        description = "Delete a VM and all its disk generations. Returns the VM as it was. Fails if a run holds it."
    )]
    fn delete_vm(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<Vm>, String> {
        self.store
            .delete(&name)
            .map(Json)
            .map_err(|e| e.to_string())
    }

    #[tool(
        description = "Check this host can run VMs: KVM, the pinned Firecracker, reflink under the VM root, the guest kernel, the base VM, NAT, the systemd unit, and the SSH key other hosts trust this one by. `ok` is false if any check fails; warnings don't fail."
    )]
    fn doctor(&self) -> Result<Json<Report>, String> {
        Ok(Json(doctor::check(&System, &self.store)))
    }

    #[tool(
        description = "List the hosts this runner reaches, this one first, each with its name, how many VMs it has (base included), how many are up (booting, running, sleeping or stopping), and its free memory in bytes (MemAvailable). A host that doesn't answer is named in `unreachable`, with why."
    )]
    async fn list_hosts(&self) -> Result<Json<HostList>, String> {
        // systemd is asked about each VM here while the peers are asked.
        let store = self.store.clone();
        let here = tokio::task::spawn_blocking(move || hosts::row(&store));
        let all: Vec<usize> = (0..self.peers.len()).collect();
        let answers = self.ask("list_hosts", &all).await;
        let mut row = here
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        row.name = self.name.clone();
        let (mut hosts, mut unreachable) = (vec![row], Vec::new());
        for (peer, answer) in self.peers.iter().zip(answers) {
            match answer.and_then(|v| peer_row(&v, &peer.name)) {
                Ok(row) => hosts.push(row),
                Err(error) => unreachable.push(Unreachable {
                    host: peer.name.clone(),
                    error,
                }),
            }
        }
        Ok(Json(HostList { hosts, unreachable }))
    }

    #[tool(
        description = "Move a VM to another host, as list_hosts names it, under the same name: its disk, your checkpoints, its settings, services and allowlist. A running or sleeping VM is stopped, which ends its processes and sessions, moved, and started there, awake. Its address and URL change, and its runs stay behind. The disks stream host to host; the VM is deleted here only once the other host has taken it, and a move that fails leaves it where it was, as it was. Fails while a run is going, when the VM is on that host already, and when that host has a VM by that name. Returns the VM as its new host has it."
    )]
    async fn move_vm(
        &self,
        Parameters(MoveArgs { name, host }): Parameters<MoveArgs>,
    ) -> Result<Json<Moved>, String> {
        // A VM on a peer went there (route), with the host as listed.
        if self.is_me(&host) {
            return Err(format!("{name} is on {} already", self.name));
        }
        let at = self.peers.iter().position(|p| p.is(&host));
        let target = match at {
            Some(i) => self.peers[i].target.clone(),
            None if !self.peers.is_empty() => return Err(self.no_host(&host)),
            // A peer's own MCP has no peers: the first host gave it the
            // host as ssh takes it, which the move holds to that.
            None => host,
        };
        let (store, user, here, vm) = (
            self.store.clone(),
            self.user.clone(),
            self.name.clone(),
            name.clone(),
        );
        // On a thread the runtime waits for as it shuts down, so a session
        // that ends partway doesn't end the move.
        let moved = blocking(move || moves::move_vm(&store, &vm, &target, &user, &here)).await?;
        if let Some(i) = at {
            self.note(&name, i, true);
        }
        Ok(moved)
    }

    #[tool(description = "Roll a VM back to its previous committed disk generation.")]
    fn undo_vm(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<Vm>, String> {
        self.store.undo(&name).map(Json).map_err(|e| e.to_string())
    }

    #[tool(
        description = "Give a stopped VM the current base's system, runner-guest and Claude Code included, as a new disk generation that keeps its /home, /root, /etc/machine-id and services. Anything else it installed, like apt packages, is gone, and so is what's in `skipped`: undo_vm brings the old disk back. Refused while it's up or a run on it is going: stop_vm first. Can take minutes for a big /home."
    )]
    async fn rebase_vm(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<Rebased>, String> {
        let store = self.store.clone();
        blocking(move || rebase::rebase(&store, &name)).await
    }

    #[tool(
        description = "Freeze a copy of a VM's disk, with an optional comment, for restore_vm to bring back. A running VM is synced and paused for the copy, then carries on; a sleeping one is woken first."
    )]
    async fn checkpoint_vm(
        &self,
        Parameters(args): Parameters<CheckpointArgs>,
    ) -> Result<Json<Checkpoint>, String> {
        let store = self.store.clone();
        blocking(move || checkpoint::checkpoint(&store, &args.name, args.comment.as_deref())).await
    }

    #[tool(
        description = "List a VM's checkpoints, newest first. Generations that runs, imports and restores made are listed too, with auto set; agent marks one the agent in the VM made itself (runner-guest checkpoint), and run one a run took before joining the running VM: gc keeps only the newest 20 of those two kinds together, and every other checkpoint; current marks the disk the next boot starts from."
    )]
    fn list_checkpoints(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<CheckpointList>, String> {
        let checkpoints = self.store.checkpoints(&name).map_err(|e| e.to_string())?;
        Ok(Json(CheckpointList { checkpoints }))
    }

    #[tool(
        description = "Run a command in a VM, starting the VM first if it's stopped, and return its exit code (or the signal that ended it), stdout and stderr. It runs as the agent user in its home directory, without a terminal, with `env` set over a clean environment; a credential's variable (CLAUDE_CODE_OAUTH_TOKEN, ANTHROPIC_API_KEY, GH_TOKEN) is refused, since credentials reach a VM only through `runner creds`. Each stream comes back whole up to 32 KiB; past that, its first 16 KiB and the last 16 KiB, with a line between saying how many bytes were cut, and its truncated flag set. Past timeout_secs (default 120) the command is killed and timed_out is true."
    )]
    async fn exec(&self, Parameters(args): Parameters<ExecArgs>) -> Result<Json<Captured>, String> {
        if args.argv.is_empty() {
            return Err("argv is empty: give the program and its arguments".into());
        }
        let env: Vec<(String, String)> = args.env.unwrap_or_default().into_iter().collect();
        if let Some((bad, _)) = env
            .iter()
            .find(|(k, _)| k.is_empty() || k.contains(['=', '\0']))
        {
            return Err(format!("{bad:?} isn't a variable name"));
        }
        // Before the VM is looked at, so a refused request boots nothing.
        creds::refuse(env.iter().map(|(k, _)| k.as_str())).map_err(|e| e.to_string())?;
        let store = self.store.clone();
        let req = Request {
            argv: args.argv,
            env,
            cwd: None,
            user: Some(self.user.clone()),
            tty: None,
        };
        let stdin = args.stdin.unwrap_or_default();
        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(EXEC_TIMEOUT_SECS));
        blocking(move || {
            // In use until the command is done.
            let exec::Conn {
                stream,
                in_use: _in_use,
            } = exec::open(&store, &args.name)?;
            exec::capture(stream, &req, stdin.as_bytes(), timeout, exec::OUTPUT_CAP)
        })
        .await
    }

    #[tool(
        description = "List the agent user's tmux sessions in a VM that's up: each one's name, the command its active pane runs, when it started, and whether a terminal is attached. A sleeping VM is woken to look; a stopped one has none, and isn't started to look."
    )]
    async fn list_sessions(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<SessionList>, String> {
        let (store, user) = (self.store.clone(), self.user.clone());
        let listed = move || sessions::list(&store, &name, &user);
        let Json(sessions) = blocking(listed).await?;
        Ok(Json(SessionList { sessions }))
    }

    #[tool(
        description = "Read the last `lines` (default 100, at most 2000) of a tmux session's active pane in a VM that's up, waking it if it's sleeping, as plain text, reaching back into its history past the screen. Keeps at most the last 32 KiB, with truncated set if fewer lines fit."
    )]
    async fn read_session(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<Json<Pane>, String> {
        let (store, user) = (self.store.clone(), self.user.clone());
        let lines = args.lines.unwrap_or(sessions::DEFAULT_LINES);
        blocking(move || sessions::read(&store, &args.name, &user, &args.session, lines)).await
    }

    #[tool(
        description = "End a tmux session in a VM that's up, waking it if it's sleeping, and everything running in it."
    )]
    async fn kill_session(
        &self,
        Parameters(args): Parameters<SessionArgs>,
    ) -> Result<Json<Killed>, String> {
        let (store, user) = (self.store.clone(), self.user.clone());
        let session = args.session.clone();
        blocking(move || sessions::kill(&store, &args.name, &user, &args.session)).await?;
        Ok(Json(Killed { killed: session }))
    }

    #[tool(
        description = "Run a command in a VM as a service: a systemd unit that runs it as the agent user, starts it now and at every boot, and starts it again a second after it exits; sleep keeps it running as it was. Starts a stopped VM, or wakes a sleeping one, first. Returns the service once its program has started; one that can't start fails, leaving nothing behind, with its command and the journal's last lines. A service gets no credentials. Fails if the VM has a service by that name: remove_service it first."
    )]
    async fn add_service(
        &self,
        Parameters(args): Parameters<AddServiceArgs>,
    ) -> Result<Json<Service>, String> {
        let user = self.user.clone();
        let store = self.store.clone();
        blocking(move || {
            let spec = services::Spec {
                name: args.service,
                argv: args.argv,
                dir: args.dir.unwrap_or_else(|| claude::DIR.to_string()),
                user,
            };
            services::add(
                &InVm {
                    store: &store,
                    vm: &args.name,
                },
                &spec,
            )
        })
        .await
    }

    #[tool(
        description = "List a VM's services, by name: the command, directory and user each runs as, its systemd unit, and systemd's state (active, activating, failed, inactive) and sub-state (running, auto-restart, dead, ...). Starts a stopped VM, or wakes a sleeping one, first. For a service's log, exec [\"sudo\", \"journalctl\", \"-u\", unit, \"-n\", \"50\", \"--no-pager\"]."
    )]
    async fn list_services(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<ServiceList>, String> {
        let store = self.store.clone();
        let listed = move || {
            services::list(&InVm {
                store: &store,
                vm: &name,
            })
        };
        let Json(services) = blocking(listed).await?;
        Ok(Json(ServiceList { services }))
    }

    #[tool(
        description = "Stop a VM's service and remove its unit, so no boot starts it again. Starts a stopped VM, or wakes a sleeping one, first."
    )]
    async fn remove_service(
        &self,
        Parameters(args): Parameters<ServiceArgs>,
    ) -> Result<Json<Removed>, String> {
        let store = self.store.clone();
        let service = args.service.clone();
        blocking(move || {
            services::remove(
                &InVm {
                    store: &store,
                    vm: &args.name,
                },
                &args.service,
            )
        })
        .await?;
        Ok(Json(Removed { removed: service }))
    }

    #[tool(
        description = "Start Claude Code in a VM on a prompt, headless (`claude -p`, stream-json) and detached, and return the run at once; read it with run_output. A stopped VM is booted for the run and shut down after it: exit 0 commits what the run wrote to the disk, and anything else rolls it back. A running VM is checkpointed and joined as it is, and left running however the run ends: a failed or cancelled run's outcome is not_rolled_back, and restore_vm with its checkpoint rolls it back. One run per VM at a time. Claude Code runs as the agent user, with its permission prompts skipped: the VM is the sandbox."
    )]
    async fn run_agent(
        &self,
        Parameters(args): Parameters<RunAgentArgs>,
    ) -> Result<Json<AgentRun>, String> {
        let (store, user) = (self.store.clone(), self.user.clone());
        blocking(move || {
            let exe = std::env::current_exe().map_err(|e| format!("finding runner: {e}"))?;
            let start = runs::Start {
                vm: &args.name,
                prompt: &args.prompt,
                dir: args.dir.as_deref().unwrap_or(claude::DIR),
                resume: args.resume.as_deref(),
                user: &user,
            };
            runs::start(&store, &exe, &start).map_err(|e| e.to_string())
        })
        .await
    }

    #[tool(
        description = "Best of N: fork a VM into <name>-1 to <name>-<n>, n from 2 to 4, start the prompt in each at once as an ordinary run, and return the n runs; read each with run_output, as any run. Each fork is new and stopped, so its run boots it and commits only a clean exit, and its result measures what that try changed against the VM's disk. The VM itself isn't touched: a stopped one's current disk is forked, and a running or sleeping one is checkpointed first (kind run), and that's forked. The forks get the default credentials, claude only, and the VM's sizes. Refused, with nothing made, if a fork's name is taken, if the host's free memory less 1 GiB can't hold n of the VM, while the VM has a run going, and while an earlier batch's forks are still VMs. Tokens cost n times one run's."
    )]
    async fn run_best_of(
        &self,
        Parameters(args): Parameters<BestOfArgs>,
    ) -> Result<Json<RunList>, String> {
        let (store, user) = (self.store.clone(), self.user.clone());
        blocking(move || {
            let exe = std::env::current_exe().map_err(|e| format!("finding runner: {e}"))?;
            let start = runs::Start {
                vm: &args.name,
                prompt: &args.prompt,
                dir: args.dir.as_deref().unwrap_or(claude::DIR),
                resume: args.resume.as_deref(),
                user: &user,
            };
            let runs = best::start(&store, &exe, &start, args.n).map_err(|e| e.to_string())?;
            Ok::<_, String>(RunList { runs })
        })
        .await
    }

    #[tool(
        description = "Read a run's stream-json events from `cursor`: 0 at first, then the cursor each reply gives. The reply has the run too. While it goes on with nothing new to read, this waits up to 45 seconds for an event. A reply holds up to 64 KiB of events and says if there's `more`; an event bigger than that comes as {\"runner_cut\": true, \"bytes\": n}. The run is over once its state isn't running: read on until `more` is false. That last reply has the run's `result`, unless the run was lost or never started: what the runner measured itself, the exit code, duration, whether the disk generation the run booted was committed or rolled back, and the paths that changed on that disk, less those only the VM's boot writes, counted in `boot_only`, with `agent_says`, the agent's own last message, kept apart: it's untrusted, so check it against `changed`."
    )]
    async fn run_output(
        &self,
        Parameters(args): Parameters<RunOutputArgs>,
    ) -> Result<Json<Events>, String> {
        let store = self.store.clone();
        let cursor = args.cursor.unwrap_or(0);
        let wait = args.wait_secs.map_or(runs::MAX_WAIT, Duration::from_secs);
        blocking(move || runs::events(&store, &args.run, cursor, wait)).await
    }

    #[tool(
        description = "Stop a run: Claude Code gets SIGTERM, then SIGKILL, and a VM the run booted rolls back and shuts down, while one it joined running is left as it is: restore_vm with the run's checkpoint rolls it back. A run whose Claude Code is done stops measuring its disk, and its result says so. Returns the run once it has ended, or as it stands after two minutes. Fails for a run that's already over."
    )]
    async fn cancel_run(
        &self,
        Parameters(RunArg { run }): Parameters<RunArg>,
    ) -> Result<Json<AgentRun>, String> {
        let store = self.store.clone();
        blocking(move || runs::cancel(&store, &run)).await
    }

    #[tool(
        description = "List a VM's runs, newest first, each with its state: running, exited (with exit_code, or signal), cancelled, failed (with error) or lost; and its outcome: committed or rolled_back for a run that booted the VM, not_rolled_back for one that joined it running and failed or was cancelled, with the checkpoint it took first to restore by hand."
    )]
    fn list_runs(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<RunList>, String> {
        let runs = runs::list(&self.store, &name).map_err(|e| e.to_string())?;
        Ok(Json(RunList { runs }))
    }

    #[tool(
        description = "List the tries of a VM's latest best of N (run_best_of), in order, side by side: each with its run's id, the run as list_runs gives it, and once the run is over, its result, as run_output's last reply has it: exit code, duration, outcome, the paths that changed, and agent_says, which is untrusted. A try whose fork or run has been deleted or moved since is gone: true, with no run. Returns at once: wait on a run with run_output. `tries` is empty if the VM never had a batch."
    )]
    fn list_tries(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<best::Standing>, String> {
        let now = best::standing(&self.store, &name).map_err(|e| e.to_string())?;
        Ok(Json(now))
    }

    #[tool(
        description = "Make a copy of checkpoint `id` the VM's current disk. undo_vm reverses it, and the checkpoint stays to restore again. A running or sleeping VM is stopped first, ending what runs in it, and started again after, awake; one stopping is waited out and stays stopped. Fails while a run is going."
    )]
    async fn restore_vm(
        &self,
        Parameters(args): Parameters<RestoreArgs>,
    ) -> Result<Json<Vm>, String> {
        let store = self.store.clone();
        blocking(move || checkpoint::restore(&store, &args.name, args.id)).await
    }
}

// Starting, stopping and exec wait for seconds or more, which would hold up
// every other request on this single-threaded server.
async fn blocking<T, E>(
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<Json<T>, String>
where
    T: Send + 'static,
    E: ToString + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(done) => done.map(Json).map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Serves until the client closes stdin. exec runs commands as `user`, here
/// and on each of `peers`, the other hosts as ssh takes them, which are
/// dialed when first needed. `listed` is this host as the laptop lists it.
pub async fn serve(
    store: Store,
    user: String,
    peers: Vec<String>,
    listed: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let peers = peers.iter().map(|p| Remote::new(p, &user)).collect();
    let name = listed.as_deref().map_or_else(hosts::hostname, |l| {
        l.rsplit_once('@').map_or(l, |(_, h)| h).to_string()
    });
    let tools = Tools {
        store,
        user,
        name,
        listed,
        peers: Arc::new(peers),
        seen: Arc::default(),
    };
    let running = tools.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

// Where a call goes, past this host's own.
impl Tools {
    /// None to run here, or the peer to send it to, as it should arrive.
    async fn route(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<(usize, CallToolRequestParams)>, String> {
        let arg = |k: &str| {
            let args = request.arguments.as_ref();
            args.and_then(|a| a.get(k)).and_then(Value::as_str)
        };
        let vm = match request.name.as_ref() {
            "list_vms" | "list_hosts" | "doctor" => return Ok(None),
            "create_vm" => return self.place(request).await,
            // A run is its VM's, the part before the last dot.
            "run_output" | "cancel_run" => arg("run")
                .and_then(|r| r.rsplit_once('.'))
                .map(|(vm, _)| vm),
            _ => arg("name"),
        };
        let Some(vm) = vm else { return Ok(None) };
        let at = self.locate(vm).await?;
        self.check_tries(request, at).await?;
        let Some(i) = at else {
            return Ok(None);
        };
        if request.name == "move_vm" {
            return self.move_from(i, vm, request).map(Some);
        }
        Ok(Some((i, request.clone())))
    }

    /// Refuses run_best_of when a host but the VM's, at `at` (None for this
    /// one), has a name its tries take, as create_vm refuses one, so nothing
    /// is made: the VM's host refuses one it has itself.
    async fn check_tries(
        &self,
        request: &CallToolRequestParams,
        at: Option<usize>,
    ) -> Result<(), String> {
        let names = tries_of(request);
        if names.is_empty() {
            return Ok(());
        }
        let here = self.store.list().map_err(|e| e.to_string())?;
        let all: Vec<usize> = (0..self.peers.len()).collect();
        let lists = self.survey(&all).await;
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for name in &names {
            let has = |l: &Result<Vec<Vm>, String>, i: usize| match l {
                Ok(l) => l.iter().any(|v| v.name == *name),
                Err(_) => seen.get(name) == Some(&i),
            };
            let on = if at.is_some() && here.iter().any(|v| v.name == *name) {
                Some(self.name.as_str())
            } else {
                (lists.iter().enumerate())
                    .find(|&(i, l)| Some(i) != at && has(l, i))
                    .map(|(i, _)| self.peers[i].name.as_str())
            };
            if let Some(on) = on {
                return Err(format!(
                    "{on} has a VM named {name} already: VM names are one per name across hosts"
                ));
            }
        }
        Ok(())
    }

    /// move_vm for a VM on peer `i`, with its host as ssh takes it, which
    /// that peer's MCP, having no peers, dials as it is: a peer as listed,
    /// or this host as the laptop listed it.
    fn move_from(
        &self,
        i: usize,
        vm: &str,
        request: &CallToolRequestParams,
    ) -> Result<(usize, CallToolRequestParams), String> {
        let args = request.arguments.as_ref();
        let host = args.and_then(|a| a.get("host")).and_then(Value::as_str);
        // Arguments that don't parse are the peer's tool's to refuse.
        let Some(host) = host else {
            return Ok((i, request.clone()));
        };
        let target = if self.is_me(host) {
            self.listed.clone().ok_or_else(|| {
                let me = &self.name;
                format!("{me} wasn't given as the laptop lists it (--name), so {vm}'s host can't reach it")
            })?
        } else {
            match self.peers.iter().position(|p| p.is(host)) {
                Some(j) if j == i => return Err(format!("{vm} is on {host} already")),
                Some(j) => self.peers[j].target.clone(),
                None => return Err(self.no_host(host)),
            }
        };
        let mut there = request.clone();
        if let Some(args) = there.arguments.as_mut() {
            args.insert("host".into(), Value::String(target));
        }
        Ok((i, there))
    }

    /// The peer that has `vm`, or None for this host: one that has it, or,
    /// when every host answered and none has it, the one to say so. The
    /// peer it was last listed or made on is asked first, alone, so a VM
    /// there isn't held up by a host that's down.
    async fn locate(&self, vm: &str) -> Result<Option<usize>, String> {
        if self.store.get(vm).is_ok() {
            return Ok(None);
        }
        let has = |l: &Lists, i: usize| l[i].as_ref().is_ok_and(|l| l.iter().any(|v| v.name == vm));
        let last = self.last_seen(vm);
        let first: Vec<usize> = last.into_iter().collect();
        let mut lists = self.survey(&first).await;
        if let Some(i) = last
            && has(&lists, i)
        {
            return Ok(Some(i));
        }
        let rest: Vec<usize> = (0..self.peers.len()).filter(|i| Some(*i) != last).collect();
        for (i, list) in self.survey(&rest).await.into_iter().enumerate() {
            if rest.contains(&i) {
                lists[i] = list;
            }
        }
        if let Some(i) = (0..lists.len()).find(|&i| has(&lists, i)) {
            return Ok(Some(i));
        }
        let down: Vec<(usize, &String)> = (lists.iter().enumerate())
            .filter_map(|(i, l)| l.as_ref().err().map(|why| (i, why)))
            .collect();
        if down.is_empty() {
            return Ok(None);
        }
        // Where it was last listed, if that's one that didn't answer now.
        if let Some((i, why)) = down.iter().find(|(i, _)| Some(*i) == self.last_seen(vm)) {
            let on = &self.peers[*i].name;
            return Err(format!("{vm} is on {on}, which didn't answer: {why}"));
        }
        let whys: Vec<String> = (down.iter())
            .map(|(i, why)| format!("{} didn't answer: {why}", self.peers[*i].name))
            .collect();
        Err(format!(
            "no host that answered has a VM named {vm}, and {}",
            whys.join("; ")
        ))
    }

    fn last_seen(&self, vm: &str) -> Option<usize> {
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.get(vm).copied()
    }

    /// The VM a call makes, (vm, true), or deletes, (vm, false), base aside:
    /// what a peer that does it has then, before the next list says so. A
    /// move deletes it there and makes it on its host (moved).
    fn changes(&self, request: &CallToolRequestParams) -> Option<(String, bool)> {
        let made = match request.name.as_ref() {
            "create_vm" => true,
            "delete_vm" | "move_vm" => false,
            _ => return None,
        };
        let args = request.arguments.as_ref();
        let vm = args.and_then(|a| a.get("name")).and_then(Value::as_str)?;
        (vm != BASE).then(|| (vm.to_string(), made))
    }

    /// The VM a move sends to another peer, and that peer, to note it there
    /// once it's done. One moved here isn't noted: locate asks here first.
    fn moves_to(&self, request: &CallToolRequestParams) -> Option<(String, usize)> {
        let args = request.arguments.as_ref();
        let text = |k: &str| args.and_then(|a| a.get(k)).and_then(Value::as_str);
        if request.name != "move_vm" {
            return None;
        }
        let host = text("host")?;
        let j = self.peers.iter().position(|p| p.is(host))?;
        Some((text("name")?.to_string(), j))
    }

    /// Notes `vm` at peer `i`, which made it, or drops it there, which
    /// deleted it, as the laptop's `runner create` and `delete` keep its
    /// cache: so a name stays taken, or is freed, while that peer is down.
    fn note(&self, vm: &str, i: usize, made: bool) {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if made {
            seen.insert(vm.to_string(), i);
        } else if seen.get(vm) == Some(&i) {
            seen.remove(vm);
        }
    }

    /// Where create_vm makes its VM, as the laptop's `runner create` places
    /// one: on `host`; base here; a fork where the VM it forks is; else on
    /// the host with the most free memory. A name another host has, or had
    /// when last listed or made there if it doesn't answer now, is refused,
    /// base aside.
    async fn place(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<(usize, CallToolRequestParams)>, String> {
        let args = request.arguments.clone().unwrap_or_default();
        let text = |k: &str| args.get(k).and_then(Value::as_str);
        // Arguments that don't parse are the tool's to refuse, here.
        let Some(name) = text("name") else {
            return Ok(None);
        };
        let host = match text("host") {
            None => None,
            Some(h) if self.is_me(h) => Some(None),
            Some(h) => match self.peers.iter().position(|p| p.is(h)) {
                Some(i) => Some(Some(i)),
                None => return Err(self.no_host(h)),
            },
        };
        let here = self.store.list().map_err(|e| e.to_string())?;
        let all: Vec<usize> = (0..self.peers.len()).collect();
        let lists = self.survey(&all).await;
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // The host that has `vm`: Some(None) for this one.
        let holder = |vm: &str| {
            if here.iter().any(|v| v.name == vm) {
                return Some(None);
            }
            let listed = |(i, l): (usize, &Result<Vec<Vm>, String>)| match l {
                Ok(l) => l.iter().any(|v| v.name == vm).then_some(i),
                Err(_) => (seen.get(vm) == Some(&i)).then_some(i),
            };
            lists.iter().enumerate().find_map(listed).map(Some)
        };
        if name != BASE
            && let Some(at) = holder(name)
        {
            let on = at.map_or(self.name.as_str(), |i| self.peers[i].name.as_str());
            return Err(format!(
                "{on} has a VM named {name} already: VM names are one per name across hosts"
            ));
        }
        let from = text("from").filter(|f| *f != BASE);
        let at = match (host, from.and_then(holder)) {
            (Some(at), _) => at,
            _ if name == BASE => None,
            (None, Some(Some(i))) if lists[i].is_err() => {
                let why = lists[i].as_ref().err().map_or("", String::as_str);
                let (from, on) = (from.unwrap_or_default(), &self.peers[i].name);
                return Err(format!("{from} is on {on}, which didn't answer: {why}"));
            }
            (None, Some(at)) => at,
            (None, None) => self.most_free(&lists).await?,
        };
        let Some(i) = at else { return Ok(None) };
        // The peer makes it on itself, whatever this host calls it.
        let mut there = request.clone();
        if let Some(args) = there.arguments.as_mut() {
            args.remove("host");
        }
        Ok(Some((i, there)))
    }

    // The host with the most free memory, this one first among equals, of
    // this one and the peers whose lists came.
    async fn most_free(&self, lists: &Lists) -> Result<Option<usize>, String> {
        let up: Vec<usize> = (0..lists.len()).filter(|&i| lists[i].is_ok()).collect();
        let answers = self.ask("list_hosts", &up).await;
        let mut best: Option<(Option<usize>, u64)> =
            crate::host::mem_available().ok().map(|free| (None, free));
        for (i, answer) in answers.into_iter().enumerate() {
            let Ok(row) = answer.and_then(|v| peer_row(&v, &self.peers[i].name)) else {
                continue;
            };
            if best.is_none_or(|(_, most)| row.mem_available > most) {
                best = Some((Some(i), row.mem_available));
            }
        }
        best.map(|(at, _)| at).ok_or_else(|| {
            "no host said how much memory it has free, so the VM has nowhere to go".into()
        })
    }

    /// The VMs of each peer `which` picks, or why it didn't answer, in
    /// order, all asked at once, and where each VM is noted for when its
    /// peer stops answering. A peer's `host` on each, which a runner from
    /// before peers doesn't give, is left: this host names them.
    async fn survey(&self, which: &[usize]) -> Lists {
        #[derive(Deserialize)]
        struct Listed {
            vms: Vec<Vm>,
        }
        let answers = self.ask("list_vms", which).await;
        let lists: Lists = (answers.into_iter())
            .map(|answer| {
                let list: Listed = serde_json::from_value(answer?)
                    .map_err(|e| format!("its list_vms gave {e}"))?;
                Ok(list.vms)
            })
            .collect();
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        for (i, list) in lists.iter().enumerate() {
            if let Ok(vms) = list {
                seen.retain(|_, at| *at != i);
                for vm in vms.iter().filter(|v| v.name != BASE) {
                    seen.insert(vm.name.clone(), i);
                }
            }
        }
        lists
    }

    /// `tool`, with no arguments, on each of the peers `which` picks, at
    /// once: each one's structured answer, or why there's none, in order,
    /// within ASK_WAIT, so a peer that has stopped answering holds no list
    /// up for longer.
    async fn ask(&self, tool: &'static str, which: &[usize]) -> Vec<Result<Value, String>> {
        let mut asked = tokio::task::JoinSet::new();
        for &i in which {
            let peers = self.peers.clone();
            asked.spawn(async move {
                let mut call = CallToolRequestParams::new(tool);
                call.arguments = Some(Default::default());
                let answer = tokio::time::timeout(ASK_WAIT, peers[i].call(call)).await;
                let answer = answer
                    .map_err(|_| format!("no answer in {} seconds", ASK_WAIT.as_secs()))
                    .and_then(|a| a.map_err(|gone| gone.why));
                (i, answer.and_then(structured))
            });
        }
        let mut answers: Vec<Result<Value, String>> = (0..self.peers.len())
            .map(|_| Err("it wasn't asked".to_string()))
            .collect();
        while let Some(done) = asked.join_next().await {
            match done {
                Ok((i, answer)) => answers[i] = answer,
                Err(e) => eprintln!("runner: asking a peer: {e}"),
            }
        }
        answers
    }

    /// Whether `host` names this one: as listed, or by its name.
    fn is_me(&self, host: &str) -> bool {
        host == self.name || self.listed.as_deref() == Some(host)
    }

    fn no_host(&self, host: &str) -> String {
        let mut names = vec![self.name.as_str()];
        names.extend(self.peers.iter().map(|p| p.name.as_str()));
        format!("no host named {host}: the hosts are {}", names.join(", "))
    }
}

/// How long list_vms and list_hosts wait on a peer, past its dial
/// (peers::HELLO), before naming it as not answering.
const ASK_WAIT: Duration = Duration::from_secs(20);

/// Each peer's VMs, in order, or why there are none.
type Lists = Vec<Result<Vec<Vm>, String>>;

// The VMs a run_best_of call makes, as best::start names them; none for any
// other call, or an n the tool refuses.
fn tries_of(request: &CallToolRequestParams) -> Vec<String> {
    if request.name != "run_best_of" {
        return Vec::new();
    }
    let args = request.arguments.as_ref();
    let vm = args.and_then(|a| a.get("name")).and_then(Value::as_str);
    let n = args.and_then(|a| a.get("n")).and_then(Value::as_u64);
    let n = n.and_then(|n| u32::try_from(n).ok());
    match (vm, n) {
        (Some(vm), Some(n)) if (best::MIN..=best::MAX).contains(&n) => best::names(vm, n),
        _ => Vec::new(),
    }
}

// A peer's own row from its list_hosts, under the name this host calls it.
fn peer_row(answer: &Value, peer: &str) -> Result<HostRow, String> {
    let list: HostList =
        serde_json::from_value(answer.clone()).map_err(|e| format!("its list_hosts gave {e}"))?;
    let mut row = (list.hosts.into_iter().next()).ok_or("it gave no row")?;
    row.name = peer.to_string();
    Ok(row)
}
