# Disposable CI fixture only: it serves synthetic Git and probe hosts over an HTTP proxy.
FROM docker.io/library/python:3.13-alpine
# Alpine's git-daemon subpackage owns git-http-backend (main/git/APKBUILD daemon()).
RUN apk add --no-cache git git-daemon \
 && test -x "$(git --exec-path)/git-http-backend"
COPY --chmod=0555 mockproxy.py /mockproxy.py
COPY --chmod=0555 railpack /usr/local/bin/railpack
ENV PYTHONDONTWRITEBYTECODE=1
USER 1000:1000
ENTRYPOINT ["python3", "/mockproxy.py"]
