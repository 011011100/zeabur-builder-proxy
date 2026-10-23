#!/usr/bin/env bash
# Lints the chart and checks that webhooks stay off unless enabled.
set -euo pipefail

chart="$(dirname "$0")/../deploy/chart"
base=(--set defaultRegistry=registry.example:5000)

helm lint --strict "$chart" "${base[@]}"
helm lint --strict "$chart" "${base[@]}" --set webhooks.enabled=true \
  --set webhooks.ingress.enabled=true --set webhooks.ingress.host=hooks.example.com
helm lint --strict "$chart" "${base[@]}" --set webhooks.enabled=true \
  --set webhooks.httpRoute.enabled=true --set 'webhooks.httpRoute.parentRefs[0].name=gateway'

off=$(helm template builder "$chart" "${base[@]}" --set webhooks.ingress.enabled=true)
if grep -qE 'kind: (Service|Ingress|HTTPRoute)$|--webhook-addr' <<<"$off"; then
  echo "webhooks are exposed although webhooks.enabled is false" >&2
  exit 1
fi

on=$(helm template builder "$chart" "${base[@]}" --set webhooks.enabled=true \
  --set webhooks.path=/git/ --set webhooks.ingress.enabled=true \
  --set webhooks.ingress.host=hooks.example.com)
for want in 'kind: Ingress' '--webhook-path=/git$' 'path: /git/$' 'host: "hooks.example.com"'; do
  if ! grep -qE -- "$want" <<<"$on"; then
    echo "the enabled chart lacks $want" >&2
    exit 1
  fi
done
echo "chart ok"
