import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

test("ReCodex boot renders through the renderer error boundary", async () => {
  const main = await readFile(new URL("./main.tsx", import.meta.url), "utf8");
  const boundary = await readFile(new URL("./recodex/BootErrorBoundary.tsx", import.meta.url), "utf8");
  assert.match(main, /BootErrorBoundary/);
  assert.match(main, /<BootErrorBoundary><App \/><\/BootErrorBoundary>/);
  assert.match(boundary, /renderer startup failed/);
  assert.match(boundary, /window\.location\.reload/);
  assert.doesNotMatch(boundary, /String\(error\)/);
});
