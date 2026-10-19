import { expect, test } from "bun:test";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const root = resolve(process.env.OPENRESPONSES_DIR ?? "/tmp/openresponses");
const { testTemplates } = await import(
  pathToFileURL(`${root}/src/lib/compliance-tests.ts`).href
);
const fixture = testTemplates
  .find((template) => template.id === "response-output-phase-schema")
  .getMockResponse({ model: "fixture-model" });

async function run(output, invalidSchema = false) {
  const requests = [];
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      const body = await request.json();
      requests.push({
        body,
        path: new URL(request.url).pathname,
        auth: request.headers.get("authorization"),
      });
      const response = {
        ...fixture,
        output: body.tools ? output : fixture.output,
      };
      if (invalidSchema) response.created_at = "invalid";
      return Response.json(response);
    },
  });
  const child = Bun.spawn({
    cmd: [
      process.execPath,
      "run",
      resolve(import.meta.dir, "openresponses-compliance.mjs"),
      "--base-url",
      `http://127.0.0.1:${server.port}/v1`,
      "--api-key",
      "fixture-key",
      "--model",
      "fixture-model",
      "--filter",
      "basic-response,tool-calling",
      "--json",
    ],
    env: { ...process.env, OPENRESPONSES_DIR: root },
    stdout: "pipe",
    stderr: "pipe",
  });
  const timeout = setTimeout(() => child.kill(), 10_000);
  try {
    const [status, stdout, stderr] = await Promise.all([
      child.exited,
      new Response(child.stdout).text(),
      new Response(child.stderr).text(),
    ]);
    expect(stderr).toBe("");
    return { status, results: JSON.parse(stdout), requests };
  } finally {
    clearTimeout(timeout);
    child.kill();
    server.stop(true);
  }
}

const toolOutput = [
  {
    id: "fc_fixture",
    type: "function_call",
    status: "completed",
    call_id: "call_fixture",
    name: "get_weather",
    arguments: '{"location":"San Francisco, CA"}',
  },
];

test("real upstream CLI sends required tool choice and preserves the other case and flags", async () => {
  const { status, results, requests } = await run(toolOutput);
  expect(status).toBe(0);
  expect(results.summary.passed).toBe(2);
  expect(requests).toHaveLength(2);
  const tool = requests.find(({ body }) => body.tools);
  const original = testTemplates
    .find((template) => template.id === "tool-calling")
    .getRequest({ model: "fixture-model" });
  expect(tool.body).toEqual({ ...original, tool_choice: "required" });
  expect(
    requests.find(({ body }) => !body.tools).body.tool_choice,
  ).toBeUndefined();
  for (const request of requests) {
    expect(request.path).toBe("/v1/responses");
    expect(request.auth).toBe("Bearer fixture-key");
  }
});

test("unchanged validators still reject a message instead of a tool call", async () => {
  const { status, results } = await run(fixture.output);
  expect(status).toBe(1);
  expect(
    results.results.find((result) => result.id === "tool-calling").errors,
  ).toContain('Expected output item of type "function_call" but none found');
});

test("unchanged schema validation still rejects malformed responses", async () => {
  const { status, results } = await run(toolOutput, true);
  expect(status).toBe(1);
  expect(results.summary.failed).toBe(2);
  expect(
    results.results.every((result) =>
      result.errors.some((error) => error.startsWith("created_at:")),
    ),
  ).toBe(true);
});
