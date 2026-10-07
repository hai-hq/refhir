/** FHIR JSON value from the server (untyped). */
export type JsonValue =
  | string
  | number
  | boolean
  | null
  | JsonValue[]
  | { [key: string]: JsonValue };

export type Args = Record<string, JsonValue>;
