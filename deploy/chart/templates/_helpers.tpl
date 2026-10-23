{{- define "builder.labels" -}}
app.kubernetes.io/name: zeabur-builder
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "builder.selector" -}}
app.kubernetes.io/name: zeabur-builder
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "builder.tag" -}}
{{ .Values.image.tag | default .Chart.AppVersion }}
{{- end }}

{{- define "builder.webhookPath" -}}
{{ .Values.webhooks.path | trimSuffix "/" }}
{{- end }}
