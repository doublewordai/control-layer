import { useState, useEffect } from "react";
import { Rocket, AlertCircle } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { Label } from "../../ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../ui/select";
import { Input } from "../../ui/input";
import { useCreateBatch, useFiles } from "../../../api/control-layer/hooks";
import { toast } from "sonner";

interface CreateBatchModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess?: () => void;
  preselectedFileId?: string;
}

const ENDPOINTS = [
  { value: "/v1/chat/completions", label: "Chat Completions" },
  { value: "/v1/completions", label: "Completions" },
  { value: "/v1/embeddings", label: "Embeddings" },
];

export function CreateBatchModal({
  isOpen,
  onClose,
  onSuccess,
  preselectedFileId,
}: CreateBatchModalProps) {
  const [selectedFileId, setSelectedFileId] = useState<string>(
    preselectedFileId || "",
  );
  const [endpoint, setEndpoint] = useState<string>("/v1/chat/completions");
  const [description, setDescription] = useState<string>("");

  const { data: filesResponse, isLoading: filesLoading } = useFiles({
    purpose: "batch",
  });
  const createBatchMutation = useCreateBatch();

  const files = filesResponse?.data || [];

  // Update selected file when preselected changes
  useEffect(() => {
    if (preselectedFileId) {
      setSelectedFileId(preselectedFileId);
    }
  }, [preselectedFileId]);

  const handleSubmit = async () => {
    if (!selectedFileId) {
      toast.error("Please select a file");
      return;
    }

    try {
      const metadata: Record<string, string> = {};
      if (description) {
        metadata.batch_description = description;
      }

      await createBatchMutation.mutateAsync({
        input_file_id: selectedFileId,
        endpoint,
        completion_window: "24h",
        metadata: Object.keys(metadata).length > 0 ? metadata : undefined,
      });

      const selectedFile = files.find((f) => f.id === selectedFileId);
      toast.success(
        `Batch created successfully from "${selectedFile?.filename || "file"}"`,
      );
      
      setSelectedFileId("");
      setEndpoint("/v1/chat/completions");
      setDescription("");
      onSuccess?.();
      onClose();
    } catch (error) {
      console.error("Failed to create batch:", error);
      toast.error(
        error instanceof Error
          ? error.message
          : "Failed to create batch. Please try again.",
      );
    }
  };

  const handleClose = () => {
    setSelectedFileId(preselectedFileId || "");
    setEndpoint("/v1/chat/completions");
    setDescription("");
    onClose();
  };

  const selectedFile = files.find((f) => f.id === selectedFileId);

  return (
    <Dialog open={isOpen} onOpenChange={handleClose}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Create New Batch</DialogTitle>
        </DialogHeader>

        <div className="space-y-6">
          {/* File Selection */}
          <div className="space-y-2">
            <Label htmlFor="file">Select File</Label>
            <Select
              value={selectedFileId}
              onValueChange={setSelectedFileId}
              disabled={filesLoading || !!preselectedFileId}
            >
              <SelectTrigger>
                <SelectValue placeholder="Choose a file..." />
              </SelectTrigger>
              <SelectContent>
                {filesLoading ? (
                  <SelectItem value="loading" disabled>
                    Loading files...
                  </SelectItem>
                ) : files.length === 0 ? (
                  <SelectItem value="none" disabled>
                    No files available
                  </SelectItem>
                ) : (
                  files.map((file) => (
                    <SelectItem key={file.id} value={file.id}>
                      {file.filename} ({(file.bytes / 1024).toFixed(1)} KB)
                    </SelectItem>
                  ))
                )}
              </SelectContent>
            </Select>
            {preselectedFileId && (
              <p className="text-xs text-gray-500">
                File is pre-selected from the files table
              </p>
            )}
          </div>

          {/* File Info */}
          {selectedFile && (
            <div className="bg-gray-50 rounded-lg p-3 space-y-1">
              <p className="text-sm font-medium text-gray-900">
                {selectedFile.filename}
              </p>
              <div className="flex gap-4 text-xs text-gray-600">
                <span>Size: {(selectedFile.bytes / 1024).toFixed(1)} KB</span>
                <span>ID: {selectedFile.id}</span>
              </div>
            </div>
          )}

          {/* Endpoint Selection */}
          <div className="space-y-2">
            <Label htmlFor="endpoint">Endpoint</Label>
            <Select value={endpoint} onValueChange={setEndpoint}>
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {ENDPOINTS.map((ep) => (
                  <SelectItem key={ep.value} value={ep.value}>
                    {ep.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-xs text-gray-500">
              All requests in the file will use this endpoint
            </p>
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
              maxLength={512}
            />
            <p className="text-xs text-gray-500">
              Add a description to help identify this batch later
            </p>
          </div>

          {/* Info Box */}
          <div className="bg-blue-50 border border-blue-200 rounded-lg p-3">
            <div className="flex gap-2">
              <AlertCircle className="w-4 h-4 text-blue-600 mt-0.5 flex-shrink-0" />
              <div className="text-sm text-blue-800">
                <p className="font-medium mb-1">Batch Processing</p>
                <p className="text-blue-700">
                  The batch will process all requests in the selected file. You
                  can track progress and download results once completed.
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
            disabled={createBatchMutation.isPending}
          >
            Cancel
          </Button>
          <Button
            type="button"
            onClick={handleSubmit}
            disabled={!selectedFileId || createBatchMutation.isPending}
          >
            {createBatchMutation.isPending ? (
              <>
                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white mr-2"></div>
                Creating...
              </>
            ) : (
              <>
                <Rocket className="w-4 h-4 mr-2" />
                Create Batch
              </>
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}