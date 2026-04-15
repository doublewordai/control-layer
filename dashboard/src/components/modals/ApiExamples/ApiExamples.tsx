import React, { useState, useMemo } from "react";
import {
  Copy,
  Code,
  Plus,
  Loader2,
  Info,
  Download,
  ExternalLink,
} from "lucide-react";
import { CodeBlock } from "../../ui/code-block";
import { toast } from "sonner";
import { useCreateApiKey, useConfig, useModels } from "../../../api/control-layer";
import { type ModelType } from "../../../utils/modelType";
import type { Model } from "../../../api/control-layer";
import { isBatchDenied, isRealtimeDenied } from "../../../utils/modelAccess";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { Input } from "../../ui/input";
import { Textarea } from "../../ui/textarea";
import { Label } from "../../ui/label";
import { Popover, PopoverContent, PopoverTrigger } from "../../ui/popover";
import { ToggleGroup, ToggleGroupItem } from "../../ui/toggle-group";
import { AlertBox } from "@/components/ui/alert-box";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../ui/select";
import { Combobox } from "../../ui/combobox";

interface ApiExamplesModalProps {
  isOpen: boolean;
  onClose: () => void;
  /** Pre-selected model. If null, the user picks from a dropdown inside the modal. */
  model?: Model | null;
  /** Which tab to open on. Defaults to "batch". */
  defaultTab?: ExampleType;
}

type Language = "python" | "javascript" | "curl";
type ExampleType = "batch" | "async" | "realtime";

const ApiExamplesModal: React.FC<ApiExamplesModalProps> = ({
  isOpen,
  onClose,
  model: initialModel,
  defaultTab = "batch",
}) => {
  const [selectedLanguage, setSelectedLanguage] = useState<Language>("python");
  const [exampleType, setExampleType] = useState<ExampleType>(defaultTab);
  const [selectedModelId, setSelectedModelId] = useState<string>(
    initialModel?.id || "",
  );
  const [apiKey, setApiKey] = useState<string | null>(null);
  const [copiedCode, setCopiedCode] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // API Key creation popover states
  const [showCreateForm, setShowCreateForm] = useState(false);
  const [newKeyName, setNewKeyName] = useState("");
  const [newKeyDescription, setNewKeyDescription] = useState("");
  const [showInfoTooltip, setShowInfoTooltip] = useState(false);

  const { data: config } = useConfig();
  const { data: modelsData } = useModels({
    accessible: true,
    limit: 100,
  });

  const allModels = modelsData?.data ?? [];

  // Resolve the active model from user selection (initialModel only sets the default)
  const model: Model | null = useMemo(() => {
    if (selectedModelId) {
      return allModels.find((m) => m.id === selectedModelId) || null;
    }
    return null;
  }, [selectedModelId, allModels]);

  // Sync selectedModelId when initialModel changes
  React.useEffect(() => {
    if (initialModel) {
      setSelectedModelId(initialModel.id);
    }
  }, [initialModel]);

  // Reset tab when opening
  React.useEffect(() => {
    if (isOpen) {
      setExampleType(defaultTab);
      if (initialModel) {
        setSelectedModelId(initialModel.id);
      } else if (!selectedModelId && allModels.length > 0) {
        // Default to first accessible chat model
        const chatModel = allModels.find(
          (m) => m.model_type?.toUpperCase() !== "EMBEDDINGS",
        );
        if (chatModel) setSelectedModelId(chatModel.id);
      }
    }
  }, [isOpen, defaultTab, initialModel, selectedModelId, allModels]);

  const modelOptions = useMemo(
    () =>
      allModels.map((m) => ({
        value: m.id,
        label: m.alias,
        description: m.model_name !== m.alias ? m.model_name : undefined,
      })),
    [allModels],
  );

  // Completion window is determined by tab — async uses configured window
  const asyncWindow =
    config?.batches?.async_requests?.completion_window ?? "1h";
  const completionWindow = exampleType === "async" ? asyncWindow : "24h";

  const createApiKeyMutation = useCreateApiKey();

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
    } catch (error) {
      setError("Error generating API key");
      console.error("Error generating API key:", error);
    }
  };

  const copyToClipboard = async (text: string, codeType: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopiedCode(codeType);
      const message =
        codeType === "api-key"
          ? "API key copied to clipboard"
          : "Code copied to clipboard";
      toast.success(message);
      setTimeout(() => setCopiedCode(null), 2000);
    } catch (err) {
      setError("Failed to copy to clipboard");
      console.error("Failed to copy to clipboard:", err);
    }
  };

  const isEmbeddingsModel =
    model?.model_type?.toLowerCase() === "embeddings";

  const getExampleJsonl = () => {
    const modelAlias = model?.alias || "model-name";
    if (isEmbeddingsModel) {
      return `{"custom_id": "request-1", "method": "POST", "url": "/v1/embeddings", "body": {"model": "${modelAlias}", "input": "What is the capital of France?"}}
{"custom_id": "request-2", "method": "POST", "url": "/v1/embeddings", "body": {"model": "${modelAlias}", "input": "Explain quantum computing in simple terms"}}
{"custom_id": "request-3", "method": "POST", "url": "/v1/embeddings", "body": {"model": "${modelAlias}", "input": "Write a haiku about programming"}}`;
    }
    return `{"custom_id": "request-1", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "${modelAlias}", "messages": [{"role": "user", "content": "What is the capital of France?"}]}}
{"custom_id": "request-2", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "${modelAlias}", "messages": [{"role": "user", "content": "Explain quantum computing in simple terms"}]}}
{"custom_id": "request-3", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "${modelAlias}", "messages": [{"role": "user", "content": "Write a haiku about programming"}]}}`;
  };

  const downloadJsonl = () => {
    const content = getExampleJsonl();
    const blob = new Blob([content], { type: "application/jsonl" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = "batch_requests.jsonl";
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    URL.revokeObjectURL(url);
    toast.success("JSONL file downloaded");
  };

  const getBaseUrl = () => `https://api.doubleword.ai/v1`;

  const generateBatchApiCode = (language: Language) => {
    const keyValue = apiKey || "your-api-key-here";
    const batchEndpoint = isEmbeddingsModel
      ? "/v1/embeddings"
      : "/v1/chat/completions";
    if (language === "python") {
      return `from openai import OpenAI

client = OpenAI(
    api_key="${keyValue}",
    base_url="${getBaseUrl()}"
)

# Step 1: Upload a batch input file
with open("batch_requests.jsonl", "rb") as file:
    batch_file = client.files.create(
        file=file,
        purpose="batch"
    )

print(f"File ID: {batch_file.id}")

# Step 2: Create a batch job
batch = client.batches.create(
    input_file_id=batch_file.id,
    endpoint="${batchEndpoint}",
    completion_window="${completionWindow}"
)

print(f"Batch ID: {batch.id}")

# Step 3: Check batch status
batch_status = client.batches.retrieve(batch.id)
print(f"Status: {batch_status.status}")`;
    } else if (language === "javascript") {
      return `import OpenAI from 'openai';
import fs from 'fs';

const client = new OpenAI({
    apiKey: '${keyValue}',
    baseURL: '${getBaseUrl()}'
});

async function runBatch() {
    // Step 1: Upload a batch input file
    const file = fs.createReadStream('batch_requests.jsonl');
    const batchFile = await client.files.create({
        file: file,
        purpose: 'batch'
    });

    console.log('File ID:', batchFile.id);

    // Step 2: Create a batch job
    const batch = await client.batches.create({
        input_file_id: batchFile.id,
        endpoint: '${batchEndpoint}',
        completion_window: '${completionWindow}'
    });

    console.log('Batch ID:', batch.id);

    // Step 3: Check batch status
    const status = await client.batches.retrieve(batch.id);
    console.log('Status:', status.status);
}

runBatch();`;
    } else {
      return `# Step 1: Upload a batch input file
curl ${getBaseUrl().replace("/v1", "")}/ai/v1/files \\
  -H "Authorization: Bearer ${keyValue}" \\
  -F "file=@batch_requests.jsonl" \\
  -F "purpose=batch"

# Step 2: Create a batch job (use the file ID from step 1)
curl ${getBaseUrl().replace("/v1", "")}/ai/v1/batches \\
  -H "Authorization: Bearer ${keyValue}" \\
  -H "Content-Type: application/json" \\
  -d '{
    "input_file_id": "YOUR_FILE_ID",
    "endpoint": "${batchEndpoint}",
    "completion_window": "${completionWindow}"
  }'

# Step 3: Check batch status (use the batch ID from step 2)
curl ${getBaseUrl().replace("/v1", "")}/ai/v1/batches/YOUR_BATCH_ID \\
  -H "Authorization: Bearer ${keyValue}"`;
    }
  };

  const generatePythonCode = (model: Model, modelType: ModelType) => {
    const keyValue = apiKey || "your-api-key-here";
    if (modelType === "embeddings") {
      return `from openai import OpenAI

client = OpenAI(
    api_key="${keyValue}",
    base_url="${getBaseUrl()}"
)

response = client.embeddings.create(
    model="${model.alias}",
    input="Your text to embed here"
)

print(response.data[0].embedding[:5])`;
    }
    if (modelType === "reranker") {
      return `import requests

response = requests.post(
    "${getBaseUrl()}/rerank",
    headers={
        "Authorization": "Bearer ${keyValue}",
        "Content-Type": "application/json"
    },
    json={
        "model": "${model.alias}",
        "query": "What is the capital of France?",
        "documents": ["Paris is the capital of France.", "London is the capital of England."]
    }
)

data = response.json()
for result in data["results"]:
    print(f"Document {result['index']}: score {result['relevance_score']}")`;
    }
    return `from openai import OpenAI

client = OpenAI(
    api_key="${keyValue}",
    base_url="${getBaseUrl()}"
)

response = client.chat.completions.create(
    model="${model.alias}",
    messages=[
        {"role": "user", "content": "Hello! How can you help me today?"}
    ]
)

print(response.choices[0].message.content)`;
  };

  const generateJavaScriptCode = (model: Model, modelType: ModelType) => {
    const keyValue = apiKey || "your-api-key-here";
    if (modelType === "embeddings") {
      return `import OpenAI from 'openai';

const client = new OpenAI({
    apiKey: '${keyValue}',
    baseURL: '${getBaseUrl()}'
});

const response = await client.embeddings.create({
    model: '${model.alias}',
    input: 'Your text to embed here'
});

console.log(response.data[0].embedding.slice(0, 5));`;
    }
    if (modelType === "reranker") {
      return `const response = await fetch('${getBaseUrl()}/rerank', {
    method: 'POST',
    headers: {
        'Authorization': 'Bearer ${keyValue}',
        'Content-Type': 'application/json'
    },
    body: JSON.stringify({
        model: '${model.alias}',
        query: 'What is the capital of France?',
        documents: ['Paris is the capital of France.', 'London is the capital of England.']
    })
});

const data = await response.json();
data.results.forEach(result => {
    console.log(\`Document \${result.index}: score \${result.relevance_score}\`);
});`;
    }
    return `import OpenAI from 'openai';

const client = new OpenAI({
    apiKey: '${keyValue}',
    baseURL: '${getBaseUrl()}'
});

const response = await client.chat.completions.create({
    model: '${model.alias}',
    messages: [
        { role: 'user', content: 'Hello! How can you help me today?' }
    ]
});

console.log(response.choices[0].message.content);`;
  };

  const generateCurlCode = (model: Model, modelType: ModelType) => {
    const keyValue = apiKey || "your-api-key-here";
    if (modelType === "embeddings") {
      return `curl ${getBaseUrl()}/embeddings \\
  -H "Authorization: Bearer ${keyValue}" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${model.alias}",
    "input": "Your text to embed here"
  }'`;
    }
    if (modelType === "reranker") {
      return `curl ${getBaseUrl()}/rerank \\
  -H "Authorization: Bearer ${keyValue}" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${model.alias}",
    "query": "What is the capital of France?",
    "documents": ["Paris is the capital of France.", "London is the capital of England."]
  }'`;
    }
    return `curl ${getBaseUrl()}/chat/completions \\
  -H "Authorization: Bearer ${keyValue}" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "${model.alias}",
    "messages": [
      {
        "role": "user",
        "content": "Hello! How can you help me today?"
      }
    ]
  }'`;
  };

  const getCurrentCode = () => {
    if (exampleType === "batch" || exampleType === "async") {
      return generateBatchApiCode(selectedLanguage);
    }

    if (!model) return "";

    const modelType = (model.model_type?.toLowerCase() || "chat") as ModelType;

    switch (selectedLanguage) {
      case "python":
        return generatePythonCode(model, modelType);
      case "javascript":
        return generateJavaScriptCode(model, modelType);
      case "curl":
        return generateCurlCode(model, modelType);
      default:
        return "";
    }
  };

  const getLanguageForHighlighting = (language: Language) => {
    switch (language) {
      case "python":
        return "python";
      case "javascript":
        return "javascript";
      case "curl":
        return "bash";
      default:
        return "text";
    }
  };

  const getInstallationInfo = (language: Language) => {
    switch (language) {
      case "python":
        return {
          title: "Python Setup",
          command: "pip install openai",
          description: "Install the OpenAI Python library to get started",
        };
      case "javascript":
        return {
          title: "JavaScript Setup",
          command: "npm install openai",
          description: "Install the OpenAI JavaScript library to get started",
        };
      case "curl":
        return {
          title: "cURL Setup",
          command: null,
          description:
            "cURL is pre-installed on most systems. No additional setup required.",
        };
      default:
        return null;
    }
  };

  const isBatchTab = exampleType === "batch" || exampleType === "async";

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-4xl max-h-[90vh] overflow-y-auto overflow-x-hidden">
        <DialogHeader>
          <DialogTitle>API Examples</DialogTitle>
          <DialogDescription>
            {model
              ? `Code examples for integrating with ${model.alias}`
              : "Select a model to see code examples"}
          </DialogDescription>
        </DialogHeader>

        <AlertBox variant="error" className="mb-4">
          {error}
        </AlertBox>

        {/* Model selector — always shown, editable when no model was passed in */}
        <div className="mb-4">
          <Label className="text-xs text-muted-foreground mb-1 block">
            Model
          </Label>
          <Combobox
            options={modelOptions}
            value={selectedModelId}
            onValueChange={setSelectedModelId}
            placeholder="Select a model..."
            searchPlaceholder="Search models..."
            emptyMessage="No models found."
            className="w-full"
          />
        </div>

        {model && (
          <div className="w-full overflow-hidden">
            {/* Example Type Selection — Three tabs */}
            <div className="mb-6">
              <ToggleGroup
                type="single"
                value={exampleType}
                onValueChange={(value) =>
                  value && setExampleType(value as ExampleType)
                }
                className="inline-flex"
                variant="outline"
                size="sm"
              >
                {!isBatchDenied(model) && (
                  <ToggleGroupItem
                    value="batch"
                    aria-label="Batch API (24h)"
                    className="px-5 py-1.5"
                  >
                    Batch
                  </ToggleGroupItem>
                )}
                {!isBatchDenied(model) && config?.batches?.async_requests?.enabled && (
                  <ToggleGroupItem
                    value="async"
                    aria-label={`Async API (${asyncWindow})`}
                    className="px-5 py-1.5"
                  >
                    Async
                  </ToggleGroupItem>
                )}
                {!isRealtimeDenied(model) && (
                  <ToggleGroupItem
                    value="realtime"
                    aria-label="Realtime API"
                    className="px-5 py-1.5"
                  >
                    Realtime
                  </ToggleGroupItem>
                )}
              </ToggleGroup>

              {/* JSONL example for batch/async tabs */}
              {isBatchTab && (
                <div className="mt-4 space-y-3">
                  <div className="bg-white border border-gray-200 rounded-lg overflow-hidden max-w-full">
                    <div className="bg-gray-50 px-4 py-2 border-b border-gray-200 flex items-center justify-between">
                      <div className="flex items-center gap-2">
                        <Code className="w-4 h-4 text-gray-600" />
                        <span className="text-sm font-medium text-gray-700">
                          batch_requests.jsonl
                        </span>
                      </div>
                      <div className="flex items-center gap-2">
                        <button
                          onClick={() =>
                            copyToClipboard(getExampleJsonl(), "jsonl")
                          }
                          className="flex items-center gap-1 px-2 py-1 text-xs text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded transition-colors"
                        >
                          <Copy className="w-3 h-3" />
                          {copiedCode === "jsonl" ? "Copied!" : "Copy"}
                        </button>
                        <button
                          onClick={downloadJsonl}
                          className="flex items-center gap-1 px-2 py-1 text-xs text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded transition-colors"
                        >
                          <Download className="w-3 h-3" />
                          Download
                        </button>
                      </div>
                    </div>
                    <div className="overflow-x-auto max-w-full">
                      <CodeBlock language="json">
                        {getExampleJsonl()}
                      </CodeBlock>
                    </div>
                  </div>
                  {config?.docs_jsonl_url && (
                    <a
                      href={config.docs_jsonl_url}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="text-sm text-blue-600 hover:text-blue-700 hover:underline inline-flex items-center gap-1"
                    >
                      How to create a JSONL file
                      <ExternalLink className="w-3 h-3" />
                    </a>
                  )}
                </div>
              )}
            </div>

            {/* Code Example */}
            <div className="bg-white border border-gray-200 rounded-lg overflow-hidden max-w-full">
              <div className="bg-gray-50 px-4 py-3 border-b border-gray-200 flex items-center justify-between">
                <div className="flex items-center gap-2">
                  <Code className="w-4 h-4 text-gray-600" />
                  <Select
                    value={selectedLanguage}
                    onValueChange={(value) =>
                      setSelectedLanguage(value as Language)
                    }
                  >
                    <SelectTrigger
                      size="sm"
                      className="h-7 border-0 bg-transparent shadow-none hover:bg-gray-100 focus-visible:ring-0"
                    >
                      <SelectValue>
                        <span className="text-sm font-medium text-gray-700">
                          {selectedLanguage.charAt(0).toUpperCase() +
                            selectedLanguage.slice(1)}
                        </span>
                      </SelectValue>
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="python">Python</SelectItem>
                      <SelectItem value="javascript">JavaScript</SelectItem>
                      <SelectItem value="curl">cURL</SelectItem>
                    </SelectContent>
                  </Select>
                  <div className="relative">
                    <button
                      onMouseEnter={() => setShowInfoTooltip(true)}
                      onMouseLeave={() => setShowInfoTooltip(false)}
                      className="p-1 text-gray-400 hover:text-gray-600 transition-colors"
                    >
                      <Info className="w-4 h-4" />
                    </button>

                    {showInfoTooltip && (
                      <div className="absolute left-0 top-full mt-2 w-64 bg-gray-900 text-white text-xs rounded-lg p-3 shadow-lg z-10">
                        <div className="space-y-2">
                          <div className="font-medium">
                            {getInstallationInfo(selectedLanguage)?.title}
                          </div>
                          <div className="text-gray-300">
                            {getInstallationInfo(selectedLanguage)?.description}
                          </div>
                          {getInstallationInfo(selectedLanguage)?.command && (
                            <div className="bg-gray-800 rounded px-2 py-1 font-mono">
                              {getInstallationInfo(selectedLanguage)?.command}
                            </div>
                          )}
                        </div>
                        <div className="absolute -top-1 left-4 w-2 h-2 bg-gray-900 rotate-45"></div>
                      </div>
                    )}
                  </div>
                </div>
                <div className="flex items-center gap-2">
                  {!apiKey && (
                    <Popover
                      open={showCreateForm}
                      onOpenChange={setShowCreateForm}
                    >
                      <PopoverTrigger asChild>
                        <button className="flex items-center gap-1 px-2 py-1 text-xs text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded transition-colors">
                          <Plus className="w-3 h-3" />
                          Fill API Key
                        </button>
                      </PopoverTrigger>
                      <PopoverContent className="w-80">
                        <form
                          onSubmit={handleCreateApiKey}
                          className="space-y-4"
                        >
                          <div className="space-y-2">
                            <h4 className="font-medium leading-none">
                              Create API Key
                            </h4>
                            <p className="text-sm text-muted-foreground">
                              Generate a new API key for your applications
                            </p>
                          </div>
                          <div className="space-y-2">
                            <Label htmlFor="keyName">Name *</Label>
                            <Input
                              id="keyName"
                              type="text"
                              value={newKeyName}
                              onChange={(e) => setNewKeyName(e.target.value)}
                              placeholder="My API Key"
                              required
                            />
                          </div>
                          <div className="space-y-2">
                            <Label htmlFor="keyDescription">Description</Label>
                            <Textarea
                              id="keyDescription"
                              value={newKeyDescription}
                              onChange={(e) =>
                                setNewKeyDescription(e.target.value)
                              }
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
                              disabled={
                                createApiKeyMutation.isPending ||
                                !newKeyName.trim()
                              }
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
                  )}
                  {apiKey && (
                    <button
                      onClick={() => copyToClipboard(apiKey, "api-key")}
                      className="flex items-center gap-1 px-2 py-1 text-xs text-green-600 hover:text-green-700 hover:bg-green-50 rounded transition-colors"
                    >
                      <Copy className="w-3 h-3" />
                      {copiedCode === "api-key" ? "Copied!" : "Copy Key"}
                    </button>
                  )}
                  <button
                    onClick={() => copyToClipboard(getCurrentCode(), "code")}
                    className="flex items-center gap-1 px-2 py-1 text-xs text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded transition-colors"
                  >
                    <Copy className="w-3 h-3" />
                    {copiedCode === "code" ? "Copied!" : "Copy"}
                  </button>
                </div>
              </div>
              <div className="overflow-x-auto max-w-full">
                <CodeBlock
                  language={
                    getLanguageForHighlighting(selectedLanguage) as
                      | "python"
                      | "javascript"
                      | "bash"
                  }
                >
                  {getCurrentCode()}
                </CodeBlock>
              </div>
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
};

export default ApiExamplesModal;
