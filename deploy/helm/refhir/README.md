# ReFHIR Helm chart

```bash
helm install refhir ./deploy/helm/refhir \
  --set image.repository=ghcr.io/hai-hq/refhir \
  --set image.tag=latest \
  --set fhir.version=r4 \
  --set conformance.packages[0]=hl7.fhir.us.core@6.1.0
```

Production notes (Postgres URL, SMART secrets, export storage): [../production.md](../../production.md).

Set `server.publicUrl` when SMART is enabled behind an ingress. Put credentials in `secretEnv` (creates a Secret and wires env vars).

## Several processes on one database

With `cluster.enabled=true` the chart runs a writers Deployment and a replicas Deployment against
one Postgres database instead of a single pod (SQLite cannot be shared between pods, so
`database.kind` must be `postgres`).

```bash
helm install refhir ./deploy/helm/refhir \
  --set database.kind=postgres \
  --set secretEnv.REFHIR_POSTGRES_URL='postgresql://refhir:...@postgres:5432/refhir?sslmode=verify-full&sslrootcert=/etc/refhir-pg-tls/ca.crt' \
  --set database.tlsSecret=postgres-ca \
  --set cluster.enabled=true \
  --set cluster.writers=2 \
  --set cluster.replicas=2
```

- **Writers** all start as standbys. One takes the writer lease and serves; the others wait and
  take over within a couple of seconds if it goes away. A rolling update or a lost node pauses
  writes only for that handover.
- **Replicas** serve reads from the same database and send writes, `$export` and
  `ImplementationGuide/$install` to the live writer through the `<release>-primary` Service.
- Readiness is `GET /healthz`: 200 from the live writer and from a replica that is keeping up, 503
  from a standby and from a replica that has fallen behind. The main Service therefore only ever
  routes to pods that can answer. `<release>-primary` only ever has the live writer behind it.
- Clients may send `X-Refhir-Min-Ts` with the `X-Refhir-Commit-Ts` they last saw (the Go and
  TypeScript clients do this on their own) to read their own writes from a replica.
- With `smart.enabled`, set `REFHIR_SMART_DCR_TOKEN` and `REFHIR_SMART_CONFIDENTIAL_SECRET` in
  `secretEnv`. Every pod must use the same values, and a pod refuses to start without them.
- Each pod keeps its own package cache and export directory on an `emptyDir`; export jobs are
  tracked by the writer that accepted them.
