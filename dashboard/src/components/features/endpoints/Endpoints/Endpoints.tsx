import { useState } from "react";
import { Server, Plus, Trash2 } from "lucide-react";
import {
  useEndpoints,
  useSynchronizeEndpoint,
  useUpdateEndpoint,
  useDeleteEndpoint,
} from "../../../../api/waycast";
import { Button } from "../../../ui/button";
import { DataTable } from "../../../ui/data-table";
import { createColumns } from "./columns";
import { toast } from "sonner";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from "../../../ui/dialog";
import {
  CreateEndpointModal,
  DeleteEndpointModal,
  EditEndpointModal,
} from "../../../modals";
import type { Endpoint } from "../../../../api/waycast/types";

export function Endpoints() {
  const [showCreateModal, setShowCreateModal] = useState(false);
  const [endpointToDelete, setEndpointToDelete] = useState<Endpoint | null>(
    null,
  );
  const [endpointToEdit, setEndpointToEdit] = useState<Endpoint | null>(null);
  const [selectedEndpoints, setSelectedEndpoints] = useState<Endpoint[]>([]);
  const [showBulkDeleteModal, setShowBulkDeleteModal] = useState(false);

  const { data: endpoints, isLoading, error, refetch } = useEndpoints();
  const synchronizeEndpointMutation = useSynchronizeEndpoint();
  const updateEndpointMutation = useUpdateEndpoint();
  const deleteEndpointMutation = useDeleteEndpoint();

  const handleEdit = (endpoint: Endpoint) => {
    setEndpointToEdit(endpoint);
  };

  const handleInlineEdit = async (
    endpoint: Endpoint,
    field: "name" | "description" | "url",
    value: string,
  ) => {
    // Don't update if value hasn't changed
    if (endpoint[field] === value) return;

    try {
      await updateEndpointMutation.mutateAsync({
        id: endpoint.id.toString(),
        data: {
          [field]: value,
        },
      });
      toast.success(
        `${field.charAt(0).toUpperCase() + field.slice(1)} updated successfully`,
      );
    } catch (error) {
      console.error(`Failed to update endpoint ${field}:`, error);
      toast.error(`Failed to update ${field}. Please try again.`);
    }
  };

  const handleDelete = (endpoint: Endpoint) => {
    setEndpointToDelete(endpoint);
  };

  const handleSynchronize = async (endpoint: Endpoint) => {
    try {
      await synchronizeEndpointMutation.mutateAsync(endpoint.id.toString());
      toast.success("Endpoint synchronized successfully");
    } catch (error) {
      console.error("Failed to synchronize endpoint:", error);
      toast.error("Failed to synchronize endpoint. Please try again.");
    }
  };

  const handleBulkDelete = async () => {
    try {
      // Delete endpoints one by one
      for (const endpoint of selectedEndpoints) {
        await deleteEndpointMutation.mutateAsync(endpoint.id.toString());
      }
      setSelectedEndpoints([]);
      setShowBulkDeleteModal(false);
      toast.success(
        `Successfully deleted ${selectedEndpoints.length} endpoint${selectedEndpoints.length !== 1 ? "s" : ""}`,
      );
      refetch();
    } catch (error) {
      console.error("Error deleting endpoints:", error);
      toast.error("Failed to delete some endpoints. Please try again.");
    }
  };

  const columns = createColumns({
    onEdit: handleInlineEdit,
    onEditModal: handleEdit,
    onDelete: handleDelete,
    onSynchronize: handleSynchronize,
    isSynchronizing: synchronizeEndpointMutation.isPending,
  });

  if (isLoading) {
    return (
      <div className="p-6">
        <div className="animate-pulse space-y-4">
          <div className="h-8 bg-doubleword-neutral-200 rounded w-1/4"></div>
          <div className="h-4 bg-doubleword-neutral-200 rounded w-1/2"></div>
          <div className="space-y-3">
            <div className="h-16 bg-doubleword-neutral-200 rounded"></div>
            <div className="h-16 bg-doubleword-neutral-200 rounded"></div>
            <div className="h-16 bg-doubleword-neutral-200 rounded"></div>
          </div>
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="p-6">
        <div className="bg-red-50 border border-red-200 rounded-lg p-4">
          <h2 className="text-lg font-medium text-red-900 mb-2">
            Error loading endpoints
          </h2>
          <p className="text-red-700">
            Unable to load endpoints. Please try again later.
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="p-6">
      <div className="mb-8">
        <div className="flex items-center justify-between">
          <div>
            <h1 className="text-3xl font-bold text-doubleword-neutral-900 mb-2">
              Endpoints
            </h1>
            <p className="text-doubleword-neutral-600">
              Manage inference endpoints and their model synchronization
            </p>
          </div>
          {endpoints && endpoints.length > 0 && (
            <Button
              onClick={() => setShowCreateModal(true)}
              className="bg-doubleword-background-dark hover:bg-doubleword-neutral-900"
            >
              <Plus className="w-4 h-4 mr-2" />
              Add Endpoint
            </Button>
          )}
        </div>
      </div>

      {endpoints && endpoints.length > 0 ? (
        <DataTable
          columns={columns}
          data={endpoints}
          searchPlaceholder="Search endpoints..."
          searchColumn="name"
          showPagination={endpoints.length > 10}
          onSelectionChange={setSelectedEndpoints}
          actionBar={
            <div className="bg-blue-50 border border-blue-200 rounded-lg p-3 mb-4 flex items-center justify-between">
              <div className="flex items-center gap-2">
                <span className="text-sm font-medium text-blue-900">
                  {selectedEndpoints.length} endpoint
                  {selectedEndpoints.length !== 1 ? "s" : ""} selected
                </span>
              </div>
              <div className="flex items-center gap-2">
                <button
                  onClick={() => setShowBulkDeleteModal(true)}
                  className="flex items-center gap-1 px-3 py-1.5 bg-red-600 text-white text-sm rounded-md hover:bg-red-700 transition-colors"
                >
                  <Trash2 className="w-4 h-4" />
                  Delete Selected
                </button>
              </div>
            </div>
          }
        />
      ) : (
        <div className="text-center py-12">
          <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
            <Server className="w-8 h-8 text-doubleword-neutral-600" />
          </div>
          <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
            No endpoints configured
          </h3>
          <p className="text-doubleword-neutral-600 mb-6">
            Add your first inference endpoint to start syncing models
          </p>
          <Button
            onClick={() => setShowCreateModal(true)}
            className="bg-doubleword-background-dark hover:bg-doubleword-neutral-900"
          >
            <Plus className="w-4 h-4 mr-2" />
            Add Endpoint
          </Button>
        </div>
      )}

      <CreateEndpointModal
        isOpen={showCreateModal}
        onClose={() => setShowCreateModal(false)}
        onSuccess={() => {
          refetch();
        }}
      />

      {endpointToDelete && (
        <DeleteEndpointModal
          isOpen={true}
          onClose={() => setEndpointToDelete(null)}
          onSuccess={() => {
            refetch();
          }}
          endpointId={endpointToDelete.id}
          endpointName={endpointToDelete.name}
          endpointUrl={endpointToDelete.url}
        />
      )}

      {endpointToEdit && (
        <EditEndpointModal
          isOpen={true}
          onClose={() => setEndpointToEdit(null)}
          onSuccess={() => {
            refetch();
          }}
          endpoint={endpointToEdit}
        />
      )}

      {/* Bulk Delete Confirmation Modal */}
      <Dialog open={showBulkDeleteModal} onOpenChange={setShowBulkDeleteModal}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <div className="flex items-center gap-3">
              <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
                <Trash2 className="w-5 h-5 text-red-600" />
              </div>
              <div>
                <DialogTitle>Delete Endpoints</DialogTitle>
                <p className="text-sm text-gray-600">
                  This action cannot be undone
                </p>
              </div>
            </div>
          </DialogHeader>

          <div className="space-y-4">
            <p className="text-gray-700">
              Are you sure you want to delete{" "}
              <strong>{selectedEndpoints.length}</strong> endpoint
              {selectedEndpoints.length !== 1 ? "s" : ""}?
            </p>

            <div className="bg-gray-50 rounded-lg p-3 max-h-32 overflow-y-auto">
              <p className="text-sm font-medium text-gray-600 mb-2">
                Endpoints to be deleted:
              </p>
              <ul className="text-sm text-gray-700 space-y-1">
                {selectedEndpoints.map((endpoint) => (
                  <li key={endpoint.id} className="flex justify-between">
                    <span>{endpoint.name}</span>
                    <span className="text-gray-500 text-xs font-mono">
                      {endpoint.url}
                    </span>
                  </li>
                ))}
              </ul>
            </div>

            <div className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg">
              <p className="text-sm text-yellow-800">
                <strong>Warning:</strong> This will permanently delete{" "}
                {selectedEndpoints.length > 1
                  ? "these endpoints"
                  : "this endpoint"}{" "}
                and stop all model synchronization. This action cannot be
                undone.
              </p>
            </div>
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setShowBulkDeleteModal(false)}
              disabled={deleteEndpointMutation.isPending}
            >
              Cancel
            </Button>
            <Button
              type="button"
              variant="destructive"
              onClick={handleBulkDelete}
              disabled={deleteEndpointMutation.isPending}
            >
              {deleteEndpointMutation.isPending ? (
                <>
                  <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white mr-2"></div>
                  Deleting...
                </>
              ) : (
                <>
                  <Trash2 className="w-4 h-4 mr-2" />
                  Delete {selectedEndpoints.length} Endpoint
                  {selectedEndpoints.length !== 1 ? "s" : ""}
                </>
              )}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
