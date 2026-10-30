//! ai-agent-runner: Firecracker microVMs for AI agents, run from one binary on
//! the KVM host and reached over SSH. docs/design.md says how it fits together.
pub mod boot;
pub mod firecracker;
pub mod mcp;
pub mod store;
pub mod systemd;
