#!/bin/sh
# Copies a directory out of the swarmops-dev pod's /archive (fuzz corpus, measurement logs) into the git-ignored
# .devspace/out/. Runs on the Mac from `devspace run copy-back` — a file transfer only, nothing executes here.
#   dev-copy-back.sh <context> <namespace> <archive-subdir>
set -eu
[ $# -eq 3 ] || { echo "usage: $0 <context> <namespace> <archive-subdir, e.g. logs>" >&2; exit 2; }
ctx=$1 ns=$2 sub=$3
case "$ns" in *-dev) ;; *) echo "FATAL: $ns is not a dev namespace" >&2; exit 1 ;; esac
case "$sub" in /*|*..*) echo "FATAL: $sub must be a path below /archive" >&2; exit 1 ;; esac
pod=$(kubectl --context "$ctx" -n "$ns" get pod -l app.kubernetes.io/name=swarmops,app.kubernetes.io/component=dev \
  --field-selector=status.phase=Running -o jsonpath='{.items[0].metadata.name}')
[ -n "$pod" ] || { echo "FATAL: no running dev pod in $ns" >&2; exit 1; }
# a fresh directory per copy: kubectl cp nests into an existing one
out=".devspace/out/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p .devspace/out
kubectl --context "$ctx" -n "$ns" cp -c dev "$pod:/archive/$sub" "$out"
echo "copied $ns/$pod:/archive/$sub -> $out"
