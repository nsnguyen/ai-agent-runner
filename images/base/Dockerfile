# The base VM: Ubuntu 24.04 that boots systemd into a dev box with Claude
# Code, like a fresh sprite. build.sh turns it into an ext4 disk. It boots as
# a VM, not a container, so nothing here starts a service.
FROM ubuntu:24.04

ARG NODE_MAJOR=26
ARG CLAUDE_CODE_VERSION=stable
ARG DEBIAN_FRONTEND=noninteractive
# A failed download piped into sh must fail the build, not install nothing.
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# systemd is init. No sshd: the host reaches the VM through runner-guest.
# Docker Engine, Compose v2 and buildx are Ubuntu's, so they update with the
# image; installed here, while the image's policy-rc.d still keeps their
# postinst from starting dockerd in the build.
RUN apt-get update && apt-get install -y --no-install-recommends \
      systemd systemd-sysv udev dbus libnss-myhostname iproute2 iputils-ping \
      ca-certificates curl git openssh-client sudo less xz-utils unzip \
      build-essential pkg-config tmux python3 python3-pip python3-venv \
      docker.io docker-compose-v2 docker-buildx \
 && curl -fsSL "https://deb.nodesource.com/setup_${NODE_MAJOR}.x" -o /tmp/nodesource.sh \
 && bash /tmp/nodesource.sh && rm /tmp/nodesource.sh \
 && apt-get install -y --no-install-recommends nodejs \
 && rm -rf /var/lib/apt/lists/*

# Undo the container tweaks in Ubuntu's image: in a VM, packages start their
# services and dpkg syncs its writes. Tools see UTF-8, so tmux draws Claude
# Code's TUI.
RUN rm -f /usr/sbin/policy-rc.d /etc/dpkg/dpkg.cfg.d/docker-apt-speedup \
 && echo 'LANG=C.UTF-8' > /etc/locale.conf

# Ubuntu's image ships an `ubuntu` user at uid 1000; agent takes its place.
RUN userdel -r ubuntu \
 && useradd --create-home --uid 1000 --shell /bin/bash agent \
 && echo 'agent ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/agent \
 && chmod 0440 /etc/sudoers.d/agent

USER agent
WORKDIR /home/agent
RUN curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --component rustfmt,clippy \
 && curl -fsSL https://claude.ai/install.sh | bash -s -- "$CLAUDE_CODE_VERSION" \
 && mkdir -p work

USER root
# Commands from runner-guest get systemd's PATH, not a login shell's, so the
# agent's own tools are linked into /usr/local/bin too.
RUN ln -s /home/agent/.local/bin/claude /usr/local/bin/claude \
 && for bin in /home/agent/.cargo/bin/*; do ln -s "$bin" /usr/local/bin/; done

# The installer's download cache and any first-run state go: every fork of
# base starts logged out, with no install id it shares with the others.
RUN rm -rf /home/agent/.claude /home/agent/.claude.json /root/.claude /root/.claude.json \
      /home/agent/.npm /home/agent/.cache/claude

# Each VM makes its own machine id on first boot, and nothing prompts on the
# serial console for a locale or a root password.
RUN : > /etc/machine-id && rm -f /var/lib/dbus/machine-id \
 && ln -s /dev/null /etc/systemd/system/systemd-firstboot.service

# Docker for agent, without sudo, started at boot so a container with a
# restart policy comes back after a reboot. Firecracker's CI kernel has no
# nf_tables, so iptables is the legacy variant, and no raw table, which the
# drop-in tells dockerd to leave alone. Container logs rotate rather than
# fill the disk.
COPY docker-daemon.json /etc/docker/daemon.json
COPY docker-no-raw-table.conf /etc/systemd/system/docker.service.d/no-raw-table.conf
RUN usermod -aG docker agent \
 && update-alternatives --set iptables /usr/sbin/iptables-legacy \
 && update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy \
 && systemctl enable containerd.service docker.service \
 && chmod 0755 /etc/docker /etc/systemd/system/docker.service.d \
 && chmod 0644 /etc/docker/daemon.json /etc/systemd/system/docker.service.d/no-raw-table.conf

COPY runner-guest /usr/local/bin/runner-guest
COPY runner-guest.service /etc/systemd/system/runner-guest.service
RUN chmod 0755 /usr/local/bin/runner-guest && systemctl enable runner-guest.service

# Claude Code stays at the version installed above: its managed settings,
# which every claude here reads however it starts, turn its updater off. Not
# under /home/agent/.claude, which forks start with empty.
COPY managed-settings.json /etc/claude-code/managed-settings.json
RUN chmod 0755 /etc/claude-code && chmod 0644 /etc/claude-code/managed-settings.json

# What the agent reads to know where it is, like a sprite's /.sprite/llm.txt.
COPY llm.txt /.runner/llm.txt
RUN chmod 0644 /.runner/llm.txt
