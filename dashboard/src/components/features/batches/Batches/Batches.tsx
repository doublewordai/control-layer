import { useState } from "react";
import {
  Upload,
  Rocket,
  FileText,
  Activity,
  Trash2,
  Download,
} from "lucide-react";
import { Button } from "../../../ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../../../ui/tabs";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../../ui/alert-dialog";
import { DataTable } from "../../../ui/data-table";
import { UploadFileModal } from "../../../modals/CreateFileModal";
import { CreateBatchModal } from "../../../modals/CreateBatchModal";
import { ViewFileRequestsModal } from "../../../modals/FileRequestsModal";
import { ViewBatchRequestsModal } from "../../../modals/BatchRequestsModal";
import { createFileColumns } from "../FilesTable/columns";
import { createBatchColumns } from "../BatchesTable/columns";
import {
  useFiles,
  useBatches,
  useDeleteFile,
  useCancelBatch,
  useDownloadBatchResults,
} from "../../../../api/control-layer/hooks";
import { toast } from "sonner";
import type { FileObject, Batch } from "../types";

export function Batches() {
  // Modal states
  const [uploadModalOpen, setUploadModalOpen] = useState(false);
  const [createBatchModalOpen, setCreateBatchModalOpen] = useState(false);
  const [viewFileRequestsModalOpen, setViewFileRequestsModalOpen] =
    useState(false);
  const [viewBatchRequestsModalOpen, setViewBatchRequestsModalOpen] =
    useState(false);

  // Selected items
  const [selectedFile, setSelectedFile] = useState<FileObject | null>(null);
  const [selectedBatch, setSelectedBatch] = useState<Batch | null>(null);
  const [preselectedFileId, setPreselectedFileId] = useState<
    string | undefined
  >();

  // Delete confirmation
  const [fileToDelete, setFileToDelete] = useState<FileObject | null>(null);
  const [batchToCancel, setBatchToCancel] = useState<Batch | null>(null);

  // Active tab
  const [activeTab, setActiveTab] = useState<"files" | "batches">("files");

  // API queries
  const { data: filesResponse, isLoading: filesLoading } = useFiles({
    purpose: "batch",
  });
  const { data: batchesResponse, isLoading: batchesLoading } = useBatches();

  // Mutations
  const deleteMutation = useDeleteFile();
  const cancelMutation = useCancelBatch();
  const downloadMutation = useDownloadBatchResults();

  const files = filesResponse?.data || [];
  const batches = batchesResponse?.data || [];

  // File actions
  const handleViewFileRequests = (file: FileObject) => {
    setSelectedFile(file);
    setViewFileRequestsModalOpen(true);
  };

  const handleDeleteFile = (file: FileObject) => {
    setFileToDelete(file);
  };

  const confirmDeleteFile = async () => {
    if (!fileToDelete) return;

    try {
      await deleteMutation.mutateAsync(fileToDelete.id);
      toast.success(`File "${fileToDelete.filename}" deleted successfully`);
      setFileToDelete(null);
    } catch (error) {
      console.error("Failed to delete file:", error);
      toast.error(
        error instanceof Error
          ? error.message
          : "Failed to delete file. Please try again.",
      );
    }
  };

  const handleCreateBatchFromFile = (file: FileObject) => {
    setPreselectedFileId(file.id);
    setCreateBatchModalOpen(true);
  };

  // Batch actions
  const handleViewBatchRequests = (batch: Batch) => {
    setSelectedBatch(batch);
    setViewBatchRequestsModalOpen(true);
  };

  const handleCancelBatch = (batch: Batch) => {
    setBatchToCancel(batch);
  };

  const confirmCancelBatch = async () => {
    if (!batchToCancel) return;

    try {
      await cancelMutation.mutateAsync(batchToCancel.id);
      toast.success(`Batch "${batchToCancel.id}" is being cancelled`);
      setBatchToCancel(null);
    } catch (error) {
      console.error("Failed to cancel batch:", error);
      toast.error(
        error instanceof Error
          ? error.message
          : "Failed to cancel batch. Please try again.",
      );
    }
  };

  const handleDownloadResults = async (batch: Batch) => {
    try {
      await downloadMutation.mutateAsync(batch.id);
      toast.success("Results downloaded successfully");
    } catch (error) {
      console.error("Failed to download results:", error);
      toast.error(
        error instanceof Error
          ? error.message
          : "Failed to download results. Please try again.",
      );
    }
  };

  // Create columns with actions
  const fileColumns = createFileColumns({
    onView: handleViewFileRequests,
    onDelete: handleDeleteFile,
  });

  const batchColumns = createBatchColumns({
    onView: handleViewBatchRequests,
    onCancel: handleCancelBatch,
    onDownload: handleDownloadResults,
  });

  // Loading state
  if (filesLoading || batchesLoading) {
    return (
      <div className="flex items-center justify-center min-h-[400px]">
        <div className="text-center">
          <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-blue-600 mx-auto"></div>
          <p className="mt-4 text-gray-600">Loading...</p>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-6 p-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-3xl font-bold text-gray-900">Batch Processing</h1>
          <p className="mt-2 text-gray-600">
            Upload files and create batches to process requests at scale
          </p>
        </div>
        <div className="flex gap-3">
          <Button onClick={() => setUploadModalOpen(true)}>
            <Upload className="w-4 h-4 mr-2" />
            Upload File
          </Button>
          <Button onClick={() => setCreateBatchModalOpen(true)}>
            <Rocket className="w-4 h-4 mr-2" />
            Create Batch
          </Button>
        </div>
      </div>

      {/* Stats Cards */}
      <div className="grid grid-cols-1 md:grid-cols-4 gap-4">
        <div className="bg-white rounded-lg border border-gray-200 p-5">
          <div className="flex items-center justify-between">
            <div>
              <p className="text-sm font-medium text-gray-600">Total Files</p>
              <p className="mt-1 text-2xl font-semibold text-gray-900">
                {files.length}
              </p>
            </div>
            <div className="p-3 bg-blue-100 rounded-lg">
              <FileText className="w-6 h-6 text-blue-600" />
            </div>
          </div>
        </div>

        <div className="bg-white rounded-lg border border-gray-200 p-5">
          <div className="flex items-center justify-between">
            <div>
              <p className="text-sm font-medium text-gray-600">Total Batches</p>
              <p className="mt-1 text-2xl font-semibold text-gray-900">
                {batches.length}
              </p>
            </div>
            <div className="p-3 bg-purple-100 rounded-lg">
              <Activity className="w-6 h-6 text-purple-600" />
            </div>
          </div>
        </div>

        <div className="bg-white rounded-lg border border-gray-200 p-5">
          <div className="flex items-center justify-between">
            <div>
              <p className="text-sm font-medium text-gray-600">
                Active Batches
              </p>
              <p className="mt-1 text-2xl font-semibold text-gray-900">
                {
                  batches.filter((b) =>
                    ["validating", "in_progress", "finalizing"].includes(
                      b.status,
                    ),
                  ).length
                }
              </p>
            </div>
            <div className="p-3 bg-green-100 rounded-lg">
              <Rocket className="w-6 h-6 text-green-600" />
            </div>
          </div>
        </div>

        <div className="bg-white rounded-lg border border-gray-200 p-5">
          <div className="flex items-center justify-between">
            <div>
              <p className="text-sm font-medium text-gray-600">
                Completed Batches
              </p>
              <p className="mt-1 text-2xl font-semibold text-gray-900">
                {batches.filter((b) => b.status === "completed").length}
              </p>
            </div>
            <div className="p-3 bg-yellow-100 rounded-lg">
              <Download className="w-6 h-6 text-yellow-600" />
            </div>
          </div>
        </div>
      </div>

      {/* Tabs */}
      <Tabs value={activeTab} onValueChange={(v) => setActiveTab(v as any)}>
        <TabsList>
          <TabsTrigger value="files" className="gap-2">
            <FileText className="w-4 h-4" />
            Files ({files.length})
          </TabsTrigger>
          <TabsTrigger value="batches" className="gap-2">
            <Activity className="w-4 h-4" />
            Batches ({batches.length})
          </TabsTrigger>
        </TabsList>

        <TabsContent value="files" className="mt-6">
          <div className="bg-white rounded-lg border border-gray-200">
            <div className="p-6 border-b border-gray-200">
              <div className="flex items-center justify-between">
                <div>
                  <h2 className="text-lg font-semibold text-gray-900">
                    Uploaded Files
                  </h2>
                  <p className="mt-1 text-sm text-gray-600">
                    Manage your batch request files
                  </p>
                </div>
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setUploadModalOpen(true)}
                >
                  <Upload className="w-4 h-4 mr-2" />
                  Upload New File
                </Button>
              </div>
            </div>

            {files.length === 0 ? (
              <div className="flex flex-col items-center justify-center py-12">
                <FileText className="w-12 h-12 text-gray-400 mb-4" />
                <h3 className="text-lg font-medium text-gray-900 mb-2">
                  No files uploaded
                </h3>
                <p className="text-gray-600 mb-4">
                  Upload a .jsonl file to get started with batch processing
                </p>
                <Button onClick={() => setUploadModalOpen(true)}>
                  <Upload className="w-4 h-4 mr-2" />
                  Upload First File
                </Button>
              </div>
            ) : (
              <DataTable columns={fileColumns} data={files} />
            )}
          </div>
        </TabsContent>

        <TabsContent value="batches" className="mt-6">
          <div className="bg-white rounded-lg border border-gray-200">
            <div className="p-6 border-b border-gray-200">
              <div className="flex items-center justify-between">
                <div>
                  <h2 className="text-lg font-semibold text-gray-900">
                    Batch Jobs
                  </h2>
                  <p className="mt-1 text-sm text-gray-600">
                    Track and manage your batch processing jobs
                  </p>
                </div>
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setCreateBatchModalOpen(true)}
                >
                  <Rocket className="w-4 h-4 mr-2" />
                  Create New Batch
                </Button>
              </div>
            </div>

            {batches.length === 0 ? (
              <div className="flex flex-col items-center justify-center py-12">
                <Activity className="w-12 h-12 text-gray-400 mb-4" />
                <h3 className="text-lg font-medium text-gray-900 mb-2">
                  No batches created
                </h3>
                <p className="text-gray-600 mb-4">
                  Create a batch from an uploaded file to start processing requests
                </p>
                <Button onClick={() => setCreateBatchModalOpen(true)}>
                  <Rocket className="w-4 h-4 mr-2" />
                  Create First Batch
                </Button>
              </div>
            ) : (
              <DataTable columns={batchColumns} data={batches} />
            )}
          </div>
        </TabsContent>
      </Tabs>

      {/* Modals */}
      <UploadFileModal
        isOpen={uploadModalOpen}
        onClose={() => setUploadModalOpen(false)}
        onSuccess={() => {
          setUploadModalOpen(false);
        }}
      />

      <CreateBatchModal
        isOpen={createBatchModalOpen}
        onClose={() => {
          setCreateBatchModalOpen(false);
          setPreselectedFileId(undefined);
        }}
        onSuccess={() => {
          setCreateBatchModalOpen(false);
          setPreselectedFileId(undefined);
          setActiveTab("batches");
        }}
        preselectedFileId={preselectedFileId}
      />

      <ViewFileRequestsModal
        isOpen={viewFileRequestsModalOpen}
        onClose={() => {
          setViewFileRequestsModalOpen(false);
          setSelectedFile(null);
        }}
        file={selectedFile}
      />

      <ViewBatchRequestsModal
        isOpen={viewBatchRequestsModalOpen}
        onClose={() => {
          setViewBatchRequestsModalOpen(false);
          setSelectedBatch(null);
        }}
        batch={selectedBatch}
      />

      {/* Delete File Confirmation */}
      <AlertDialog
        open={!!fileToDelete}
        onOpenChange={(open) => !open && setFileToDelete(null)}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Delete File</AlertDialogTitle>
            <AlertDialogDescription>
              Are you sure you want to delete "{fileToDelete?.filename}"? This
              action cannot be undone.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction
              onClick={confirmDeleteFile}
              className="bg-red-600 hover:bg-red-700"
            >
              <Trash2 className="w-4 h-4 mr-2" />
              Delete
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      {/* Cancel Batch Confirmation */}
      <AlertDialog
        open={!!batchToCancel}
        onOpenChange={(open) => !open && setBatchToCancel(null)}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Cancel Batch</AlertDialogTitle>
            <AlertDialogDescription>
              Are you sure you want to cancel batch "{batchToCancel?.id}"? This
              will stop processing and may result in partial results.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Keep Running</AlertDialogCancel>
            <AlertDialogAction
              onClick={confirmCancelBatch}
              className="bg-red-600 hover:bg-red-700"
            >
              Cancel Batch
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}