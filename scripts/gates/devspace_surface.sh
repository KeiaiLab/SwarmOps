#!/bin/sh
# devspace.yaml surface gate (CONTRIBUTING.md) — `just check` and ci.yml job dev-check call it. DevSpace deploys and attaches to the dev namespace
# only; production is never a target. The chart guard (swarmops.devNamespaceGuard) holds the deploy side against
# CLI overrides; attach targets and commands are not helm's business, so this gate is their only check.
#   ① top-level keys equal an allow-list (an unknown surface such as images: or hooks: turns it red)
#   ② NAMESPACE = swarmops, DEV_NAMESPACE = swarmops-dev, and values-dev.yaml's dev.namespace is that same name
#   ③ every deployment target names ${DEV_NAMESPACE} and uses values-dev.yaml
#   ④ every attach target names ${DEV_NAMESPACE}
#   ⑤ ${NAMESPACE} is referenced nowhere; every -n/--namespace flag in commands is ${DEV_NAMESPACE}
#   ⑥ the dev and purge pipelines check the target context and namespace first
set -eu

DS=${1:-devspace.yaml}
DEV_VALUES=${2:-packaging/helm/swarmops-dev/values-dev.yaml}
TOPLEVEL="commands deployments dev name pipelines vars version"

fatal() { echo "FATAL(devspace): $*"; exit 1; }
block() { awk -v k="^$1:" '$0 ~ k {f = 1; next} f && /^[a-zA-Z]/ {f = 0} f' "$DS"; }
targets() { grep -cE '^  [A-Za-z][A-Za-z0-9_.-]*:' || true; }

[ -s "$DS" ] || fatal "$DS is missing"
[ -s "$DEV_VALUES" ] || fatal "$DEV_VALUES is missing"

# ①
keys=$(grep -E '^[a-zA-Z]' "$DS" | cut -d: -f1 | sort -u | tr '\n' ' ' | sed 's/ $//')
[ "$keys" = "$TOPLEVEL" ] || fatal "top-level keys [$keys] != [$TOPLEVEL]"

# ②
ns=$(awk '/^[[:space:]]+NAMESPACE:/ {print $2; exit}' "$DS")
dev=$(awk '/^[[:space:]]+DEV_NAMESPACE:/ {print $2; exit}' "$DS")
[ "$ns" = swarmops ] || fatal "NAMESPACE is [$ns], expected swarmops"
[ "$dev" = swarmops-dev ] || fatal "DEV_NAMESPACE is [$dev], expected swarmops-dev"
vdev=$(awk '/^dev:/ {f = 1; next} f && /^[a-zA-Z]/ {f = 0} f && /^  namespace:/ {print $2; exit}' "$DEV_VALUES")
[ "$vdev" = "$dev" ] || fatal "values-dev.yaml dev.namespace [$vdev] != DEV_NAMESPACE [$dev]"

# ③
dep=$(block deployments)
n=$(printf '%s\n' "$dep" | targets)
n_ns=$(printf '%s\n' "$dep" | grep -c 'namespace:' || true)
n_dev=$(printf '%s\n' "$dep" | grep -cF 'namespace: ${DEV_NAMESPACE}' || true)
n_vals=$(printf '%s\n' "$dep" | grep -cF 'valuesFiles: [./packaging/helm/swarmops-dev/values-dev.yaml]' || true)
{ [ "$n" -gt 0 ] && [ "$n_ns" = "$n" ] && [ "$n_dev" = "$n" ]; } ||
  fatal "$n deployment targets, $n_dev name \${DEV_NAMESPACE} ($n_ns namespace lines) — the rest go to whatever namespace is current"
[ "$n_vals" = "$n" ] || fatal "$n deployment targets, $n_vals use values-dev.yaml only"

# ④
att=$(block dev)
a=$(printf '%s\n' "$att" | targets)
a_ns=$(printf '%s\n' "$att" | grep -c 'namespace:' || true)
a_dev=$(printf '%s\n' "$att" | grep -cF 'namespace: ${DEV_NAMESPACE}' || true)
{ [ "$a" -gt 0 ] && [ "$a_ns" = "$a" ] && [ "$a_dev" = "$a" ]; } ||
  fatal "$a attach targets, $a_dev name \${DEV_NAMESPACE} ($a_ns namespace lines)"

# ⑤
! grep -v '^[[:space:]]*#' "$DS" | grep -qF '${NAMESPACE}' || fatal "\${NAMESPACE} (production) is referenced — even through a variable it reaches swarmops"
bad=$(block commands | grep -oE '(-n|--namespace)[ =][^ ]+' | grep -vF '${DEV_NAMESPACE}' || true)
[ -z "$bad" ] || fatal "commands target a namespace other than \${DEV_NAMESPACE}: $bad"
! block commands | grep -qE '(^|[^-])kubectl ' || fatal "commands call kubectl directly — go through devspace enter / scripts/dev-copy-back.sh"

# ⑥
for p in dev purge; do
  body=$(awk -v k="^  $p:" '$0 ~ k {f = 1; next} f && /^  [a-z]/ {f = 0} f' "$DS")
  printf '%s\n' "$body" | grep -qF '"${DEVSPACE_CONTEXT}" != "${TARGET_CONTEXT}"' &&
    printf '%s\n' "$body" | grep -qF '"${DEVSPACE_NAMESPACE}" != "${DEV_NAMESPACE}"' ||
    fatal "pipeline $p does not check the target context and namespace first"
done

echo "devspace surface OK ($n deployment, $a attach targets in $dev)"
