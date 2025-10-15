import React, { useState } from "react";
import { AlertTriangle } from "lucide-react";
import { useDeleteEndpoint } from "../../../api/waycast";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { Button } from "../../ui/button";

interface DeleteEndpointModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
  endpointId: string;
  endpointName: string;
  endpointUrl: string;
}

export const DeleteEndpointModal: React.FC<DeleteEndpointModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
  endpointId,
  endpointName,
  endpointUrl,
}) => {
  const [error, setError] = useState<string | null>(null);

  const deleteEndpointMutation = useDeleteEndpoint();

  const handleDelete = async () => {
    setError(null);

    try {
      await deleteEndpointMutation.mutateAsync(endpointId.toString());
      console.log("Endpoint deleted successfully:", {
        endpointId,
        endpointName,
      });
      onSuccess();
      onClose();
    } catch (err) {
      console.error("Failed to delete endpoint:", err);
      setError(
        err instanceof Error ? err.message : "Failed to delete endpoint",
      );
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <div className="flex items-center gap-3">
            <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
              <AlertTriangle className="w-5 h-5 text-red-600" />
            </div>
            <div>
              <DialogTitle>Delete Endpoint</DialogTitle>
              <DialogDescription>
                This action cannot be undone
              </DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <div className="space-y-4">
          {error && (
            <div className="p-3 bg-red-50 border border-red-200 rounded-lg">
              <p className="text-sm text-red-600">{error}</p>
            </div>
          )}

          <p className="text-gray-700">
            Are you sure you want to delete the endpoint{" "}
            <strong>{endpointName}</strong>?
          </p>

          <div
            className="bg-gray-50 rounded-lg p-3"
            role="group"
            aria-labelledby="endpoint-details-heading"
          >
            <h4 id="endpoint-details-heading" className="sr-only">
              Endpoint Details
            </h4>
            <p className="text-sm text-gray-600">
              <strong>Name:</strong> {endpointName}
            </p>
            <p className="text-sm text-gray-600 mt-1">
              <strong>URL:</strong> {endpointUrl}
            </p>
            <p className="text-sm text-gray-600 mt-1">
              <strong>ID:</strong> {endpointId}
            </p>
          </div>

          <div
            className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg"
            role="alert"
            aria-label="Deletion warning"
          >
            <p className="text-sm text-yellow-800">
              <strong>Warning:</strong> This will permanently delete the
              endpoint and remove all associated models. This action cannot be
              undone.
            </p>
          </div>
        </div>

        <DialogFooter>
          <Button
            onClick={onClose}
            disabled={deleteEndpointMutation.isPending}
            variant="outline"
            aria-label="Cancel deletion"
          >
            Cancel
          </Button>
          <Button
            onClick={handleDelete}
            disabled={deleteEndpointMutation.isPending}
            variant="destructive"
            className="gap-2"
            aria-label={
              deleteEndpointMutation.isPending
                ? "Deleting endpoint"
                : "Confirm deletion"
            }
          >
            {deleteEndpointMutation.isPending ? (
              <>
                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white"></div>
                Deleting...
              </>
            ) : (
              <>
                <AlertTriangle className="w-4 h-4" />
                Delete Endpoint
              </>
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
