import { createElement } from "react";
import { act, create } from "react-test-renderer";
import { describe, expect, it, vi } from "vitest";

import type { Client } from "../src/client.js";
import type { JsonValue } from "../src/types.js";
import {
  useFHIRQueryWith,
  type DialFn,
  type UseFHIRQueryOptions,
} from "../src/react/useFHIRQuery.js";

function Probe(props: { connect: DialFn; options: UseFHIRQueryOptions }) {
  const state = useFHIRQueryWith(props.connect, props.options);
  return createElement("pre", null, JSON.stringify({ data: state.data, loading: state.loading }));
}

function deferred<T>() {
  let resolve: (value: T) => void = () => {};
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function clientWith(value: JsonValue, close = vi.fn(async () => {})): Client {
  return {
    async watch() {
      return {
        latest: () => value,
        next: () => new Promise<JsonValue>(() => {}),
        close: vi.fn(async () => {}),
      };
    },
    close,
  } as unknown as Client;
}

describe("useFHIRQuery", () => {
  it("closes a client that connects after unmount", async () => {
    const pending = deferred<Client>();
    const close = vi.fn(async () => {});
    const connect: DialFn = () => pending.promise.then(() => clientWith(null, close));

    let renderer: ReturnType<typeof create>;
    await act(async () => {
      renderer = create(
        createElement(Probe, {
          connect,
          options: { resourceType: "Patient" },
        }),
      );
    });
    await act(async () => {
      renderer.unmount();
    });
    await act(async () => {
      pending.resolve(clientWith(null, close));
      await pending.promise;
    });
    expect(close).toHaveBeenCalled();
  });

  it("clears the previous result when the query changes", async () => {
    const second = deferred<Client>();
    const connect = vi
      .fn<DialFn>()
      .mockResolvedValueOnce(clientWith({ n: 1 }))
      .mockImplementationOnce(() => second.promise);

    let renderer: ReturnType<typeof create>;
    await act(async () => {
      renderer = create(
        createElement(Probe, {
          connect,
          options: { resourceType: "Patient", params: { family: "Ann" } },
        }),
      );
    });
    expect(renderer!.toJSON()).toMatchObject({
      children: [expect.stringContaining('"n":1')],
    });

    await act(async () => {
      renderer.update(
        createElement(Probe, {
          connect,
          options: { resourceType: "Patient", params: { family: "Bee" } },
        }),
      );
    });
    expect(renderer!.toJSON()).toMatchObject({
      children: [expect.stringContaining('"data":null')],
    });
    second.resolve(clientWith({ n: 2 }));
  });
});
