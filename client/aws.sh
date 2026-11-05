#!/bin/sh
# Makes a runner host on AWS, from the laptop, with the AWS CLI. Sign in
# first (`aws login`) and set a region (`aws configure set region us-west-2`);
# AWS_PROFILE picks a profile other than default.
#
#   client/aws.sh up        launch the host if it isn't there, then set it up
#   client/aws.sh setup     set up the host up launched, and nothing else
#   client/aws.sh ip        print its public address
#   client/aws.sh ssh ...   log in as ubuntu, or run a command there (over the
#                           tailnet once close has shut SSH to the internet)
#   client/aws.sh open      let this laptop's address in over SSH (up does)
#   client/aws.sh close     let nothing in, once the host is on the tailnet
#   client/aws.sh stop      stop it: then only its disks cost
#   client/aws.sh start     start it again, at a new address
#
# The host is an m7i-flex.large with nested virtualization on, which gives it
# /dev/kvm at a regular instance's price, running Ubuntu 24.04, with a 40 GiB
# root disk and a 50 GiB one for /var/lib/runner (README.md, On AWS).
#
# The setup goes in over SSH as ubuntu, and each step checks first, so a rerun
# after a failure redoes nothing that's done: XFS on the 50 GiB disk at
# /var/lib/runner (never over a disk that holds anything else); this checkout,
# by rsync, then Rust, the build tools and podman, then cargo build --release
# and host/install.sh; the base image and kernel, built there and imported;
# Tailscale, as $RUNNER_AWS_NAME (it stops for the login link); the laptop's
# ~/.ssh/id_ed25519.pub in ~runner/.ssh/authorized_keys; the credentials; then
# runner doctor as runner@<name> over the tailnet, and only once every line is
# ok, close. The credentials come from the host RUNNER_AWS_TOKEN_FROM names
# (kvm-host; empty for none), host to host through one pipe, so a token is
# never in an argument, a variable, a file or on the screen here: the Claude
# token, else it's typed in on the host with echo off, and GH_TOKEN and
# ANTHROPIC_API_KEY when that host has them.
#
# RUNNER_AWS_NAME names the instance, its key pair and its security group
# (kvm-host), and is the tailnet name ssh dials once SSH is closed: the host
# joins Tailscale under it (`tailscale up --hostname`). RUNNER_AWS_TYPE picks another type that has nested
# virtualization. RUNNER_AWS_KEY is where the key pair's private key is kept
# (~/.ssh/<name>.pem); the token's source host is reached with its own,
# ~/.ssh/<that name>.pem.
set -eu

name=${RUNNER_AWS_NAME:-kvm-host}
from=${RUNNER_AWS_TOKEN_FROM-kvm-host}
type=${RUNNER_AWS_TYPE:-m7i-flex.large}
key=${RUNNER_AWS_KEY:-$HOME/.ssh/$name.pem}
pub=$HOME/.ssh/id_ed25519.pub
ami=resolve:ssm:/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id
# Ubuntu's root is /dev/sda1. The second disk shows up as an NVMe one,
# usually /dev/nvme1n1, but not always, so the setup finds it by its size.
data_gib=50
disks='[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":40,"VolumeType":"gp3"}},{"DeviceName":"/dev/sdf","Ebs":{"VolumeSize":'$data_gib',"VolumeType":"gp3"}}]'
# No prompts for ubuntu's key, and a dropped connection ends a step in a
# minute and a half. IdentitiesOnly: an agent offering six other keys first
# would use up sshd's tries.
opts="-o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 -o ServerAliveInterval=30"
# The CLI pages what it prints to a terminal.
AWS_PAGER=
export AWS_PAGER
nl='
'

die() {
  printf 'aws.sh: %s\n' "$*" >&2
  exit 1
}

say() {
  printf '==> %s\n' "$*"
}

usage() {
  sed -n '6,14s/^# //p' "$0" >&2
  exit 2
}

# A name that goes into commands run on the host, and is its tailnet name:
# letters, digits and dashes.
tailnet_name() {
  case $1 in
  '' | -* | *[!A-Za-z0-9-]*) return 1 ;;
  esac
}

# The instance named $name that isn't terminated, as "id state address".
instance() {
  aws ec2 describe-instances \
    --filters "Name=tag:Name,Values=$name" \
    "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --query 'Reservations[].Instances[].[InstanceId,State.Name,PublicIpAddress]' \
    --output text
}

# Sets id, state and addr (None while it has none) from $found, or dies.
parse() {
  [ -n "$found" ] || die "there's no $name: make it with $0 up"
  case $found in
  *"$nl"*) die "more than one instance is named $name:$nl$found" ;;
  esac
  read -r id state addr <<EOF
$found
EOF
}

find_host() {
  found=$(instance)
  parse
}

# The address, or dies.
address() {
  find_host
  [ "$addr" != None ] || die "$name is $state, with no address: $0 start"
}

# Waits for $id's status checks, which pass once it has booted, so the first
# ssh gets in; running comes a minute before sshd does.
booted() {
  aws ec2 wait instance-status-ok --instance-ids "$id"
}

keypair() {
  if aws ec2 describe-key-pairs --key-names "$name" >/dev/null 2>&1; then
    [ -f "$key" ] || die "AWS has key pair $name, but its private key isn't at $key: set RUNNER_AWS_KEY to it, or delete the pair (aws ec2 delete-key-pair --key-name $name)"
    return
  fi
  [ ! -e "$key" ] || die "$key is there, but AWS has no key pair $name: move it away, or set RUNNER_AWS_KEY"
  mkdir -p "$(dirname "$key")"
  # Through a temporary file, so a refusal leaves no key behind for the next
  # up to trip on.
  if ! (umask 077 && aws ec2 create-key-pair --key-name "$name" \
    --query KeyMaterial --output text >"$key.new"); then
    rm -f "$key.new"
    exit 1
  fi
  mv "$key.new" "$key"
  printf 'Made key pair %s: its private key is %s\n' "$name" "$key"
}

# Takes every inbound rule off the group. Fails if nothing was on it. It's
# called as a condition, where set -e is off, so it exits by hand.
revoke_all() {
  perms=$(aws ec2 describe-security-groups --group-names "$name" \
    --query 'SecurityGroups[0].IpPermissions' --output json) || exit 1
  [ "$perms" != "[]" ] || return 1
  aws ec2 revoke-security-group-ingress --group-name "$name" \
    --ip-permissions "$perms" >/dev/null || exit 1
}

# Only this laptop's address now: the ones it had before, a café's say, go.
open_ssh() {
  aws ec2 describe-security-groups --group-names "$name" >/dev/null 2>&1 ||
    aws ec2 create-security-group --group-name "$name" \
      --description "runner host: SSH from the laptop until client/aws.sh close" >/dev/null
  ip=$(curl -fsS https://checkip.amazonaws.com) || die "can't tell this laptop's address"
  revoke_all || true
  aws ec2 authorize-security-group-ingress --group-name "$name" \
    --protocol tcp --port 22 --cidr "$ip/32" >/dev/null
  printf 'SSH to %s is open to %s only.\n' "$name" "$ip"
}

close_ssh() {
  if revoke_all; then
    printf 'Nothing gets in to %s now: reach it over the tailnet.\n' "$name"
  else
    printf 'Nothing gets in to %s.\n' "$name"
  fi
}

# Sets target: the address while SSH is open to the internet, else the
# name, on the tailnet, once `close` has let nothing in. A group that can't
# be read leaves the address.
reach() {
  target=$addr
  perms=$(aws ec2 describe-security-groups --group-names "$name" \
    --query 'SecurityGroups[0].IpPermissions' --output json 2>/dev/null) || true
  [ "$perms" != "[]" ] || target=$name
}

# ssh to the host being set up, as ubuntu, with its key. $opts is the
# options, split.
# shellcheck disable=SC2086
on_host() {
  ssh -i "$key" $opts "$@"
}

# ssh to the token's source host, with its own key.
# shellcheck disable=SC2086
on_from() {
  ssh -i "$from_key" $opts "$@"
}

stopped() {
  die "the setup stopped at $1: once that's fixed, rerun it with ${rerun}"
}

# What runs on the host, as ubuntu, in sh. Each step checks first, so a rerun
# redoes nothing that's done. The laptop's values come in as assignments put
# in front.

# XFS on the $gib GiB disk, at /var/lib/runner. A disk holding anything but
# XFS, a partition table included, stays as it is.
disk_sh() {
  cat <<'EOF'
set -eu
mnt=/var/lib/runner
xfs() { [ "$(findmnt -n -o FSTYPE --mountpoint "$mnt" || true)" = xfs ]; }
if xfs; then
  echo "disk: $mnt is on XFS already"
  exit 0
fi
size=$((gib * 1024 * 1024 * 1024))
disk=$(lsblk -bdnp -o NAME,SIZE,TYPE | while read -r dev bytes kind; do
  if [ "$bytes" = "$size" ] && [ "$kind" = disk ]; then echo "$dev"; fi
done)
case $disk in
'')
  echo "disk: there's no $gib GiB disk here (lsblk)" >&2
  exit 1
  ;;
*'
'*)
  echo "disk: more than one $gib GiB disk, so it won't pick one:" $disk >&2
  exit 1
  ;;
esac
# -p reads the disk itself, not a cache that may not have seen a new mkfs.
probe=$(sudo blkid -p -o export "$disk" || true)
if [ -z "$probe" ]; then
  echo "disk: making XFS on $disk"
  sudo mkfs.xfs -q "$disk"
elif ! printf '%s\n' "$probe" | grep -qx 'TYPE=xfs'; then
  echo "disk: $disk already holds" $(printf '%s\n' "$probe" | grep -E '^(PT)?TYPE=') "so it stays as it is. Wipe it by hand only if nothing on it matters." >&2
  exit 1
fi
uuid=$(sudo blkid -p -o value -s UUID "$disk")
if ! grep -qs "[[:space:]]$mnt[[:space:]]" /etc/fstab; then
  echo "UUID=$uuid $mnt xfs defaults,nofail 0 2" | sudo tee -a /etc/fstab >/dev/null
fi
sudo mkdir -p "$mnt"
sudo mount "$mnt"
xfs || { echo "disk: $mnt isn't XFS after mounting it (findmnt $mnt)" >&2; exit 1; }
echo "disk: $disk is XFS, at $mnt"
EOF
}

# What the build needs, before the copy, which needs rsync there too. apt
# waits out the lock that unattended-upgrades holds after a first boot.
tools_sh() {
  cat <<'EOF'
set -eu
missing=
for p in build-essential podman e2fsprogs rsync curl git; do
  dpkg -s "$p" >/dev/null 2>&1 || missing="$missing $p"
done
if [ -n "$missing" ]; then
  echo "tools: installing$missing"
  sudo apt-get -o DPkg::Lock::Timeout=600 update -q
  sudo DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=600 install -y -q $missing
fi
[ ! -f "$HOME/.cargo/env" ] || . "$HOME/.cargo/env"
if ! command -v cargo >/dev/null; then
  echo "tools: installing Rust; cargo takes the version rust-toolchain.toml pins"
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
fi
EOF
}

# host/install.sh is safe to rerun, and cargo builds only what changed.
install_sh() {
  cat <<'EOF'
set -eu
[ ! -f "$HOME/.cargo/env" ] || . "$HOME/.cargo/env"
cd ai-agent-runner
cargo build --release
sudo host/install.sh
EOF
}

# The base image and kernel, built here with podman (docker drops the VMs'
# forwarded traffic), unless a build is there already.
base_sh() {
  cat <<'EOF'
set -eu
[ ! -f "$HOME/.cargo/env" ] || . "$HOME/.cargo/env"
cd ai-agent-runner
kernel=/var/lib/runner/kernel/vmlinux
has_base() { sudo -u runner runner status base >/dev/null 2>&1; }
if has_base && sudo test -s "$kernel"; then
  echo "base: imported already, and the kernel installed"
  exit 0
fi
if [ ! -s images/base/out/base.ext4 ] || [ ! -s images/base/out/vmlinux ]; then
  echo "base: building it, about 10 minutes"
  cargo build --release --target x86_64-unknown-linux-musl --bin runner-guest
  sudo ENGINE=podman images/base/build.sh
fi
if ! has_base; then
  # The runner user can't read this checkout, so the disk comes in on stdin.
  sudo -u runner runner import --replace base - <images/base/out/base.ext4
fi
sudo test -s "$kernel" || sudo install -D -m 0644 images/base/out/vmlinux "$kernel"
echo "base: imported, and the kernel installed"
EOF
}

# On the tailnet as $name. A name the tailnet gave it instead, like
# kvm-host-2-1, means another machine there has $name. The status's first
# DNSName is its own: Self comes before Peer.
tailscale_sh() {
  cat <<'EOF'
set -eu
if ! command -v tailscale >/dev/null; then
  echo "tailscale: installing it"
  curl -fsSL https://tailscale.com/install.sh | sh
fi
status() { tailscale status --json 2>/dev/null || true; }
if ! status | grep -q '"BackendState": *"Running"'; then
  echo "tailscale: open the link it prints to add $name to your tailnet"
  sudo tailscale up --hostname "$name"
fi
got=$(status | sed -n 's/^ *"DNSName": *"\([^."]*\).*/\1/p' | head -n 1)
if [ "$got" != "$name" ]; then
  echo "tailscale: the tailnet named it $got, not $name, so another machine there is $name already. Remove the old $name (login.tailscale.com/admin/machines) and rename this one $name there." >&2
  exit 1
fi
echo "tailscale: on the tailnet as $name"
EOF
}

# The laptop's key line, from stdin, once, as README.md writes it: from the
# tailnet only, with a terminal and ssh -L.
key_sh() {
  cat <<'EOF'
set -eu
IFS= read -r line
set -- $line
keys=~runner/.ssh/authorized_keys
if sudo grep -qsF -- "$1 $2" "$keys"; then
  echo "key: the laptop's is in already"
  exit 0
fi
printf 'from="100.64.0.0/10,fd7a:115c:a1e0::/48",restrict,pty,port-forwarding %s\n' "$line" | sudo -u runner tee -a "$keys" >/dev/null
echo "key: the laptop's is in"
EOF
}

# Credential $c, from stdin, kept only if it holds something, readable by
# root alone. After $prompt, a line typed on the terminal with echo off.
cred_sh() {
  cat <<'EOF'
set -eu
f=/etc/credstore/$c
sudo install -d -m 0700 /etc/credstore
if [ -n "${prompt:-}" ]; then
  printf '%s: ' "$prompt"
  stty -echo || { echo "credentials: there's no terminal to type $c in" >&2; exit 1; }
  trap 'stty echo' EXIT
  sudo sh -c "umask 077 && head -n 1 >$f.new"
  stty echo
  echo
else
  sudo sh -c "umask 077 && cat >$f.new"
fi
if ! sudo grep -q '[^[:space:]]' "$f.new"; then
  sudo rm -f "$f.new"
  echo "credentials: read nothing for $c, so wrote nothing" >&2
  exit 1
fi
sudo mv "$f.new" "$f"
EOF
}

# Each credential the host lacks. The Claude token comes from $from, else
# it's typed in; the others only from $from.
creds() {
  src=
  from_key=$HOME/.ssh/$from.pem
  if [ -z "$from" ] || [ "$from" = "$name" ]; then
    :
  elif [ ! -r "$from_key" ]; then
    say "credentials: there's no $from_key, so none come from $from"
  elif on_from -n "ubuntu@$from" true; then
    src=$from
  else
    say "credentials: $from doesn't answer as ubuntu@$from, so none come from it"
  fi
  for c in CLAUDE_CODE_OAUTH_TOKEN GH_TOKEN ANTHROPIC_API_KEY; do
    if on_host -n "$at" "sudo test -s /etc/credstore/$c"; then
      say "credentials: $c is there already"
    elif [ -n "$src" ] && on_from -n "ubuntu@$src" "sudo test -s /etc/credstore/$c"; then
      # Host to host: the laptop only passes it along.
      on_from -n "ubuntu@$src" "sudo cat /etc/credstore/$c" |
        on_host "$at" "c=$c; $(cred_sh)" || stopped "copying $c from $src"
      say "credentials: $c copied from $src"
    elif [ "$c" = CLAUDE_CODE_OAUTH_TOKEN ]; then
      on_host -t "$at" "c=$c; prompt='Claude Code token for $name (claude setup-token makes one), then Enter'; $(cred_sh)" ||
        stopped "the Claude token"
      say "credentials: $c typed in"
    fi
  done
}

# runner doctor as the laptop will reach the host: runner@$name, over the
# tailnet. Every line ok, or SSH stays open.
doctor() {
  report=$(ssh -n -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 \
    "runner@$name" runner doctor 2>&1) && code=0 || code=$?
  printf '%s\n' "$report"
  if [ "$code" -eq 255 ]; then
    die "runner@$name doesn't answer over the tailnet, so SSH from the internet stays open: check Tailscale is up on this laptop, then rerun it with ${rerun}"
  fi
  bad=$(printf '%s\n' "$report" | awk 'NF && $1 != "ok"')
  if [ -n "$bad" ] || [ -z "$report" ] || [ "$code" -ne 0 ]; then
    die "runner doctor isn't all ok, so SSH from the internet stays open:$nl${bad:-(exit $code)}${nl}Once that's fixed, rerun it with ${rerun}"
  fi
}

setup() {
  address
  reach
  rerun="$0 setup"
  [ "$name" = kvm-host ] || rerun="RUNNER_AWS_NAME=$name $rerun"
  at=ubuntu@$target
  say "Setting $name up, as $at"

  say "disk: XFS on the $data_gib GiB disk, at /var/lib/runner"
  on_host -n "$at" "gib=$data_gib; $(disk_sh)" || stopped "the disk"

  say "build: this checkout, then cargo build --release and host/install.sh"
  on_host -n "$at" "$(tools_sh)" || stopped "the build tools"
  # rsync's -e takes single quotes around an argument. With CDPATH set, cd
  # would print where it went.
  (CDPATH='' cd "$(dirname "$0")/.." &&
    rsync -az --delete --exclude target --exclude images/base/out \
      -e "ssh -i '$key' $opts" ./ "$at:ai-agent-runner/") || stopped "the copy"
  on_host -n "$at" "$(install_sh)" || stopped "the build"

  say "base: the base image and kernel"
  on_host -n "$at" "$(base_sh)" || stopped "the base image"

  say "tailscale: on the tailnet as $name"
  on_host -n "$at" "name=$name; $(tailscale_sh)" || stopped "Tailscale"

  say "key: this laptop's, for runner"
  on_host "$at" "$(key_sh)" <"$pub" || stopped "the laptop's key"

  say "credentials: the Claude token and the rest"
  creds

  say "doctor: as runner@$name, over the tailnet"
  doctor
  close_ssh
  printf '%s is set up. Next, on this laptop: runner hosts add runner@%s\n' "$name" "$name"
}

# Before anything is launched or changed.
need_pub() {
  [ -r "$pub" ] || die "there's no $pub for the host to let this laptop in as runner: make one with ssh-keygen -t ed25519"
}

up() {
  need_pub
  found=$(instance)
  if [ -n "$found" ]; then
    parse
    case $state in
    stopping | stopped)
      printf '%s (%s) is already there, %s: start it with %s start, then set it up with %s setup\n' "$name" "$id" "$state" "$0" "$0"
      return
      ;;
    esac
    printf '%s is already there: %s, %s, at %s\n' "$name" "$id" "$state" "$addr"
    # A pending one hasn't booted yet.
    booted
    setup
    return
  fi
  keypair
  open_ssh
  id=$(aws ec2 run-instances \
    --image-id "$ami" \
    --instance-type "$type" \
    --cpu-options NestedVirtualization=enabled \
    --key-name "$name" \
    --security-groups "$name" \
    --block-device-mappings "$disks" \
    --metadata-options HttpTokens=required,HttpPutResponseHopLimit=1 \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$name}]" \
    --query 'Instances[0].InstanceId' --output text)
  printf 'Launched %s (%s), waiting a few minutes for it to boot...\n' "$name" "$id"
  booted
  address
  printf '%s is up at %s.\n' "$name" "$addr"
  setup
}

[ $# -gt 0 ] || usage
case $1 in
up | setup)
  tailnet_name "$name" || die "RUNNER_AWS_NAME=$name: the host's tailnet name too, so letters, digits and dashes only"
  [ -z "$from" ] || tailnet_name "$from" || die "RUNNER_AWS_TOKEN_FROM=$from: a tailnet name, so letters, digits and dashes only"
  ;;
esac
command -v aws >/dev/null || die "needs the AWS CLI: brew install awscli, then aws login"
aws sts get-caller-identity >/dev/null ||
  die "AWS won't take this laptop's credentials: sign in with aws login (or aws login --profile <name>, then AWS_PROFILE=<name>)"

cmd=$1
shift
case $cmd in
up) up ;;
setup)
  need_pub
  find_host
  case $state in
  stopping | stopped) die "$name is $state: start it with $0 start, then rerun it" ;;
  esac
  setup
  ;;
ip)
  address
  printf '%s\n' "$addr"
  ;;
ssh)
  address
  # Once `close` lets nothing in from the internet, the host is reached as
  # $name over the tailnet (README.md, On AWS); until then, at its address.
  reach
  # IdentitiesOnly: an agent offering six other keys first would use up
  # sshd's tries.
  exec ssh -i "$key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new "ubuntu@$target" "$@"
  ;;
open) open_ssh ;;
close) close_ssh ;;
stop)
  find_host
  aws ec2 stop-instances --instance-ids "$id" >/dev/null
  printf 'Stopping %s (%s)...\n' "$name" "$id"
  # Stopped, not stopping: a start in between gets IncorrectInstanceState.
  aws ec2 wait instance-stopped --instance-ids "$id"
  printf '%s is stopped: only its disks cost now.\n' "$name"
  ;;
start)
  find_host
  # AWS refuses to start a stopping instance: IncorrectInstanceState.
  [ "$state" != stopping ] || aws ec2 wait instance-stopped --instance-ids "$id"
  aws ec2 start-instances --instance-ids "$id" >/dev/null
  booted
  address
  printf '%s is up at %s.\n' "$name" "$addr"
  ;;
*) usage ;;
esac
