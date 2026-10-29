#!/usr/bin/env bash
# temper handoff: Stop hook.
# Before a session stops, checks that the shared state shows its work (state.md):
# a handoff written after its last push or claim, and its own claims released.
# If not, it holds the session once and says what to run. It asks once per piece
# of work, so a session that's mid-task and waiting on an answer can stop, and it
# never holds a stop that follows a Stop hook's hold, so it can't loop.
#
# Only in repos that installed temper (.claude/temper/) and in temper checkouts,
# so no other repo's work lands on the state branch. Claims and handoffs come from
# state.sh's cache, under this session's name. Pushes and claims made come from
# the transcripts, subagents' included, read as a shell would read the commands.
# Needs python3; without it, does nothing.
#
# It can't hold sessions that run no repo hooks: project threads and multi-repo
# cloud sessions. The rule in state.md still applies there.

input="$(cat)"

self_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
project="${CLAUDE_PROJECT_DIR:-}"
[ -n "$project" ] && [ -d "$project" ] || exit 0
project="$(cd "$project" && pwd -P)"
if [ -f "$project/.claude/temper/handoff.sh" ]; then
  # The project's own copy decides.
  [ "$self_dir" = "$(cd "$project/.claude/temper" && pwd -P)" ] || exit 0
  state="$project/.claude/temper/state.sh"
elif [ -f "$project/state/state.sh" ] && [ -f "$project/hooks/handoff.sh" ]; then
  # A temper checkout.
  state="$project/state/state.sh"
else
  exit 0
fi
command -v python3 >/dev/null 2>&1 || exit 0

# The check itself. Loaded into a variable first: bash 3.2 misreads a heredoc inside $( ).
IFS= read -r -d '' check <<'PY'
import glob, hashlib, json, os, re, shlex, subprocess, sys, tempfile
from datetime import datetime, timezone

state, project = sys.argv[1], sys.argv[2]
real_project = os.path.realpath(project)
try:
    hook = json.load(sys.stdin)
except ValueError:
    sys.exit(0)
if not isinstance(hook, dict) or hook.get("stop_hook_active"):
    sys.exit(0)
session = str(hook.get("session_id") or "")

# This session's name, repo, cache and claims, as state.sh sees them. No network.
try:
    out = subprocess.run(["bash", state, "mine", "--session-id", session],
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=5,
                         universal_newlines=True, env=dict(os.environ, CLAUDE_PROJECT_DIR=project)).stdout
except Exception:
    sys.exit(0)
info, held = {}, []
for line in out.splitlines():
    key, _, value = line.partition(" ")
    if key == "claim":
        held.append(value)
    elif key:
        info[key] = value
me = info.get("me", "")
if not me:
    sys.exit(0)
who = re.sub(r"[^A-Za-z0-9._-]", "_", me)

def epoch(stamp, fmt=None):
    try:
        if fmt:
            return datetime.strptime(stamp, fmt).replace(tzinfo=timezone.utc).timestamp()
        return datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
    except (TypeError, ValueError):
        return None

# When this session last handed off this repo: state.sh logs each handoff as STAMP-WHO.md.
handed = None
for path in glob.glob(os.path.join(info.get("dir", ""), "projects", info.get("repo", ""), "log", "*.md")):
    name = os.path.basename(path)
    if name[19:-3] != who:
        continue
    t = epoch(name[:18], "%Y-%m-%dT%H%M%SZ")
    if t is not None and (handed is None or t > handed):
        handed = t

def resolve(base, path):
    return os.path.realpath(os.path.join(base, os.path.expanduser(os.path.expandvars(path))))

def in_project(path):
    return path == real_project or path.startswith(real_project + os.sep)

def commands(text):
    """The simple commands in a command line, as word lists. Heredoc bodies are data."""
    kept, tag = [], None
    text = text.replace("\\\n", " ")  # a backslash-newline joins two lines
    for line in text.split("\n"):
        if tag is not None:
            if line.strip() == tag:
                tag = None
            continue
        kept.append(line)
        m = re.search(r"(?:^|[^<])<<-?\s*[\"']?([A-Za-z_][A-Za-z0-9_]*)", line)
        if m:
            tag = m.group(1)
    lexer = shlex.shlex(" ;\n".join(kept), posix=True, punctuation_chars=True)
    lexer.whitespace_split = True
    found, words = [], []
    try:
        for token in lexer:
            if token and set(token) <= set(";&|()<>"):
                if words:
                    found.append(words)
                words = []
            else:
                words.append(token)
    except ValueError:
        pass
    if words:
        found.append(words)
    return found

# Commands that run another command, and their options that take a value.
RUNNERS = {
    "sudo": {"-u", "-g", "-h", "-p", "-C", "-D", "-r", "-t", "-U"}, "doas": {"-u"},
    "env": {"-u", "-C", "-S", "--unset", "--chdir", "--split-string"},
    "nice": {"-n"}, "ionice": {"-c", "-n", "-p"}, "stdbuf": {"-i", "-o", "-e"},
    "xargs": {"-n", "-I", "-L", "-P", "-s", "-d", "-E", "-a"},
    "timeout": {"-s", "-k", "--signal", "--kill-after"},
    "command": set(), "builtin": set(), "exec": set(), "nohup": set(), "time": set(),
    "caffeinate": set(), "!": set(), "{": set(),
    "if": set(), "then": set(), "else": set(), "elif": set(), "while": set(), "until": set(), "do": set(),
}

def strip(words):
    """Drop what runs a command without being it: VAR=value, sudo, env, timeout and the like."""
    i = 0
    while i < len(words):
        w = words[i]
        if re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", w):
            i += 1
        elif w in RUNNERS:
            i += 1
            while i < len(words) and words[i].startswith("-") and words[i] != "--":
                i += 2 if words[i] in RUNNERS[w] else 1
            if i < len(words) and words[i] == "--":
                i += 1
            if w == "timeout" and i < len(words):
                i += 1
        else:
            break
    return words[i:]

def pushes_here(command, cwd):
    """Whether the command line runs a git push in this project, and whether that
    push is the line's last command, so the call's exit status is the push's."""
    where = cwd
    found = commands(command)
    pushed = False
    for n, words in enumerate(found):
        words = strip(words)
        i = 0
        if not words:
            continue
        if words[i] in ("cd", "pushd"):
            where = resolve(where, words[i + 1]) if i + 1 < len(words) else os.path.expanduser("~")
            continue
        if os.path.basename(words[i]) != "git":
            continue
        i += 1
        target = where
        while i < len(words) and words[i].startswith("-"):
            if words[i] == "-C" and i + 1 < len(words):
                target = resolve(target, words[i + 1])
                i += 2
            elif words[i] in ("-c", "--git-dir", "--work-tree", "--namespace") and i + 1 < len(words):
                i += 2
            else:
                i += 1
        if i >= len(words) or words[i] != "push":
            continue
        if any(w == "--dry-run" or re.match(r"^-[a-zA-Z]*n[a-zA-Z]*$", w) for w in words[i + 1:]):
            continue
        if in_project(target):
            pushed, last = True, n == len(found) - 1
    return (pushed, last) if pushed else (False, False)

def text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(text(b.get("text", b.get("content"))) for b in content if isinstance(b, dict))
    return ""

failed = re.compile(r"temper guard:|\[rejected\]|error: failed to push")
pushed_ok = re.compile(r"\s->\s|\[new branch\]|Everything up-to-date")
claimed = re.compile(r"^temper state: claimed ", re.M)

# When this session last pushed this repo or claimed a task, from its transcripts.
transcript = os.path.expanduser(str(hook.get("transcript_path") or ""))
files = [transcript]
if transcript.endswith(".jsonl"):
    files += sorted(glob.glob(os.path.join(transcript[:-6], "subagents", "*.jsonl")))
work = None
for path in files:
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            lines = f.read().splitlines()
    except OSError:
        continue
    calls, results = {}, {}
    for line in lines:
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        message = entry.get("message") if isinstance(entry, dict) else None
        content = message.get("content") if isinstance(message, dict) else None
        if not isinstance(content, list):
            continue
        at = epoch(str(entry.get("timestamp") or "")) or 0.0
        cwd = str(entry.get("cwd") or project)
        for block in content:
            if not isinstance(block, dict):
                continue
            if block.get("type") == "tool_use" and block.get("name") == "Bash":
                calls[block.get("id")] = (at, str((block.get("input") or {}).get("command", "")), cwd)
            elif block.get("type") == "tool_result":
                results[block.get("tool_use_id")] = (at, text(block.get("content")), block.get("is_error") is True)
    for tid, (at, command, cwd) in calls.items():
        result = results.get(tid)
        times = []
        pushed, last = pushes_here(command, cwd)
        if pushed:
            # Count it unless its output says it failed, or it ended the call and the call failed.
            if result is None or pushed_ok.search(result[1]):
                times.append(at)
            elif not failed.search(result[1]) and not (result[2] and last):
                times.append(at)
        if result is not None and claimed.search(result[1]):
            times.append(result[0])
        for t in times:
            if work is None or t > work:
                work = t

pending = work is not None and (handed is None or work > handed + 1)
if not pending and not held:
    sys.exit(0)

# Ask once per piece of work: the same work and claims don't hold the session again.
key = re.sub(r"[^A-Za-z0-9_-]", "", session) or "unknown"
mark = os.path.join(tempfile.gettempdir(), "temper-handoff-" + key)
digest = hashlib.sha1(repr((work, sorted(held))).encode()).hexdigest()
try:
    with open(mark) as f:
        if f.read().strip() == digest:
            sys.exit(0)
except OSError:
    pass
try:
    with open(mark, "w") as f:
        f.write(digest)
except OSError:
    pass

q = shlex.quote(state)
out = ["temper handoff: before you stop, put this session on the shared state (state.md)."]
if pending:
    out.append('- You pushed or claimed work with no handoff since. Hand off: printf "Done: ...\\nIn flight: ...\\nNext: ...\\n" | bash %s handoff' % q)
for name in held:
    out.append("- You hold the claim %s. If that task is done or you're leaving it: bash %s release %s" % (name, q, shlex.quote(name)))
out.append("If you're mid-task and waiting on Nguyen, say so in one line and stop. This asks once per piece of work.")
print("\n".join(out))
PY

msg="$(printf '%s' "$input" | python3 -c "$check" "$state" "$project" 2>/dev/null)"
[ -n "$msg" ] || exit 0
printf '%s\n' "$msg" >&2
exit 2
