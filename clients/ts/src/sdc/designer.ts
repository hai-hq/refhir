import type { Client } from "../client.js";
import { doRequest } from "../rest.js";
import type { JsonValue } from "../types.js";
import { assemble } from "./filler.js";

type JsonObject = { [key: string]: JsonValue };

async function write(client: Client, typeName: string, resource: JsonObject): Promise<JsonObject> {
  const id = typeof resource.id === "string" ? resource.id : "";
  const path = id ? `/${typeName}/${id}` : `/${typeName}`;
  const method = id ? "PUT" : "POST";
  const bytes = await doRequest(
    client,
    method,
    path,
    new TextEncoder().encode(JSON.stringify({ resourceType: typeName, ...resource })),
  );
  return JSON.parse(new TextDecoder().decode(bytes)) as JsonObject;
}

export function saveQuestionnaire(client: Client, questionnaire: JsonObject) {
  return write(client, "Questionnaire", questionnaire);
}

export function saveValueSet(client: Client, valueSet: JsonObject) {
  return write(client, "ValueSet", valueSet);
}

export function saveCodeSystem(client: Client, codeSystem: JsonObject) {
  return write(client, "CodeSystem", codeSystem);
}

export function saveLibrary(client: Client, library: JsonObject) {
  return write(client, "Library", library);
}

export function saveStructureMap(client: Client, map: JsonObject) {
  return write(client, "StructureMap", map);
}

/** Assemble a modular questionnaire into the form a filler can render. */
export function publishForm(client: Client, questionnaire: JsonValue) {
  return assemble(client, questionnaire);
}
