{{/*
The chart's name, cut to the 63 characters Kubernetes allows.
*/}}
{{- define "ledger.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 }}
{{- end }}

{{/*
The release's full name, without the chart's name when the release already holds it.
*/}}
{{- define "ledger.fullname" -}}
{{- if hasPrefix .Chart.Name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}

{{/*
The labels every object of the chart carries.
Use ledger.selectorLabels for anything that must match.
*/}}
{{- define "ledger.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{ include "ledger.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}

{{/*
The labels a selector matches.
*/}}
{{- define "ledger.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}
