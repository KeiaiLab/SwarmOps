# The real-entrance test image: OpenSSH's sshd, nginx and fcgiwrap in front of
# `swarmops serve`, with git 2.43.0 on both sides. It holds no SwarmOps binary: the run mounts
# the build under test at /opt/swarmops/bin, so one image serves every commit. Built once with
# the network (scripts/server-entrance.py build); every run after it is `--network none`.
#
#   client account `dev` ── ssh so@so-host ──▶ sshd ── forced command ──▶ swarmops serve
#                      └── https://so-host ──▶ nginx ── fcgiwrap (as so) ──▶ serve --cgi
#                                                         stores under /srv/so, owner `so`
#
# This file is also the one place that names the digest of every container image the automation
# runs: Dependabot's docker entry reads its FROM lines and moves them, and `just check` fails
# while a workflow or a script names another digest. The four stages below build nothing and
# are in no image; scripts/packages.py reads the first three by stage name, release.yml names the
# fourth's image:
#
#   install-deb, install-rpm   where `smoke` installs the .deb and the .rpm
#   package-build              where the release makes the packages (its Python and liblzma)
#   qemu-binfmt                registers QEMU for the server image's aarch64 half
FROM debian:12@sha256:d74df7598108a173a621eda4d58a2adbf850b2d9a9d81bc2cca1a6c21174a880 AS install-deb
FROM almalinux:9@sha256:245f8aab832fe800aa42bdab5fad75ebbd8e0b4c9f24820a83c638b177857ae2 AS install-rpm
FROM python:3.12-slim-bookworm@sha256:935c3c5b12357a7e10caa9c1e6ca5692344d97cf229832ef584be2cec9fc03df AS package-build
FROM tonistiigi/binfmt:qemu-v9.2.2@sha256:1b804311fe87047a4c96d38b4b3ef6f62fca8cd125265917a9e3dc3c996c39e6 AS qemu-binfmt

# The base is ubuntu:24.04 by digest, the one build.yml's release build and scheduled.yml's git
# matrix run in: when Dependabot moves it here, move it there in the same pull request. Ubuntu
# 24.04's git is 2.43.0, which the version pin below holds and the run checks.
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
