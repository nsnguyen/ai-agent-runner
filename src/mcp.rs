// The CLI's commands as MCP tools, served on stdio. Local Claude Code reaches
// it as `ssh -T kvm-host runner mcp`, so anything printed to stdout must be
// protocol: log to stderr only.
use crate::checkpoint;
use crate::claude;
use crate::doctor::{self, Report, System};
use crate::exec::{self, Captured};
use crate::proto::Request;
use crate::runs::{self, AgentRun, Events};
use crate::services::{self, InVm, Service};
use crate::sessions::{self, Pane, Session};
use crate::store::{BASE, Checkpoint, Config, Store, Vm, parse_mem};
use crate::systemd::{self, VmStatus};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Tools {
    store: Store,
    /// Who exec runs commands as.
    user: String,
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
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct NameArg {
    /// The VM's name.
    name: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct VmList {
    vms: Vec<Vm>,
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
}

// Tool errors are strings, so a bad call becomes an error result the client
// can read, not a broken session.
#[tool_router]
impl Tools {
    #[tool(
        description = "List the VMs on this host, each one's current disk generation, and the space its disks use (allocated blocks, not apparent size)."
    )]
    fn list_vms(&self) -> Result<Json<VmList>, String> {
        let vms = self.store.list().map_err(|e| e.to_string())?;
        Ok(Json(VmList { vms }))
    }

    #[tool(
        description = "Create a VM whose disk is a copy-on-write fork of another VM's current disk (base unless `from` names one). It keeps that VM's sizes unless told others."
    )]
    fn create_vm(&self, Parameters(args): Parameters<CreateArgs>) -> Result<Json<Vm>, String> {
        let from = args.from.as_deref().unwrap_or(BASE);
        let mem_mib = args.mem.as_deref().map(parse_mem).transpose();
        let config = Config {
            cpus: args.cpus,
            mem_mib: mem_mib.map_err(|e| e.to_string())?,
            kernel: args.kernel,
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
        description = "Say whether a VM is stopped, booting, running, sleeping or stopping, and while it's up, which credentials its guest took, by name."
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
        description = "Check this host can run VMs: KVM, the pinned Firecracker, reflink under the VM root, the guest kernel, the base VM, NAT, and the systemd unit. `ok` is false if any check fails; warnings don't fail."
    )]
    fn doctor(&self) -> Result<Json<Report>, String> {
        Ok(Json(doctor::check(&System, &self.store)))
    }

    #[tool(description = "Roll a VM back to its previous committed disk generation.")]
    fn undo_vm(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<Vm>, String> {
        self.store.undo(&name).map(Json).map_err(|e| e.to_string())
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
        description = "List a VM's checkpoints, newest first. Generations that runs, imports and restores made are listed too, with auto set; agent marks one the agent in the VM made itself (runner-guest checkpoint); current marks the disk the next boot starts from."
    )]
    fn list_checkpoints(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<CheckpointList>, String> {
        let checkpoints = self.store.checkpoints(&name).map_err(|e| e.to_string())?;
        Ok(Json(CheckpointList { checkpoints }))
    }

    #[tool(
        description = "Run a command in a VM, starting the VM first if it's stopped, and return its exit code (or the signal that ended it), stdout and stderr. It runs as the agent user in its home directory, without a terminal. Each stream keeps its first 32 KiB, with a truncated flag. Past timeout_secs (default 120) the command is killed and timed_out is true."
    )]
    async fn exec(&self, Parameters(args): Parameters<ExecArgs>) -> Result<Json<Captured>, String> {
        if args.argv.is_empty() {
            return Err("argv is empty: give the program and its arguments".into());
        }
        let store = self.store.clone();
        let req = Request {
            argv: args.argv,
            env: Vec::new(),
            cwd: None,
            user: Some(self.user.clone()),
            tty: None,
        };
        let stdin = args.stdin.unwrap_or_default();
        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(EXEC_TIMEOUT_SECS));
        blocking(move || {
            let conn = exec::open(&store, &args.name)?;
            exec::capture(conn, &req, stdin.as_bytes(), timeout, exec::OUTPUT_CAP)
        })
        .await
    }

    #[tool(
        description = "List the agent user's tmux sessions in a running VM: each one's name, the command its active pane runs, when it started, and whether a terminal is attached. A VM that isn't running has none, and isn't started to look."
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
        description = "Read the last `lines` (default 100, at most 2000) of a tmux session's active pane in a running VM, as plain text, reaching back into its history past the screen. Keeps at most the last 32 KiB, with truncated set if fewer lines fit."
    )]
    async fn read_session(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<Json<Pane>, String> {
        let (store, user) = (self.store.clone(), self.user.clone());
        let lines = args.lines.unwrap_or(sessions::DEFAULT_LINES);
        blocking(move || sessions::read(&store, &args.name, &user, &args.session, lines)).await
    }

    #[tool(description = "End a tmux session in a running VM, and everything running in it.")]
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
        description = "Run a command in a VM as a service: a systemd unit that runs it as the agent user, starts it now and at every boot, and starts it again a second after it exits; sleep keeps it running as it was. Starts a stopped VM, or wakes a sleeping one, first. Returns the service once its program has started; one that can't start fails, leaving nothing behind, with the journal's last lines. A service gets no credentials. Fails if the VM has a service by that name: remove_service it first."
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
        description = "Start Claude Code in a VM on a prompt, headless (`claude -p`, stream-json) and detached, and return the run at once; read it with run_output. A stopped VM is booted for the run and shut down after it: exit 0 commits what the run wrote to the disk, and anything else rolls it back. A running VM is checkpointed and joined as it is: exit 0 leaves it running, and anything else restores that checkpoint, which restarts the VM. One run per VM at a time. Claude Code runs as the agent user, with its permission prompts skipped: the VM is the sandbox."
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
        description = "Read a run's stream-json events from `cursor`: 0 at first, then the cursor each reply gives. The reply has the run too. While it goes on with nothing new to read, this waits up to 45 seconds for an event. A reply holds up to 64 KiB of events and says if there's `more`; an event bigger than that comes as {\"runner_cut\": true, \"bytes\": n}. The run is over once its state isn't running: read on until `more` is false. That last reply has the run's `result`, unless the run was lost or never started: what the runner measured itself, the exit code, duration, whether the disk generation the run booted was committed or rolled back, and the paths that changed on that disk, with `agent_says`, the agent's own last message, kept apart: it's untrusted, so check it against `changed`."
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
        description = "Stop a run: Claude Code gets SIGTERM, then SIGKILL, and a VM the run booted rolls back and shuts down, while one it joined running gets the checkpoint it took back, which restarts it. A run whose Claude Code is done stops measuring its disk, and its result says so. Returns the run once it has ended, or as it stands after two minutes. Fails for a run that's already over."
    )]
    async fn cancel_run(
        &self,
        Parameters(RunArg { run }): Parameters<RunArg>,
    ) -> Result<Json<AgentRun>, String> {
        let store = self.store.clone();
        blocking(move || runs::cancel(&store, &run)).await
    }

    #[tool(
        description = "List a VM's runs, newest first, each with its state: running, exited (with exit_code, or signal), cancelled, failed (with error) or lost; and its outcome: committed or rolled_back for a run that booted the VM, rolled_back for one that joined it running and restored the checkpoint it took first."
    )]
    fn list_runs(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<RunList>, String> {
        let runs = runs::list(&self.store, &name).map_err(|e| e.to_string())?;
        Ok(Json(RunList { runs }))
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

/// Serves until the client closes stdin. exec runs commands as `user`.
pub async fn serve(
    store: Store,
    user: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let running = Tools { store, user }
        .serve(rmcp::transport::stdio())
        .await?;
    running.waiting().await?;
    Ok(())
}
