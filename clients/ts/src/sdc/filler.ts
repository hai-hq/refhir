import type { Client } from "../client.js";
import { HttpError } from "../errors.js";
import { doRequest } from "../rest.js";
import type { JsonValue } from "../types.js";
import { responseEndpoint, taskOutput } from "./render.js";

type JsonObject = { [key: string]: JsonValue };

export type BehaviorEngine = (questionnaire: string, response: string) => string;

let behaviorEngine: BehaviorEngine | null = null;

/** Install the wasm `behavior_json` export. The server `$behavior` operation is the fallback. */
export function useBehaviorEngine(engine: BehaviorEngine | null) {
  behaviorEngine = engine;
}

async function post(client: Client, path: string, body: JsonValue): Promise<JsonObject> {
  const bytes = await doRequest(client, "POST", path, new TextEncoder().encode(JSON.stringify(body)));
  return JSON.parse(new TextDecoder().decode(bytes)) as JsonObject;
}

async function put(client: Client, path: string, body: JsonValue): Promise<JsonObject> {
  const bytes = await doRequest(client, "PUT", path, new TextEncoder().encode(JSON.stringify(body)));
  return JSON.parse(new TextDecoder().decode(bytes)) as JsonObject;
}

export function populate(client: Client, questionnaire: JsonValue, subject?: string): Promise<JsonObject> {
  return post(client, "/Questionnaire/$populate", {
    resourceType: "Parameters",
    parameter: [
      { name: "questionnaire", resource: questionnaire },
      ...(subject ? [{ name: "subject", valueString: subject }] : []),
    ],
  });
}

export function assemble(client: Client, questionnaire: JsonValue): Promise<JsonObject> {
  return post(client, "/Questionnaire/$assemble", questionnaire);
}

export function extract(client: Client, response: JsonValue): Promise<JsonObject> {
  return post(client, "/QuestionnaireResponse/$extract", response);
}

export function nextQuestion(client: Client, response: JsonValue): Promise<JsonObject> {
  return post(client, "/Questionnaire/$next-question", response);
}

export async function behavior(
  client: Client,
  questionnaire: JsonValue,
  response: JsonValue,
): Promise<JsonObject> {
  if (behaviorEngine) {
    return JSON.parse(behaviorEngine(JSON.stringify(questionnaire), JSON.stringify(response))) as JsonObject;
  }
  return post(client, "/Questionnaire/$behavior", { questionnaire, response });
}

export function saveResponse(client: Client, response: JsonObject): Promise<JsonObject> {
  const id = typeof response.id === "string" ? response.id : "";
  if (id) return put(client, `/QuestionnaireResponse/${id}`, response);
  return post(client, "/QuestionnaireResponse", response);
}

/** Submit a completed response to a Form Receiver or Form Archiver. */
export function submitResponse(client: Client, response: JsonObject): Promise<JsonObject> {
  return saveResponse(client, { ...response, status: "completed" });
}

/** Path or absolute URL for a Task `responseEndpoint`. */
export function responseTarget(endpoint: string): string {
  if (endpoint.startsWith("https://") || endpoint.startsWith("http://")) return endpoint;
  return endpoint.startsWith("/") ? endpoint : `/${endpoint}`;
}

/** Write the response onto an SDC Task and POST it to responseEndpoint when present. */
export async function fulfillTask(client: Client, task: JsonObject, response: JsonValue): Promise<JsonObject> {
  const output = taskOutput(response);
  const updated: JsonObject = {
    ...task,
    status: "completed",
    output: output.output,
  };
  const id = typeof task.id === "string" ? task.id : "";
  const saved = id ? await put(client, `/Task/${id}`, updated) : updated;
  const endpoint = responseEndpoint(task);
  if (endpoint) {
    const target = responseTarget(endpoint);
    if (target.startsWith("https://") || target.startsWith("http://")) {
      await postAbsolute(target, response);
    } else {
      await post(client, target, response);
    }
  }
  return saved;
}

async function postAbsolute(url: string, body: JsonValue): Promise<void> {
  const response = await fetch(url, {
    method: "POST",
    headers: {
      Accept: "application/fhir+json",
      "Content-Type": "application/fhir+json",
    },
    body: JSON.stringify(body),
  });
  if (response.status < 200 || response.status >= 300) {
    throw new HttpError(response.status, new Uint8Array(await response.arrayBuffer()));
  }
}
