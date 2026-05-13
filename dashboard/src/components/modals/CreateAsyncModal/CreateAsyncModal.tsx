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

  // API key — required for the direct /v1/responses call, also baked into snippet.
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

  // Submit URL — routed through the dashboard's own origin to avoid CORS.
  // In dev the Vite proxy forwards /ai → dwctl on :3001; in prod app.doubleword.ai
  // serves /ai/v1 from the same host as the dashboard.
  const submitUrl = "/ai/v1/responses";

  const buildResponseBody = useCallback(() => {
    const body: Record<string, unknown> = {
      model: model || "model-name",
      input: userPrompt.trim() || "Hello!",
      service_tier: serviceTier,
    };
    if (systemPrompt.trim()) {
      body.instructions = systemPrompt.trim();
    }
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
      const instructions = typeof body.instructions === "string" ? body.instructions : "";
      const input = typeof body.input === "string" ? body.input : "";

      if (lang === "python") {
        const lines: string[] = ["from openai import OpenAI"];
        if (isBackground) lines.push("from time import sleep");
        lines.push("");
        lines.push(`client = OpenAI(api_key="${keyValue}", base_url="${baseUrl}")`);
        lines.push("");
        lines.push("resp = client.responses.create(");
        lines.push(`    model="${body.model}",`);
        if (instructions) lines.push(`    instructions=${JSON.stringify(instructions)},`);
        lines.push(`    input=${JSON.stringify(input)},`);
        lines.push(`    service_tier="${body.service_tier}",`);
        if (isBackground) lines.push("    background=True,");
        lines.push(")");
        if (isBackground) {
          lines.push("");
          lines.push('while resp.status in {"queued", "in_progress"}:');
          lines.push("    sleep(2)");
          lines.push("    resp = client.responses.retrieve(resp.id)");
          lines.push("");
          lines.push('print(f"Final status: {resp.status}\\nOutput:\\n{resp.output_text}")');
        } else {
          lines.push("");
          lines.push("print(resp.output_text)");
        }
        return lines.join("\n");
      }

      if (lang === "javascript") {
        const lines: string[] = [
          "import OpenAI from 'openai';",
          "",
          `const client = new OpenAI({ apiKey: '${keyValue}', baseURL: '${baseUrl}' });`,
          "",
          "let resp = await client.responses.create({",
          `  model: '${body.model}',`,
        ];
        if (instructions) lines.push(`  instructions: ${JSON.stringify(instructions)},`);
        lines.push(`  input: ${JSON.stringify(input)},`);
        lines.push(`  service_tier: '${body.service_tier}',`);
        if (isBackground) lines.push("  background: true,");
        lines.push("});");
        if (isBackground) {
          lines.push("");
          lines.push("while (['queued', 'in_progress'].includes(resp.status)) {");
          lines.push("  await new Promise((r) => setTimeout(r, 2000));");
          lines.push("  resp = await client.responses.retrieve(resp.id);");
          lines.push("}");
          lines.push("");
          lines.push("console.log(`Final status: ${resp.status}\\nOutput:\\n${resp.output_text}`);");
        } else {
          lines.push("");
          lines.push("console.log(resp.output_text);");
        }
        return lines.join("\n");
      }

      // curl — use a heredoc with a single-quoted delimiter so prompts
      // containing apostrophes (or any other shell metacharacter) don't
      // break the command when copy-pasted into a terminal.
      const bodyJson = JSON.stringify(body, null, 2);
      const submit = `# Submit a response${isBackground ? " (returns 202 + response id)" : ""}\ncurl ${baseUrl}/responses \\\n  -H "Authorization: Bearer ${keyValue}" \\\n  -H "Content-Type: application/json" \\\n  --data-binary @- <<'EOF'\n${bodyJson}\nEOF`;
      if (!isBackground) return submit;
      return `${submit}\n\n# Poll until terminal (replace YOUR_RESP_ID with the id returned above)\ncurl ${baseUrl}/responses/YOUR_RESP_ID \\\n  -H "Authorization: Bearer ${keyValue}"`;
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

    if (!apiKey) {
      setError("Enter an API key before submitting");
      return;
    }
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
      // Call the Open Responses API the same way the snippet shows, but via
      // the dashboard's same-origin /ai/v1 path so the browser doesn't block
      // it on CORS. The backend responses middleware turns this into a
      // tracked async/realtime request that surfaces on the Responses page.
      const res = await fetch(submitUrl, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${apiKey}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify(buildResponseBody()),
      });

      if (!res.ok) {
        const text = await res.text().catch(() => "");
        if (res.status === 401 || res.status === 403) {
          throw new Error(
            "API key was rejected — try minting a new one with the button above",
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
    // apiKey intentionally persists across resets so users don't have to
    // re-mint a key for every response they submit in the same session.
  };

  const handleClose = () => {
    resetForm();
    onClose();
  };

  const canSubmit =
    Boolean(apiKey) &&
    Boolean(model) &&
    userPrompt.trim().length > 0 &&
    !isSubmitting;

  const renderApiKeyPopover = (trigger: React.ReactNode) => (
    <Popover open={showCreateForm} onOpenChange={setShowCreateForm}>
      <PopoverTrigger asChild>{trigger}</PopoverTrigger>
      <PopoverContent className="w-80" align="end">
        <form onSubmit={handleCreateApiKey} className="space-y-4">
          <div className="space-y-1">
            <h4 className="font-medium leading-none">Create API Key</h4>
            <p className="text-sm text-muted-foreground">
              Used to authenticate this and future response submissions
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

        {/* Shared API key bar — both the Compose submit and the Snippet tab
            use the same key, so a single source of truth at the top is
            clearer than duplicating the affordance. */}
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
                  Fill in the model and user prompt on the Compose tab to see
                  them rendered in the snippet.
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
