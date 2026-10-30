{{- define "builder.labels" -}}
app.kubernetes.io/name: zeabur-builder
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- /* Validate the proxy without echoing a possibly credential-bearing invalid value. */ -}}
{{- define "builder.proxy" -}}
{{- $proxy := .Values.proxy -}}
{{- if not (kindIs "string" $proxy.url) -}}
{{- fail "proxy.url must be a string: empty or an unauthenticated http://IPv4:port endpoint" -}}
{{- end -}}
{{- if $proxy.url -}}
{{- if not (regexMatch "^http://([0-9]{1,3}\\.){3}[0-9]{1,3}:[0-9]{1,5}/?$" $proxy.url) -}}
{{- fail "proxy.url must be an unauthenticated http://IPv4:port endpoint without a path, query or fragment" -}}
{{- end -}}
{{- $url := trimSuffix "/" $proxy.url -}}
{{- $host := trimPrefix "http://" $url -}}
{{- $parts := splitList ":" $host -}}
{{- $ip := first $parts -}}
{{- $portText := last $parts -}}
{{- $port := atoi $portText -}}
{{- if or (lt $port 1) (gt $port 65535) (ne (printf "%d" $port) $portText) -}}
{{- fail "proxy.url must use a decimal TCP port from 1 to 65535" -}}
{{- end -}}
{{- range splitList "." $ip -}}
{{- $octet := atoi . -}}
{{- if or (gt $octet 255) (ne (printf "%d" $octet) .) -}}
{{- fail "proxy.url must contain a canonical IPv4 address" -}}
{{- end -}}
{{- end -}}
{{- if or (not (kindIs "string" $proxy.noProxy)) (not $proxy.noProxy) -}}
{{- fail "proxy.noProxy must be a non-empty comma-separated string covering internal cluster addresses" -}}
{{- end -}}
{{- if regexMatch "[[:space:]]" $proxy.noProxy -}}
{{- fail "proxy.noProxy must not contain whitespace" -}}
{{- end -}}
{{- $mandatory := list "localhost" "127.0.0.1" "::1" ".svc" ".svc.cluster.local" "10.43.0.1" "10.42.0.0/16" "10.43.0.0/16" "172.16.0.0/12" -}}
{{- $noProxy := concat (splitList "," $proxy.noProxy) $mandatory | compact | uniq | join "," -}}
{{- dict "url" $url "ip" $ip "port" $port "noProxy" $noProxy | toJson -}}
{{- else -}}
{}
{{- end -}}
{{- end }}

{{- /* The six standard variables, with identical upper/lower-case values. */ -}}
{{- define "builder.proxyValues" -}}
{{- $proxy := include "builder.proxy" . | fromJson -}}
{{- if $proxy.url -}}
{{- dict "HTTP_PROXY" $proxy.url "http_proxy" $proxy.url "HTTPS_PROXY" $proxy.url "https_proxy" $proxy.url "NO_PROXY" $proxy.noProxy "no_proxy" $proxy.noProxy | toJson -}}
{{- else -}}
{}
{{- end -}}
{{- end }}

{{- define "builder.proxyEnv" -}}
{{- range $name, $value := (include "builder.proxyValues" . | fromJson) }}
- name: {{ $name }}
  value: {{ $value | quote }}
{{- end -}}
{{- end }}

{{- define "builder.selector" -}}
app.kubernetes.io/name: zeabur-builder
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "builder.webhookPath" -}}
{{ .Values.webhooks.path | trimSuffix "/" }}
{{- end }}

{{- /* An image reference: repository@digest, else repository:tag, else repository:appVersion. */ -}}
{{- define "builder.image" -}}
{{- $image := index . 0 -}}
{{- $chart := index . 1 -}}
{{- if $image.digest -}}
{{ $image.repository }}@{{ $image.digest }}
{{- else -}}
{{ $image.repository }}:{{ $image.tag | default $chart.AppVersion }}
{{- end -}}
{{- end }}

{{- /* The registry's CA: registry.ca, else the key of registry.caFrom's Secret, else nothing. */ -}}
{{- define "builder.registryCA" -}}
{{- $registry := .Values.registry -}}
{{- if $registry.ca -}}
{{ $registry.ca }}
{{- else if and $registry.caFrom.namespace $registry.caFrom.secret -}}
{{- $secret := lookup "v1" "Secret" $registry.caFrom.namespace $registry.caFrom.secret -}}
{{- if $secret -}}
{{ index $secret.data $registry.caFrom.key | default "" | b64dec }}
{{- end -}}
{{- end -}}
{{- end }}

{{- /* A list of strings as a CEL list literal: ['a', 'b']. */ -}}
{{- define "builder.celList" -}}
{{- $items := list -}}
{{- range . }}{{ $items = append $items (printf "'%s'" .) }}{{ end -}}
[{{ join ", " $items }}]
{{- end }}

{{- /* The registry token key as namespace/secret. The chart fails without both fields. */ -}}
{{- define "builder.tokenKey" -}}
{{- $key := .Values.registry.tokenKey -}}
{{ required "registry.tokenKey.namespace is required: the Secret whose tls.key signs registry tokens" $key.namespace }}/{{ required "registry.tokenKey.secret is required: the Secret whose tls.key signs registry tokens" $key.secret }}
{{- end }}
