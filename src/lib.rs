//! ai-agent-runner: Firecracker microVMs for AI agents, run from one binary on
//! the KVM host and reached over SSH. docs/design.md says how it fits together.
pub mod mcp;
pub mod proto;
pub mod store;
