{{/*
Common labels
*/}}
{{- define "a3s-box.labels" -}}
app.kubernetes.io/name: a3s-box
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "a3s-box.selectorLabels" -}}
app.kubernetes.io/name: a3s-box
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: cri-runtime
{{- end }}

{{/*
Service account name
*/}}
{{- define "a3s-box.serviceAccountName" -}}
{{- .Values.serviceAccount.name | default "a3s-box-cri" }}
{{- end }}
