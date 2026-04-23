{{/*
Common labels applied to every resource.
*/}}
{{- define "tw.labels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end -}}

{{/*
Image tag: explicit override else .Chart.AppVersion.
Usage: {{ include "tw.imageTag" (dict "tag" .Values.image.server.tag "ctx" .) }}
*/}}
{{- define "tw.imageTag" -}}
{{- if .tag -}}{{ .tag }}{{- else -}}{{ .ctx.Chart.AppVersion }}{{- end -}}
{{- end -}}

{{/*
Resource names for bundled databases — stable across upgrades.
*/}}
{{- define "tw.postgres.name"   -}}{{ .Release.Name }}-postgres{{- end -}}
{{- define "tw.redis.name"      -}}{{ .Release.Name }}-redis{{- end -}}
{{- define "tw.clickhouse.name" -}}{{ .Release.Name }}-clickhouse{{- end -}}
{{- define "tw.secret.name"     -}}{{ .Release.Name }}-secrets{{- end -}}
