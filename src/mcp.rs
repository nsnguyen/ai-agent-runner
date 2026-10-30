// The CLI's commands as MCP tools, served on stdio. Local Claude Code reaches
// it as `ssh -T kvm-host runner mcp`, so anything printed to stdout must be
// protocol: log to stderr only.
use crate::store::{BASE, Config, Store, Vm, parse_mem};
use crate::systemd::{self, VmStatus};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct Tools {
    store: Store,
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
    #[tool(description = "List the VMs on this host and each one's current disk generation.")]
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
        description = "Boot a VM from its current disk. Returns once the guest has started. The VM can't be deleted or undone while it runs."
    )]
    async fn start_vm(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<VmStatus>, String> {
        let store = self.store.clone();
        blocking(move || systemd::start(&store, &name)).await
    }

    #[tool(
        description = "Shut a VM down with Ctrl-Alt-Del, killing it after 30 seconds. A clean shutdown keeps what the VM wrote to its disk; a kill rolls the disk back."
    )]
    async fn stop_vm(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<VmStatus>, String> {
        let store = self.store.clone();
        blocking(move || systemd::stop(&store, &name)).await
    }

    #[tool(description = "Say whether a VM is stopped, booting, running or stopping.")]
    fn vm_status(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<VmStatus>, String> {
        systemd::status(&self.store, &name)
            .map(Json)
            .map_err(|e| e.to_string())
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

    #[tool(description = "Roll a VM back to its previous committed disk generation.")]
    fn undo_vm(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> Result<Json<Vm>, String> {
        self.store.undo(&name).map(Json).map_err(|e| e.to_string())
    }
}

// Starting and stopping wait on systemd for seconds, which would hold up
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

/// Serves until the client closes stdin.
pub async fn serve(store: Store) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let running = Tools { store }.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}
