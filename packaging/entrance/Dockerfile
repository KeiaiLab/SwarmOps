# The real-entrance test image: OpenSSH's sshd, nginx and fcgiwrap in front of
# `swarmops serve`, with git 2.43.0 on both sides. It holds no SwarmOps binary: the run mounts
# the build under test at /opt/swarmops/bin, so one image serves every commit. Built once with
# the network (scripts/server-entrance.py build); every run after it is `--network none`.
#
#   client account `dev` ── ssh so@so-host ──▶ sshd ── forced command ──▶ swarmops serve
#                      └── https://so-host ──▶ nginx ── fcgiwrap (as so) ──▶ serve --cgi
#                                                         stores under /srv/so, owner `so`
#
# The base is ubuntu:24.04 by digest, the one scheduled.yml's git matrix pins; Ubuntu 24.04's
# git is 2.43.0, which the version pin below holds and the run checks.
FROM ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac

ARG DEBIAN_FRONTEND=noninteractive

# 1. The entrances, git, and what the run's helpers need (python3 for the race relay, openssl
#    for the TLS certificate and the web passwords, made at start).
RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        ca-certificates fcgiwrap 'git=1:2.43.0-*' nginx openssh-client openssh-server openssl \
        python3 \
    && rm -rf /var/lib/apt/lists/*

# 2. The serving account owns the stores; nginx's workers reach fcgiwrap's socket through its
#    group. The client account runs every git and `so` of the tests.
RUN useradd --create-home --shell /bin/sh so \
    && useradd --create-home --shell /bin/sh dev \
    && usermod --append --groups so www-data \
    && install -d -o so -g so -m 0700 /srv/so \
    && install -d -o so -g so -m 0750 /run/swarmops \
    && ln -s /opt/swarmops/bin/swarmops /usr/local/bin/swarmops \
    && ln -s /opt/swarmops/bin/so /usr/local/bin/so \
    && rm -f /etc/nginx/sites-enabled/default

COPY sshd-swarmops.conf /etc/ssh/sshd_config.d/swarmops.conf
COPY nginx-swarmops.conf /etc/nginx/sites-enabled/swarmops.conf
COPY --chmod=0755 swarmops-serve.cgi /usr/local/libexec/swarmops-serve.cgi
COPY --chmod=0755 start.sh /usr/local/sbin/entrance-start
COPY --chmod=0755 plant-any-key /usr/local/sbin/plant-any-key
COPY --chmod=0755 race-ssh.py /usr/local/bin/race-ssh

CMD ["/usr/local/sbin/entrance-start"]
