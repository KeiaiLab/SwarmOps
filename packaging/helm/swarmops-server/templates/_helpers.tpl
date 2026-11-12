{{/*
Shared pieces of the templates. Every object is a pure function of the values: no lookup, no
random value, no generated key.
*/}}

{{/* The serving account's numeric uid and gid, fixed in the image. */}}
{{- define "so.uid" -}}10022{{- end -}}

{{/* Paths inside the pod; kit.conf names the ones the kit reads. */}}
{{- define "so.etc" -}}/etc/swarmops-server{{- end -}}

{{/* The listeners inside the pod: kit.conf places them, the containers and Service name them. */}}
{{- define "so.sshPort" -}}2222{{- end -}}
{{- define "so.httpsPort" -}}8443{{- end -}}

{{/* The kit's run directory: rendered configurations, pid files, the CGI socket, the page. */}}
{{- define "so.run" -}}/run/swarmops-server{{- end -}}

{{- define "so.labels" -}}
app.kubernetes.io/name: swarmops-server
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{/* The selector of one component: server, rehearsal or accept. */}}
{{- define "so.selector" -}}
app.kubernetes.io/name: swarmops-server
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{/* The one image, by digest. */}}
{{- define "so.image" -}}
{{ .Values.image.repository }}@{{ .Values.image.digest }}
{{- end -}}

{{/*
Under wiring.strict an empty wiring value fails the render and names the key, so that an
unsubstituted variable never becomes the cluster's default. The stop grace period must cover a
push at the bound.
*/}}
{{- define "so.checks" -}}
{{- $w := .Values.wiring -}}
{{- if $w.strict -}}
{{- range $key, $value := dict "wiring.storageClassName" $w.storageClassName "wiring.storageSize" $w.storageSize "wiring.service.type" $w.service.type -}}
{{- if not $value -}}{{- fail (printf "%s is empty under wiring.strict" $key) -}}{{- end -}}
{{- end -}}
{{- if not $w.networkPolicy.egress -}}{{- fail "wiring.networkPolicy.egress is empty under wiring.strict" -}}{{- end -}}
{{- end -}}
{{- if lt (int .Values.installation.stopGraceSeconds) (int .Values.installation.pushBoundSeconds) -}}
{{- fail "installation.stopGraceSeconds is below installation.pushBoundSeconds" -}}
{{- end -}}
{{- end -}}

{{/* The restricted Pod Security level, for every container. */}}
{{- define "so.containerSecurity" -}}
securityContext:
  runAsNonRoot: true
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true
  capabilities:
    drop: ["ALL"]
  seccompProfile:
    type: RuntimeDefault
{{- end -}}

{{- define "so.podSecurity" -}}
securityContext:
  runAsNonRoot: true
  runAsUser: {{ include "so.uid" . }}
  runAsGroup: {{ include "so.uid" . }}
  fsGroup: {{ include "so.uid" . }}
  fsGroupChangePolicy: OnRootMismatch
  seccompProfile:
    type: RuntimeDefault
{{- end -}}

{{/* Where a pod may run, and how it pulls; the cluster's wiring. */}}
{{- define "so.placement" -}}
{{- with .Values.wiring.nodeSelector }}
nodeSelector:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .Values.wiring.tolerations }}
tolerations:
  {{- toYaml . | nindent 2 }}
{{- end }}
{{- with .Values.wiring.imagePullSecrets }}
imagePullSecrets:
  {{- range . }}
  - name: {{ . }}
  {{- end }}
{{- end }}
{{- end -}}

{{/* A secret volume, read-only to its owner: mode 0400. */}}
{{- define "so.secretVolume" -}}
- name: {{ .name }}
  secret:
    secretName: {{ .secret }}
    defaultMode: 0400
    {{- with .items }}
    items:
      {{- range $key, $path := . }}
      - key: {{ $key }}
        path: {{ $path }}
      {{- end }}
    {{- end }}
{{- end -}}

{{/* The settings and the two writable scratch directories every container mounts. */}}
{{- define "so.commonMounts" -}}
- name: settings
  mountPath: {{ include "so.etc" . }}/kit.conf
  subPath: kit.conf
  readOnly: true
- name: settings
  mountPath: {{ include "so.etc" . }}/texts
  readOnly: true
- name: run
  mountPath: {{ include "so.run" . }}
- name: tmp
  mountPath: /tmp
{{- end -}}

{{- define "so.commonVolumes" -}}
- name: settings
  configMap:
    name: {{ .Release.Name }}-settings
- name: run
  emptyDir: {}
- name: tmp
  emptyDir: {}
{{- end -}}

{{/* The data volume's two directories. */}}
{{- define "so.dataMounts" -}}
- name: data
  mountPath: /srv/so
  subPath: srv
- name: data
  mountPath: /var/lib/swarmops-server
  subPath: lib
{{- end -}}

{{/* The volume claim's spec: one ReadWriteOnce volume. */}}
{{- define "so.claimSpec" -}}
accessModes: ["ReadWriteOnce"]
{{- with .Values.wiring.storageClassName }}
storageClassName: {{ . }}
{{- end }}
resources:
  requests:
    storage: {{ .Values.wiring.storageSize }}
{{- end -}}
