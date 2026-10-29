#!/usr/bin/env bash
# temper guard: PreToolUse hook for Bash.
# Blocks the boundaries.md rules a shell command can break: force-pushing or
# deleting main, deleting a repository, making a repository public, merging pull
# requests, and committing secrets. Exit 2 blocks the command and tells Claude why.
# A project opts into merging its own pull requests with .claude/temper/allow-merge.
#
# It reads a command roughly the way a shell would. Quoted text with spaces and
# heredoc bodies are data, so a commit message may name a blocked command, unless
# the heredoc feeds a shell. bash -c, sh -c and eval payloads, and commands run
# through sudo, env, timeout, nice or xargs, are checked as commands. cd and
# git -C decide which repo a push that names no branch acts on.

input="$(cat)"
set -f

# When the project repo carries its own copy of this hook, let that copy decide.
self_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
vendored="${CLAUDE_PROJECT_DIR:-}/.claude/temper"
if [ -n "${CLAUDE_PROJECT_DIR:-}" ] && [ -f "$vendored/guard.sh" ] \
  && [ "$self_dir" != "$(cd "$vendored" 2>/dev/null && pwd -P)" ]; then
  exit 0
fi

field() {
  if command -v jq >/dev/null 2>&1; then
    printf '%s' "$input" | jq -r "$1 // empty" 2>/dev/null
  elif command -v python3 >/dev/null 2>&1; then
    printf '%s' "$input" | python3 -c '
import json, sys
d = json.load(sys.stdin)
for k in sys.argv[1].lstrip(".").split("."):
    d = d.get(k, {}) if isinstance(d, dict) else {}
print(d if isinstance(d, str) else "")' "$1" 2>/dev/null
  fi
}

cmd="$(field .tool_input.command)"
[ -n "$cmd" ] || cmd="$input"
cwd="$(field .cwd)"
[ -n "$cwd" ] || cwd="${CLAUDE_PROJECT_DIR:-$PWD}"
project="${CLAUDE_PROJECT_DIR:-$cwd}"

block() {
  printf 'temper guard: %s (boundaries.md). If it really has to happen, ask Nguyen to run it.\n' "$1" >&2
  exit 2
}

# Heredoc bodies are data, unless the heredoc feeds a shell; then they're a script.
IFS= read -r -d '' heredoc_awk <<'AWK'
tag != "" {
  line = $0; sub(/^[ \t]+/, "", line)
  if (keep) print
  if (line == tag) tag = ""
  next
}
{ print }
$0 !~ /^[ \t]*#/ && match($0, /(^|[^<])<<-?[ \t]*["']?[A-Za-z_][A-Za-z0-9_]*/) {
  tag = substr($0, RSTART, RLENGTH); sub(/^[^<]?<<-?[ \t]*["']?/, "", tag)
  opener = $0; sub(/^[ \t]+/, "", opener)
  keep = (opener ~ /^((sudo|env|exec|command|nohup|time)[ \t]+)*([^ \t]*\/)?(ba|z|da|k)?sh([ \t]+-[a-zA-Z]+)*[ \t]*<</) \
    || ($0 ~ /\|[ \t]*([^ \t]*\/)?(ba|z|da|k)?sh([ \t]|$)/)
}
AWK

# Quoted text with whitespace becomes Q; a quoted single word is just the word.
# Quotes may span lines. Outside quotes, a # comment runs to the end of its line
# and a backslash-newline joins two lines. One pass over the line split into
# characters: substr per character is quadratic in macOS's awk.
IFS= read -r -d '' unquote_awk <<'AWK'
function keep(ch) { if (ch ~ /[ \t]/) spaced = 1; if (length(body) <= 200) body = body ch }
BEGIN { q = ""; prev = " " }
{
  n = split($0, ch, ""); i = 1; cont = 0
  if (q != "") spaced = 1
  while (i <= n) {
    c = ch[i]
    if (q == "") {
      if (c == "\\") {
        if (i == n) { cont = 1; break }
        c = ch[i + 1]; printf "%s", c; prev = c; i += 2; continue
      }
      if (c == "#" && prev ~ /[ \t;&|(]/) break
      if (c == "'" || c == "\"") { q = c; body = ""; spaced = 0; i++; continue }
      printf "%s", c; prev = c; i++; continue
    }
    if (q == "\"" && c == "\\") { if (i < n) keep(ch[i + 1]); i += 2; continue }
    if (c == q) {
      if (spaced || body == "" || length(body) > 200) printf " Q "; else printf "%s", body
      prev = "Q"; q = ""; i++; continue
    }
    keep(c); i++
  }
  if (cont) { printf " "; prev = " " }
  else if (q == "") { printf "\n"; prev = " " }
}
END { if (q != "") printf " Q " }
AWK

# bash -c '...', sh -c "..." and eval '...': the quoted part is a command too.
payloads() {
  printf '%s\n' "$1" \
    | grep -oE "((ba|z|da)?sh +(-[a-zA-Z]+ +)*-[a-zA-Z]*c[a-zA-Z]* +|eval +)('[^']*'|\"([^\"\\\\]|\\\\.)*\")" \
    | sed -E "s/^[^'\"]*//; s/^['\"]//; s/['\"]$//"
}

# One command per line: split chains, pipes, subshells and backticks.
split() { awk '{ gsub(/&&|\|\||;|\||&|\(|\)|`/, "\n"); print }'; }

# Drop the first word of $s, in the caller's scope (no subshell).
drop_s() { case "$s" in *' '*) s="${s#* }" ;; *) s="" ;; esac; }

# Drop leading options from WORDS. Those listed in TAKES_ARG drop their value too.
skip_opts() {
  local s="$1" w
  while [ -n "$s" ]; do
    w="${s%% *}"
    case "$w" in
      --) drop_s; break ;;
      -*)
        drop_s
        case " $2 " in *" $w "*) drop_s ;; esac ;;
      *) break ;;
    esac
  done
  printf '%s' "$s"
}

# Drop what runs a command without being it: { ! if do sudo env timeout nice xargs
# VAR=value. At most 50 words, so a huge command can't stall the hook.
strip() {
  local s prev="" first name n=0
  s="$(printf '%s' "$1" | tr -s '[:space:]' ' ')"
  s="${s# }"
  s="${s% }"
  while [ "$s" != "$prev" ] && [ "$n" -lt 50 ]; do
    prev="$s"
    n=$((n + 1))
    first="${s%% *}"
    case "$first" in
      '{'|'!'|exec|command|builtin|nohup|time|caffeinate|if|then|else|elif|while|until|do) drop_s ;;
      sudo|doas) drop_s; s="$(skip_opts "$s" "-u -g -h -p -C -D -r -t -U")" ;;
      env) drop_s; s="$(skip_opts "$s" "-u -C -S --unset --chdir --split-string")" ;;
      nice|ionice) drop_s; s="$(skip_opts "$s" "-n -c -p")" ;;
      stdbuf) drop_s; s="$(skip_opts "$s" "-i -o -e")" ;;
      xargs) drop_s; s="$(skip_opts "$s" "-n -I -L -P -s -d -E -a")" ;;
      timeout) drop_s; s="$(skip_opts "$s" "-s -k --signal --kill-after")"; drop_s ;;
      [A-Za-z_]*=*)
        name="${first%%=*}"
        case "$name" in *[!A-Za-z0-9_]*) ;; *) drop_s ;; esac ;;
    esac
  done
  printf '%s' "$s"
}

# Where `cd TARGET` lands, starting from BASE.
cd_to() {
  local to="${2%% *}"
  case "$to" in
    '$HOME'*) to="$HOME${to:5}" ;;
    '${HOME}'*) to="$HOME${to:7}" ;;
    '$PWD'*) to="$1${to:4}" ;;
    '~') to="$HOME" ;;
    '~/'*) to="$HOME/${to:2}" ;;
  esac
  case "$to" in
    "") printf '%s' "$HOME" ;;
    -*) printf '%s' "$1" ;;
    /*) printf '%s' "$to" ;;
    *) printf '%s/%s' "$1" "$to" ;;
  esac
}

on_branch() { git -C "$1" symbolic-ref --short -q HEAD 2>/dev/null; }
is_main() { case "${1#refs/heads/}" in main|master) return 0 ;; esac; return 1; }

# owner/name of a GitHub remote URL or a gh -R value, in lowercase.
norm_repo() {
  printf '%s' "$1" | sed -E 's#^(https?://|ssh://)?([^@/]*@)?github\.com[:/]##; s#\.git$##; s#/$##' \
    | tr '[:upper:]' '[:lower:]'
}

# allow-merge covers the project's own pull requests only. TARGET is the repo a
# command names; when it names none, gh uses the repo of DIR, where it runs.
may_merge() {
  local own here
  [ -f "$project/.claude/temper/allow-merge" ] || return 1
  own="$(norm_repo "$(git -C "$project" remote get-url origin 2>/dev/null)")"
  if [ -z "$1" ]; then
    [ "$(cd "$2" 2>/dev/null && pwd -P)" = "$(cd "$project" && pwd -P)" ] && return 0
    here="$(norm_repo "$(git -C "$2" remote get-url origin 2>/dev/null)")"
    [ -n "$here" ] && [ "$here" = "$own" ]
    return
  fi
  [ "$(norm_repo "$1")" = "$own" ]
}

check_push() {
  local dir="$1" w dst b force=0 delete=0 all=0 n=0 specs=""
  shift
  for w in "$@"; do
    case "$w" in
      --force|--force-with-lease|--force-with-lease=*|--force-if-includes) force=1 ;;
      --delete) delete=1 ;;
      --mirror) block "git push --mirror can overwrite or delete main" ;;
      --all|--branches) all=1 ;;
      --dry-run) return 0 ;;
      --*) ;;
      -*)
        case "$w" in *n*) return 0 ;; esac
        case "$w" in *f*) force=1 ;; esac
        case "$w" in *d*) delete=1 ;; esac ;;
      *)
        # The first word is the remote; the rest are refspecs.
        n=$((n + 1))
        [ "$n" -gt 1 ] && specs="$specs $w" ;;
    esac
  done
  for w in $specs; do
    case "$w" in
      :*) is_main "${w#:}" && block "deleting main is off-limits" ;;
      +*) force=1 ;;
    esac
    [ "$delete" -eq 1 ] && is_main "$w" && block "deleting main is off-limits"
  done
  [ "$delete" -eq 0 ] && [ "$force" -eq 1 ] || return 0
  [ "$all" -eq 1 ] && block "force-pushing every branch includes main"
  if [ -z "$specs" ]; then
    # No branch named: git pushes the current branch, which an earlier
    # checkout or switch in the same command may have changed.
    b="${switched:-$(on_branch "$dir")}"
    [ -n "$b" ] || block "a force-push must name its branch when the repo or branch can't be told"
    is_main "$b" && block "force-pushing main is off-limits"
    return 0
  fi
  for w in $specs; do
    w="${w#+}"
    dst="${w#*:}"
    case "$dst" in
      *'$'*|Q) block "a force-push must name its branch, not compute it" ;;
      *'*'*) block "force-pushing a pattern can include main" ;;
      HEAD|@)
        dst="${switched:-$(on_branch "$dir")}"
        [ -n "$dst" ] || block "a force-push must name its branch when the repo or branch can't be told" ;;
    esac
    is_main "$dst" && block "force-pushing main is off-limits"
  done
  return 0
}

# git add in the same command line decides what a following commit takes. When
# the guard can't read which paths it adds (xargs, $(...), a loop, a file list),
# it assumes every untracked file.
note_add() {
  local w take=0 named=0
  for w in "$@"; do
    if [ "$take" -eq 1 ]; then take=0; continue; fi
    case "$w" in
      -A|--all|--no-ignore-removal|.|:/|:) add_all=1 ;;
      -u|--update) add_tracked=1 ;;
      -f|--force) add_force=1 ;;
      --chmod) take=1 ;;
      --pathspec-from-file|--pathspec-from-file=*) add_all=1 ;;
      --*) ;;
      -*)
        case "$w" in *A*) add_all=1 ;; esac
        case "$w" in *u*) add_tracked=1 ;; esac
        case "$w" in *f*) add_force=1 ;; esac ;;
      *'$'*|Q|*'*'*|*'?'*) add_all=1; named=1 ;;
      *) add_paths="$add_paths $w"; named=1 ;;
    esac
  done
  [ "$named" -eq 1 ] || [ "$add_tracked" -eq 1 ] || add_all=1
}

# git checkout or switch earlier in the command line changes the current branch.
note_switch() {
  local w take=0
  for w in "$@"; do
    if [ "$take" -eq 1 ]; then switched="$w"; return 0; fi
    case "$w" in
      -b|-B|-c|-C|--orphan) take=1 ;;
      --) return 0 ;;
      -*) ;;
      *) switched="$w"; return 0 ;;
    esac
  done
}

# A commit takes what's staged, plus what the command stages. Check that for secrets.
check_commit() {
  local dir="$1" w take=0 worktree=0 paths="" flags=""
  shift
  for w in "$@"; do
    if [ "$take" -eq 1 ]; then take=0; continue; fi
    case "$w" in
      --all) worktree=1 ;;
      --message|--file|--reuse-message|--reedit-message|--author|--date|--fixup|--squash|--template|--cleanup|--trailer) take=1 ;;
      --*) ;;
      -*)
        case "$w" in *a*) worktree=1 ;; esac
        case "$w" in *[mFCct]) take=1 ;; esac ;;
      *) paths="$paths $w" ;;
    esac
  done
  [ "$add_tracked" -eq 1 ] && worktree=1
  [ "$worktree" -eq 1 ] && flags="--worktree"
  [ "$add_all" -eq 1 ] && flags="--all-untracked"
  [ "$add_force" -eq 1 ] && flags="$flags --include-ignored"
  # A secrets.sh that doesn't parse would exit 2 and block every commit: skip it instead.
  [ -f "$self_dir/secrets.sh" ] && bash -n "$self_dir/secrets.sh" 2>/dev/null || return 0
  # Word-split on purpose: flags and paths are single words.
  bash "$self_dir/secrets.sh" --scan-repo "$dir" $flags -- $add_paths $paths </dev/null >/dev/null
  [ $? -eq 2 ] && exit 2
  return 0
}

check_git() {
  local dir="$1" sub
  shift
  while [ $# -gt 0 ]; do
    case "$1" in
      -C) [ $# -ge 2 ] || return 0; dir="$(cd_to "$dir" "$2")"; shift 2 ;;
      -c|--git-dir|--work-tree|--namespace) [ $# -ge 2 ] || return 0; shift 2 ;;
      -*) shift ;;
      *) break ;;
    esac
  done
  sub="${1:-}"
  [ $# -gt 0 ] && shift
  case "$sub" in
    push) check_push "$dir" "$@" ;;
    add|stage) note_add "$@" ;;
    checkout|switch) note_switch "$@" ;;
    commit) check_commit "$dir" "$@" ;;
  esac
  return 0
}

# GraphQL mutations live in quoted text, so read them from the raw command.
check_graphql() {
  printf '%s' "$cmd" | grep -q 'deleteRepository' && block "deleting a repository is off-limits"
  printf '%s' "$cmd" | grep -q 'updateRepository' && printf '%s' "$cmd" | grep -Eiq 'visibility[^a-z]*public' \
    && block "making a repository public is off-limits"
  printf '%s' "$cmd" | grep -Eq 'mergePullRequest|enablePullRequestAutoMerge|mergeBranch' \
    && block "merging pull requests is Nguyen's call"
  return 0
}

check_gh() {
  local dir="$1" line group="" sub="" w skip=0 target="" repos
  shift
  line=" $* "
  has() { printf '%s' "$line" | grep -Eq -e "$1"; }
  for w in "$@"; do
    if [ "$skip" -eq 1 ]; then skip=0; target="$w"; continue; fi
    case "$w" in
      -R|--repo) skip=1 ;;
      --repo=*) target="${w#--repo=}" ;;
      https://github.com/*/pull/*) target="$(printf '%s' "$w" | sed -E 's#^https://github\.com/([^/]+/[^/]+)/pull/.*#\1#')" ;;
      -*) ;;
      *) if [ -z "$group" ]; then group="$w"; elif [ -z "$sub" ]; then sub="$w"; fi ;;
    esac
  done
  [ -n "$target" ] || target="$gh_repo"
  case "$group $sub" in
    "repo delete") block "deleting a repository is off-limits" ;;
    "repo edit") has ' --visibility[ =]public ' && block "making a repository public is off-limits" ;;
    "repo create"|"repo new") has ' --public ' && block "making a repository public is off-limits" ;;
    "pr merge") may_merge "$target" "$dir" || block "merging pull requests is Nguyen's call" ;;
  esac
  if [ "$group" = api ]; then
    has '(^| |/)graphql( |$)' && check_graphql
    repos=' (https?://[^/ ]+/)?/?repos/'
    if has ' (-X|--method)[ =]?DELETE '; then
      has "${repos}[^/ ]+/[^/ ]+/? " && block "deleting a repository is off-limits"
      has "${repos}[^/ ]+/[^/ ]+/git/refs/heads/(main|master)/? " && block "deleting main is off-limits"
    fi
    has '(private=false|visibility=public)' && block "making a repository public is off-limits"
    if has 'pulls/[0-9]+/merge'; then
      target="$(printf '%s' "$line" | sed -nE 's#.*repos/([^/ ]+/[^/ ]+)/pulls/[0-9]+/merge.*#\1#p')"
      [ "$target" = "{owner}/{repo}" ] && target=""
      may_merge "$target" "$dir" || block "merging pull requests is Nguyen's call"
    fi
  fi
  return 0
}

check_segment() {
  local dir="$1" prog
  # Word-split on purpose: quotes are already resolved, and globbing is off.
  set -- $2
  prog="${1:-}"
  [ $# -gt 0 ] && shift
  case "$prog" in
    git|*/git) check_git "$dir" "$@" ;;
    gh|*/gh) check_gh "$dir" "$@" ;;
  esac
  return 0
}

check() {
  local dir="$1" text depth="$3" payload seg
  [ "$depth" -le 3 ] || return 0
  text="$(printf '%s\n' "$2" | awk "$heredoc_awk")"
  while IFS= read -r payload; do
    [ -n "$payload" ] && check "$dir" "$payload" $((depth + 1))
  done <<EOF
$(payloads "$text")
EOF
  add_all=0
  add_tracked=0
  add_force=0
  add_paths=""
  switched=""
  gh_repo="${GH_REPO:-}"
  while IFS= read -r seg; do
    # GH_REPO=x before a gh command, or exported for the rest of the line.
    case " $seg " in
      *" GH_REPO="*) gh_repo="$(printf '%s' " $seg" | sed -E 's/.* GH_REPO=([^ ]*).*/\1/')" ;;
    esac
    seg="$(strip "$seg")"
    case "$seg" in
      "") ;;
      cd|pushd) dir="$HOME" ;;
      "cd "*|"pushd "*) dir="$(cd_to "$dir" "${seg#* }")" ;;
      *) check_segment "$dir" "$seg" ;;
    esac
  done <<EOF
$(printf '%s' "$text" | awk "$unquote_awk" | split)
EOF
}

check "$cwd" "$cmd" 0
exit 0
