import { describe, expect, it } from "vitest";

import { behavior, fulfillTask, renderItems, responseTarget, taskOutput, useBehaviorEngine } from "../src/sdc/index.js";
import type { Client } from "../src/client.js";

describe("form filler", () => {
  it("renders enabled controls and hides disabled ones", () => {
    const items = renderItems({
      questionnaire: {
        resourceType: "Questionnaire",
        item: [
          { linkId: "gate", text: "Start", type: "boolean", required: true },
          {
            linkId: "detail",
            text: "Detail",
            type: "string",
            extension: [
              {
                url: "http://hl7.org/fhir/StructureDefinition/questionnaire-itemControl",
                valueCodeableConcept: { coding: [{ code: "text-box" }] },
              },
            ],
          },
          {
            linkId: "secret",
            text: "Secret",
            type: "string",
            extension: [{ url: "http://hl7.org/fhir/StructureDefinition/questionnaire-hidden", valueBoolean: true }],
          },
        ],
      },
      enabled: { gate: true, detail: true, secret: true },
      candidates: { detail: [{ valueString: "Ada" }] },
    });
    expect(items.map((item) => item.linkId)).toEqual(["gate", "detail"]);
    expect(items[0]?.required).toBe(true);
    expect(items[1]?.control).toBe("text-box");
    expect(items[1]?.candidates).toEqual([{ valueString: "Ada" }]);
  });

  it("puts the response on a Task and uses the wasm behavior engine when installed", async () => {
    const task = taskOutput({
      resourceType: "QuestionnaireResponse",
      status: "completed",
      id: "qr1",
    });
    expect(task.output).toMatchObject([
      { valueReference: { resourceType: "QuestionnaireResponse", id: "qr1" } },
    ]);
    useBehaviorEngine(() => JSON.stringify({ enabled: { q1: true }, response: { resourceType: "QuestionnaireResponse" } }));
    const view = await behavior({} as Client, { resourceType: "Questionnaire" }, { resourceType: "QuestionnaireResponse" });
    expect(view.enabled).toEqual({ q1: true });
    useBehaviorEngine(null);
  });

  it("posts an absolute responseEndpoint without rewriting it", async () => {
    expect(responseTarget("https://receiver.example/QuestionnaireResponse")).toBe(
      "https://receiver.example/QuestionnaireResponse",
    );
    expect(responseTarget("QuestionnaireResponse")).toBe("/QuestionnaireResponse");
    const seen: { url: string; authorization: string | null }[] = [];
    const original = globalThis.fetch;
    globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      const headers = new Headers(init?.headers);
      seen.push({ url: String(input), authorization: headers.get("authorization") });
      return new Response("{}", { status: 200 });
    }) as typeof fetch;
    try {
      await fulfillTask(
        {} as Client,
        {
          extension: [
            {
              url: "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-task-responseEndpoint",
              valueUrl: "https://receiver.example/QuestionnaireResponse",
            },
          ],
        },
        { resourceType: "QuestionnaireResponse", status: "completed" },
      );
    } finally {
      globalThis.fetch = original;
    }
    expect(seen).toEqual([
      { url: "https://receiver.example/QuestionnaireResponse", authorization: null },
    ]);
  });
});
