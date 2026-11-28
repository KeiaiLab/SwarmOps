{{/* The dev namespace boundary, the same three refusals as kitbash.devNamespaceGuard. A devspace or helm flag can
     be overridden by a person; a helm `fail` cannot, so the chart carries it.
       · dev.namespace set but not ending in -dev → fail
       · dev.namespace set and different from the release namespace → fail (dev values never land elsewhere)
       · dev.namespace empty while the release namespace ends in -dev → fail (production values never land in dev) */}}
{{- define "swarmops.devSuffix" -}}-dev{{- end -}}

{{- define "swarmops.devNamespaceGuard" -}}
{{- $dev := .Values.dev.namespace -}}
{{- if $dev -}}
  {{- if not (hasSuffix (include "swarmops.devSuffix" .) $dev) -}}
    {{- fail (printf "dev.namespace %q must end in %s" $dev (include "swarmops.devSuffix" .)) -}}
  {{- end -}}
  {{- if ne .Release.Namespace $dev -}}
    {{- fail (printf "the dev overlay renders only into %s (release namespace is %s)" $dev .Release.Namespace) -}}
  {{- end -}}
{{- else if hasSuffix (include "swarmops.devSuffix" .) .Release.Namespace -}}
  {{- fail (printf "production values cannot render into %s — a dev namespace takes values-dev.yaml only" .Release.Namespace) -}}
{{- end -}}
{{- end -}}

{{- define "swarmops.dev.labels" -}}
app.kubernetes.io/name: swarmops
app.kubernetes.io/component: dev
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
{{- end -}}

{{- define "swarmops.dev.selector" -}}
app.kubernetes.io/name: swarmops
app.kubernetes.io/component: dev
{{- end -}}
