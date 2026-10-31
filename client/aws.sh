#!/bin/sh
# Makes a runner host on AWS, from the laptop, with the AWS CLI. Sign in
# first (`aws login`) and set a region (`aws configure set region us-west-2`);
# AWS_PROFILE picks a profile other than default.
#
#   client/aws.sh up        launch the host, or say where it is
#   client/aws.sh ip        print its public address
#   client/aws.sh ssh ...   log in as ubuntu, or run a command there
#   client/aws.sh open      let this laptop's address in over SSH (up does)
#   client/aws.sh close     let nothing in, once the host is on the tailnet
#   client/aws.sh stop      stop it: then only its disks cost
#   client/aws.sh start     start it again, at a new address
#
# The host is an m7i-flex.large with nested virtualization on, which gives it
# /dev/kvm at a regular instance's price, running Ubuntu 24.04, with a 40 GiB
# root disk and a 50 GiB one for /var/lib/runner (README.md, On AWS).
#
# RUNNER_AWS_NAME names the instance, its key pair and its security group
# (kvm-host). RUNNER_AWS_TYPE picks another type that has nested
# virtualization. RUNNER_AWS_KEY is where the key pair's private key is kept
# (~/.ssh/<name>.pem).
set -eu

name=${RUNNER_AWS_NAME:-kvm-host}
type=${RUNNER_AWS_TYPE:-m7i-flex.large}
key=${RUNNER_AWS_KEY:-$HOME/.ssh/$name.pem}
ami=resolve:ssm:/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id
# Ubuntu's root is /dev/sda1. The second disk shows up as an NVMe one,
# usually /dev/nvme1n1.
disks='[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":40,"VolumeType":"gp3"}},{"DeviceName":"/dev/sdf","Ebs":{"VolumeSize":50,"VolumeType":"gp3"}}]'
# The CLI pages what it prints to a terminal.
AWS_PAGER=
export AWS_PAGER
nl='
'

die() {
  printf 'aws.sh: %s\n' "$*" >&2
  exit 1
}

usage() {
  sed -n '6,12s/^# *//p' "$0" >&2
  exit 2
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

open_ssh() {
  aws ec2 describe-security-groups --group-names "$name" >/dev/null 2>&1 ||
    aws ec2 create-security-group --group-name "$name" \
      --description "runner host: SSH from the laptop until client/aws.sh close" >/dev/null
  ip=$(curl -fsS https://checkip.amazonaws.com) || die "can't tell this laptop's address"
  if ! err=$(aws ec2 authorize-security-group-ingress --group-name "$name" \
    --protocol tcp --port 22 --cidr "$ip/32" 2>&1 >/dev/null); then
    case $err in
    *InvalidPermission.Duplicate*) ;;
    *) die "$err" ;;
    esac
  fi
  printf 'SSH to %s is open to %s only.\n' "$name" "$ip"
}

close_ssh() {
  perms=$(aws ec2 describe-security-groups --group-names "$name" \
    --query 'SecurityGroups[0].IpPermissions' --output json)
  if [ "$perms" = "[]" ]; then
    printf 'Nothing gets in to %s.\n' "$name"
    return
  fi
  aws ec2 revoke-security-group-ingress --group-name "$name" \
    --ip-permissions "$perms" >/dev/null
  printf 'Nothing gets in to %s now: reach it over the tailnet.\n' "$name"
}

up() {
  found=$(instance)
  if [ -n "$found" ]; then
    parse
    printf '%s is already there: %s, %s, at %s\n' "$name" "$id" "$state" "$addr"
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
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$name}]" \
    --query 'Instances[0].InstanceId' --output text)
  printf 'Launched %s (%s), waiting for it to run...\n' "$name" "$id"
  aws ec2 wait instance-running --instance-ids "$id"
  address
  printf '%s is up at %s. Check it has KVM: %s ssh ls -l /dev/kvm\n' "$name" "$addr" "$0"
}

[ $# -gt 0 ] || usage
command -v aws >/dev/null || die "needs the AWS CLI: brew install awscli, then aws login"
aws sts get-caller-identity >/dev/null ||
  die "AWS won't take this laptop's credentials: sign in with aws login (or aws login --profile <name>, then AWS_PROFILE=<name>)"

cmd=$1
shift
case $cmd in
up) up ;;
ip)
  address
  printf '%s\n' "$addr"
  ;;
ssh)
  address
  exec ssh -i "$key" -o StrictHostKeyChecking=accept-new "ubuntu@$addr" "$@"
  ;;
open) open_ssh ;;
close) close_ssh ;;
stop)
  find_host
  aws ec2 stop-instances --instance-ids "$id" >/dev/null
  printf 'Stopping %s (%s).\n' "$name" "$id"
  ;;
start)
  find_host
  aws ec2 start-instances --instance-ids "$id" >/dev/null
  aws ec2 wait instance-running --instance-ids "$id"
  address
  printf '%s is up at %s.\n' "$name" "$addr"
  ;;
*) usage ;;
esac
