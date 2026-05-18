import { useState, useCallback, useMemo, useEffect } from "react";
import {
  AlertCircle,
  Code,
  Copy,
  Info,
  KeyRound,
  Loader2,
  Plus,
} from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
  DialogDescription,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { Label } from "../../ui/label";
import { Input } from "../../ui/input";
import { Textarea } from "../../ui/textarea";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../../ui/tabs";
import { Combobox } from "../../ui/combobox";
import { CodeBlock } from "../../ui/code-block";
import { Popover, PopoverContent, PopoverTrigger } from "../../ui/popover";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../ui/select";
import {
  useModels,
  useCreateApiKey,
  useConfig,
} from "../../../api/control-layer/hooks";
import { toast } from "sonner";
import { useQueryClient } from "@tanstack/react-query";

interface CreateAsyncModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
  defaultModel?: string;
}

type ServiceTier = "flex" | "priority";
type Language = "python" | "javascript" | "curl";

export function CreateAsyncModal({
  isOpen,
  onClose,
  onSuccess,
  defaultModel,
}: CreateAsyncModalProps) {
  const [activeTab, setActiveTab] = useState<"compose" | "snippet">("compose");

  // Shared form state — driven by Compose tab, mirrored into the Snippet tab.
  const [model, setModel] = useState<string>(defaultModel ?? "");
  const [serviceTier, setServiceTier] = useState<ServiceTier>("flex");
  const [systemPrompt, setSystemPrompt] = useState<string>("");
  const [userPrompt, setUserPrompt] = useState<string>("");

  // API key — only used to fill in the Code Snippet tab so users can paste a
  // working call into their own apps. Submissions from this modal go through
  // the dashboard's session-proxied /admin/api/v1/ai path, so no key is needed
  // to create a response here.
  const [apiKey, setApiKey] = useState<string | null>(null);
  const [showCreateForm, setShowCreateForm] = useState(false);
  const [newKeyName, setNewKeyName] = useState("");
  const [newKeyDescription, setNewKeyDescription] = useState("");

  // Snippet tab state
  const [language, setLanguage] = useState<Language>("python");
  const [copiedCode, setCopiedCode] = useState<string | null>(null);

  // Submit state
  const [error, setError] = useState<string | null>(null);
  const [isSubmitting, setIsSubmitting] = useState(false);

  const createApiKeyMutation = useCreateApiKey();
  const queryClient = useQueryClient();
  const { data: config } = useConfig();
  const { data: modelsData } = useModels({ accessible: true, limit: 100 });

  useEffect(() => {
    if (isOpen && defaultModel) {
      setModel(defaultModel);
    }
  }, [isOpen, defaultModel]);

  const modelOptions = useMemo(
    () =>
      (modelsData?.data ?? []).map((m) => ({
        value: m.alias,
        label: m.alias,
        description: m.model_name !== m.alias ? m.model_name : undefined,
      })),
    [modelsData?.data],
  );

  // Snippet base URL — the public-facing API URL that customers would use
  // from their own apps. Shown in the Code Snippet tab.
  const getBaseUrl = useCallback(() => {
    const base = config?.ai_api_base_url || "https://api.doubleword.ai";
    return base.endsWith("/v1") ? base : `${base}/v1`;
  }, [config?.ai_api_base_url]);

  // Submit URL — the admin/ai proxy rewrites this to /ai/v1/responses and
  // injects a per-user hidden API key derived from the session, so the
  // browser submits with cookies and never needs to handle a user-facing key.
  const submitUrl = "/admin/api/v1/ai/v1/responses";

  const buildResponseBody = useCallback(() => {
    // Key order matters for the rendered snippet — keep `instructions`
    // adjacent to `model` so the JSON/Python/JS examples read naturally.
    const body: Record<string, unknown> = {
      model: model || "model-name",
    };
    if (systemPrompt.trim()) {
      body.instructions = systemPrompt.trim();
    }
    body.input = userPrompt.trim() || "Hello!";
    body.service_tier = serviceTier;
    if (serviceTier === "flex") {
      // Background mode is the recommended flex pattern — submit returns 202,
      // poll for terminal status. Realtime/priority requests stay synchronous.
      body.background = true;
    }
    return body;
  }, [model, userPrompt, systemPrompt, serviceTier]);

  const generateSnippet = useCallback(
    (lang: Language): string => {
      const keyValue = apiKey || "your-api-key-here";
      const baseUrl = getBaseUrl();
      const body = buildResponseBody();
      const isBackground = body.background === true;
      const modelValue = String(body.model);
      const tierValue = String(body.service_tier);
      const inputValue = typeof body.input === "string" ? body.input : "";
      const instructions =
        typeof body.instructions === "string" ? body.instructions : "";

      if (lang === "python") {
        // JSON.stringify produces a double-quoted string with all special
        // characters escaped — that's also a valid Python string literal,
        // so it survives model aliases like `acme's-llm-v2` or `path\foo`.
        const createArgs = [`    model=${JSON.stringify(modelValue)},`];
        if (instructions) {
          createArgs.push(`    instructions=${JSON.stringify(instructions)},`);
        }
        createArgs.push(`    input=${JSON.stringify(inputValue)},`);
        createArgs.push(`    service_tier="${tierValue}",`);
        if (isBackground) createArgs.push("    background=True,");

        const header = isBackground
          ? "from openai import OpenAI\nfrom time import sleep"
          : "from openai import OpenAI";

        const tail = isBackground
          ? `while resp.status in {"queued", "in_progress"}:
    sleep(2)
    resp = client.responses.retrieve(resp.id)

print(f"Final status: {resp.status}\\nOutput:\\n{resp.output_text}")`
          : "print(resp.output_text)";

        return `${header}

client = OpenAI(
    api_key="${keyValue}",
    base_url="${baseUrl}",
)

resp = client.responses.create(
${createArgs.join("\n")}
)

${tail}`;
      }

      if (lang === "javascript") {
        // Same reasoning as the Python branch — JSON-quoted strings are
        // valid JS strings, so apostrophes/backslashes in the alias don't
        // break the snippet. Yields `model: "alias"` rather than `'alias'`.
        const createArgs = [`    model: ${JSON.stringify(modelValue)},`];
        if (instructions) {
          createArgs.push(
            `    instructions: ${JSON.stringify(instructions)},`,
          );
        }
        createArgs.push(`    input: ${JSON.stringify(inputValue)},`);
        createArgs.push(`    service_tier: '${tierValue}',`);
        if (isBackground) createArgs.push("    background: true,");

        const tail = isBackground
          ? `while (['queued', 'in_progress'].includes(resp.status)) {
    await new Promise((r) => setTimeout(r, 2000));
    resp = await client.responses.retrieve(resp.id);
}

console.log(\`Final status: \${resp.status}\\nOutput:\\n\${resp.output_text}\`);`
          : "console.log(resp.output_text);";

        return `import OpenAI from 'openai';

const client = new OpenAI({
    apiKey: '${keyValue}',
    baseURL: '${baseUrl}',
});

let resp = await client.responses.create({
${createArgs.join("\n")}
});

${tail}`;
      }

      // curl
      const bodyLines = [`    "model": ${JSON.stringify(modelValue)}`];
      if (instructions) {
        bodyLines.push(`    "instructions": ${JSON.stringify(instructions)}`);
      }
      bodyLines.push(`    "input": ${JSON.stringify(inputValue)}`);
      bodyLines.push(`    "service_tier": "${tierValue}"`);
      if (isBackground) bodyLines.push(`    "background": true`);

      // The body sits inside a single-quoted shell argument, so any `'` in
      // the JSON (from a prompt like "don't" or an alias containing one)
      // would terminate the argument early. Escape with the standard POSIX
      // close-quote / escaped-quote / reopen-quote dance.
      const bodyContent = `{\n${bodyLines.join(",\n")}\n  }`;
      const shellSafeBody = bodyContent.replace(/'/g, "'\\''");

      const submit = `# Submit a response${isBackground ? " — capture the id from the response body" : ""}
curl ${baseUrl}/responses \\
  -H "Authorization: Bearer ${keyValue}" \\
  -H "Content-Type: application/json" \\
  -d '${shellSafeBody}'`;

      if (!isBackground) return submit;
      return `${submit}

# Poll until terminal (replace YOUR_RESP_ID with the id returned above)
curl ${baseUrl}/responses/YOUR_RESP_ID \\
  -H "Authorization: Bearer ${keyValue}"`;
    },
    [apiKey, getBaseUrl, buildResponseBody],
  );

  const snippet = useMemo(() => generateSnippet(language), [generateSnippet, language]);

  const handleCopy = async (text: string, codeType: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopiedCode(codeType);
      toast.success(codeType === "api-key" ? "API key copied" : "Code copied");
      setTimeout(() => setCopiedCode(null), 2000);
    } catch (err) {
      console.error("Failed to copy", err);
      toast.error("Failed to copy");
    }
  };

  const handleCreateApiKey = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!newKeyName.trim()) return;
    try {
      const response = await createApiKeyMutation.mutateAsync({
        data: {
          name: newKeyName.trim(),
          description: newKeyDescription.trim() || undefined,
          purpose: "realtime",
        },
      });
      setApiKey(response.key);
      setNewKeyName("");
      setNewKeyDescription("");
      setShowCreateForm(false);
    } catch (err) {
      console.error("Error generating API key", err);
      toast.error("Failed to create API key");
    }
  };

  const handleSubmit = async () => {
    setError(null);

    if (!model) {
      setError("Please select a model");
      return;
    }
    if (!userPrompt.trim()) {
      setError("Please enter a user prompt");
      return;
    }

    setIsSubmitting(true);
    try {
      // Submit through the admin/ai proxy. The backend middleware reads the
      // session cookie, looks up (or mints) a hidden playground API key for
      // the user, and rewrites the path to /ai/v1/responses before passing
      // through onwards and the responses middleware. The browser never
      // needs to know about the API key.
      const res = await fetch(submitUrl, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(buildResponseBody()),
      });

      if (!res.ok) {
        const text = await res.text().catch(() => "");
        if (res.status === 401 || res.status === 403) {
          throw new Error(
            "Your session was rejected — try signing in again and retrying",
          );
        }
        throw new Error(
          `Request failed (${res.status}): ${text.slice(0, 500) || res.statusText}`,
        );
      }

      toast.success(
        serviceTier === "flex"
          ? "Response queued — check the Responses page"
          : "Response created",
      );
      queryClient.invalidateQueries({ queryKey: ["asyncRequests"] });

      resetForm();
      onSuccess();
      onClose();
    } catch (err) {
      console.error("Failed to create response:", err);
      setError(err instanceof Error ? err.message : "Failed to create response.");
    } finally {
      setIsSubmitting(false);
    }
  };

  const resetForm = () => {
    setModel("");
    setServiceTier("flex");
    setSystemPrompt("");
    setUserPrompt("");
    setError(null);
    setActiveTab("compose");
    // apiKey intentionally persists across resets so users who minted a key
    // for the snippet don't lose it after creating a response.
  };

  const handleClose = () => {
    resetForm();
    onClose();
  };

  const canSubmit =
    Boolean(model) && userPrompt.trim().length > 0 && !isSubmitting;

  const renderApiKeyPopover = (trigger: React.ReactNode) => (
    <Popover open={showCreateForm} onOpenChange={setShowCreateForm}>
      <PopoverTrigger asChild>{trigger}</PopoverTrigger>
      <PopoverContent className="w-80" align="end">
        <form onSubmit={handleCreateApiKey} className="space-y-4">
          <div className="space-y-1">
            <h4 className="font-medium leading-none">Create API Key</h4>
            <p className="text-sm text-muted-foreground">
              Filled into the code snippet so you can paste a working call
              into your own apps. Not required to submit from here.
            </p>
          </div>
          <div className="space-y-2">
            <Label htmlFor="response-key-name">Name *</Label>
            <Input
              id="response-key-name"
              type="text"
              value={newKeyName}
              onChange={(e) => setNewKeyName(e.target.value)}
              placeholder="Responses dashboard"
              required
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="response-key-description">Description</Label>
            <Textarea
              id="response-key-description"
              value={newKeyDescription}
              onChange={(e) => setNewKeyDescription(e.target.value)}
              placeholder="What will this key be used for?"
              rows={3}
              className="resize-none"
            />
          </div>
          <div className="flex justify-end gap-2">
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => {
                setShowCreateForm(false);
                setNewKeyName("");
                setNewKeyDescription("");
              }}
            >
              Cancel
            </Button>
            <Button
              type="submit"
              size="sm"
              disabled={createApiKeyMutation.isPending || !newKeyName.trim()}
            >
              {createApiKeyMutation.isPending && (
                <Loader2 className="w-3 h-3 animate-spin" />
              )}
              Create
            </Button>
          </div>
        </form>
      </PopoverContent>
    </Popover>
  );

  return (
    <Dialog open={isOpen} onOpenChange={(open) => !open && handleClose()}>
      <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Create Response</DialogTitle>
          <DialogDescription>
            Submit a single request to the Open Responses API
          </DialogDescription>
        </DialogHeader>

        <Tabs
          value={activeTab}
          onValueChange={(v) => setActiveTab(v as "compose" | "snippet")}
        >
          <TabsList className="w-full">
            <TabsTrigger value="compose" className="flex-1">
              Compose
            </TabsTrigger>
            <TabsTrigger value="snippet" className="flex-1">
              Code Snippet
            </TabsTrigger>
          </TabsList>

          <TabsContent value="compose" className="space-y-4 mt-4">
            <div className="grid grid-cols-2 gap-4">
              <div className="space-y-2">
                <Label>Model</Label>
                <Combobox
                  options={modelOptions}
                  value={model}
                  onValueChange={setModel}
                  placeholder="Select a model..."
                  searchPlaceholder="Search models..."
                  emptyMessage="No models found."
                  className="w-full"
                />
              </div>
              <div className="space-y-2">
                <Label>Service Tier</Label>
                <Select
                  value={serviceTier}
                  onValueChange={(v) => setServiceTier(v as ServiceTier)}
                >
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="flex">Flex</SelectItem>
                    <SelectItem value="priority">Priority</SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </div>

            <div className="space-y-2">
              <Label htmlFor="system-prompt">
                System prompt{" "}
                <span className="text-muted-foreground font-normal">(optional)</span>
              </Label>
              <Textarea
                id="system-prompt"
                placeholder="e.g. You are a senior code reviewer..."
                value={systemPrompt}
                onChange={(e) => setSystemPrompt(e.target.value)}
                rows={3}
              />
            </div>

            <div className="space-y-2">
              <Label htmlFor="user-prompt">User prompt</Label>
              <Textarea
                id="user-prompt"
                placeholder="What would you like the model to do?"
                value={userPrompt}
                onChange={(e) => setUserPrompt(e.target.value)}
                rows={5}
              />
            </div>
          </TabsContent>

          <TabsContent value="snippet" className="space-y-3 mt-4">
            {/* Model picker mirrored from Compose — without it the snippet
                shows whatever model was last selected with no way to swap
                without flipping tabs. */}
            <div className="space-y-2">
              <Label>Model</Label>
              <Combobox
                options={modelOptions}
                value={model}
                onValueChange={setModel}
                placeholder="Select a model..."
                searchPlaceholder="Search models..."
                emptyMessage="No models found."
                className="w-full"
              />
            </div>

            {/* API key bar lives in the snippet tab because that's its only
                purpose — submissions from this modal use the dashboard
                session, so no key is needed to hit Create. */}
            <div className="flex items-center justify-between gap-3 rounded-md border bg-muted/30 px-3 py-2">
              <div className="flex items-center gap-2 min-w-0">
                <KeyRound className="h-4 w-4 text-muted-foreground flex-shrink-0" />
                <span className="text-sm font-medium text-doubleword-neutral-700">
                  API key
                </span>
                <span className="text-sm font-mono text-muted-foreground truncate">
                  {apiKey
                    ? `${apiKey.slice(0, 6)}…${apiKey.slice(-4)}`
                    : "not set"}
                </span>
              </div>
              <div className="flex items-center gap-1">
                {apiKey && (
                  <button
                    type="button"
                    onClick={() => handleCopy(apiKey, "api-key")}
                    className="flex items-center gap-1 px-2 py-1 text-xs text-green-600 hover:text-green-700 hover:bg-green-50 rounded transition-colors"
                  >
                    <Copy className="w-3 h-3" />
                    {copiedCode === "api-key" ? "Copied!" : "Copy"}
                  </button>
                )}
                {renderApiKeyPopover(
                  <button
                    type="button"
                    className="flex items-center gap-1 px-2 py-1 text-xs text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded transition-colors"
                  >
                    <Plus className="w-3 h-3" />
                    {apiKey ? "Replace" : "Fill API key"}
                  </button>,
                )}
              </div>
            </div>

            <div className="bg-white border border-gray-200 rounded-lg overflow-hidden">
              <div className="bg-gray-50 px-4 py-2 border-b border-gray-200 flex items-center justify-between">
                <div className="flex items-center gap-2">
                  <Code className="w-4 h-4 text-gray-600" />
                  <Select
                    value={language}
                    onValueChange={(v) => setLanguage(v as Language)}
                  >
                    <SelectTrigger
                      size="sm"
                      className="h-7 border-0 bg-transparent shadow-none hover:bg-gray-100 focus-visible:ring-0"
                    >
                      <SelectValue>
                        <span className="text-sm font-medium text-gray-700">
                          {language === "python"
                            ? "Python"
                            : language === "javascript"
                              ? "JavaScript"
                              : "cURL"}
                        </span>
                      </SelectValue>
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="python">Python</SelectItem>
                      <SelectItem value="javascript">JavaScript</SelectItem>
                      <SelectItem value="curl">cURL</SelectItem>
                    </SelectContent>
                  </Select>
                </div>
                <button
                  type="button"
                  onClick={() => handleCopy(snippet, "code")}
                  disabled={!model || !userPrompt.trim()}
                  className="flex items-center gap-1 px-2 py-1 text-xs text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded transition-colors disabled:opacity-50 disabled:cursor-not-allowed disabled:hover:bg-transparent disabled:hover:text-gray-500"
                >
                  <Copy className="w-3 h-3" />
                  {copiedCode === "code" ? "Copied!" : "Copy"}
                </button>
              </div>
              <div className="overflow-x-auto">
                <CodeBlock language={language === "curl" ? "bash" : language}>
                  {snippet}
                </CodeBlock>
              </div>
            </div>

            {(!model || !userPrompt.trim()) && (
              <div className="flex items-start gap-2 text-xs text-muted-foreground">
                <Info className="h-3.5 w-3.5 mt-0.5 flex-shrink-0" />
                <span>
                  {!model && !userPrompt.trim()
                    ? "Pick a model above and add a user prompt on the Compose tab to render the snippet."
                    : !model
                      ? "Pick a model above to render the snippet."
                      : "Add a user prompt on the Compose tab to render the snippet."}
                </span>
              </div>
            )}
          </TabsContent>
        </Tabs>

        {error && (
          <div className="flex items-start gap-2 text-destructive text-sm">
            <AlertCircle className="h-4 w-4 mt-0.5 flex-shrink-0" />
            <span>{error}</span>
          </div>
        )}

        <DialogFooter>
          <div className="flex w-full items-center justify-end gap-2">
            <Button variant="outline" onClick={handleClose}>
              {activeTab === "snippet" ? "Close" : "Cancel"}
            </Button>
            {activeTab === "compose" && (
              <Button onClick={handleSubmit} disabled={!canSubmit}>
                {isSubmitting ? "Creating..." : "Create"}
              </Button>
            )}
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
