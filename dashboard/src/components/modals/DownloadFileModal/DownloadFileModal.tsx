import { useState } from "react";
import { Copy, Code, Download } from "lucide-react";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import { oneDark } from "react-syntax-highlighter/dist/esm/styles/prism";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from "../../ui/dialog";
import { ToggleGroup, ToggleGroupItem } from "../../ui/toggle-group";
import { Button } from "../../ui/button";
import { toast } from "sonner";

interface DownloadFileModalProps {
  isOpen: boolean;
  onClose: () => void;
  title: string;
  description: string;
  resourceType: "file" | "batch-results";
  resourceId: string;
  filename?: string;
}

type Language = "python" | "javascript" | "curl";

export function DownloadFileModal({
  isOpen,
  onClose,
  title,
  description,
  resourceType,
  resourceId,
  filename,
}: DownloadFileModalProps) {
  const [selectedLanguage, setSelectedLanguage] = useState<Language>("python");
  const [copiedCode, setCopiedCode] = useState(false);
  const [downloading, setDownloading] = useState(false);

  const copyToClipboard = (text: string) => {
    navigator.clipboard.writeText(text);
    setCopiedCode(true);
    setTimeout(() => setCopiedCode(false), 2000);
  };

  const getBaseUrl = () => `${window.location.origin}/admin/api/v1`;

  const handleDirectDownload = async () => {
    setDownloading(true);
    try {
      const endpoint =
        resourceType === "file"
          ? `/admin/api/v1/files/${resourceId}/content`
          : `/admin/api/v1/batches/${resourceId}/output`;

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
      toast.error("Failed to download file. Please try again.");
    } finally {
      setDownloading(false);
    }
  };

  const generatePythonCode = () => {
    if (resourceType === "file") {
      return `import requests

# Download file content
url = "${getBaseUrl()}/files/${resourceId}/content"
headers = {
    "Authorization": "Bearer your-api-key-here"
}

response = requests.get(url, headers=headers)

# Save to file
with open("${filename || "downloaded_file.jsonl"}", "wb") as f:
    f.write(response.content)

print("File downloaded successfully!")`;
    } else {
      return `import requests

# Download batch results
url = "${getBaseUrl()}/batches/${resourceId}/output"
headers = {
    "Authorization": "Bearer your-api-key-here"
}

response = requests.get(url, headers=headers)

# Save to file
with open("batch_results_${resourceId}.jsonl", "wb") as f:
    f.write(response.content)

print("Results downloaded successfully!")`;
    }
  };

  const generateJavaScriptCode = () => {
    if (resourceType === "file") {
      return `import fs from 'fs';

// Download file content
const url = '${getBaseUrl()}/files/${resourceId}/content';
const headers = {
    'Authorization': 'Bearer your-api-key-here'
};

const response = await fetch(url, { headers });
const blob = await response.blob();

// Save to file
const arrayBuffer = await blob.arrayBuffer();
const buffer = Buffer.from(arrayBuffer);
fs.writeFileSync('${filename || "downloaded_file.jsonl"}', buffer);

console.log('File downloaded successfully!');`;
    } else {
      return `import fs from 'fs';

// Download batch results
const url = '${getBaseUrl()}/batches/${resourceId}/output';
const headers = {
    'Authorization': 'Bearer your-api-key-here'
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
    if (resourceType === "file") {
      return `curl -X GET "${getBaseUrl()}/files/${resourceId}/content" \\
  -H "Authorization: Bearer your-api-key-here" \\
  -o "${filename || "downloaded_file.jsonl"}"`;
    } else {
      return `curl -X GET "${getBaseUrl()}/batches/${resourceId}/output" \\
  -H "Authorization: Bearer your-api-key-here" \\
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

  const languageTabs = [
    { id: "python" as Language, label: "Python" },
    { id: "javascript" as Language, label: "JavaScript" },
    { id: "curl" as Language, label: "cURL" },
  ];

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-3xl max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>
            Choose how you'd like to download this file
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          {/* Direct Download Option */}
          <div className="bg-gradient-to-r from-blue-50 to-indigo-50 border border-blue-200 rounded-lg p-4">
            <div className="flex items-start justify-between gap-4">
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
              >
                <Download className="w-4 h-4 mr-2" />
                {downloading ? "Downloading..." : "Download Now"}
              </Button>
            </div>
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
            
            {/* Language Selection */}
            <div className="mb-4">
              <label className="block text-sm font-medium text-gray-700 mb-2">
                Select Language
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
                    Code Example
                  </span>
                </div>
                <button
                  onClick={() => copyToClipboard(getCurrentCode())}
                  className="flex items-center gap-1 px-2 py-1 text-xs text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded transition-colors"
                >
                  <Copy className="w-3 h-3" />
                  {copiedCode ? "Copied!" : "Copy"}
                </button>
              </div>
              <div className="p-0">
                <SyntaxHighlighter
                  language={getLanguageForHighlighting(selectedLanguage)}
                  style={oneDark}
                  customStyle={{
                    margin: 0,
                    borderRadius: 0,
                    fontSize: "14px",
                    padding: "16px",
                  }}
                  showLineNumbers={false}
                  wrapLines={true}
                  wrapLongLines={true}
                >
                  {getCurrentCode()}
                </SyntaxHighlighter>
              </div>
            </div>

            <div className="bg-blue-50 border border-blue-200 rounded-lg p-3 mt-3">
              <p className="text-sm text-blue-800">
                <strong>Note:</strong> Replace <code>your-api-key-here</code>{" "}
                with your actual API key. You can create one in the API Keys
                section.
              </p>
            </div>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}