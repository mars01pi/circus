FROM node:24.18.0-bookworm-slim@sha256:6f7b03f7c2c8e2e784dcf9295400527b9b1270fd37b7e9a7285cf83b6951452d

USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git python3 ripgrep \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/circus/agent
COPY package.json package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts --no-audit --no-fund --cache /tmp/circus-npm-cache \
    && rm -rf /tmp/circus-npm-cache

ENV PATH="/opt/circus/agent/node_modules/.bin:${PATH}" \
    HOME=/workspace/.circus/home \
    CODEX_HOME=/workspace/.circus/home/.codex \
    XDG_CONFIG_HOME=/workspace/.circus/home/.config \
    XDG_CACHE_HOME=/workspace/.circus/home/.cache \
    XDG_DATA_HOME=/workspace/.circus/home/.local/share \
    XDG_STATE_HOME=/workspace/.circus/home/.local/state \
    TMPDIR=/workspace/.circus/tmp \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    NPM_CONFIG_CACHE=/workspace/.circus/home/.cache/npm

COPY --chmod=0555 circus-agent /usr/local/bin/circus-agent
COPY --chmod=0444 verify.py /opt/circus/verify.py
RUN node --version > /opt/circus/versions.txt \
    && npm --version >> /opt/circus/versions.txt \
    && circus-agent codex --version >> /opt/circus/versions.txt \
    && dpkg-query -W ca-certificates git python3 ripgrep >> /opt/circus/versions.txt \
    && mkdir -p /workspace/project \
    && chown -R 65534:65534 /workspace

USER 65534:65534
WORKDIR /workspace/project
ENTRYPOINT ["circus-agent"]
CMD ["codex", "--version"]
