import { useEffect, useState } from "react";

import type { Client, DialOptions } from "../client.js";
import type { Args, JsonValue } from "../types.js";
import type { Watch } from "../watch.js";

type CommonOptions = {
  /** Reuse an existing session instead of opening a new one. */
  client?: Client;
  baseURL?: string;
  token?: string;
  runtime?: "browser" | "node";
  enabled?: boolean;
};

export type UseFHIRQueryOptions = CommonOptions &
  (
    | { resourceType: string; params?: Record<string, string> }
    | { path: string; args?: Args }
  );

export type UseFHIRQueryResult = {
  data: JsonValue | null;
  error: Error | null;
  loading: boolean;
};

export type DialFn = (baseURL: string, options?: DialOptions) => Promise<Client>;

type Attempt = {
  cancelled: boolean;
  watch: Watch | null;
  owned: Client | null;
};

/**
 * Subscribe to a live FHIR query (search or arbitrary watch path).
 * Opens its own `dial()` when `client` is not provided.
 */
export function useFHIRQueryWith(
  connect: DialFn,
  options: UseFHIRQueryOptions,
): UseFHIRQueryResult {
  const enabled = options.enabled ?? true;
  const [data, setData] = useState<JsonValue | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [loading, setLoading] = useState(enabled);

  const watchPath = "path" in options ? options.path : `${options.resourceType}/search`;
  const watchArgs: Args = "path" in options ? (options.args ?? {}) : (options.params ?? {});
  const argsKey = JSON.stringify(watchArgs);
  const shared = options.client;

  useEffect(() => {
    if (!enabled) {
      setLoading(false);
      return;
    }

    const attempt: Attempt = { cancelled: false, watch: null, owned: null };
    const args = JSON.parse(argsKey) as Args;
    setData(null);
    setLoading(true);
    setError(null);

    void (async () => {
      let pendingOwned: Client | null = null;
      try {
        const client =
          shared ??
          (await connect(options.baseURL ?? "http://127.0.0.1:8080", {
            token: options.token,
            runtime: options.runtime,
          }));
        if (!shared) pendingOwned = client;
        if (attempt.cancelled) {
          await pendingOwned?.close();
          return;
        }

        const subscription = await client.watch(watchPath, args);
        attempt.watch = subscription;
        attempt.owned = pendingOwned;
        pendingOwned = null;
        if (attempt.cancelled) {
          await subscription.close();
          await attempt.owned?.close();
          return;
        }

        setData(subscription.latest());
        setLoading(false);

        while (!attempt.cancelled) {
          const next = await subscription.next();
          if (attempt.cancelled) break;
          setData(next);
        }
      } catch (err) {
        await pendingOwned?.close();
        await attempt.watch?.close();
        await attempt.owned?.close();
        if (!attempt.cancelled) {
          setError(err instanceof Error ? err : new Error(String(err)));
          setLoading(false);
        }
      }
    })();

    return () => {
      attempt.cancelled = true;
      const watch = attempt.watch;
      const owned = attempt.owned;
      attempt.watch = null;
      attempt.owned = null;
      void watch?.close();
      void owned?.close();
    };
  }, [
    connect,
    enabled,
    watchPath,
    argsKey,
    shared,
    options.baseURL,
    options.token,
    options.runtime,
  ]);

  return { data, error, loading };
}
