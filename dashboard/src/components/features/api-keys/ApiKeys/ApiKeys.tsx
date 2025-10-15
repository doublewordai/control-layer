import React, { useState } from "react";
import { Key, Plus, Trash2, Copy, Loader2, Check } from "lucide-react";
import {
  useApiKeys,
  useCreateApiKey,
  useDeleteApiKey,
  type ApiKeyCreateResponse,
} from "../../../../api/waycast";
import { useUser } from "../../../../api/waycast/hooks";
import { DataTable } from "../../../ui/data-table";
import { createColumns } from "./columns";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from "../../../ui/dialog";
import { Button } from "../../../ui/button";
import { Input } from "../../../ui/input";
import { Textarea } from "../../../ui/textarea";
import { Label } from "../../../ui/label";

export const ApiKeys: React.FC = () => {
  const { data: user } = useUser("current");
  const [showCreateForm, setShowCreateForm] = useState(false);
  const [newKeyName, setNewKeyName] = useState("");
  const [newKeyDescription, setNewKeyDescription] = useState("");
  const [newKeyRequestsPerSecond, setNewKeyRequestsPerSecond] = useState<
    number | ""
  >("");
  const [newKeyBurstSize, setNewKeyBurstSize] = useState<number | "">("");
  const [newKeyResponse, setNewKeyResponse] =
    useState<ApiKeyCreateResponse | null>(null);
  const [deleteModal, setDeleteModal] = useState<{
    keyId: string;
    keyName: string;
  } | null>(null);
  const [copiedKey, setCopiedKey] = useState<string | null>(null);
  const [selectedKeys, setSelectedKeys] = useState<any[]>([]);
  const [showBulkDeleteModal, setShowBulkDeleteModal] = useState(false);

  const {
    data: apiKeys = [],
    isLoading,
    error,
  } = useApiKeys(user?.id || "current");
  const createApiKeyMutation = useCreateApiKey();
  const deleteApiKeyMutation = useDeleteApiKey();

  const handleCreateApiKey = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!newKeyName.trim()) return;

    const newKey = await createApiKeyMutation.mutateAsync({
      data: {
        name: newKeyName.trim(),
        description: newKeyDescription.trim() || undefined,
        requests_per_second:
          newKeyRequestsPerSecond === ""
            ? null
            : Number(newKeyRequestsPerSecond),
        burst_size: newKeyBurstSize === "" ? null : Number(newKeyBurstSize),
      },
      userId: user?.id || "current",
    });

    setNewKeyResponse(newKey);
    // Don't close the form - show success state instead
  };

  const handleDeleteApiKey = (keyId: string) => {
    deleteApiKeyMutation.mutate(
      {
        keyId,
        userId: user?.id || "current",
      },
      {
        onSuccess: () => setDeleteModal(null),
      },
    );
  };

  const handleDeleteFromTable = (apiKey: any) => {
    setDeleteModal({
      keyId: apiKey.id,
      keyName: apiKey.name,
    });
  };

  const handleBulkDelete = async () => {
    try {
      // Delete keys one by one
      for (const key of selectedKeys) {
        await deleteApiKeyMutation.mutateAsync({
          keyId: key.id,
          userId: user?.id || "current",
        });
      }
      setSelectedKeys([]);
      setShowBulkDeleteModal(false);
    } catch (error) {
      console.error("Error deleting API keys:", error);
    }
  };

  const copyToClipboard = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopiedKey(text);

      // Reset the copied state after 2 seconds
      setTimeout(() => {
        setCopiedKey(null);
      }, 2000);
    } catch (err) {
      console.error("Failed to copy to clipboard:", err);
    }
  };

  const columns = createColumns({
    onDelete: handleDeleteFromTable,
  });

  if (isLoading) {
    return (
      <div className="p-6">
        <div className="animate-pulse">
          <div className="h-8 bg-gray-200 rounded w-48 mb-6"></div>
          <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-6">
            <div className="space-y-4">
              {[...Array(3)].map((_, i) => (
                <div key={i} className="h-16 bg-gray-200 rounded"></div>
              ))}
            </div>
          </div>
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
              API Keys
            </h1>
            <p className="text-doubleword-neutral-600">
              Manage your API keys for programmatic access
            </p>
          </div>
          {apiKeys.length > 0 && (
            <Button
              onClick={() => setShowCreateForm(true)}
              className="bg-doubleword-background-dark hover:bg-doubleword-neutral-900"
              aria-label="Create new API key"
            >
              <Plus className="w-4 h-4 mr-2" />
              Create API Key
            </Button>
          )}
        </div>
      </div>

      {error && (
        <div className="mb-6 bg-red-50 border border-red-200 text-red-700 px-4 py-3 rounded-lg">
          {(error as Error)?.message || "An error occurred"}
        </div>
      )}

      {(createApiKeyMutation.isSuccess || deleteApiKeyMutation.isSuccess) &&
        !createApiKeyMutation.isPending &&
        !deleteApiKeyMutation.isPending && (
          <div className="mb-6 bg-green-50 border border-green-200 text-green-700 px-4 py-3 rounded-lg">
            {createApiKeyMutation.isSuccess
              ? "API key created successfully!"
              : "API key deleted successfully"}
          </div>
        )}

      {(createApiKeyMutation.error || deleteApiKeyMutation.error) && (
        <div className="mb-6 bg-red-50 border border-red-200 text-red-700 px-4 py-3 rounded-lg">
          {(createApiKeyMutation.error as Error)?.message ||
            (deleteApiKeyMutation.error as Error)?.message ||
            "An error occurred"}
        </div>
      )}

      {apiKeys.length > 0 ? (
        <DataTable
          columns={columns}
          data={apiKeys}
          searchPlaceholder="Search API keys..."
          searchColumn="name"
          showPagination={apiKeys.length > 10}
          onSelectionChange={setSelectedKeys}
          actionBar={
            <div className="bg-blue-50 border border-blue-200 rounded-lg p-3 mb-4 flex items-center justify-between">
              <div className="flex items-center gap-2">
                <span className="text-sm font-medium text-blue-900">
                  {selectedKeys.length} key
                  {selectedKeys.length !== 1 ? "s" : ""} selected
                </span>
              </div>
              <div className="flex items-center gap-2">
                <button
                  onClick={() => setShowBulkDeleteModal(true)}
                  className="flex items-center gap-1 px-3 py-1.5 bg-red-600 text-white text-sm rounded-md hover:bg-red-700 transition-colors"
                  aria-label={`Delete ${selectedKeys.length} selected API key${selectedKeys.length !== 1 ? "s" : ""}`}
                >
                  <Trash2 className="w-4 h-4" />
                  Delete Selected
                </button>
              </div>
            </div>
          }
        />
      ) : (
        <div
          className="text-center py-12"
          role="status"
          aria-label="No API keys"
        >
          <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
            <Key className="w-8 h-8 text-doubleword-neutral-600" />
          </div>
          <h3
            className="text-lg font-medium text-doubleword-neutral-900 mb-2"
            role="heading"
            aria-level={3}
          >
            No API keys configured
          </h3>
          <p className="text-doubleword-neutral-600 mb-6">
            Create your first API key to start using the API
          </p>
          <Button
            onClick={() => setShowCreateForm(true)}
            className="bg-doubleword-background-dark hover:bg-doubleword-neutral-900"
            aria-label="Create first API key"
          >
            <Plus className="w-4 h-4 mr-2" />
            Create API Key
          </Button>
        </div>
      )}

      {/* Create/Success Modal */}
      <Dialog
        open={showCreateForm}
        onOpenChange={(open) => {
          if (!open) {
            setShowCreateForm(false);
            setNewKeyName("");
            setNewKeyDescription("");
            setNewKeyRequestsPerSecond("");
            setNewKeyBurstSize("");
            setNewKeyResponse(null);
          } else {
            setShowCreateForm(true);
          }
        }}
      >
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>
              {newKeyResponse
                ? "API Key Created Successfully"
                : "Create New API Key"}
            </DialogTitle>
          </DialogHeader>

          {newKeyResponse ? (
            <>
              <div className="space-y-4">
                <div
                  className="p-3 bg-green-50 border border-green-200 rounded-lg"
                  role="alert"
                >
                  <div className="flex items-center gap-2">
                    <Key className="w-4 h-4 text-green-600" />
                    <p className="text-sm text-green-800 font-medium">
                      Save this key - it won't be shown again
                    </p>
                  </div>
                </div>

                <div className="space-y-2">
                  <Label>Key Name</Label>
                  <p className="text-sm text-gray-900">{newKeyResponse.name}</p>
                </div>

                <div className="space-y-2">
                  <Label>API Key</Label>
                  <div className="flex items-center gap-2">
                    <div className="flex-1 overflow-hidden rounded border bg-gray-50">
                      <code className="block text-xs font-mono px-3 py-2 overflow-x-auto whitespace-nowrap">
                        {newKeyResponse.key}
                      </code>
                    </div>
                    <Button
                      type="button"
                      variant="outline"
                      size="icon"
                      onClick={() => copyToClipboard(newKeyResponse.key)}
                      aria-label={
                        copiedKey === newKeyResponse.key
                          ? "API key copied"
                          : "Copy API key"
                      }
                    >
                      {copiedKey === newKeyResponse.key ? (
                        <Check className="h-4 w-4 text-green-600" />
                      ) : (
                        <Copy className="h-4 w-4" />
                      )}
                    </Button>
                  </div>
                </div>
              </div>

              <DialogFooter>
                <Button
                  onClick={() => {
                    setShowCreateForm(false);
                    setNewKeyName("");
                    setNewKeyDescription("");
                    setNewKeyRequestsPerSecond("");
                    setNewKeyBurstSize("");
                    setNewKeyResponse(null);
                  }}
                  className="w-full sm:w-auto"
                >
                  Done
                </Button>
              </DialogFooter>
            </>
          ) : (
            <>
              <form
                id="create-key-form"
                onSubmit={handleCreateApiKey}
                className="space-y-4"
              >
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
                    onChange={(e) => setNewKeyDescription(e.target.value)}
                    placeholder="What will this key be used for?"
                    rows={3}
                    className="resize-none"
                  />
                </div>

                {/* Rate Limiting Section */}
                <div className="space-y-4 border-t pt-4">
                  <div className="space-y-1">
                    <Label className="text-sm font-medium">Rate Limiting</Label>
                    <p className="text-xs text-gray-600">
                      Optional limits for this API key. Leave blank for no
                      limits.
                    </p>
                  </div>

                  <div className="grid grid-cols-2 gap-3">
                    <div className="space-y-2">
                      <Label htmlFor="requestsPerSecond">Requests/Second</Label>
                      <Input
                        id="requestsPerSecond"
                        type="number"
                        min="1"
                        max="10000"
                        step="1"
                        value={newKeyRequestsPerSecond}
                        onChange={(e) =>
                          setNewKeyRequestsPerSecond(
                            e.target.value === "" ? "" : Number(e.target.value),
                          )
                        }
                        placeholder="None"
                      />
                    </div>

                    <div className="space-y-2">
                      <Label htmlFor="burstSize">Burst Size</Label>
                      <Input
                        id="burstSize"
                        type="number"
                        min="1"
                        max="50000"
                        step="1"
                        value={newKeyBurstSize}
                        onChange={(e) =>
                          setNewKeyBurstSize(
                            e.target.value === "" ? "" : Number(e.target.value),
                          )
                        }
                        placeholder="None"
                      />
                    </div>
                  </div>
                </div>
              </form>

              <DialogFooter>
                <Button
                  type="button"
                  variant="outline"
                  onClick={() => {
                    setShowCreateForm(false);
                    setNewKeyName("");
                    setNewKeyDescription("");
                    setNewKeyRequestsPerSecond("");
                    setNewKeyBurstSize("");
                  }}
                >
                  Cancel
                </Button>
                <Button
                  type="submit"
                  form="create-key-form"
                  disabled={
                    createApiKeyMutation.isPending || !newKeyName.trim()
                  }
                >
                  {createApiKeyMutation.isPending && (
                    <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                  )}
                  Create Key
                </Button>
              </DialogFooter>
            </>
          )}
        </DialogContent>
      </Dialog>

      {/* Delete Confirmation Modal */}
      <Dialog open={!!deleteModal} onOpenChange={() => setDeleteModal(null)}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <div className="flex items-center gap-3">
              <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
                <Trash2 className="w-5 h-5 text-red-600" />
              </div>
              <div>
                <DialogTitle>Delete API Key</DialogTitle>
                <p className="text-sm text-gray-600">
                  This action cannot be undone
                </p>
              </div>
            </div>
          </DialogHeader>

          <div className="py-4">
            <p className="text-sm text-gray-700">
              Are you sure you want to delete the API key{" "}
              <strong>"{deleteModal?.keyName}"</strong>? Any applications using
              this key will lose access immediately.
            </p>
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setDeleteModal(null)}
            >
              Cancel
            </Button>
            <Button
              onClick={() =>
                deleteModal && handleDeleteApiKey(deleteModal.keyId)
              }
              disabled={deleteApiKeyMutation.isPending}
              variant="destructive"
            >
              {deleteApiKeyMutation.isPending && (
                <Loader2 className="w-4 h-4 mr-2 animate-spin" />
              )}
              Delete API Key
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Bulk Delete Confirmation Modal */}
      <Dialog open={showBulkDeleteModal} onOpenChange={setShowBulkDeleteModal}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <div className="flex items-center gap-3">
              <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
                <Trash2 className="w-5 h-5 text-red-600" />
              </div>
              <div>
                <DialogTitle>Delete API Keys</DialogTitle>
                <p className="text-sm text-gray-600">
                  This action cannot be undone
                </p>
              </div>
            </div>
          </DialogHeader>

          <div className="space-y-4">
            <p className="text-gray-700">
              Are you sure you want to delete{" "}
              <strong>{selectedKeys.length}</strong> API key
              {selectedKeys.length !== 1 ? "s" : ""}?
            </p>

            <div className="bg-gray-50 rounded-lg p-3 max-h-32 overflow-y-auto">
              <p className="text-sm font-medium text-gray-600 mb-2">
                Keys to be deleted:
              </p>
              <ul className="text-sm text-gray-700 space-y-1">
                {selectedKeys.map((key) => (
                  <li key={key.id} className="flex justify-between">
                    <span>{key.name}</span>
                    <span className="text-gray-500">
                      {key.description || "No description"}
                    </span>
                  </li>
                ))}
              </ul>
            </div>

            <div className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg">
              <p className="text-sm text-yellow-800">
                <strong>Warning:</strong> This will permanently delete{" "}
                {selectedKeys.length > 1 ? "these API keys" : "this API key"}{" "}
                and any applications using{" "}
                {selectedKeys.length > 1 ? "them" : "it"} will lose access
                immediately.
              </p>
            </div>
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setShowBulkDeleteModal(false)}
              disabled={deleteApiKeyMutation.isPending}
            >
              Cancel
            </Button>
            <Button
              type="button"
              variant="destructive"
              onClick={handleBulkDelete}
              disabled={deleteApiKeyMutation.isPending}
            >
              {deleteApiKeyMutation.isPending ? (
                <>
                  <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                  Deleting...
                </>
              ) : (
                <>
                  <Trash2 className="w-4 h-4 mr-2" />
                  Delete {selectedKeys.length} Key
                  {selectedKeys.length !== 1 ? "s" : ""}
                </>
              )}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
};
