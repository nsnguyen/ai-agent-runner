//! ai-agent-runner: Firecracker microVMs for AI agents, run from one binary on
//! the KVM host and reached over SSH. docs/design.md says how it fits together.
pub mod boot;
pub mod changes;
pub mod creds;
pub mod doctor;
pub mod exec;
pub mod firecracker;
pub mod guest;
pub mod host;
pub mod mcp;
pub mod net;
pub mod proto;
pub mod result;
pub mod store;
pub mod systemd;
