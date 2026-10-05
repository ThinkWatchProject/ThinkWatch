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

{{/*
The TCP ports a URL reaches, space-separated: the port of each host in it
(comma-separated hosts included), of each `node=` in its query (the seed
nodes of a Redis Cluster or Sentinel) and of a `port=` in its query (which
Postgres honours); `default` for a host written without a port. Never
fails on a URL it cannot read: the default stands.
Usage: {{ include "tw.urlPorts" (dict "url" $url "default" 6379) }}
*/}}
{{- define "tw.urlPorts" -}}
{{- $rest := regexReplaceAll "^[A-Za-z][A-Za-z0-9+.-]*://" .url "" -}}
{{- $hosts := regexReplaceAll "^.*@" (regexFind "^[^/?#]*" $rest) "" -}}
{{- $query := "" -}}
{{- if contains "?" $rest -}}
{{- $query = regexReplaceAll "#.*$" (regexReplaceAll "^[^?]*[?]" $rest "") "" -}}
{{- end -}}
{{- $ports := list -}}
{{- range $host := splitList "," $hosts -}}
{{- $port := trimPrefix ":" (regexFind ":[0-9]+$" (regexReplaceAll "\\[[^]]*\\]" $host "")) -}}
{{- $ports = append $ports (default (toString $.default) $port) -}}
{{- end -}}
{{- range $param := splitList "&" $query -}}
{{- if hasPrefix "node=" $param -}}
{{- $node := trimPrefix "node=" $param | replace "%3A" ":" | replace "%3a" ":" -}}
{{- $port := trimPrefix ":" (regexFind ":[0-9]+$" (regexReplaceAll "\\[[^]]*\\]" $node "")) -}}
{{- if $port -}}{{- $ports = append $ports $port -}}{{- end -}}
{{- else if hasPrefix "port=" $param -}}
{{- $port := regexFind "^[0-9]+$" (trimPrefix "port=" $param) -}}
{{- if $port -}}{{- $ports = append $ports $port -}}{{- end -}}
{{- end -}}
{{- end -}}
{{- $ports | uniq | join " " -}}
{{- end -}}

{{/*
The ports the server reaches each database on, for the network policy:
the bundled service's, or those of its externalUrl, with the client's
default for the scheme where the URL names none (Postgres 5432; Redis
6379, a Sentinel 26379 and 6379 for its primary; ClickHouse 80, for the
http:// URL the server requires — an https:// one, which it refuses,
would mean 443).
*/}}
{{- define "tw.postgres.ports" -}}
{{- if .Values.postgres.bundled -}}5432
{{- else -}}{{ include "tw.urlPorts" (dict "url" .Values.postgres.externalUrl "default" 5432) }}
{{- end -}}
{{- end -}}

{{- define "tw.redis.ports" -}}
{{- if .Values.redis.bundled -}}6379
{{- else -}}
{{- $url := .Values.redis.externalUrl -}}
{{- $sentinel := or (regexMatch "^[A-Za-z]+-sentinel://" $url) (contains "sentinelServiceName=" $url) -}}
{{- include "tw.urlPorts" (dict "url" $url "default" (ternary 26379 6379 $sentinel)) -}}
{{- /* The primary a Sentinel points to: the URL can't say where. */ -}}
{{- if $sentinel }} 6379{{ end -}}
{{- end -}}
{{- end -}}

{{- define "tw.clickhouse.ports" -}}
{{- if .Values.clickhouse.bundled -}}8123
{{- else -}}
{{- $url := .Values.clickhouse.externalUrl -}}
{{- include "tw.urlPorts" (dict "url" $url "default" (ternary 443 80 (hasPrefix "https://" (lower $url)))) -}}
{{- end -}}
{{- end -}}
