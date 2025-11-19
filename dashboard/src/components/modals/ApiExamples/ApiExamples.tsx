import React, { useState } from "react";
import { Copy, Code, Plus, Loader2, Info } from "lucide-react";
import { CodeBlock } from "../../ui/code-block";
import { toast } from "sonner";
import { useCreateApiKey } from "../../../api/control-layer";
import { type ModelType } from "../../../utils/modelType";
import type { Model } from "../../../api/control-layer";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { Input } from "../../ui/input";
import { Textarea } from "../../ui/textarea";
import { Label } from "../../ui/label";
import { Popover, PopoverContent, PopoverTrigger } from "../../ui/popover";
import { ToggleGroup, ToggleGroupItem } from "../../ui/toggle-group";

interface ApiExamplesModalProps {
  isOpen: boolean;
  onClose: () => void;
  model: Model | null;
}

type Language = "python" | "javascript" | "curl";

const ApiExamplesModal: React.FC<ApiExamplesModalProps> = ({
  isOpen,
  onClose,
  model,
}) => {
  const [selectedLanguage, setSelectedLanguage] = useState<Language>("python");
  const [apiKey, setApiKey] = useState<string | null>(null);
  const [copiedCode, setCopiedCode] = useState<string | null>(null);

  // API Key creation popover states
  const [showCreateForm, setShowCreateForm] = useState(false);
  const [newKeyName, setNewKeyName] = useState("");
  const [newKeyDescription, setNewKeyDescription] = useState("");
  const [showInfoTooltip, setShowInfoTooltip] = useState(false);

  const createApiKeyMutation = useCreateApiKey();

  const handleCreateApiKey = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!newKeyName.trim()) return;

    try {
      const response = await createApiKeyMutation.mutateAsync({
        data: {
          name: newKeyName.trim(),
          description: newKeyDescription.trim() || undefined,
          purpose: "inference",
        },
      });

      setApiKey(response.key);
      setNewKeyName("");
      setNewKeyDescription("");
      setShowCreateForm(false);
    } catch (error) {
      console.error("Error generating API key:", error);
    }
  };

  const copyToClipboard = async (text: string, codeType: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopiedCode(codeType);
      const message =
        codeType === "api-key" ? "API key copied to clipboard" : "Code copied to clipboard";
      toast.success(message);
      setTimeout(() => setCopiedCode(null), 2000);
    } catch (err) {
      console.error("Failed to copy to clipboard:", err);
      toast.error("Failed to copy to clipboard");
    }
  };

  const getBaseUrl = () => `${window.location.origin}/ai/v1`;

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

print(response.data[0].embedding)`;
    } else if (modelType === "reranker") {
      return `import requests

url = "${window.location.origin}/ai/rerank"
headers = {
    "Content-Type": "application/json",
    "Authorization": "Bearer ${keyValue}"
}

data = {
    "model": "${model.alias}",
    "query": "What is the capital of France?",
    "documents": [
        "The capital of Brazil is Brasilia.",
        "The capital of France is Paris.",
        "Horses and cows are both animals"
    ]
}

response = requests.post(url, json=data, headers=headers)
result = response.json()

for doc in result["results"]:
    print(f"Score: {doc['relevance_score']:.4f} - {doc['document']['text']}")`;
    } else {
      return `from openai import OpenAI

client = OpenAI(
    api_key="${keyValue}",
    base_url="${getBaseUrl()}"
)

response = client.chat.completions.create(
    model="${model.alias}",
    messages=[
        {"role": "user", "content": "Hello, how are you?"}
    ]
)

print(response.choices[0].message.content)`;
    }
  };

  const generateJavaScriptCode = (model: Model, modelType: ModelType) => {
    const keyValue = apiKey || "your-api-key-here";
    if (modelType === "embeddings") {
      return `import OpenAI from 'openai';

const client = new OpenAI({
    apiKey: '${keyValue}',
    baseURL: '${getBaseUrl()}'
});

async function getEmbedding() {
    const response = await client.embeddings.create({
        model: '${model.alias}',
        input: 'Your text to embed here'
    });

    console.log(response.data[0].embedding);
}

getEmbedding();`;
    } else if (modelType === "reranker") {
      return `async function rerankDocuments() {
    const response = await fetch('${window.location.origin}/ai/rerank', {
        method: 'POST',
        headers: {
            'Content-Type': 'application/json',
            'Authorization': 'Bearer ${keyValue}'
        },
        body: JSON.stringify({
            model: '${model.alias}',
            query: 'What is the capital of France?',
            documents: [
                'The capital of Brazil is Brasilia.',
                'The capital of France is Paris.',
                'Horses and cows are both animals'
            ]
        })
    });

    const result = await response.json();

    result.results.forEach(doc => {
        console.log(\`Score: \${doc.relevance_score.toFixed(4)} - \${doc.document.text}\`);
    });
}

rerankDocuments();`;
    } else {
      return `import OpenAI from 'openai';

const client = new OpenAI({
    apiKey: '${keyValue}',
    baseURL: '${getBaseUrl()}'
});

async function chatCompletion() {
    const response = await client.chat.completions.create({
        model: '${model.alias}',
        messages: [
            { role: 'user', content: 'Hello, how are you?' }
        ]
    });

    console.log(response.choices[0].message.content);
}

chatCompletion();`;
    }
  };

  const generateCurlCode = (model: Model, modelType: ModelType) => {
    const keyValue = apiKey || "your-api-key-here";
    if (modelType === "embeddings") {
      return `curl -X POST "${getBaseUrl()}/embeddings" \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer ${keyValue}" \\
  -d '{
    "model": "${model.alias}",
    "input": "Your text to embed here"
  }'`;
    } else if (modelType === "reranker") {
      return `curl -X POST "${window.location.origin}/ai/rerank" \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer ${keyValue}" \\
  -d '{
    "model": "${model.alias}",
    "query": "What is the capital of France?",
    "documents": [
      "The capital of Brazil is Brasilia.",
      "The capital of France is Paris.",
      "Horses and cows are both animals"
    ]
  }'`;
    } else {
      return `curl -X POST "${getBaseUrl()}/chat/completions" \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer ${keyValue}" \\
  -d '{
    "model": "${model.alias}",
    "messages": [
      {
        "role": "user",
        "content": "Hello, how are you?"
      }
    ]
  }'`;
    }
  };

  const getCurrentCode = () => {
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

  const languageTabs = [
    { id: "python" as Language, label: "Python" },
    { id: "javascript" as Language, label: "JavaScript" },
    { id: "curl" as Language, label: "cURL" },
  ];

  if (!model) {
    return (
      <Dialog open={isOpen} onOpenChange={onClose}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>API Examples</DialogTitle>
            <DialogDescription>No model selected</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button onClick={onClose} variant="outline">
              Close
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    );
  }

  return (
    <>
      <Dialog open={isOpen} onOpenChange={onClose}>
        <DialogContent className="sm:max-w-4xl max-h-[90vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>API Examples</DialogTitle>
            <DialogDescription>
              Code examples for integrating with {model.alias}
            </DialogDescription>
          </DialogHeader>

          <div>
            {/* Language Selection */}
            <div className="mb-6">
              <label className="block text-sm font-medium text-gray-700 mb-2">
                Language
              </label>
              <ToggleGroup
                type="single"
                value={selectedLanguage}
                onValueChange={(value) =>
                  value && setSelectedLanguage(value as Language)
                }
                className="inline-flex"
                variant="outline"
                size="sm"
              >
                {languageTabs.map((tab) => (
                  <ToggleGroupItem
                    key={tab.id}
                    value={tab.id}
                    aria-label={`Select ${tab.label}`}
                    className="px-5 py-1.5"
                  >
                    {tab.label}
                  </ToggleGroupItem>
                ))}
              </ToggleGroup>
            </div>

            {/* Code Example */}
            <div className="bg-white border border-gray-200 rounded-lg overflow-hidden">
              <div className="bg-gray-50 px-4 py-3 border-b border-gray-200 flex items-center justify-between">
                <div className="flex items-center gap-2">
                  <Code className="w-4 h-4 text-gray-600" />
                  <span className="text-sm font-medium text-gray-700">
                    {selectedLanguage.charAt(0).toUpperCase() +
                      selectedLanguage.slice(1)}{" "}
                    Example
                  </span>
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
                        {/* Arrow pointer */}
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
              <div className="p-4">
                <CodeBlock language={getLanguageForHighlighting(selectedLanguage) as "python" | "javascript" | "bash"}>
                  {getCurrentCode()}
                </CodeBlock>
              </div>
            </div>
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
};

export default ApiExamplesModal;
