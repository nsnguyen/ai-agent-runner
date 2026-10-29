# ai-agent-runner

## Purpose
A cloud runner for AI agents on Firecracker microVMs, like fly.io Sprites. Each agent gets its own microVM. My local Claude Code starts and drives one through an MCP server, or I start one with a CLI command and land in its terminal over SSH.

## Done looks like
- From my local Claude Code, an MCP tool call starts an agent in its own microVM and returns its output.
- One CLI command starts a microVM and drops me into its shell over SSH.

## Not this
- Not a hosted service for other people. It runs my agents.
- Not a model client. The agent inside the VM talks to its own model; the runner starts, connects to, and stops VMs.
- Not a cloud buyer. It never signs up for or rents hosts; it runs on machines I give it.

## Autonomy
- Merging: I merge. <!-- Or: "Claude may merge when every check passes." If so, also add an empty .claude/temper/allow-merge, or temper's guard blocks the merge. -->
- Deploys: previews only.

## Stack
- Firecracker microVMs, driven through Firecracker's API socket. Everything else follows temper's defaults: Rust for the host service, CLI, and MCP server.

## Commands
- Build: none yet
- Test: none yet
- Run locally: none yet

## Sharp edges
- Firecracker needs Linux with `/dev/kvm`. Claude's cloud sessions don't have it, so tests there can't boot a VM. Keep tests that boot one apart from the rest, so they're skippable, and say in each PR which ones didn't run.
