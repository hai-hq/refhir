# ReFHIR Helm chart

**Upgrade note:** `conformance.extraPackages` was renamed to `conformance.packages` (additional IGs on top of `[fhir].version` defaults).

```bash
helm install refhir ./deploy/helm/refhir \
  --set image.repository=ghcr.io/hai-hq/refhir \
  --set image.tag=latest \
  --set fhir.version=r4 \
  --set conformance.packages[0]=hl7.fhir.us.core@6.1.0
```

Production notes (Postgres URL, SMART secrets, export storage): [../production.md](../production.md).

Set `server.publicUrl` when SMART is enabled behind an ingress. Put credentials in `secretEnv` (creates a Secret and wires env vars).
