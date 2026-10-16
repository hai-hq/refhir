---
title: Subscriptions
description: Create rest-hook and websocket Subscriptions, and see what they deliver and when.
---

ReFHIR delivers FHIR R4 `Subscription` resources with a `rest-hook` or a `websocket` channel, for
telling another system that a resource was created or updated.

:::note[Keeping a client current is a different job]
A Subscription reports one matching resource. It does not report that a resource stopped matching
or was deleted. To keep a screen or an agent's context current, use a
[live query](/docs/concepts/vs-subscriptions/) instead.
:::

## A rest-hook Subscription

Post a `Subscription` whose channel names an endpoint:

```bash
curl -s -X POST http://127.0.0.1:8080/Subscription \
  -H 'Content-Type: application/fhir+json' \
  -d '{
    "resourceType": "Subscription",
    "status": "requested",
    "reason": "Tell me about active patients",
    "criteria": "Patient?active=true",
    "channel": {"type": "rest-hook", "endpoint": "http://127.0.0.1:9000/hook"}
  }'
```

These four fields are required, as in FHIR R4: `status`, `reason`, `criteria` and `channel.type`. A
rest-hook channel also needs `endpoint`. Leave one out and the answer is a `400` with
`Subscription.reason requires at least 1` (or the field you missed). The server assigns the id of
what it creates and returns it in `Location`.

`criteria` is a resource type, optionally followed by search parameters:
`Patient`, or `Patient?active=true`. With no parameters every resource of that type matches.

### What your endpoint receives

After a commit that creates or updates a matching resource, ReFHIR sends one `POST` with the whole
resource as the body and `Content-Type: application/fhir+json`. Any `2xx` answer counts as
delivered. A receiver can be this small:

```js
import http from "node:http";

http
  .createServer((req, res) => {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      const resource = JSON.parse(body);
      console.log(`${req.method} ${req.url} ${resource.resourceType}/${resource.id}`);
      res.writeHead(204).end();
    });
  })
  .listen(9000, "127.0.0.1");
```

Create a matching patient, then update it, and the endpoint receives two `POST`s. Create a patient
with `active` set to `false` and it receives nothing, because that patient does not match.

## A websocket Subscription

Set the channel type to `websocket`. No endpoint is needed:

```bash
curl -s -X POST http://127.0.0.1:8080/Subscription \
  -H 'Content-Type: application/fhir+json' \
  -d '{
    "resourceType": "Subscription",
    "status": "requested",
    "reason": "Watch active patients",
    "criteria": "Patient?active=true",
    "channel": {"type": "websocket"}
  }'
```

Then connect to `GET /Subscription/<id>/$websocket` with the id from `Location`. It is an ordinary
WebSocket upgrade. The server sends nothing when you connect, and then one text frame per
notification, each holding the resource as JSON. There is no handshake frame and no heartbeat.

```js
import WebSocket from "ws";

const ws = new WebSocket(`ws://127.0.0.1:8080/Subscription/${id}/$websocket`);
ws.on("message", (data) => {
  const resource = JSON.parse(data.toString());
  console.log(`${resource.resourceType}/${resource.id} active=${resource.active}`);
});
```

With SMART on, the route needs a bearer token like any other request. A notification sent while no
socket is connected is not kept.

## What to expect from delivery

- **Only creates and updates are notified.** A delete sends nothing. A resource that stops
  matching sends nothing, and neither does a resource that never matched.
- **The server never changes a Subscription's `status`.** It stays whatever you posted, and a
  failing endpoint does not move it to `error`. Delivery is skipped only when the status is `off`,
  so setting it to `off` is how you pause one.
- **`channel.header`, `channel.payload` and `end` are ignored.** You cannot add an `Authorization`
  header to the request your endpoint receives, the body is always the whole resource, and a
  Subscription does not expire.
- **Retries are short and in memory.** A failed delivery is tried three times in quick succession,
  then again every couple of seconds, up to eight attempts in about ten seconds. After that the
  notification is dropped, with no change of status. Because the queue is in memory, notifications
  waiting for a retry do not survive a restart.
- **No ordering or exactly-once guarantee.** A retry can arrive after a newer notification.
- **The primary delivers.** Subscription delivery and the `$websocket` route live on the primary,
  not on a standby or a replica.
- **R4 only.** The `Subscription` resource is implemented. R5 topic-based subscriptions are not.

To keep a screen or a process in step with the current result, use a live query instead. See
[Live queries and FHIR Subscriptions](/docs/concepts/vs-subscriptions/).
