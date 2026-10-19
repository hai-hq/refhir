# syntax=docker/dockerfile:1

FROM node:26-alpine AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY tsconfig.json tsconfig.build.json ./
COPY src ./src
RUN npm run build

FROM node:26-alpine
ENV NODE_ENV=production
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --omit=dev && npm cache clean --force
COPY --from=build /app/dist ./dist
COPY migrations ./migrations
# With a Postgres the host keeps nothing here. With DATABASE_URL=sqlite:///data/links.db the links are the
# file in /data: mount a volume there to keep them across restarts.
RUN mkdir -p /data && chown node:node /data
USER node
ENV PORT=8090 HOST=0.0.0.0
VOLUME ["/data"]
EXPOSE 8090
# Ready, which is more than up: a host that cannot write to its database, or that is going away, says no.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["node", "dist/healthcheck.js"]
STOPSIGNAL SIGTERM
# DATABASE_URL, ADMIN_TOKEN (or CLIENTS_FILE) and PUBLIC_URL are required: see the README for these and
# the rest. The schema is made on first start.
ENTRYPOINT ["node", "dist/server.js"]
