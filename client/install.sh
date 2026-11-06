#!/bin/sh
# Installs the laptop's runner to ~/.local/bin, with aws.sh beside it, which
# runner runs to start a host aws.sh made that it finds stopped. Pass the
# host, as ssh takes it, to save it to ~/.config/runner/host, and with several
# hosts in ~/.config/runner/hosts (`runner hosts add`), to put it first there,
# the one MCP uses; a rerun without one keeps them. Rerun after a git pull to
# upgrade.
#
#   client/install.sh runner@kvm-host
set -eu

# With CDPATH set, cd prints where it went, which would land in $here.
unset CDPATH
here=$(cd "$(dirname "$0")" && pwd -P)
bin="$HOME/.local/bin"
conf="$HOME/.config/runner"

# runner refuses these, so don't save one.
if [ $# -gt 0 ]; then
  case $1 in
  '' | -*)
    echo "install.sh: '$1' isn't a host: give it as ssh takes it, like runner@kvm-host" >&2
    exit 2
    ;;
  esac
fi

mkdir -p "$bin"
for f in runner aws.sh; do
  cp "$here/$f" "$bin/$f.new"
  chmod 0755 "$bin/$f.new"
  mv -f "$bin/$f.new" "$bin/$f"
done
echo "Installed $bin/runner, and $bin/aws.sh beside it"

if [ $# -gt 0 ]; then
  mkdir -p "$conf"
  printf '%s\n' "$1" >"$conf/host"
  if [ -e "$conf/hosts" ]; then
    {
      printf '%s\n' "$1"
      # Every other line as it was, comments too.
      while IFS= read -r line || [ -n "$line" ]; do
        # shellcheck disable=SC2086 # its first word is the host.
        h=$(set -f && set -- $line && printf '%s' "${1-}")
        [ "$h" = "$1" ] || printf '%s\n' "$line"
      done <"$conf/hosts"
    } >"$conf/hosts.new"
    mv -f "$conf/hosts.new" "$conf/hosts"
  fi
fi
if [ -e "$conf/hosts" ]; then
  echo "Hosts, the first for MCP: $(grep -v '^[[:space:]]*#' "$conf/hosts" | tr '\n' ' ')"
elif [ -r "$conf/host" ]; then
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
  claude mcp add --scope user runner -- $bin/runner mcp

Claude Desktop (~/Library/Application Support/Claude/claude_desktop_config.json)
and Cursor (~/.cursor/mcp.json):
  { "mcpServers": { "runner": { "command": "$bin/runner", "args": ["mcp"] } } }
CONFIG
