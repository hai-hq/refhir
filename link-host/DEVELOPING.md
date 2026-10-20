# Working on the link host

For whoever changes the host. This page is not part of the published documentation.

```bash
npm ci
npm run typecheck && npm run build
LINK_HOST_TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres npm test
LINK_HOST_TEST_STORE=postgres LINK_HOST_TEST_DATABASE_URL=... npm test   # every test of the host on Postgres
LINK_HOST_TEST_STORE=sqlite npm test                                     # and on SQLite, which needs nothing
```

Without a Postgres the tests that need one are skipped. Everything else runs, on the memory store and on SQLite.
