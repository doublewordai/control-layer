import { useState } from "react";
import { Play, AlertCircle } from "lucide-react";
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
import { useCreateBatch } from "../../../api/control-layer/hooks";
import { toast } from "sonner";
import type { FileObject } from "../../features/batches/types";
import { AlertBox } from "@/components/ui/alert-box";

interface CreateBatchModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess?: () => void;
  preselectedFile?: FileObject;
}

export function CreateBatchModal({
  isOpen,
  onClose,
  onSuccess,
  preselectedFile,
}: CreateBatchModalProps) {
  const [endpoint, setEndpoint] = useState<string>("/v1/chat/completions");
  const [description, setDescription] = useState<string>("");
  const [error, setError] = useState<string | null>(null);

  const createBatchMutation = useCreateBatch();

  const handleSubmit = async () => {
    if (!preselectedFile) {
      setError("No file selected");
      return;
    }

    try {
      const metadata: Record<string, string> = {};
      if (description) {
        metadata.batch_description = description;
      }

      await createBatchMutation.mutateAsync({
        input_file_id: preselectedFile.id,
        endpoint,
        completion_window: "24h",
        metadata: Object.keys(metadata).length > 0 ? metadata : undefined,
      });

      toast.success(
        `Batch created successfully from "${preselectedFile.filename}"`,
      );

      setEndpoint("/v1/chat/completions");
      setDescription("");
      onSuccess?.();
      onClose();
    } catch (error) {
      console.error("Failed to create batch:", error);
      setError("Failed to create batch. Please try again.");
    }
  };

  const handleClose = () => {
    setEndpoint("/v1/chat/completions");
    setDescription("");
    onClose();
  };

  return (
    <Dialog open={isOpen} onOpenChange={handleClose}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Create New Batch</DialogTitle>
          <DialogDescription>
            Enter a description for the batch.
          </DialogDescription>
        </DialogHeader>

        <AlertBox variant="error" className="mb-4">
          {error}
        </AlertBox>

        <div className="space-y-6">
          {/* File Info */}
          {preselectedFile && (
            <div className="space-y-2">
              <Label>Selected File</Label>
              <div className="bg-gray-50 rounded-lg p-3 space-y-1">
                <p className="text-sm font-medium text-gray-900">
                  {preselectedFile.filename}
                </p>
                <div className="flex gap-4 text-xs text-gray-600">
                  <span>
                    Size: {(preselectedFile.bytes / 1024).toFixed(1)} KB
                  </span>
                  <span>ID: {preselectedFile.id}</span>
                </div>
              </div>
            </div>
          )}

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
              <AlertCircle className="w-4 h-4 text-blue-600 mt-0.5 shrink-0" />
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
            variant="outline"
            onClick={handleSubmit}
            disabled={!preselectedFile || createBatchMutation.isPending}
            className="group"
          >
            {createBatchMutation.isPending ? (
              <>
                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white mr-2"></div>
                Creating...
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
