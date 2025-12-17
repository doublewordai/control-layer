import { useState } from "react";
import { Copy, Code, Download, Plus, Loader2 } from "lucide-react";
import { CodeBlock } from "../../ui/code-block";
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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../ui/select";
import { toast } from "sonner";
import { useCreateApiKey } from "../../../api/control-layer/hooks";
import { AlertBox } from "@/components/ui/alert-box";

interface DownloadFileModalProps {
  isOpen: boolean;
  onClose: () => void;
  title: string;
  description: string;
  resourceType: "file" | "batch-results";
  resourceId: string;
  filename?: string;
  isPartial?: boolean;
}

type Language = "python" | "javascript" | "curl";

export function DownloadFileModal({
  isOpen,
  onClose,
  title,
  resourceType,
  resourceId,
  filename,
  isPartial,
}: DownloadFileModalProps) {
  const [selectedLanguage, setSelectedLanguage] = useState<Language>("python");
  const [copiedCode, setCopiedCode] = useState<string | null>(null);
  const [downloading, setDownloading] = useState(false);
  const [apiKey, setApiKey] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // API Key creation popover states
  const [showCreateForm, setShowCreateForm] = useState(false);
  const [newKeyName, setNewKeyName] = useState("");
  const [newKeyDescription, setNewKeyDescription] = useState("");

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
      toast.success("API key created successfully!");
    } catch (error) {
      console.error("Error generating API key:", error);
      setError("Failed to create API key. Please try again.");
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
      console.error("Failed to copy to clipboard:", err);
      setError("Failed to copy to clipboard");
    }
  };

  const getBaseUrl = () => `https://api.doubleword.ai/v1`;

  const handleDirectDownload = async () => {
    setDownloading(true);
    try {
      const endpoint =
        resourceType === "file"
          ? `/ai/v1/files/${resourceId}/content`
          : `/ai/v1/batches/${resourceId}/output`;

      const response = await fetch(endpoint, {
        headers: {
          Authorization: `Bearer ${localStorage.getItem("auth_token") || ""}`,
        },
      });

      if (!response.ok) {
        throw new Error("Download failed");
      }

      const blob = await response.blob();
      const url = window.URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download =
        resourceType === "file"
          ? filename || "downloaded_file.jsonl"
          : `batch_results_${resourceId}.jsonl`;
      document.body.appendChild(a);
      a.click();
      window.URL.revokeObjectURL(url);
      document.body.removeChild(a);

      toast.success("File downloaded successfully!");
    } catch (error) {
      console.error("Download failed:", error);
      setError("Failed to download file. Please try again.");
    } finally {
      setDownloading(false);
    }
  };

  const generatePythonCode = () => {
    const keyValue = apiKey || "your-api-key-here";
    if (resourceType === "file") {
      if (isPartial) {
        return `import requests

# Download file content
url = "${getBaseUrl()}/files/${resourceId}/content"
headers = {
    "Authorization": "Bearer ${keyValue}"
}

response = requests.get(url, headers=headers)

# Check if file is incomplete (batch still running)
is_incomplete = response.headers.get("X-Incomplete") == "true"
last_line = response.headers.get("X-Last-Line")

# Save to file
with open("${filename || "downloaded_file.jsonl"}", "wb") as f:
    f.write(response.content)

if is_incomplete:
    print(f"Partial file downloaded (up to line {last_line})")
    print(f"To resume: add ?offset={last_line} to the URL")
else:
    print("Complete file downloaded!")`;
      } else {
        return `import requests

# Download file content
url = "${getBaseUrl()}/files/${resourceId}/content"
headers = {
    "Authorization": "Bearer ${keyValue}"
}

response = requests.get(url, headers=headers)

# Save to file
with open("${filename || "downloaded_file.jsonl"}", "wb") as f:
    f.write(response.content)

print("File downloaded successfully!")`;
      }
    } else {
      return `import requests

# Download batch results
url = "${getBaseUrl()}/batches/${resourceId}/output"
headers = {
    "Authorization": "Bearer ${keyValue}"
}

response = requests.get(url, headers=headers)

# Save to file
with open("batch_results_${resourceId}.jsonl", "wb") as f:
    f.write(response.content)

print("Results downloaded successfully!")`;
    }
  };

  const generateJavaScriptCode = () => {
    const keyValue = apiKey || "your-api-key-here";
    if (resourceType === "file") {
      if (isPartial) {
        return `import fs from 'fs';

// Download file content
const url = '${getBaseUrl()}/files/${resourceId}/content';
const headers = {
    'Authorization': 'Bearer ${keyValue}'
};

const response = await fetch(url, { headers });

// Check if file is incomplete (batch still running)
const isIncomplete = response.headers.get("X-Incomplete") === "true";
const lastLine = response.headers.get("X-Last-Line");

const blob = await response.blob();

// Save to file
const arrayBuffer = await blob.arrayBuffer();
const buffer = Buffer.from(arrayBuffer);
fs.writeFileSync('${filename || "downloaded_file.jsonl"}', buffer);

if (isIncomplete) {
    console.log(\`Partial file downloaded (up to line \${lastLine})\`);
    console.log(\`To resume: add ?offset=\${lastLine} to the URL\`);
} else {
    console.log('Complete file downloaded!');
}`;
      } else {
        return `import fs from 'fs';

// Download file content
const url = '${getBaseUrl()}/files/${resourceId}/content';
const headers = {
    'Authorization': 'Bearer ${keyValue}'
};

const response = await fetch(url, { headers });
const blob = await response.blob();

// Save to file
const arrayBuffer = await blob.arrayBuffer();
const buffer = Buffer.from(arrayBuffer);
fs.writeFileSync('${filename || "downloaded_file.jsonl"}', buffer);

console.log('File downloaded successfully!');`;
      }
    } else {
      return `import fs from 'fs';

// Download batch results
const url = '${getBaseUrl()}/batches/${resourceId}/output';
const headers = {
    'Authorization': 'Bearer ${keyValue}'
};

const response = await fetch(url, { headers });
const blob = await response.blob();

// Save to file
const arrayBuffer = await blob.arrayBuffer();
const buffer = Buffer.from(arrayBuffer);
fs.writeFileSync('batch_results_${resourceId}.jsonl', buffer);

console.log('Results downloaded successfully!');`;
    }
  };

  const generateCurlCode = () => {
    const keyValue = apiKey || "your-api-key-here";
    if (resourceType === "file") {
      if (isPartial) {
        return `# Download file and show headers (to check if incomplete)
curl -i -X GET "${getBaseUrl()}/files/${resourceId}/content" \\
  -H "Authorization: Bearer ${keyValue}" \\
  -o "${filename || "downloaded_file.jsonl"}"

# Headers to check:
#   X-Incomplete: true/false (if true, batch is still running)
#   X-Last-Line: N (use this as offset to resume: ?offset=N)

# To resume download from line N:
# curl -X GET "${getBaseUrl()}/files/${resourceId}/content?offset=N" \\
#   -H "Authorization: Bearer ${keyValue}" \\
#   -o "${filename || "downloaded_file.jsonl"}"`;
      } else {
        return `curl -X GET "${getBaseUrl()}/files/${resourceId}/content" \\
  -H "Authorization: Bearer ${keyValue}" \\
  -o "${filename || "downloaded_file.jsonl"}"`;
      }
    } else {
      return `curl -X GET "${getBaseUrl()}/batches/${resourceId}/output" \\
  -H "Authorization: Bearer ${keyValue}" \\
  -o "batch_results_${resourceId}.jsonl"`;
    }
  };

  const getCurrentCode = () => {
    switch (selectedLanguage) {
      case "python":
        return generatePythonCode();
      case "javascript":
        return generateJavaScriptCode();
      case "curl":
        return generateCurlCode();
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

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-3xl max-h-[90vh] overflow-y-auto overflow-x-hidden">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>
            Choose how you'd like to download this file
          </DialogDescription>
        </DialogHeader>

        <AlertBox variant="error" className="mb-4">
          {error}
        </AlertBox>

        <div className="space-y-4 overflow-x-hidden">
          {/* Direct Download Option */}
          <div className="bg-linear-to-r from-blue-50 to-indigo-50 border border-blue-200 rounded-lg p-4">
            <div className="flex items-start justify-between gap-4 mb-3">
              <div className="flex-1">
                <h3 className="text-sm font-semibold text-gray-900 mb-1">
                  Option 1: Direct Download
                </h3>
                <p className="text-sm text-gray-600">
                  Download the file directly to your computer
                </p>
              </div>
              <Button
                onClick={handleDirectDownload}
                disabled={downloading}
                className="shrink-0"
                variant="outline"
              >
                {downloading ? (
                  <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                ) : (
                  <Download className="w-4 h-4 mr-2" />
                )}
                {downloading ? "Downloading..." : "Download Now"}
              </Button>
            </div>

            {/* Info for output/error files about partial results */}
            {isPartial && (
              <div className="pt-3 border-t border-blue-300">
                <h4 className="text-sm font-semibold text-blue-900 mb-2 flex items-center gap-2">
                  <Loader2 className="w-4 h-4" />
                  Partial File Downloads
                </h4>
                <p className="text-xs text-blue-800">
                  This batch is still running. Downloads will return partial
                  results.
                </p>
              </div>
            )}
          </div>

          {/* Divider */}
          <div className="relative">
            <div className="absolute inset-0 flex items-center">
              <div className="w-full border-t border-gray-200"></div>
            </div>
            <div className="relative flex justify-center text-sm">
              <span className="px-2 bg-white text-gray-500">or</span>
            </div>
          </div>

          {/* API Code Option */}
          <div>
            <h3 className="text-sm font-semibold text-gray-900 mb-3">
              Option 2: Download via API
            </h3>

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
                                <Loader2 className="w-3 h-3 animate-spin mr-1" />
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
        </div>
      </DialogContent>
    </Dialog>
  );
}
