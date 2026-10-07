import type { JsonValue } from "./types.js";

export type QueryModification =
  | { type: "Add"; queryId: number; udfPath: string; args: JsonValue }
  | { type: "Remove"; queryId: number };

export type StateModification =
  | { type: "QueryUpdated"; queryId: number; value: JsonValue; logLines?: unknown[] }
  | { type: "QueryFailed"; queryId: number; error: string; logLines?: unknown[] }
  | { type: "QueryRemoved"; queryId: number };

export function modifyQuerySet(
  baseVersion: number,
  newVersion: number,
  modifications: QueryModification[],
): JsonValue {
  return {
    type: "ModifyQuerySet",
    baseVersion,
    newVersion,
    modifications,
  };
}

export function mutationMessage(
  requestId: number,
  udfPath: string,
  args: JsonValue,
): JsonValue {
  return {
    type: "Mutation",
    requestId,
    udfPath,
    args,
  };
}
