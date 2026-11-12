# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Tests the server's Helm chart in packaging/helm/swarmops-server/, offline, on rendered objects.

    python3 -I scripts/test-server-chart.py [--values <file>] [--kit <so-kit>]

Each case renders the chart with `helm template`, reads the objects as JSON through
`kubectl patch --local` (no cluster, no network) and checks one rule of the chart's contract:
one pod with one volume, images by digest only, the restricted Pod Security level, no secret
rendered, secrets mounted 0400 and read-only, the settings file in the kit's format (checked by
`so-kit` itself), the test pod's grants, the two modes' secrets kept apart, strict wiring, the
values schema, the stop grace period and the listed egress. The self-checks plant a violation
in rendered objects and show that its rule finds it.

`--values` runs every case on that value set instead of the chart's defaults (the hosted
configuration's test passes its file); `--kit` names the `so-kit` that reads the settings file
(default: the kit's own, packaging/server/bin/so-kit). A missing `helm`, `kubectl`, `sh` or
`ssh-keygen` fails the run and names the tool; nothing is skipped. Prints a summary; exit 1 on a
failure, 2 on a usage error.
"""

import argparse
import copy
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Callable
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CHART = ROOT / "packaging" / "helm" / "swarmops-server"
STRICT_EXAMPLE = CHART / "ci" / "strict-values.yaml"
KIT = ROOT / "packaging" / "server" / "bin" / "so-kit"
TOOLS = ("helm", "kubectl", "sh", "ssh-keygen")

RELEASE = "so"
NAMESPACE = "so-test"
HOSTED_NAME = "swarmops.cc"

# The two listeners inside the pod and the Service ports in front of them.
CONTAINER_PORTS = {2222, 8443}
SERVICE_PORTS = {22: 2222, 443: 8443}
DNS_PORT = 53
SECRET_MODE = 0o400
READ_WRITE_ONCE = ["ReadWriteOnce"]
DIGEST_IMAGE = re.compile(r"^[a-z0-9.-]+(?::[0-9]+)?(?:/[a-z0-9._-]+)+@sha256:[0-9a-f]{64}$")
PLANTED_EGRESS = [{"cidr": "192.0.2.0/24", "port": 443}, {"cidr": "198.51.100.7/32", "port": 9000}]

WORKLOAD_KINDS = ("StatefulSet", "Deployment", "DaemonSet", "ReplicaSet", "Job", "CronJob", "Pod")
SERVING_SECRETS = ("hostKeys", "access", "backup")
REHEARSAL_SECRETS = ("rehearsalKey", "rehearsalBucket")


class Run:
    """The value set every case renders with, and the kit that reads the settings file."""

    def __init__(self, values: Path | None, kit: Path, scratch: Path) -> None:
        self.values = values
        self.kit = kit
        self.scratch = scratch
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(scratch),
            "HELM_CACHE_HOME": str(scratch / "helm-cache"),
            "HELM_CONFIG_HOME": str(scratch / "helm-config"),
            "HELM_DATA_HOME": str(scratch / "helm-data"),
            "KUBECONFIG": str(scratch / "no-kubeconfig"),
            "LC_ALL": "C",
        }

    def base(self) -> list[str]:
        """The value-file arguments of the run's value set."""
        return ["-f", str(self.values)] if self.values else []

    def strict(self) -> list[str]:
        """A value set with `wiring.strict: true`: the run's own if given, else the example."""
        return self.base() or ["-f", str(STRICT_EXAMPLE)]

    def helm(self, *arguments: str) -> subprocess.CompletedProcess:
        return subprocess.run(["helm", *arguments], capture_output=True, text=True,
                              env=self.env, check=False)

    def render(self, *arguments: str, chart: Path = CHART) -> subprocess.CompletedProcess:
        """`helm template` of `chart` with the given value arguments."""
        return self.helm("template", RELEASE, str(chart), "--namespace", NAMESPACE, *arguments)

    def objects(self, *arguments: str) -> list[dict]:
        """The rendered objects as JSON; a render that fails fails the case."""
        done = self.render(*arguments)
        assert done.returncode == 0, f"render failed: {done.stderr.strip()}"
        return self.as_json(done.stdout)

    def as_json(self, text: str) -> list[dict]:
        """Rendered YAML as JSON objects, read by `kubectl patch --local` with no cluster."""
        if not text.strip():
            return []
        rendered = self.scratch / "rendered.yaml"
        rendered.write_text(text)
        done = subprocess.run(["kubectl", "patch", "--local", "-f", str(rendered), "--type",
                               "merge", "-p", "{}", "-o", "json"],
                              capture_output=True, text=True, env=self.env, check=False)
        assert done.returncode == 0, f"kubectl cannot read the render: {done.stderr.strip()}"

        # kubectl prints one JSON document per object, one after the other.
        decoder = json.JSONDecoder()
        found, at = [], 0
        while at < len(done.stdout):
            if done.stdout[at].isspace():
                at += 1
                continue
            item, at = decoder.raw_decode(done.stdout, at)
            found.extend(item.get("items", [item]) if item.get("kind") == "List" else [item])
        return found

    def values_of(self, *arguments: str) -> dict:
        """The merged values (chart defaults, then `arguments`), read by helm itself."""
        reader = self.scratch / "values-reader"
        shutil.rmtree(reader, ignore_errors=True)
        (reader / "templates").mkdir(parents=True)
        (reader / "Chart.yaml").write_text("apiVersion: v2\nname: values-reader\nversion: 0.0.0\n")
        shutil.copy(CHART / "values.yaml", reader / "values.yaml")
        (reader / "templates" / "values.yaml").write_text("{{ toJson .Values }}\n")
        done = self.render(*arguments, chart=reader)
        assert done.returncode == 0, f"values unreadable: {done.stderr.strip()}"
        body = [line for line in done.stdout.splitlines()
                if line and not line.startswith(("#", "---"))]
        return json.loads("\n".join(body))


# Readers of rendered objects.

def of_kind(objects: list[dict], kind: str) -> list[dict]:
    return [item for item in objects if item.get("kind") == kind]


def label(item: dict) -> str:
    return f"{item.get('kind')}/{item.get('metadata', {}).get('name')}"


def pod_specs(objects: list[dict]) -> list[tuple[dict, dict]]:
    """(workload, pod spec) for every object that runs a pod."""
    found = []
    for item in objects:
        spec = item.get("spec", {})
        match item.get("kind"):
            case "Pod":
                found.append((item, spec))
            case "CronJob":
                found.append((item, spec.get("jobTemplate", {}).get("spec", {})
                              .get("template", {}).get("spec", {})))
            case kind if kind in WORKLOAD_KINDS:
                found.append((item, spec.get("template", {}).get("spec", {})))
    return found


def containers(pod: dict) -> list[dict]:
    return [*pod.get("initContainers", []), *pod.get("containers", []),
            *pod.get("ephemeralContainers", [])]


def secret_volumes(pod: dict) -> dict[str, list[tuple[str, dict]]]:
    """Volume name → (secret name, the source that names it) for every volume holding a secret."""
    found: dict[str, list[tuple[str, dict]]] = {}
    for volume in pod.get("volumes", []):
        if "secret" in volume:
            found.setdefault(volume["name"], []).append((volume["secret"].get("secretName", ""),
                                                         volume["secret"]))
        for source in volume.get("projected", {}).get("sources", []):
            if "secret" in source:
                found.setdefault(volume["name"], []).append((source["secret"].get("name", ""),
                                                             volume["projected"]))
    return found


def secret_names(pod: dict) -> set[str]:
    """Every secret a pod spec names: volumes, environment, pull secrets aside."""
    names = {name for sources in secret_volumes(pod).values() for name, _ in sources}
    for container in containers(pod):
        for variable in container.get("env", []):
            reference = variable.get("valueFrom", {}).get("secretKeyRef")
            if reference:
                names.add(reference.get("name", ""))
        for source in container.get("envFrom", []):
            if "secretRef" in source:
                names.add(source["secretRef"].get("name", ""))
    return names


# The rules: each returns its findings, empty when the objects keep it.

def shape_problems(objects: list[dict], mode: str = "serve") -> list[str]:
    """The objects a mode needs: serve a StatefulSet, a Service and the settings; rehearsal a
    CronJob and the settings."""
    needed = {"serve": ("StatefulSet", "Service", "ConfigMap"),
              "rehearsal": ("CronJob", "ConfigMap")}[mode]
    return [f"no {kind} rendered" for kind in needed if not of_kind(objects, kind)]


def replica_problems(objects: list[dict]) -> list[str]:
    problems = [f"{label(item)} is a replicated workload" for item in objects
                if item.get("kind") in ("Deployment", "DaemonSet", "ReplicaSet")]
    sets = of_kind(objects, "StatefulSet")
    if len(sets) > 1:
        problems.append(f"{len(sets)} StatefulSets")
    problems += [f"{label(item)} has replicas {item['spec'].get('replicas')}" for item in sets
                 if item["spec"].get("replicas") != 1]
    return problems


def claim_problems(objects: list[dict]) -> list[str]:
    claims = [(label(item), claim.get("spec", {})) for item in of_kind(objects, "StatefulSet")
              for claim in item["spec"].get("volumeClaimTemplates", [])]
    claims += [(label(item), item.get("spec", {}))
               for item in of_kind(objects, "PersistentVolumeClaim")]
    for workload, pod in pod_specs(objects):
        claims += [(label(workload), volume["ephemeral"].get("volumeClaimTemplate", {})
                    .get("spec", {})) for volume in pod.get("volumes", []) if "ephemeral" in volume]

    problems = [f"{len(claims)} volume claims"] if len(claims) > 1 else []
    problems += [f"{owner} claims {spec.get('accessModes')}" for owner, spec in claims
                 if spec.get("accessModes") != READ_WRITE_ONCE]
    return problems


def image_problems(objects: list[dict]) -> list[str]:
    images = {(label(workload), container["name"]): container.get("image", "")
              for workload, pod in pod_specs(objects) for container in containers(pod)}
    problems = [f"{owner}/{name}: image {image!r} is not by digest"
                for (owner, name), image in images.items() if not DIGEST_IMAGE.match(image)]
    if len(set(images.values())) > 1:
        problems.append(f"containers run different images: {sorted(set(images.values()))}")
    return problems


def restricted_problems(objects: list[dict]) -> list[str]:
    """The restricted Pod Security level, container by container, the pod's settings counted."""
    problems = []
    for workload, pod in pod_specs(objects):
        owner = label(workload)
        pod_context = pod.get("securityContext", {})
        for field in ("hostNetwork", "hostPID", "hostIPC"):
            if pod.get(field):
                problems.append(f"{owner}: {field}")
        problems += [f"{owner}: hostPath volume {volume['name']}"
                     for volume in pod.get("volumes", []) if "hostPath" in volume]

        for container in containers(pod):
            where = f"{owner}/{container['name']}"
            context = container.get("securityContext", {})

            def setting(key: str, context: dict = context) -> object:
                return context.get(key, pod_context.get(key))

            if setting("runAsNonRoot") is not True:
                problems.append(f"{where}: runAsNonRoot not set")
            if setting("runAsUser") == 0 or setting("runAsGroup") == 0:
                problems.append(f"{where}: runs as root")
            if context.get("allowPrivilegeEscalation") is not False:
                problems.append(f"{where}: privilege escalation allowed")
            if context.get("privileged"):
                problems.append(f"{where}: privileged")
            capabilities = context.get("capabilities", {})
            if "ALL" not in capabilities.get("drop", []) or capabilities.get("add"):
                problems.append(f"{where}: capabilities not all dropped")
            if context.get("readOnlyRootFilesystem") is not True:
                problems.append(f"{where}: writable root filesystem")
            profile = setting("seccompProfile") or {}
            if profile.get("type") not in ("RuntimeDefault", "Localhost"):
                problems.append(f"{where}: no seccomp profile")
    return problems


def port_problems(objects: list[dict]) -> list[str]:
    problems = []
    for workload, pod in pod_specs(objects):
        for container in containers(pod):
            for port in container.get("ports", []):
                if port.get("containerPort") not in CONTAINER_PORTS or port.get("hostPort"):
                    problems.append(f"{label(workload)}/{container['name']}: port {port}")
    for service in of_kind(objects, "Service"):
        for port in service["spec"].get("ports", []):
            if SERVICE_PORTS.get(port.get("port")) != port.get("targetPort"):
                problems.append(f"{label(service)}: port {port}")
    return problems


def secret_object_problems(objects: list[dict]) -> list[str]:
    return [f"{label(item)} rendered" for item in of_kind(objects, "Secret")]


def secret_mount_problems(objects: list[dict]) -> list[str]:
    problems = []
    for workload, pod in pod_specs(objects):
        owner = label(workload)
        volumes = secret_volumes(pod)
        for volume, sources in volumes.items():
            for _, source in sources:
                modes = [source.get("defaultMode")] + [item.get("mode", SECRET_MODE)
                                                       for item in source.get("items", [])]
                if any(mode != SECRET_MODE for mode in modes):
                    problems.append(f"{owner}: volume {volume} has mode {modes}")
        for container in containers(pod):
            where = f"{owner}/{container['name']}"
            problems += [f"{where}: {mount['mountPath']} is writable"
                         for mount in container.get("volumeMounts", [])
                         if mount["name"] in volumes and mount.get("readOnly") is not True]
            if any("secretKeyRef" in variable.get("valueFrom", {})
                   for variable in container.get("env", [])) or any(
                       "secretRef" in source for source in container.get("envFrom", [])):
                problems.append(f"{where}: a secret in the environment")
    return problems


def grant_problems(objects: list[dict], verbs: set[str]) -> list[str]:
    """Grants stay in the release's namespace; every rule is `verbs` on pods, delete only on
    named pods."""
    problems = [f"{label(item)} rendered" for item in objects
                if item.get("kind") in ("ClusterRole", "ClusterRoleBinding")]
    for item in [*of_kind(objects, "Role"), *of_kind(objects, "RoleBinding")]:
        if item["metadata"].get("namespace") != NAMESPACE:
            problems.append(f"{label(item)} is outside the release's namespace")
    for binding in of_kind(objects, "RoleBinding"):
        if binding.get("roleRef", {}).get("kind") != "Role":
            problems.append(f"{label(binding)} binds a {binding.get('roleRef', {}).get('kind')}")
        problems += [f"{label(binding)} binds {subject}" for subject in binding.get("subjects", [])
                     if subject.get("kind") != "ServiceAccount"
                     or subject.get("namespace") != NAMESPACE]
    for role in of_kind(objects, "Role"):
        for rule in role.get("rules", []):
            granted = set(rule.get("verbs", []))
            if (rule.get("apiGroups") != [""] or rule.get("resources") != ["pods"]
                    or not granted <= verbs
                    or ("delete" in granted and not rule.get("resourceNames"))):
                problems.append(f"{label(role)} grants {rule}")
    return problems


def egress_problems(objects: list[dict], listed: list[dict]) -> list[str]:
    """Every egress rule is the cluster's DNS or one listed (CIDR, port) pair."""
    allowed = {(pair["cidr"], pair["port"]) for pair in listed}
    problems = []
    for policy in of_kind(objects, "NetworkPolicy"):
        for rule in policy["spec"].get("egress", []):
            ports = rule.get("ports", [])
            peers = rule.get("to", [])
            if peers and ports and all(port.get("port") == DNS_PORT for port in ports) and all(
                    "ipBlock" not in peer for peer in peers):
                continue
            pairs = {(peer.get("ipBlock", {}).get("cidr"), port.get("port"))
                     for peer in peers for port in ports}
            if not peers or not ports or not pairs <= allowed or any(
                    set(peer) != {"ipBlock"} or peer["ipBlock"].get("except")
                    for peer in peers):
                problems.append(f"{label(policy)}: egress rule {rule} is not listed")
    return problems


def must_fail(done: subprocess.CompletedProcess, names: str) -> None:
    """A render that had to be refused, naming `names`."""
    assert done.returncode != 0, f"rendered where it must be refused ({names})"
    assert names in done.stderr, f"refused without naming {names}: {done.stderr.strip()}"


# The cases.

def renders_default_and_strict_example(run: Run) -> None:
    """The chart renders with its defaults, the strict example and the run's value set."""
    for arguments in ([], ["-f", str(STRICT_EXAMPLE)], run.base()):
        objects = run.objects(*arguments)
        assert not shape_problems(objects), (arguments, shape_problems(objects))
    strict = run.values_of("-f", str(STRICT_EXAMPLE))
    assert strict["wiring"]["strict"] is True, "the strict example is not strict"
    assert strict["installation"]["hostName"] == "hub.example.invalid", strict["installation"]


def render_is_deterministic(run: Run) -> None:
    """Two renders of one value set are equal byte for byte, in both modes."""
    for arguments in (run.base(), [*run.base(), "--set", "mode=rehearsal"]):
        first, second = run.render(*arguments), run.render(*arguments)
        assert first.returncode == 0, first.stderr
        assert first.stdout == second.stdout, f"renders differ: {arguments}"


def replicas_is_one_and_not_a_value(run: Run) -> None:
    objects = run.objects(*run.base())
    assert len(of_kind(objects, "StatefulSet")) == 1, "not one StatefulSet"
    assert not replica_problems(objects), replica_problems(objects)
    must_fail(run.render(*run.base(), "--set", "replicas=2"), "replicas")


def one_claim_read_write_once(run: Run) -> None:
    for mode in ("serve", "rehearsal"):
        objects = run.objects(*run.base(), "--set", f"mode={mode}")
        assert not claim_problems(objects), (mode, claim_problems(objects))


def image_is_by_digest_everywhere(run: Run) -> None:
    for mode in ("serve", "rehearsal"):
        objects = run.objects(*run.base(), "--set", f"mode={mode}")
        assert not image_problems(objects), (mode, image_problems(objects))
    must_fail(run.render(*run.base(), "--set", "image.digest=latest"), "digest")
    must_fail(run.render(*run.base(), "--set", "image.tag=1.0"), "tag")


def every_container_is_restricted(run: Run) -> None:
    for level in ("smoke", "full"):
        for mode in ("serve", "rehearsal"):
            objects = run.objects(*run.base(), "--set", f"mode={mode}",
                                  "--set", f"acceptance.level={level}")
            assert not restricted_problems(objects), restricted_problems(objects)


def no_plain_http_port(run: Run) -> None:
    for arguments in (run.base(), run.strict()):
        objects = run.objects(*arguments)
        assert not port_problems(objects), port_problems(objects)


def no_secret_object_is_rendered(run: Run) -> None:
    for mode in ("serve", "rehearsal"):
        objects = run.objects(*run.base(), "--set", f"mode={mode}",
                              "--set", "acceptance.level=full")
        assert not secret_object_problems(objects), secret_object_problems(objects)


def secrets_mounted_0400_read_only(run: Run) -> None:
    for mode in ("serve", "rehearsal"):
        objects = run.objects(*run.base(), "--set", f"mode={mode}")
        assert any(secret_volumes(pod) for _, pod in pod_specs(objects)), f"{mode}: no secret"
        assert not secret_mount_problems(objects), secret_mount_problems(objects)


def settings_equal_kit_format(run: Run) -> None:
    """The ConfigMap's kit.conf passes the kit's own settings parser: `so-kit key-line` reads
    the settings first and prints a key line naming the rendered root."""
    assert run.kit.is_file(), f"no so-kit at {run.kit}"
    maps = [item for item in run.objects(*run.base()) if item.get("kind") == "ConfigMap"
            and "kit.conf" in item.get("data", {})]
    assert len(maps) == 1, f"{len(maps)} ConfigMaps hold kit.conf"
    settings = maps[0]["data"]["kit.conf"]
    conf = run.scratch / "kit.conf"
    conf.write_text(settings)

    key = run.scratch / "key"
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)],
                   check=True, capture_output=True)
    done = subprocess.run(["sh", str(run.kit), "key-line", "person/ada", "-"],
                          input=key.with_suffix(".pub").read_text(), capture_output=True,
                          text=True, env={**run.env, "SO_KIT_CONF": str(conf)}, check=False)
    assert done.returncode == 0, f"so-kit refuses the settings: {done.stderr.strip()}"
    root = re.search(r"^root=(.*)$", settings, re.MULTILINE)
    assert root and f"--root {root.group(1)} " in done.stdout, done.stdout


def smoke_level_reads_pods_only(run: Run) -> None:
    objects = run.objects(*run.base(), "--set", "acceptance.level=smoke")
    assert of_kind(objects, "Role"), "no Role for the test pod"
    assert not grant_problems(objects, {"get", "list"}), grant_problems(objects, {"get", "list"})


def full_level_role_is_own_namespace(run: Run) -> None:
    objects = run.objects(*run.base(), "--set", "acceptance.level=full")
    verbs = {"get", "list", "delete"}
    assert not grant_problems(objects, verbs), grant_problems(objects, verbs)
    rules = [rule for role in of_kind(objects, "Role") for rule in role.get("rules", [])]
    assert any("delete" in rule["verbs"] for rule in rules), "full level cannot delete its pod"


def rehearsal_mode_mounts_no_serving_secret(run: Run) -> None:
    values = run.values_of(*run.base())
    serving = {values["secrets"][name] for name in SERVING_SECRETS}
    objects = run.objects(*run.base(), "--set", "mode=rehearsal")
    assert not shape_problems(objects, "rehearsal"), shape_problems(objects, "rehearsal")
    named = {name for _, pod in pod_specs(objects) for name in secret_names(pod)}
    assert not named & serving, f"rehearsal names {sorted(named & serving)}"


def serve_mode_mounts_no_rehearsal_key(run: Run) -> None:
    values = run.values_of(*run.base())
    rehearsal = {values["secrets"][name] for name in REHEARSAL_SECRETS}
    objects = run.objects(*run.base(), "--set", "mode=serve")
    named = {name for workload, pod in pod_specs(objects) if workload["kind"] == "StatefulSet"
             for name in secret_names(pod)}
    assert named, "the StatefulSet mounts no secret"
    assert not named & rehearsal, f"serve names {sorted(named & rehearsal)}"


def strict_refuses_empty_wiring(run: Run) -> None:
    run.objects(*run.strict())
    for key in ("storageClassName", "storageSize"):
        must_fail(run.render(*run.strict(), "--set", f"wiring.{key}="), f"wiring.{key}")
    must_fail(run.render(*run.strict(), "--set-json", "wiring.networkPolicy.egress=[]"),
              "wiring.networkPolicy.egress")


def unknown_key_is_refused(run: Run) -> None:
    for key in ("installation.hostNmae", "wiring.storageClass", "secrets.hostkeys", "imge"):
        must_fail(run.render(*run.base(), "--set", f"{key}=x"), key.rsplit(".", 1)[-1])


def chart_holds_no_host_name(_: Run) -> None:
    named = [str(path.relative_to(ROOT)) for path in (ROOT / "packaging" / "helm").rglob("*")
             if path.is_file() and HOSTED_NAME in path.read_text(errors="replace")]
    assert not named, f"{HOSTED_NAME} in {named}"


def grace_period_covers_the_push_bound(run: Run) -> None:
    values = run.values_of(*run.base())["installation"]
    bound = max(values["stopGraceSeconds"], values["pushBoundSeconds"])
    for workload, pod in pod_specs(run.objects(*run.base())):
        if workload["kind"] == "StatefulSet":
            grace = pod.get("terminationGracePeriodSeconds", 30)
            assert grace >= bound, f"grace {grace} s is below {bound} s"
    must_fail(run.render(*run.base(), "--set", "installation.stopGraceSeconds=60",
                         "--set", "installation.pushBoundSeconds=61"), "stopGraceSeconds")


def egress_is_listed_only(run: Run) -> None:
    values = run.values_of(*run.base())["wiring"]["networkPolicy"]
    objects = run.objects(*run.base())
    assert not egress_problems(objects, values["egress"]), egress_problems(objects,
                                                                           values["egress"])

    planted = run.objects(*run.base(), "--set", "wiring.networkPolicy.enabled=true",
                          "--set-json", f"wiring.networkPolicy.egress={json.dumps(PLANTED_EGRESS)}")
    policies = of_kind(planted, "NetworkPolicy")
    assert policies, "no NetworkPolicy rendered"
    assert not egress_problems(planted, PLANTED_EGRESS), egress_problems(planted, PLANTED_EGRESS)
    rendered = {(peer["ipBlock"]["cidr"], port["port"]) for policy in policies
                for rule in policy["spec"].get("egress", []) for peer in rule.get("to", [])
                for port in rule.get("ports", []) if "ipBlock" in peer}
    assert rendered == {(pair["cidr"], pair["port"]) for pair in PLANTED_EGRESS}, rendered


# Self-checks: each rule finds a violation planted in rendered objects.

def planted(run: Run, change: Callable[[list[dict]], None]) -> list[dict]:
    objects = copy.deepcopy(run.objects(*run.base()))
    change(objects)
    return objects


def first_container(objects: list[dict]) -> dict:
    return containers(of_kind(objects, "StatefulSet")[0]["spec"]["template"]["spec"])[-1]


def planted_tag_image_is_found(run: Run) -> None:
    def change(objects: list[dict]) -> None:
        first_container(objects)["image"] = "registry.example.invalid/swarmops-server:1.0"
    assert image_problems(planted(run, change)), "a tag image passed"


def planted_root_container_is_found(run: Run) -> None:
    def change(objects: list[dict]) -> None:
        first_container(objects).setdefault("securityContext", {})["runAsUser"] = 0
    assert restricted_problems(planted(run, change)), "a root container passed"


def planted_second_replica_is_found(run: Run) -> None:
    def change(objects: list[dict]) -> None:
        of_kind(objects, "StatefulSet")[0]["spec"]["replicas"] = 2
    assert replica_problems(planted(run, change)), "a second replica passed"


def planted_secret_object_is_found(run: Run) -> None:
    def change(objects: list[dict]) -> None:
        objects.append({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "planted"}})
    assert secret_object_problems(planted(run, change)), "a Secret passed"


def planted_open_egress_is_found(run: Run) -> None:
    def change(objects: list[dict]) -> None:
        policy = {"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
                  "metadata": {"name": "planted"}, "spec": {"egress": [{}]}}
        objects.append(policy)
    assert egress_problems(planted(run, change), []), "an open egress rule passed"


def empty_render_fails(run: Run) -> None:
    assert shape_problems(run.as_json("")), "an empty render passed"
    assert shape_problems([], "rehearsal"), "an empty rehearsal render passed"


CASES: tuple[tuple[str, Callable[[Run], None]], ...] = (
    ("renders_default_and_strict_example", renders_default_and_strict_example),
    ("render_is_deterministic", render_is_deterministic),
    ("replicas_is_one_and_not_a_value", replicas_is_one_and_not_a_value),
    ("one_claim_read_write_once", one_claim_read_write_once),
    ("image_is_by_digest_everywhere", image_is_by_digest_everywhere),
    ("every_container_is_restricted", every_container_is_restricted),
    ("no_plain_http_port", no_plain_http_port),
    ("no_secret_object_is_rendered", no_secret_object_is_rendered),
    ("secrets_mounted_0400_read_only", secrets_mounted_0400_read_only),
    ("settings_equal_kit_format", settings_equal_kit_format),
    ("smoke_level_reads_pods_only", smoke_level_reads_pods_only),
    ("full_level_role_is_own_namespace", full_level_role_is_own_namespace),
    ("rehearsal_mode_mounts_no_serving_secret", rehearsal_mode_mounts_no_serving_secret),
    ("serve_mode_mounts_no_rehearsal_key", serve_mode_mounts_no_rehearsal_key),
    ("strict_refuses_empty_wiring", strict_refuses_empty_wiring),
    ("unknown_key_is_refused", unknown_key_is_refused),
    ("chart_holds_no_host_name", chart_holds_no_host_name),
    ("grace_period_covers_the_push_bound", grace_period_covers_the_push_bound),
    ("egress_is_listed_only", egress_is_listed_only),
    ("planted_tag_image_is_found", planted_tag_image_is_found),
    ("planted_root_container_is_found", planted_root_container_is_found),
    ("planted_second_replica_is_found", planted_second_replica_is_found),
    ("planted_secret_object_is_found", planted_secret_object_is_found),
    ("planted_open_egress_is_found", planted_open_egress_is_found),
    ("empty_render_fails", empty_render_fails),
)


def main() -> int:
    parser = argparse.ArgumentParser(description="Tests the server's Helm chart, offline.")
    parser.add_argument("--values", type=Path, help="run every case on this value file")
    parser.add_argument("--kit", type=Path, default=KIT, help="the so-kit that reads kit.conf")
    arguments = parser.parse_args()
    if arguments.values and not arguments.values.is_file():
        parser.error(f"no value file {arguments.values}")

    # A missing tool fails the run: the cases are never skipped.
    missing = [tool for tool in TOOLS if shutil.which(tool) is None]
    if missing:
        print(f"test-server-chart: FAILED: missing tool: {', '.join(missing)}")
        return 1

    failures = []
    for name, case in CASES:
        with tempfile.TemporaryDirectory(prefix="test-server-chart-") as scratch:
            run = Run(arguments.values and arguments.values.resolve(), arguments.kit.resolve(),
                      Path(scratch))
            try:
                case(run)
            except Exception as error:  # a case's crash is its failure, not the run's
                failures.append(f"{name}: {type(error).__name__}: {error}")

    print(f"test-server-chart: {len(CASES) - len(failures)} of {len(CASES)} cases passed")
    for failure in failures:
        print(f"test-server-chart: FAILED {failure}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
