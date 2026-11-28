#!/bin/sh
# The dev chart's namespace guard and the devspace.yaml surface gate, offline (ci.yml job dev-check, `just dev-check`).
# A missing helm fails. Each case must pass or fail as stated; a guard that stops failing turns this red.
set -eu
cd "$(dirname "$0")/.."
command -v helm >/dev/null || { echo "FATAL: helm is not installed"; exit 1; }
chart=packaging/helm/swarmops-dev
dev=$chart/values-dev.yaml
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
export HELM_CACHE_HOME="$tmp/c" HELM_CONFIG_HOME="$tmp/f" HELM_DATA_HOME="$tmp/d"
fail=0
ok()   { if "$@" >"$tmp/out" 2>&1; then echo "ok    $*"; else echo "FAIL  (should pass) $*"; cat "$tmp/out"; fail=1; fi; }
must() { want=$1; shift
         if "$@" >"$tmp/out" 2>&1; then echo "FAIL  (should fail) $*"; fail=1
         elif grep -qF "$want" "$tmp/out"; then echo "ok    refused: $*"
         else echo "FAIL  (wrong refusal, want '$want') $*"; cat "$tmp/out"; fail=1; fi; }
r() { helm template swarmops-dev "$chart" "$@"; }

# the dev render: guard passes, and the objects are the ones the overlay owns
ok r -n swarmops-dev -f "$dev"
kinds=$(r -n swarmops-dev -f "$dev" | awk '/^kind:/ {print $2}' | sort | tr '\n' ' ')
[ "$kinds" = "CiliumNetworkPolicy Deployment ExternalSecret PersistentVolumeClaim PersistentVolumeClaim PersistentVolumeClaim " ] ||
  { echo "FAIL  dev render kinds: $kinds"; fail=1; }
r -n swarmops-dev -f "$dev" | grep -q 'livenessProbe\|readinessProbe\|startupProbe' && { echo "FAIL  dev pod has a probe"; fail=1; }
r -n swarmops-dev -f "$dev" | grep -qF 'claimName: swarmops-dev-target' || { echo "FAIL  target PVC not mounted"; fail=1; }
# values-dev into any other namespace
must "renders only into swarmops-dev" r -n swarmops -f "$dev"
must "renders only into swarmops-dev" r -n default -f "$dev"
must "renders only into swarmops-dev" r -n kitbash-dev -f "$dev"
# production values (no dev.namespace) into a -dev namespace
must "production values cannot render into swarmops-dev" r -n swarmops-dev
must "production values cannot render into other-dev" r -n other-dev
# a dev.namespace that is not a dev namespace
must "must end in -dev" r -n swarmops -f "$dev" --set dev.namespace=swarmops
# production values outside -dev render nothing
[ -z "$(r -n swarmops | grep '^kind:' || true)" ] || { echo "FAIL  production render is not empty"; fail=1; }
ok helm lint "$chart" -f "$dev" --namespace swarmops-dev

# the surface gate: this file passes, and each broken copy is refused
ok sh scripts/gates/devspace_surface.sh devspace.yaml "$dev"
sed 's/^name: swarmops/images: {}\nname: swarmops/' devspace.yaml | awk '{gsub(/\\n/, "\n")} 1' >"$tmp/ds1.yaml"
must "top-level keys" sh scripts/gates/devspace_surface.sh "$tmp/ds1.yaml" "$dev"
sed '/^deployments:/,/^dev:/ s/namespace: \${DEV_NAMESPACE}/namespace: swarmops/' devspace.yaml >"$tmp/ds2.yaml"
must "deployment targets" sh scripts/gates/devspace_surface.sh "$tmp/ds2.yaml" "$dev"
sed '/^dev:/,/^commands:/ s/namespace: \${DEV_NAMESPACE}/namespace: ${NAMESPACE}/' devspace.yaml >"$tmp/ds3.yaml"
must "attach targets" sh scripts/gates/devspace_surface.sh "$tmp/ds3.yaml" "$dev"
sed 's/-n \${DEV_NAMESPACE} -l/-n swarmops -l/' devspace.yaml >"$tmp/ds4.yaml"
must "other than" sh scripts/gates/devspace_surface.sh "$tmp/ds4.yaml" "$dev"
sed 's/"\${DEVSPACE_NAMESPACE}" != "\${DEV_NAMESPACE}"/true/' devspace.yaml >"$tmp/ds5.yaml"
must "does not check" sh scripts/gates/devspace_surface.sh "$tmp/ds5.yaml" "$dev"
printf '  shell:\n    command: "kubectl -n ${DEV_NAMESPACE} exec x"\n' | cat devspace.yaml - >"$tmp/ds6.yaml"
must "kubectl directly" sh scripts/gates/devspace_surface.sh "$tmp/ds6.yaml" "$dev"

[ "$fail" = 0 ] && echo "dev chart and devspace surface: all cases hold" || exit 1
