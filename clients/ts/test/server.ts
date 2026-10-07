import { spawn, type ChildProcess } from "node:child_process";
import { createServer } from "node:net";
import { fileURLToPath } from "node:url";
import path from "node:path";
import fs from "node:fs";
import os from "node:os";

export async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address();
      if (!addr || typeof addr === "string") {
        reject(new Error("no address"));
        return;
      }
      const port = addr.port;
      server.close(() => resolve(port));
    });
  });
}

export function refhirBin(): string {
  const env = process.env.REFHIR_BIN;
  const here = path.dirname(fileURLToPath(import.meta.url));
  const repo = path.join(here, "..", "..", "..");
  if (env) {
    if (path.isAbsolute(env)) return env;
    const rel = path.join(repo, env);
    if (fs.existsSync(rel)) return rel;
    if (fs.existsSync(env)) return env;
    return rel;
  }
  return path.join(repo, "target", "debug", "refhir");
}

export async function startServer(port: number): Promise<{
  base: string;
  stop: () => void;
}> {
  const db = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "refhir-ts-")), "db.sqlite");
  const bin = refhirBin();
  const child: ChildProcess = spawn(
    bin,
    ["--sqlite", db, "--bind", `127.0.0.1:${port}`],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  const base = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`${base}/metadata`);
      if (res.ok) break;
    } catch {
      // retry
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  return {
    base,
    stop: () => {
      child.kill("SIGTERM");
      try {
        fs.rmSync(path.dirname(db), { recursive: true, force: true });
      } catch {
        // ignore
      }
    },
  };
}
