# The server image: Debian 12, the release's `swarmops` and `swarmops-server` packages and the
# entrances the server package recommends, run as the serving account. Built by
# scripts/server-image.py from a context that holds this file and the packages, nothing else:
# no COPY or ADD, so no file of the image comes from this repository directly.
#
#   fetch   Debian's packages the two need (the network, once; a layer the image leaves out)
#   image   the base + those packages + the two release packages, installed offline
#
# One build per architecture (`TARGETARCH` is dpkg's name too: amd64, arm64); the script makes
# one image index of both. Dates are SOURCE_DATE_EPOCH, the packages' own, so the same packages
# give the same image, byte for byte. The base is debian:12 by digest, the one packages.py
# installs the .deb in.
FROM debian:12@sha256:d74df7598108a173a621eda4d58a2adbf850b2d9a9d81bc2cca1a6c21174a880 AS fetch

ARG TARGETARCH

# Every dependency the two packages need, downloaded only; the release packages themselves are
# local, so they are dropped from the cache.
RUN --mount=type=bind,target=/packages \
    apt-get update \
    && apt-get install --yes --download-only --no-install-recommends \
        /packages/swarmops_*_"$TARGETARCH".deb /packages/swarmops-server_*_"$TARGETARCH".deb \
        fcgiwrap nginx openssh-server \
    && rm -f /var/cache/apt/archives/swarmops*.deb

FROM debian:12@sha256:d74df7598108a173a621eda4d58a2adbf850b2d9a9d81bc2cca1a6c21174a880 AS image

ARG TARGETARCH
ARG SOURCE_DATE_EPOCH

# 1. The serving account at the fixed uid and gid the chart runs as, before the server package's
#    post-install script, which keeps an account that exists.
# 2. The packages, without the network: Debian's from the fetch stage, the release's from the
#    context. Both are mounted, never copied, so neither lands in a layer.
# 3. What a build would make differ, or a container must not inherit: ssh host keys (the
#    settings name the installation's own), the machine id, logs and caches. Then no setuid or
#    setgid file stays: nothing in the image runs as root.
RUN --network=none \
    --mount=type=bind,target=/packages \
    --mount=type=bind,from=fetch,source=/var/cache/apt/archives,target=/dependencies \
    set -eu \
    && groupadd --gid 10022 so \
    && useradd --uid 10022 --gid 10022 --home-dir /var/lib/swarmops-server --no-create-home \
        --shell /bin/sh --comment "SwarmOps serving account" so \
    && apt-get install --yes --no-install-recommends /dependencies/*.deb \
        /packages/swarmops_*_"$TARGETARCH".deb /packages/swarmops-server_*_"$TARGETARCH".deb \
    && rm -f /etc/machine-id /etc/ssh/ssh_host_* /var/cache/apt/*.bin /var/cache/debconf/*-old \
        /var/lib/dpkg/*-old /var/cache/ldconfig/aux-cache /var/log/dpkg.log \
        /var/log/alternatives.log \
    && rm -rf /var/log/apt \
    && find / -xdev -type f -perm /6000 -exec chmod ug-s {} +

# The kit's container roles (`so-kit run <role>`): `all` serves ssh on 2222 and HTTPS on 8443.
USER 10022:10022
EXPOSE 2222 8443
ENTRYPOINT ["/usr/sbin/so-kit"]
CMD ["run", "all"]
