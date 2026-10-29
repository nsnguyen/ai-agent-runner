#!/usr/bin/env bash
# temper state: shared memory for every session, kept on temper's `state` branch.
#
#   state.sh show [--brief] [--hook] [--repo NAME]
#       Handoff and open claims for this repo and the repos it depends on.
#   state.sh claim SLUG [--repos a,b] [--note TEXT] [--force]
#       Claim a task before starting it. Exit 3 if someone else holds it.
#       --force takes over a claim only when it has gone stale.
#   state.sh release SLUG [--force]
#       Drop your claim when the task is done or abandoned. Exit 3 if another
#       session holds it; --force drops another's only once it has gone stale.
#   state.sh handoff [--repo NAME] < note
#       Replace this repo's HANDOFF.md with the note on stdin, and log it.
#   state.sh mine [--session-id ID]
#       This session's name, repo, cache, and claims, from the cache alone.
#
# A claim is a file under claims/. Pushes to the branch are the lock: when two
# sessions race, GitHub accepts one push, and the other re-reads and backs off.
#
# Environment:
#   TEMPER_STATE_REMOTE  where the branch lives (default: ~/temper's origin, else GitHub)
#   TEMPER_STATE_DIR     local cache of the branch (default: ~/.cache/temper-state)
#   TEMPER_SESSION       who is acting, when Claude Code doesn't say (the SessionStart
#                        hook sets it; default: the user name)
#   TEMPER_STALE_HOURS   claims older than this count as stale (default: 24)

set -u
branch="state"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
export GIT_TERMINAL_PROMPT=0
export GIT_SSH_COMMAND="${GIT_SSH_COMMAND:-ssh -o BatchMode=yes -o ConnectTimeout=5}"

if [ -n "${TEMPER_STATE_REMOTE:-}" ]; then
  remote="$TEMPER_STATE_REMOTE"
elif remote="$(git -C "$HOME/temper" remote get-url origin 2>/dev/null)"; then
  :
else
  remote="https://github.com/nsnguyen/temper"
fi
dir="${TEMPER_STATE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/temper-state}"
stale_hours="${TEMPER_STALE_HOURS:-24}"
# The first 8 characters of a Claude Code session id.
short_id() { printf '%s' "$1" | tr -cd 'A-Za-z0-9-' | cut -c1-8; }

# Who is acting: one name per Claude Code session, so its later calls still own its
# claims. The session's id when known (CLAUDE_CODE_SESSION_ID, or one a hook passes
# in), else TEMPER_SESSION (which the SessionStart hook exports), else the cloud
# session's id, else the user at a terminal. No host name: macOS changes it when
# the network changes.
session_name() {
  local sid
  sid="$(short_id "${CLAUDE_CODE_SESSION_ID:-${1:-}}")"
  if [ -n "$sid" ]; then
    printf 'claude-%s' "$sid"
  elif [ -n "${TEMPER_SESSION:-}" ]; then
    printf '%s' "$TEMPER_SESSION"
  elif [ -n "${CLAUDE_CODE_REMOTE_SESSION_ID:-}" ]; then
    printf 'cloud-%s' "${CLAUDE_CODE_REMOTE_SESSION_ID#cse_}"
  else
    printf '%s' "${USER:-$(id -un 2>/dev/null)}"
  fi
}
me="$(session_name "")"

die() { printf 'temper state: %s\n' "$1" >&2; exit "${2:-1}"; }

# One process at a time per cache directory. take_lock [TRIES] waits up to TRIES
# tenths of a second. A lock older than two minutes was left by a process that
# died, so it's cleared.
lock="$dir.lock"
unlock() { rmdir "$lock" 2>/dev/null; }
take_lock() {
  local i=0
  mkdir -p "$(dirname "$dir")"
  until mkdir "$lock" 2>/dev/null; do
    [ -n "$(find "$lock" -maxdepth 0 -mmin +2 2>/dev/null)" ] && rmdir "$lock" 2>/dev/null
    i=$((i + 1))
    [ "$i" -gt "${1:-100}" ] && return 1
    sleep 0.1
  done
  trap unlock EXIT
}
busy() { die "another state.sh is still running ($lock)"; }

check_slug() {
  printf '%s' "$1" | grep -Eq '^[a-z0-9][a-z0-9._-]{0,60}$' \
    || die "claim names use lowercase letters, digits, dots, dashes and underscores"
}

# Bring the cache to the branch's current tip. Pass "fresh" to always fetch.
sync() {
  if [ -d "$dir/.git" ]; then
    git -C "$dir" remote set-url origin "$remote" 2>/dev/null
    if [ "${1:-}" != fresh ] && [ "${TEMPER_STATE_NO_SYNC:-0}" = 1 ]; then
      return 0
    fi
    git -C "$dir" fetch --quiet origin "$branch" >/dev/null 2>&1 || return 1
    git -C "$dir" reset --quiet --hard "origin/$branch" >/dev/null 2>&1 || return 1
  else
    git clone --quiet --single-branch --branch "$branch" "$remote" "$dir" >/dev/null 2>&1 || return 1
  fi
  git -C "$dir" config user.name >/dev/null 2>&1 || git -C "$dir" config user.name "temper state"
  git -C "$dir" config user.email >/dev/null 2>&1 || git -C "$dir" config user.email "state@temper.invalid"
}

# Apply a change to the tip, commit and push. If another session pushed first,
# start over from the new tip. The apply function returns 3 to back off.
publish() {
  local msg="$1" apply="$2" try rc
  for try in 1 2 3 4 5; do
    "$apply"
    rc=$?
    [ "$rc" -eq 0 ] || return "$rc"
    git -C "$dir" add -A
    git -C "$dir" diff --cached --quiet && return 0
    git -C "$dir" commit --quiet -m "$msg" || return 1
    git -C "$dir" push --quiet origin "HEAD:$branch" >/dev/null 2>&1 && return 0
    sync fresh || return 1
  done
  return 1
}

this_repo() {
  local top url
  top="$(git -C "${CLAUDE_PROJECT_DIR:-$PWD}" rev-parse --show-toplevel 2>/dev/null)" \
    || { basename "${CLAUDE_PROJECT_DIR:-$PWD}"; return; }
  if url="$(git -C "$top" remote get-url origin 2>/dev/null)"; then
    basename "$url" .git
  else
    basename "$top"
  fi
}

# owner/name for a GitHub remote, else the remote as given.
repo_name() {
  case "$1" in
    *github.com[/:]*) local s="${1#*github.com[/:]}"; s="${s%/}"; printf '%s' "${s%.git}" ;;
    *) printf '%s' "$1" ;;
  esac
}

field_of() { sed -n "s/^$1: //p" "$2" | head -n 1; }

deps_of() { [ -f "$dir/deps.md" ] && sed -n "s/^- $1 -> //p" "$dir/deps.md" | tr ',' ' '; }

next_line() {
  local f="$dir/projects/$1/HANDOFF.md"
  [ -f "$f" ] || return 1
  grep -i -m 1 '^next:' "$f" || grep -v '^[[:space:]]*$' "$f" | head -n 1
}

touches() {
  # Does the claim file $1 name any of the repos in $2?
  local r
  for r in $(field_of repos "$1" | tr ',' ' '); do
    case " $2 " in *" $r "*) return 0 ;; esac
  done
  return 1
}

claim_line() {
  local f="$1" now epoch age tag=""
  now="$(date +%s)"
  epoch="$(field_of epoch "$f")"
  age=$(( (now - ${epoch:-$now}) / 3600 ))
  [ "$age" -ge "$stale_hours" ] && tag=" (stale)"
  printf '  claim %s [%s] by %s, %sh ago%s\n' "$(basename "$f" .md)" "$(field_of repos "$f")" \
    "$(field_of by "$f")" "$age" "$tag"
}

cmd_show() {
  local brief=0 hook=0 repo=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --brief) brief=1 ;;
      --hook) hook=1; brief=1 ;;
      --repo) repo="$2"; shift ;;
      *) die "unknown option $1" ;;
    esac
    shift
  done
  if [ "$hook" -eq 1 ]; then
    # When the project repo carries its own copy, let that copy speak at session start.
    local vendored="${CLAUDE_PROJECT_DIR:-}/.claude/temper"
    if [ -n "${CLAUDE_PROJECT_DIR:-}" ] && [ -f "$vendored/state.sh" ] \
      && [ "$here" != "$(cd "$vendored" 2>/dev/null && pwd -P)" ]; then
      exit 0
    fi
    # Pass this session's name to its later shell commands, which may not get
    # Claude Code's session id themselves.
    if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
      local sid
      sid="$(cat | tr -d '\n' | sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
      if [ -n "$(short_id "${CLAUDE_CODE_SESSION_ID:-$sid}")" ]; then
        me="$(session_name "$sid")"
        printf "export TEMPER_SESSION='%s'\n" "$me" >> "$CLAUDE_ENV_FILE"
      fi
    fi
  fi
  [ -n "$repo" ] || repo="$(this_repo)"
  if [ "$hook" -eq 1 ]; then
    take_lock 20 || exit 0
  else
    take_lock || busy
  fi
  if ! sync; then
    if [ "$hook" -eq 1 ]; then
      # A cloud session gets only the repos it was started with, and temper is private.
      [ "${CLAUDE_CODE_REMOTE:-}" = true ] \
        && printf "temper state: this session can't reach %s. Attach it with push access (add_repo), then run: bash %s show\n" \
          "$(repo_name "$remote")" "$here/$(basename "${BASH_SOURCE[0]}")"
      exit 0
    fi
    die "can't reach the state branch at $remote"
  fi

  local deps scope out="" f d n
  deps="$(deps_of "$repo")"
  scope="$repo $deps"

  if [ "$brief" -eq 1 ]; then
    n="$(next_line "$repo")" && out="$out  $n"$'\n'
  elif [ -f "$dir/projects/$repo/HANDOFF.md" ]; then
    out="$out$(sed 's/^/  /' "$dir/projects/$repo/HANDOFF.md")"$'\n'
  fi
  for d in $deps; do
    n="$(next_line "$d")" && out="$out  depends on $d: $n"$'\n'
  done
  for f in "$dir"/claims/*.md; do
    [ -f "$f" ] || continue
    touches "$f" "$scope" && out="$out$(claim_line "$f")"$'\n'
  done

  if [ -z "$out" ]; then
    [ "$hook" -eq 1 ] && exit 0
    printf 'temper state · %s: nothing recorded yet\n' "$repo"
    exit 0
  fi
  printf 'temper state · %s\n%s' "$repo" "$out"
}

cmd_claim() {
  slug="${1:-}"
  [ -n "$slug" ] || die "usage: state.sh claim SLUG [--repos a,b] [--note TEXT] [--force]"
  shift
  check_slug "$slug"
  repos="" note="" force=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --repos) repos="$2"; shift ;;
      --note) note="$2"; shift ;;
      --force) force=1 ;;
      *) die "unknown option $1" ;;
    esac
    shift
  done
  # owner/name and name are the same repo here: keep the name.
  [ -n "$repos" ] && repos="$(printf '%s' "$repos" | tr ',' '\n' | sed 's#.*/##; s#\.git$##' | grep -v '^$' | paste -sd, -)"
  take_lock || busy
  sync || die "can't reach the state branch at $remote"
  if [ -z "$repos" ]; then
    [ -f "$dir/claims/$slug.md" ] && repos="$(field_of repos "$dir/claims/$slug.md")"
    [ -n "$repos" ] || repos="$(this_repo)"
  fi
  conflict=""
  publish "claim $slug ($repos) by $me" apply_claim
  case $? in
    0) printf 'temper state: claimed %s for %s\n' "$slug" "$repos" ;;
    3) die "$slug is already claimed by $conflict" 3 ;;
    *) die "couldn't publish the claim to $remote" ;;
  esac
}

apply_claim() {
  local f="$dir/claims/$slug.md" by epoch now
  now="$(date +%s)"
  # Without --repos, keep the repos an existing claim names, else use this repo.
  if [ -z "$repos" ]; then
    [ -f "$f" ] && repos="$(field_of repos "$f")"
    [ -n "$repos" ] || repos="$(this_repo)"
  fi
  if [ -f "$f" ]; then
    by="$(field_of by "$f")"
    [ "$by" = "$me" ] && return 0
    epoch="$(field_of epoch "$f")"
    if [ "$force" -ne 1 ] || [ $(( (now - ${epoch:-$now}) / 3600 )) -lt "$stale_hours" ]; then
      conflict="$by since $(field_of at "$f")"
      return 3
    fi
  fi
  mkdir -p "$dir/claims"
  printf 'slug: %s\nrepos: %s\nby: %s\nat: %s\nepoch: %s\nnote: %s\n' \
    "$slug" "$repos" "$me" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$now" "$note" > "$f"
}

cmd_release() {
  slug="${1:-}"
  [ -n "$slug" ] || die "usage: state.sh release SLUG [--force]"
  shift
  check_slug "$slug"
  force=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --force) force=1 ;;
      *) die "unknown option $1" ;;
    esac
    shift
  done
  take_lock || busy
  sync || die "can't reach the state branch at $remote"
  conflict=""
  publish "release $slug by $me" apply_release
  case $? in
    0) printf 'temper state: released %s\n' "$slug" ;;
    3) die "$slug is held by $conflict, not this session. Leave it; once it's stale, --force can drop it" 3 ;;
    4) die "no claim named $slug" ;;
    *) die "couldn't publish the release to $remote" ;;
  esac
}

apply_release() {
  local f="$dir/claims/$slug.md" by epoch now
  [ -f "$f" ] || return 4
  by="$(field_of by "$f")"
  if [ "$by" != "$me" ]; then
    now="$(date +%s)"
    epoch="$(field_of epoch "$f")"
    if [ "$force" -ne 1 ] || [ $(( (now - ${epoch:-$now}) / 3600 )) -lt "$stale_hours" ]; then
      conflict="$by"
      return 3
    fi
  fi
  rm -f "$f"
}

# For hooks: no lock, no network. A claim counts as this session's when its by: matches.
cmd_mine() {
  local f
  while [ $# -gt 0 ]; do
    case "$1" in
      --session-id) me="$(session_name "${2:-}")"; shift ;;
      *) die "unknown option $1" ;;
    esac
    shift
  done
  printf 'me %s\nrepo %s\ndir %s\n' "$me" "$(this_repo)" "$dir"
  for f in "$dir"/claims/*.md; do
    [ -f "$f" ] || continue
    [ "$(field_of by "$f")" = "$me" ] && printf 'claim %s\n' "$(basename "$f" .md)"
  done
  return 0
}

cmd_handoff() {
  repo=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --repo) repo="$2"; shift ;;
      *) die "unknown option $1" ;;
    esac
    shift
  done
  [ -n "$repo" ] || repo="$(this_repo)"
  note="$(cat)"
  [ -n "$note" ] || die "pipe the handoff note into state.sh handoff"
  stamp="$(date -u +%Y-%m-%dT%H%M%SZ)"
  take_lock || busy
  sync || die "can't reach the state branch at $remote"
  publish "handoff $repo by $me" apply_handoff || die "couldn't publish the handoff to $remote"
  printf 'temper state: handoff saved for %s\n' "$repo"
}

apply_handoff() {
  local p="$dir/projects/$repo" who
  who="$(printf '%s' "$me" | tr -c 'A-Za-z0-9._-' '_')"
  mkdir -p "$p/log"
  printf '%s\n\nupdated: %s by %s\n' "$note" "$stamp" "$me" > "$p/HANDOFF.md"
  cp "$p/HANDOFF.md" "$p/log/$stamp-$who.md"
}

case "${1:-}" in
  show) shift; cmd_show "$@" ;;
  claim) shift; cmd_claim "$@" ;;
  release) shift; cmd_release "$@" ;;
  handoff) shift; cmd_handoff "$@" ;;
  mine) shift; cmd_mine "$@" ;;
  *) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
