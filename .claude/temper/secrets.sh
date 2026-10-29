#!/usr/bin/env bash
# temper secrets: PreToolUse hook for Write, Edit and MultiEdit.
# Blocks text that looks like a live credential from landing in a file git tracks.
# Git-ignored files (a local .env) are allowed. Exit 2 blocks the write.
#
# guard.sh also runs `secrets.sh --scan-repo` before a git commit, to check what
# that commit takes (see below).

# What kind of credential the text on stdin looks like, if any.
kind_of() {
  local text line kind re
  # AWS's own documentation example key is not a secret.
  text="$(head -c 5000000 | sed 's/AKIAIOSFODNN7EXAMPLE//g')"
  while IFS= read -r line; do
    kind="${line%%|*}"
    re="${line#*|}"
    if printf '%s' "$text" | grep -Eq -e "$re"; then
      printf '%s' "$kind"
      return 0
    fi
  done <<'EOF'
a private key|-----BEGIN ([A-Z]+ )?PRIVATE KEY-----
a PGP private key|-----BEGIN PGP PRIVATE KEY BLOC[K]-----
an AWS access key|(AKIA|ASIA)[0-9A-Z]{16}
a GitHub token|(gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,})
an Anthropic API key|sk-ant-[A-Za-z0-9_-]{20,}
an OpenAI API key|sk-(proj-)?[A-Za-z0-9_-]{40,}
a Slack token|xox[abpors]-[A-Za-z0-9-]{10,}
a Stripe live key|(sk|rk)_live_[A-Za-z0-9]{20,}
a Google API key|AIza[0-9A-Za-z_-]{35}
EOF
}

# secrets.sh --scan-repo DIR [--worktree] [--all-untracked] [--include-ignored] [-- PATH...]
# What a commit run in DIR takes: everything staged; with --worktree, the tracked
# changes it stages too (commit -a, add -u); with --all-untracked, untracked files
# (add -A); and the PATHs a git add or commit in the same command names, ignored
# ones too with --include-ignored (add -f).
if [ "${1:-}" = "--scan-repo" ]; then
  cd "${2:-.}" 2>/dev/null || exit 0
  shift 2 2>/dev/null || shift $#
  worktree=0
  all_untracked=0
  ignored=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --worktree) worktree=1 ;;
      --all-untracked) all_untracked=1; worktree=1 ;;
      --include-ignored) ignored=1 ;;
      --) shift; break ;;
      *) break ;;
    esac
    shift
  done
  top="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0
  if git rev-parse -q --verify HEAD >/dev/null 2>&1; then
    base=HEAD
  else
    base="$(git hash-object -t tree /dev/null)"
  fi
  added() { sed -n '/^+++ /d; s/^+//p'; }
  report() {
    printf 'temper secrets: committing now would put %s from %s into git. Secrets come from the environment or the OS keychain, never from tracked files (boundaries.md).\n' "$1" "$2" >&2
    exit 2
  }
  # diff_scan DIFF-ARGS: check the lines a diff adds; on a hit, name the file.
  diff_scan() {
    local f k names
    [ -n "$(git diff "$@" 2>/dev/null | added | kind_of)" ] || return 0
    names="$(git diff --name-only "$@" 2>/dev/null)"
    while IFS= read -r f; do
      [ -n "$f" ] || continue
      k="$(git diff "$@" 2>/dev/null | awk -v f="$f" '/^diff --git /{on = (index($0, " b/" f) > 0)} on' | added | kind_of)"
      [ -n "$k" ] && report "$k" "$f"
    done <<EOF
$names
EOF
    report "a credential" "the changes"
  }
  # file_scan FILES (top-relative, one per line): check whole text files up to 1 MB.
  file_scan() {
    local f k files=""
    while IFS= read -r f; do
      [ -n "$f" ] && [ -f "$top/$f" ] && grep -Iq . "$top/$f" 2>/dev/null || continue
      [ "$(wc -c < "$top/$f")" -le 1048576 ] && files="$files$f
"
    done <<EOF
$1
EOF
    [ -n "$files" ] || return 0
    [ -n "$(printf '%s' "$files" | (cd "$top" && tr '\n' '\0' | xargs -0 cat) | kind_of)" ] || return 0
    while IFS= read -r f; do
      [ -n "$f" ] || continue
      k="$(kind_of < "$top/$f")"
      [ -n "$k" ] && report "$k" "$f"
    done <<EOF
$files
EOF
  }
  diff_scan --cached "$base"
  [ "$worktree" -eq 1 ] && diff_scan
  if [ $# -gt 0 ]; then
    diff_scan -- "$@"
    if [ -n "$ignored" ]; then
      file_scan "$(git ls-files --others --full-name -- "$@" 2>/dev/null)"
    else
      file_scan "$(git ls-files --others --exclude-standard --full-name -- "$@" 2>/dev/null)"
    fi
  fi
  [ "$all_untracked" -eq 1 ] && file_scan "$(git ls-files --others --exclude-standard --full-name -- :/ 2>/dev/null)"
  exit 0
fi

input="$(cat)"

# When the project repo carries its own copy of this hook, let that copy decide.
self_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
vendored="${CLAUDE_PROJECT_DIR:-}/.claude/temper"
if [ -n "${CLAUDE_PROJECT_DIR:-}" ] && [ -f "$vendored/secrets.sh" ] \
  && [ "$self_dir" != "$(cd "$vendored" 2>/dev/null && pwd -P)" ]; then
  exit 0
fi

if command -v jq >/dev/null 2>&1; then
  file="$(printf '%s' "$input" | jq -r '.tool_input.file_path // empty' 2>/dev/null)"
  text="$(printf '%s' "$input" | jq -r '[.tool_input.content, .tool_input.new_string,
    ((.tool_input.edits // [])[] | .new_string)] | map(select(. != null)) | join("\n")' 2>/dev/null)"
elif command -v python3 >/dev/null 2>&1; then
  file="$(printf '%s' "$input" | python3 -c '
import json, sys
print(json.load(sys.stdin).get("tool_input", {}).get("file_path", ""))' 2>/dev/null)"
  text="$(printf '%s' "$input" | python3 -c '
import json, sys
t = json.load(sys.stdin).get("tool_input", {})
parts = [t.get("content"), t.get("new_string")] + [e.get("new_string") for e in t.get("edits") or []]
print("\n".join(p for p in parts if p))' 2>/dev/null)"
else
  file=""
  text="$input"
fi

kind="$(printf '%s' "$text" | kind_of)"
[ -n "$kind" ] || exit 0

# A file git ignores (like a local .env) never gets committed, so it may hold secrets.
if [ -n "$file" ]; then
  dir="$(dirname "$file")"
  if [ -d "$dir" ] && git -C "$dir" check-ignore -q "$file" 2>/dev/null; then
    exit 0
  fi
fi

printf 'temper secrets: that looks like %s going into %s. Secrets come from the environment or the OS keychain, never from tracked files (boundaries.md).\n' \
  "$kind" "${file:-a file}" >&2
exit 2
