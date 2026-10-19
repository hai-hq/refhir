# syntax=docker/dockerfile:1

FROM node:22-alpine
WORKDIR /app
COPY package.json ./
COPY src ./src
# The links are files in /data: mount a volume there to keep them across restarts.
RUN mkdir -p /data && chown node:node /data
USER node
ENV DATA_DIR=/data PORT=8090
VOLUME ["/data"]
EXPOSE 8090
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
  CMD node -e "fetch('http://127.0.0.1:' + (process.env.PORT || 8090) + '/healthz').then((r) => process.exit(r.ok ? 0 : 1), () => process.exit(1))"
# ADMIN_TOKEN is required, and PUBLIC_URL should be the address the host is reached at.
ENTRYPOINT ["node", "src/server.mjs"]
