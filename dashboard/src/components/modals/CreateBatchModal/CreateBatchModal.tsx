import { useState, useEffect } from "react";
import { Play, AlertCircle, X, Upload, ExternalLink } from "lucide-react";
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
  useCreateBatch,
  useFiles,
  useUploadFile,
  useConfig,
} from "../../../api/control-layer/hooks";
import { toast } from "sonner";
import type { FileObject } from "../../features/batches/types";
import { AlertBox } from "@/components/ui/alert-box";

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
  const [description, setDescription] = useState<string>("");
  const [error, setError] = useState<string | null>(null);
  const [dragActive, setDragActive] = useState(false);
  const [isUploading, setIsUploading] = useState(false);

  const createBatchMutation = useCreateBatch();
  const uploadMutation = useUploadFile();
  const { data: config } = useConfig();

  // Fetch available files for combobox (only input files with purpose "batch")
  const { data: filesResponse } = useFiles({
    purpose: "batch",
    limit: 1000, // Fetch plenty for the dropdown
  });

  const availableFiles = filesResponse?.data || [];

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
  };

  const handleSubmit = async () => {
    let finalFileId = selectedFileId;

    // If a file needs to be uploaded, upload it first
    if (fileToUpload) {
      setIsUploading(true);
      try {
        const uploadedFile = await uploadMutation.mutateAsync({
          file: fileToUpload,
          purpose: "batch",
          expires_after: {
            anchor: "created_at",
            seconds: expirationSeconds,
          },
        });
        finalFileId = uploadedFile.id;
        toast.success(`File "${fileToUpload.name}" uploaded successfully`);
      } catch (error) {
        console.error("Failed to upload file:", error);
        setError(
          error instanceof Error
            ? error.message
            : "Failed to upload file. Please try again.",
        );
        setIsUploading(false);
        return;
      } finally {
        setIsUploading(false);
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
        completion_window: "24h",
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
      setDescription("");
      setExpirationSeconds(2592000);
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
    setDescription("");
    setExpirationSeconds(2592000);
    setError(null);
    onClose();
  };

  const selectedFile = selectedFileId
    ? availableFiles.find((f) => f.id === selectedFileId)
    : null;

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
                          <span className="text-blue-600">Ready to upload</span>
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

          {/* Description (Optional) */}
          <div className="space-y-2">
            <Label htmlFor="description">
              Description <span className="text-gray-400">(optional)</span>
            </Label>
            <Input
              id="description"
              placeholder="e.g., Daily evaluation batch"
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
