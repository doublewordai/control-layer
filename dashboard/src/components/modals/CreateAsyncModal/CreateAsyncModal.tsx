import { useState, useCallback, useMemo, useEffect } from "react";
import { AlertCircle, Upload } from "lucide-react";
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
import {
  useCreateBatch,
  useUploadFileWithProgress,
  useModels,
  useFiles,
} from "../../../api/control-layer/hooks";
import { useDebounce } from "../../../hooks/useDebounce";
import { toast } from "sonner";
import { useQueryClient } from "@tanstack/react-query";

interface CreateAsyncModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
  completionWindow?: string;
}

export function CreateAsyncModal({
  isOpen,
  onClose,
  onSuccess,
  completionWindow = "1h",
}: CreateAsyncModalProps) {
  const [activeTab, setActiveTab] = useState<"compose" | "upload">("compose");

  // Compose state
  const [model, setModel] = useState<string>("");
  const [prompts, setPrompts] = useState<string>("");
  const [temperature, setTemperature] = useState<string>("0.7");
  const [maxTokens, setMaxTokens] = useState<string>("4096");

  // Upload state
  const [fileToUpload, setFileToUpload] = useState<File | null>(null);
  const [selectedFileId, setSelectedFileId] = useState<string | null>(null);
  const [dragActive, setDragActive] = useState(false);
  const [fileSearchQuery, setFileSearchQuery] = useState<string>("");
  const debouncedFileSearch = useDebounce(fileSearchQuery, 300);
  const [hasLoadedFiles, setHasLoadedFiles] = useState(false);

  // Shared state
  const [error, setError] = useState<string | null>(null);
  const [isSubmitting, setIsSubmitting] = useState(false);

  const createBatchMutation = useCreateBatch();
  const uploadMutation = useUploadFileWithProgress();

  // Fetch existing batch files for the upload tab
  const { data: filesResponse } = useFiles({
    purpose: "batch",
    limit: 20,
    search: debouncedFileSearch.trim() || undefined,
    own: true,
    enabled: isOpen && activeTab === "upload",
  });
  const availableFiles = filesResponse?.data || [];

  useEffect(() => {
    if (availableFiles.length > 0 && !hasLoadedFiles) {
      setHasLoadedFiles(true);
    }
  }, [availableFiles.length, hasLoadedFiles]);

  const fileOptions = availableFiles.map((file) => ({
    value: file.id,
    label: file.filename,
  }));

  const { data: modelsData } = useModels({
    accessible: true,
    limit: 100,
  });
  const queryClient = useQueryClient();

  const modelOptions = useMemo(
    () =>
      (modelsData?.data ?? []).map((m) => ({
        value: m.alias,
        label: m.alias,
        description: m.model_name !== m.alias ? m.model_name : undefined,
      })),
    [modelsData?.data],
  );

  const allModels = modelsData?.data ?? [];
  const selectedModel = allModels.find((m) => m.alias === model);
  const isEmbeddingsModel =
    selectedModel?.model_type?.toUpperCase() === "EMBEDDINGS";

  const promptLines = prompts
    .split("\n")
    .filter((line) => line.trim().length > 0);
  const requestCount = promptLines.length;

  const buildJsonl = useCallback((): string => {
    const lines = promptLines.map((prompt, index) => {
      if (isEmbeddingsModel) {
        return JSON.stringify({
          custom_id: `req-${index + 1}`,
          method: "POST",
          url: "/v1/embeddings",
          body: {
            model,
            input: prompt.trim(),
          },
        });
      }
      return JSON.stringify({
        custom_id: `req-${index + 1}`,
        method: "POST",
        url: "/v1/chat/completions",
        body: {
          model,
          messages: [{ role: "user", content: prompt.trim() }],
          temperature: parseFloat(temperature) || 0.7,
          max_tokens: parseInt(maxTokens) || 4096,
        },
      });
    });

    return lines.join("\n");
  }, [promptLines, model, isEmbeddingsModel, temperature, maxTokens]);

  const handleSubmit = async () => {
    setError(null);
    setIsSubmitting(true);

    try {
      let fileId: string;

      if (activeTab === "compose") {
        if (!model) {
          setError("Please select a model");
          setIsSubmitting(false);
          return;
        }
        if (requestCount === 0) {
          setError("Please enter at least one prompt");
          setIsSubmitting(false);
          return;
        }

        // Build JSONL and upload as file
        const jsonl = buildJsonl();
        const blob = new Blob([jsonl], { type: "application/jsonl" });
        const timestamp = new Date().toISOString().replace(/[:.]/g, "-");
        const file = new File([blob], `async-requests-${timestamp}.jsonl`, {
          type: "application/jsonl",
        });

        const uploadedFile = await uploadMutation.mutateAsync({
          data: {
            file,
            purpose: "batch",
          },
          onProgress: () => {},
        });
        fileId = uploadedFile.id;
      } else {
        if (selectedFileId) {
          // Use existing file
          fileId = selectedFileId;
        } else if (fileToUpload) {
          // Upload new file
          const uploadedFile = await uploadMutation.mutateAsync({
            data: {
              file: fileToUpload,
              purpose: "batch",
            },
            onProgress: () => {},
          });
          fileId = uploadedFile.id;
        } else {
          setError("Please select or upload a file");
          setIsSubmitting(false);
          return;
        }
      }

      const endpoint = isEmbeddingsModel
        ? "/v1/embeddings"
        : "/v1/chat/completions";

      await createBatchMutation.mutateAsync({
        input_file_id: fileId,
        endpoint,
        completion_window: completionWindow,
      });

      toast.success(
        `Async request${requestCount > 1 ? "s" : ""} created successfully`,
      );

      queryClient.invalidateQueries({ queryKey: ["asyncRequests"] });

      resetForm();
      onSuccess();
      onClose();
    } catch (err) {
      console.error("Failed to create async requests:", err);
      setError(
        err instanceof Error
          ? err.message
          : "Failed to create async requests. Please try again.",
      );
    } finally {
      setIsSubmitting(false);
    }
  };

  const resetForm = () => {
    setModel("");
    setPrompts("");
    setTemperature("0.7");
    setMaxTokens("4096");
    setFileToUpload(null);
    setSelectedFileId(null);
    setFileSearchQuery("");
    setHasLoadedFiles(false);
    setError(null);
    setActiveTab("compose");
  };

  const handleClose = () => {
    resetForm();
    onClose();
  };

  const handleDrop = (e: React.DragEvent) => {
    e.preventDefault();
    setDragActive(false);
    if (e.dataTransfer.files?.[0]) {
      setFileToUpload(e.dataTransfer.files[0]);
      setError(null);
    }
  };

  const handleFileChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    if (e.target.files?.[0]) {
      setFileToUpload(e.target.files[0]);
      setError(null);
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={(open) => !open && handleClose()}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Create Async Requests</DialogTitle>
          <DialogDescription>
            Submit requests for async processing ({completionWindow} completion window)
          </DialogDescription>
        </DialogHeader>

        <Tabs
          value={activeTab}
          onValueChange={(v) => setActiveTab(v as "compose" | "upload")}
        >
          <TabsList className="w-full">
            <TabsTrigger value="compose" className="flex-1">
              Compose
            </TabsTrigger>
            <TabsTrigger value="upload" className="flex-1">
              Upload JSONL
            </TabsTrigger>
          </TabsList>

          <TabsContent value="compose" className="space-y-4 mt-4">
            {/* Model selector */}
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

            {/* Prompts */}
            <div className="space-y-2">
              <div className="flex justify-between items-baseline">
                <Label>Prompts</Label>
                <span className="text-xs text-muted-foreground">
                  Each line is a separate request
                </span>
              </div>
              <Textarea
                placeholder="Enter your prompts, one per line..."
                value={prompts}
                onChange={(e) => setPrompts(e.target.value)}
                rows={6}
              />
            </div>

            {/* Parameters - only for non-embedding models */}
            {!isEmbeddingsModel && model && (
              <div className="flex gap-4">
                <div className="flex-1 space-y-2">
                  <Label>Temperature</Label>
                  <Input
                    type="number"
                    min="0"
                    max="2"
                    step="0.1"
                    value={temperature}
                    onChange={(e) => setTemperature(e.target.value)}
                  />
                </div>
                <div className="flex-1 space-y-2">
                  <Label>Max Tokens</Label>
                  <Input
                    type="number"
                    min="1"
                    value={maxTokens}
                    onChange={(e) => setMaxTokens(e.target.value)}
                  />
                </div>
              </div>
            )}
          </TabsContent>

          <TabsContent value="upload" className="mt-4 space-y-4">
            {/* Existing file picker */}
            {(availableFiles.length > 0 || hasLoadedFiles) && (
              <>
                <div className="space-y-2">
                  <Label>Select an existing file</Label>
                  <Combobox
                    options={fileOptions}
                    value={selectedFileId || ""}
                    onValueChange={(value) => {
                      setSelectedFileId(value);
                      setFileToUpload(null);
                      setError(null);
                    }}
                    onSearchChange={setFileSearchQuery}
                    placeholder="Select an existing file..."
                    searchPlaceholder="Search files..."
                    emptyMessage="No files found."
                    className="w-full"
                  />
                </div>

                <div className="relative py-1">
                  <div className="absolute inset-0 flex items-center">
                    <span className="w-full border-t" />
                  </div>
                  <div className="relative flex justify-center text-xs uppercase">
                    <span className="bg-background px-2 text-muted-foreground">
                      Or
                    </span>
                  </div>
                </div>
              </>
            )}

            {/* Drop zone */}
            <div
              className={`border-2 border-dashed rounded-lg p-12 text-center cursor-pointer transition-colors ${
                dragActive
                  ? "border-primary bg-primary/5"
                  : "border-muted-foreground/20 hover:border-muted-foreground/40"
              }`}
              onDragEnter={(e) => {
                e.preventDefault();
                setDragActive(true);
              }}
              onDragLeave={(e) => {
                e.preventDefault();
                setDragActive(false);
              }}
              onDragOver={(e) => e.preventDefault()}
              onDrop={(e) => {
                handleDrop(e);
                setSelectedFileId(null);
              }}
              onClick={() =>
                document.getElementById("async-file-input")?.click()
              }
            >
              <input
                id="async-file-input"
                type="file"
                accept=".jsonl"
                className="hidden"
                onChange={(e) => {
                  handleFileChange(e);
                  setSelectedFileId(null);
                }}
              />
              {fileToUpload ? (
                <div className="space-y-1">
                  <Upload className="mx-auto h-8 w-8 text-muted-foreground/40" />
                  <p className="text-sm font-medium">{fileToUpload.name}</p>
                  <button
                    className="text-xs text-muted-foreground hover:text-foreground"
                    onClick={(e) => {
                      e.stopPropagation();
                      setFileToUpload(null);
                    }}
                  >
                    Remove
                  </button>
                </div>
              ) : (
                <div className="space-y-1">
                  <Upload className="mx-auto h-8 w-8 text-muted-foreground/40" />
                  <p className="text-sm text-muted-foreground">
                    Drop a .jsonl file here
                  </p>
                  <p className="text-xs text-muted-foreground/60">
                    or click to browse
                  </p>
                </div>
              )}
            </div>
          </TabsContent>
        </Tabs>

        {/* Error */}
        {error && (
          <div className="flex items-start gap-2 text-destructive text-sm">
            <AlertCircle className="h-4 w-4 mt-0.5 flex-shrink-0" />
            <span>{error}</span>
          </div>
        )}

        <DialogFooter>
          <div className="flex w-full items-center justify-between">
            <span className="text-xs text-muted-foreground">
              {activeTab === "compose" && requestCount > 0
                ? `${requestCount} request${requestCount > 1 ? "s" : ""}`
                : ""}
            </span>
            <div className="flex gap-2">
              <Button variant="outline" onClick={handleClose}>
                Cancel
              </Button>
              <Button
                onClick={handleSubmit}
                disabled={
                  isSubmitting ||
                  (activeTab === "compose" && (requestCount === 0 || !model)) ||
                  (activeTab === "upload" && !fileToUpload && !selectedFileId)
                }
              >
                {isSubmitting ? "Creating..." : "Create"}
              </Button>
            </div>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
