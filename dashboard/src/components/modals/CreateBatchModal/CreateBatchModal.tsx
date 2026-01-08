import { useState, useEffect, useMemo } from "react";
import { Play, AlertCircle, X, Upload, ExternalLink, Info } from "lucide-react";
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
import { Combobox } from "../../ui/combobox";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../ui/select";
import {
  useCreateBatch,
  useFiles,
  useUploadFileWithProgress,
  useConfig,
  useFileCostEstimate,
} from "../../../api/control-layer/hooks";
import { toast } from "sonner";
import type { FileObject } from "../../features/batches/types";
import { AlertBox } from "@/components/ui/alert-box";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";

interface CreateBatchModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess?: () => void;
  preselectedFile?: FileObject;
  preselectedFileToUpload?: File;
}

export function CreateBatchModal({
  isOpen,
  onClose,
  onSuccess,
  preselectedFile,
  preselectedFileToUpload,
}: CreateBatchModalProps) {
  const [selectedFileId, setSelectedFileId] = useState<string | null>(
    preselectedFile?.id || null,
  );
  const [fileToUpload, setFileToUpload] = useState<File | null>(
    preselectedFileToUpload || null,
  );
  const [expirationSeconds, setExpirationSeconds] = useState<number>(2592000); // 30 days default
  const [endpoint, setEndpoint] = useState<string>("/v1/chat/completions");
  const [completionWindow, setCompletionWindow] = useState<string>("24h"); // Default SLA
  const [description, setDescription] = useState<string>("");
  const [error, setError] = useState<string | null>(null);
  const [dragActive, setDragActive] = useState(false);
  const [isUploading, setIsUploading] = useState(false);
  const [filename, setFilename] = useState<string>("");
  const [uploadProgress, setUploadProgress] = useState<number>(0);

  const createBatchMutation = useCreateBatch();
  const uploadMutation = useUploadFileWithProgress();

  // Fetch config to get available SLAs
  const { data: config } = useConfig();
  const availableSLAs = useMemo(
    () => config?.batches?.allowed_completion_windows || ["24h"],
    [config?.batches?.allowed_completion_windows],
  );

  // Fetch available files for combobox (only input files with purpose "batch")
  const { data: filesResponse } = useFiles({
    purpose: "batch",
    limit: 1000, // Fetch plenty for the dropdown
  });

  const availableFiles = filesResponse?.data || [];

  // Fetch cost estimate for selected file with current SLA
  const { data: costEstimate, isLoading: isLoadingCost } = useFileCostEstimate(
    selectedFileId || undefined,
    completionWindow,
  );

  // Update default SLA when available SLAs change
  useEffect(() => {
    if (availableSLAs.length > 0 && !availableSLAs.includes(completionWindow)) {
      setCompletionWindow(availableSLAs[0]);
    }
  }, [availableSLAs, completionWindow]);

  // Update selected file when preselected file changes
  useEffect(() => {
    if (preselectedFile) {
      setSelectedFileId(preselectedFile.id);
      setFileToUpload(null); // Clear any file to upload
    }
  }, [preselectedFile]);

  // Update selected file when preselected file to upload changes
  useEffect(() => {
    if (preselectedFileToUpload) {
      setFileToUpload(preselectedFileToUpload);
      setSelectedFileId(null);
    }
  }, [preselectedFileToUpload]);

  const handleDrag = (e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    if (e.type === "dragenter" || e.type === "dragover") {
      setDragActive(true);
    } else if (e.type === "dragleave") {
      setDragActive(false);
    }
  };

  const handleDrop = (e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setDragActive(false);

    if (e.dataTransfer.files && e.dataTransfer.files[0]) {
      const droppedFile = e.dataTransfer.files[0];
      if (droppedFile.name.endsWith(".jsonl")) {
        setFileToUpload(droppedFile);
        setFilename(droppedFile.name);
        setSelectedFileId(null); // Clear combobox selection
        setError(null);
      } else {
        setError("Please upload a .jsonl file");
      }
    }
  };

  const handleFileChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    if (e.target.files && e.target.files[0]) {
      const file = e.target.files[0];
      if (file.name.endsWith(".jsonl")) {
        setFileToUpload(file);
        setFilename(file.name);
        setSelectedFileId(null); // Clear combobox selection
        setError(null);
      } else {
        setError("Please upload a .jsonl file");
      }
    }
  };

  const handleRemoveFile = () => {
    setFileToUpload(null);
    setSelectedFileId(null);
    setFilename("");
  };

  const handleSubmit = async () => {
    let finalFileId = selectedFileId;

    // If a file needs to be uploaded, upload it first
    if (fileToUpload) {
      setIsUploading(true);
      setUploadProgress(0);
      try {
        const uploadedFile = await uploadMutation.mutateAsync({
          data: {
            file: fileToUpload,
            purpose: "batch",
            filename: filename || undefined,
            expires_after: {
              anchor: "created_at",
              seconds: expirationSeconds,
            },
          },
          onProgress: setUploadProgress,
        });
        finalFileId = uploadedFile.id;
        toast.success(`File "${filename || fileToUpload.name}" uploaded successfully`);
      } catch (error) {
        console.error("Failed to upload file:", error);
        setError(
          error instanceof Error
            ? error.message
            : "Failed to upload file. Please try again.",
        );
        setIsUploading(false);
        setUploadProgress(0);
        return;
      } finally {
        setIsUploading(false);
        setUploadProgress(0);
      }
    }

    if (!finalFileId) {
      setError("Please select or upload a file");
      return;
    }

    try {
      const metadata: Record<string, string> = {};
      if (description) {
        metadata.batch_description = description;
      }

      await createBatchMutation.mutateAsync({
        input_file_id: finalFileId,
        endpoint,
        completion_window: completionWindow,
        metadata: Object.keys(metadata).length > 0 ? metadata : undefined,
      });

      const fileName =
        fileToUpload?.name ||
        availableFiles.find((f) => f.id === finalFileId)?.filename ||
        "file";
      toast.success(`Batch created successfully from "${fileName}"`);

      // Reset form
      setSelectedFileId(null);
      setFileToUpload(null);
      setEndpoint("/v1/chat/completions");
      setCompletionWindow(availableSLAs[0] || "24h");
      setDescription("");
      setExpirationSeconds(2592000);
      setFilename("");
      setUploadProgress(0);
      setError(null);
      onSuccess?.();
      onClose();
    } catch (error) {
      console.error("Failed to create batch:", error);
      setError("Failed to create batch. Please try again.");
    }
  };

  const handleClose = () => {
    setSelectedFileId(preselectedFile?.id || null);
    setFileToUpload(null);
    setEndpoint("/v1/chat/completions");
    setCompletionWindow(availableSLAs[0] || "24h");
    setDescription("");
    setExpirationSeconds(2592000);
    setFilename("");
    setUploadProgress(0);
    setError(null);
    onClose();
  };

  const selectedFile = selectedFileId
    ? availableFiles.find((f) => f.id === selectedFileId)
    : null;

  // Clear fileToUpload once selectedFile becomes available (after upload + refetch)
  useEffect(() => {
    if (selectedFile && fileToUpload) {
      setFileToUpload(null);
    }
  }, [selectedFile, fileToUpload]);

  const fileOptions = availableFiles.map((file) => ({
    value: file.id,
    label: file.filename,
  }));

  const isPending = createBatchMutation.isPending || isUploading;

  return (
    <Dialog open={isOpen} onOpenChange={handleClose}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Create New Batch</DialogTitle>
          <DialogDescription>
            Select or upload a{" "}
            {config?.docs_jsonl_url ? (
              <a
                href={config.docs_jsonl_url}
                target="_blank"
                rel="noopener noreferrer"
                className="text-blue-600 hover:text-blue-700 hover:underline inline-flex items-center gap-1"
              >
                JSONL file
                <ExternalLink className="w-3 h-3" />
              </a>
            ) : (
              "JSONL file"
            )}{" "}
            to create a batch.
          </DialogDescription>
        </DialogHeader>

        <AlertBox variant="error" className="mb-4">
          {error}
        </AlertBox>

        <div className="space-y-6">
          {/* File Selection/Upload */}
          <div className="space-y-2">
            <Label>File Selection</Label>

            {/* Show selected file or file to upload */}
            {selectedFile || fileToUpload ? (
              <div className="bg-gray-50 rounded-lg p-3 space-y-1 relative">
                <div className="flex items-start justify-between gap-2">
                  <div className="flex-1 min-w-0">
                    <p className="text-sm font-medium text-gray-900 truncate">
                      {fileToUpload?.name || selectedFile?.filename}
                    </p>
                    <div className="flex gap-4 text-xs text-gray-600">
                      {fileToUpload ? (
                        <>
                          <span>
                            Size: {(fileToUpload.size / 1024).toFixed(1)} KB
                          </span>
                          <span className="text-blue-600">
                            {isUploading ? "Uploading..." : "Ready to upload"}
                          </span>
                        </>
                      ) : (
                        selectedFile && (
                          <>
                            <span>
                              Size: {(selectedFile.bytes / 1024).toFixed(1)} KB
                            </span>
                            <span>ID: {selectedFile.id}</span>
                          </>
                        )
                      )}
                    </div>
                  </div>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    onClick={handleRemoveFile}
                    className="h-8 w-8 p-0 text-gray-500 hover:text-red-600 hover:bg-red-50 shrink-0"
                    disabled={isPending}
                  >
                    <X className="w-4 h-4" />
                  </Button>
                </div>
                {/* Upload Progress Bar */}
                {isUploading && (
                  <div className="h-2 bg-gray-200 rounded-full overflow-hidden">
                    <div
                      className="h-full bg-blue-600 rounded-full transition-all duration-150 ease-out"
                      style={{ width: `${uploadProgress}%` }}
                    />
                  </div>
                )}
              </div>
            ) : (
              <>
                {/* Combobox for selecting existing file */}
                {availableFiles.length > 0 && (
                  <div className="space-y-2">
                    <Combobox
                      options={fileOptions}
                      value={selectedFileId || ""}
                      onValueChange={(value) => {
                        setSelectedFileId(value);
                        setFileToUpload(null); // Clear file to upload
                        setError(null);
                      }}
                      placeholder="Select an existing file..."
                      searchPlaceholder="Search files..."
                      emptyMessage="No files found."
                      className="w-full"
                    />
                    <p className="text-xs text-gray-500">
                      Choose from your uploaded batch files
                    </p>
                  </div>
                )}

                {/* Separator */}
                {availableFiles.length > 0 && (
                  <div className="relative py-2">
                    <div className="absolute inset-0 flex items-center">
                      <span className="w-full border-t" />
                    </div>
                    <div className="relative flex justify-center text-xs uppercase">
                      <span className="bg-white px-2 text-muted-foreground">
                        Or
                      </span>
                    </div>
                  </div>
                )}

                {/* Drop zone for new file */}
                <div
                  className={`relative border-2 border-dashed rounded-lg p-6 text-center transition-colors ${
                    dragActive
                      ? "border-blue-500 bg-blue-50"
                      : "border-gray-300 hover:border-gray-400"
                  }`}
                  onDragEnter={handleDrag}
                  onDragLeave={handleDrag}
                  onDragOver={handleDrag}
                  onDrop={handleDrop}
                >
                  <input
                    type="file"
                    id="file-upload-batch"
                    accept=".jsonl"
                    onChange={handleFileChange}
                    className="absolute inset-0 w-full h-full opacity-0 cursor-pointer"
                    disabled={isPending}
                  />

                  <div className="space-y-2 pointer-events-none">
                    <Upload className="w-10 h-10 mx-auto text-gray-400" />
                    <div>
                      <p className="font-medium text-gray-700 text-sm">
                        Drop a .jsonl file here
                      </p>
                      <p className="text-xs text-gray-500">
                        or click to browse
                      </p>
                    </div>
                  </div>
                </div>
              </>
            )}
          </div>

          {/* New Filename (optional, only for files to upload) */}
          {fileToUpload && (
            <div className="space-y-2">
              <Label htmlFor="filename">
                New filename <span className="text-gray-400">(optional)</span>
              </Label>
              <Input
                id="filename"
                value={filename}
                onChange={(e) => setFilename(e.target.value)}
                placeholder={fileToUpload.name}
                disabled={isPending}
              />
            </div>
          )}

          {/* Description (Optional) */}
          <div className="space-y-2">
            <Label htmlFor="description">
              Description <span className="text-gray-400">(optional)</span>
            </Label>
            <Input
              id="description"
              placeholder="e.g., Data generation task"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              onKeyDown={(e) => {
                if (
                  e.key === "Enter" &&
                  !isPending &&
                  (selectedFileId || fileToUpload)
                ) {
                  e.preventDefault();
                  handleSubmit();
                }
              }}
              maxLength={512}
              disabled={isPending}
            />
            <p className="text-xs text-gray-500">
              Add a description to help identify this batch later
            </p>
          </div>

          {/* SLA Selection */}
          <div className="space-y-2">
            <Label htmlFor="completion-window">Completion Window (SLA)</Label>
            <Select
              value={completionWindow}
              onValueChange={setCompletionWindow}
              disabled={isPending}
            >
              <SelectTrigger id="completion-window">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {availableSLAs.map((sla) => (
                  <SelectItem key={sla} value={sla}>
                    {sla}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-xs text-gray-500">
              Select the maximum time allowed for batch completion
            </p>
          </div>

          {/* Cost Estimate */}
          {(selectedFileId || fileToUpload) && (
            <div className="bg-gray-50 border border-gray-200 rounded-lg p-3">
              <div className="space-y-2">
                <div className="flex items-center gap-1.5">
                  <p className="text-sm font-medium text-gray-900">
                    Cost Estimate
                  </p>
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Info className="w-3.5 h-3.5 text-gray-400 cursor-help" />
                    </TooltipTrigger>
                    <TooltipContent side="top" className="max-w-[250px]">
                      Based on {completionWindow} SLA pricing and average output
                      tokens for the requested model(s). Actual cost may vary.
                    </TooltipContent>
                  </Tooltip>
                </div>
                {selectedFileId ? (
                  isLoadingCost ? (
                    <div className="flex items-center gap-2 text-sm text-gray-600">
                      <div className="animate-spin rounded-full h-3 w-3 border-b-2 border-gray-600"></div>
                      Calculating...
                    </div>
                  ) : costEstimate ? (
                    <div className="space-y-1.5">
                      <div className="flex justify-between text-sm">
                        <span className="text-gray-600">Total Requests:</span>
                        <span className="font-medium text-gray-900">
                          {costEstimate.total_requests.toLocaleString()}
                        </span>
                      </div>
                      <div className="flex justify-between text-sm">
                        <span className="text-gray-600">Estimated Cost:</span>
                        <span className="font-semibold text-gray-900">
                          $
                          {parseFloat(
                            costEstimate.total_estimated_cost,
                          ).toFixed(4)}
                        </span>
                      </div>
                    </div>
                  ) : (
                    <p className="text-sm text-gray-500">
                      Cost estimate unavailable
                    </p>
                  )
                ) : (
                  <button
                    type="button"
                    onClick={async () => {
                      if (!fileToUpload || isPending) return;
                      setIsUploading(true);
                      setUploadProgress(0);
                      try {
                        const uploadedFile = await uploadMutation.mutateAsync({
                          data: {
                            file: fileToUpload,
                            purpose: "batch",
                            filename: filename || undefined,
                            expires_after: {
                              anchor: "created_at",
                              seconds: expirationSeconds,
                            },
                          },
                          onProgress: setUploadProgress,
                        });
                        setSelectedFileId(uploadedFile.id);
                        toast.success(
                          `File "${filename || fileToUpload.name}" uploaded successfully`,
                        );
                      } catch (err) {
                        console.error("Failed to upload file:", err);
                        setError(
                          err instanceof Error
                            ? err.message
                            : "Failed to upload file. Please try again.",
                        );
                      } finally {
                        setIsUploading(false);
                        setUploadProgress(0);
                      }
                    }}
                    disabled={isPending}
                    className={`text-xs text-blue-600 hover:text-blue-700 hover:underline disabled:opacity-50 flex items-center gap-1 ${isPending ? "cursor-not-allowed" : "cursor-pointer"}`}
                  >
                    <Upload className="w-3 h-3" />
                    Complete file upload to generate inference cost estimate
                  </button>
                )}
              </div>
            </div>
          )}

          {/* Info Box */}
          <div className="bg-blue-50 border border-blue-200 rounded-lg p-3">
            <div className="flex gap-2">
              <AlertCircle className="w-4 h-4 text-blue-600 mt-0.5 shrink-0" />
              <div className="text-sm text-blue-800">
                <p className="font-medium mb-1">Batch Processing</p>
                <p className="text-blue-700">
                  {fileToUpload
                    ? "The file will be uploaded and the batch will process all requests. "
                    : "The batch will process all requests in the selected file. "}
                  You can track progress and download results once completed.
                </p>
              </div>
            </div>
          </div>
        </div>

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={handleClose}
            disabled={isPending}
          >
            Cancel
          </Button>
          <Button
            type="button"
            variant="outline"
            onClick={handleSubmit}
            disabled={(!selectedFileId && !fileToUpload) || isPending}
            className="group"
          >
            {isPending ? (
              <>
                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white mr-2"></div>
                {isUploading ? "Uploading..." : "Creating..."}
              </>
            ) : (
              <>
                <Play className="w-4 h-4 mr-2 transition-transform group-hover:translate-x-px" />
                Create Batch
              </>
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
