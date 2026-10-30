#!/bin/sh
# Installs the laptop's runner to ~/.local/bin. Pass the host, as ssh takes it,
# to save it to ~/.config/runner/host; a rerun without one keeps it. Rerun after
# a git pull to upgrade.
#
#   client/install.sh runner@kvm-host
set -eu

here=$(cd "$(dirname "$0")" && pwd -P)
bin="$HOME/.local/bin"
conf="$HOME/.config/runner"

mkdir -p "$bin"
cp "$here/runner" "$bin/runner.new"
chmod 0755 "$bin/runner.new"
mv -f "$bin/runner.new" "$bin/runner"
echo "Installed $bin/runner"

if [ $# -gt 0 ]; then
  mkdir -p "$conf"
  printf '%s\n' "$1" >"$conf/host"
fi
if [ -r "$conf/host" ]; then
  echo "Host: $(cat "$conf/host")"
else
  echo "No host yet: rerun with one, like client/install.sh runner@kvm-host"
fi
case ":$PATH:" in
*":$bin:"*) ;;
*) echo "Add $bin to PATH" ;;
esac

cat <<CONFIG

Claude Code:
  claude mcp add --scope user runner -- runner mcp

Claude Desktop (~/Library/Application Support/Claude/claude_desktop_config.json)
and Cursor (~/.cursor/mcp.json):
  { "mcpServers": { "runner": { "command": "$bin/runner", "args": ["mcp"] } } }
CONFIG
