import type { JsonValue } from "../types.js";

export interface RenderedItem {
  linkId: string;
  text: string;
  type: string;
  required: boolean;
  repeats: boolean;
  enabled: boolean;
  control: string;
  unit: string;
  hidden: boolean;
  columns: number;
  answerOptions: JsonValue[];
  candidates: JsonValue[];
}

type JsonObject = { [key: string]: JsonValue };

function object(value: JsonValue | undefined): JsonObject | undefined {
  if (value && typeof value === "object" && !Array.isArray(value)) return value;
  return undefined;
}

function text(value: JsonValue | undefined): string {
  return typeof value === "string" ? value : "";
}

function flag(value: JsonValue | undefined): boolean {
  return value === true;
}

function extensions(node: JsonObject | undefined, url: string): JsonObject[] {
  const list = node?.extension;
  if (!Array.isArray(list)) return [];
  return list
    .map(object)
    .filter((item): item is JsonObject => item !== undefined && item.url === url);
}

function extensionCode(node: JsonObject | undefined, url: string): string {
  const ext = extensions(node, url)[0];
  const coding = object(ext?.valueCodeableConcept)?.coding;
  if (Array.isArray(coding)) {
    const first = object(coding[0]);
    return text(first?.code);
  }
  return text(object(ext)?.valueCode);
}

function walk(item: JsonObject, enabled: JsonObject, candidates: JsonObject, out: RenderedItem[]) {
  const linkId = text(item.linkId);
  const on = enabled[linkId];
  out.push({
    linkId,
    text: text(item.text),
    type: text(item.type) || "display",
    required: flag(item.required),
    repeats: flag(item.repeats),
    enabled: on === undefined ? true : on === true,
    control: extensionCode(item, "http://hl7.org/fhir/StructureDefinition/questionnaire-itemControl"),
    unit: extensionCode(item, "http://hl7.org/fhir/StructureDefinition/questionnaire-unit"),
    hidden: extensions(item, "http://hl7.org/fhir/StructureDefinition/questionnaire-hidden").length > 0,
    columns: typeof item.extension === "undefined" ? 1 : columnCount(item),
    answerOptions: Array.isArray(item.answerOption) ? item.answerOption : [],
    candidates: Array.isArray(candidates[linkId]) ? candidates[linkId] : [],
  });
  if (Array.isArray(item.item)) {
    for (const child of item.item) {
      const node = object(child);
      if (node) walk(node, enabled, candidates, out);
    }
  }
}

function columnCount(item: JsonObject): number {
  const ext = extensions(item, "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-choiceColumn")[0];
  const width = object(ext)?.valueInteger;
  return typeof width === "number" && width > 0 ? width : 1;
}

/** View model for the controls the advanced-rendering profile marks must-support. */
export function renderItems(view: JsonValue): RenderedItem[] {
  const root = object(view);
  const questionnaire = object(root?.questionnaire) ?? root;
  const enabled = object(root?.enabled) ?? {};
  const candidates = object(root?.candidates) ?? {};
  const items = questionnaire?.item;
  const out: RenderedItem[] = [];
  if (Array.isArray(items)) {
    for (const item of items) {
      const node = object(item);
      if (node) walk(node, enabled, candidates, out);
    }
  }
  return out.filter((item) => item.enabled && !item.hidden);
}

export function taskOutput(response: JsonValue): JsonObject {
  return {
    resourceType: "Task",
    output: [
      {
        type: {
          coding: [
            {
              system: "http://hl7.org/fhir/uv/sdc/CodeSystem/temp",
              code: "questionnaire-response",
            },
          ],
        },
        valueReference: response,
      },
    ],
  };
}

export function responseEndpoint(task: JsonValue): string {
  const root = object(task);
  const ext = extensions(root, "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-task-responseEndpoint")[0];
  return text(object(ext)?.valueUrl) || text(object(ext)?.valueUri);
}
