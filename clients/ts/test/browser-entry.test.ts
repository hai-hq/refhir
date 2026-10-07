import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

const src = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "src");

function graph(start: string): string {
  const seen = new Set<string>();
  const chunks: string[] = [];
  const visit = (file: string) => {
    const resolved = file.endsWith(".js") ? `${file.slice(0, -3)}.ts` : file.endsWith(".ts") ? file : `${file}.ts`;
    if (seen.has(resolved)) return;
    seen.add(resolved);
    const text = readFileSync(resolved, "utf8");
    chunks.push(text);
    for (const match of text.matchAll(/from "(\.[^"]+)"/g)) {
      visit(path.resolve(path.dirname(resolved), match[1]));
    }
  };
  visit(path.join(src, start));
  return chunks.join("\n");
}

describe("browser entry", () => {
  it("does not import the Node ws transport", () => {
    for (const entry of ["index-browser.ts", "react/index-browser.ts"]) {
      const text = graph(entry);
      expect(text).not.toContain('from "ws"');
      expect(text).not.toContain("transport-node");
    }
  });
});
