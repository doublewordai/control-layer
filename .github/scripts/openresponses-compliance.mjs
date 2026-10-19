// Keep the upstream runner and validators, but make the tool-call request
// match its assertion. With the default `auto`, a clarification is valid and
// model behaviour can fail CI even when the protocol implementation is correct.
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const root = resolve(process.env.OPENRESPONSES_DIR ?? "/tmp/openresponses");
const { testTemplates } = await import(
  pathToFileURL(`${root}/src/lib/compliance-tests.ts`).href
);
const template = testTemplates.find((test) => test.id === "tool-calling");
if (!template) {
  throw new Error(
    "Upstream tool-calling test is missing; review the CI adapter",
  );
}
const getRequest = template.getRequest;
template.getRequest = (config) => ({
  ...getRequest(config),
  tool_choice: "required",
});

// The CLI imports the same module instance and reads the original argv flags.
await import(pathToFileURL(`${root}/bin/compliance-test.ts`).href);
