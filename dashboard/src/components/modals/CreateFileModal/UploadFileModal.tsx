import { useState, useEffect } from "react";
import { Upload, X, FileText, AlertCircle, ExternalLink } from "lucide-react";
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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../ui/select";
import { useUploadFileWithProgress, useConfig } from "../../../api/control-layer/hooks";
import { toast } from "sonner";
import { AlertBox } from "@/components/ui/alert-box";

interface UploadFileModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess?: () => void;
  preselectedFile?: File;
}

const EXPIRATION_PRESETS = [
  { label: "1 hour", seconds: 3600 },
  { label: "24 hours", seconds: 86400 },
  { label: "7 days", seconds: 604800 },
  { label: "30 days (default)", seconds: 2592000 },
  { label: "60 days", seconds: 5184000 },
  { label: "90 days", seconds: 7776000 },
];

export function UploadFileModal({
  isOpen,
  onClose,
  onSuccess,
  preselectedFile,
}: UploadFileModalProps) {
  const [file, setFile] = useState<File | null>(preselectedFile || null);
  const [filename, setFilename] = useState<string>(preselectedFile?.name || "");
  const [expirationSeconds, setExpirationSeconds] = useState<number>(2592000); // 30 days default
  const [dragActive, setDragActive] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [uploadProgress, setUploadProgress] = useState<number>(0);

  const uploadMutation = useUploadFileWithProgress();
  const { data: config } = useConfig();

  // Update file when preselected file changes
  useEffect(() => {
    if (preselectedFile) {
      setFile(preselectedFile);
      setFilename(preselectedFile.name);
    }
  }, [preselectedFile]);

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
        setFile(droppedFile);
        setFilename(droppedFile.name);
      } else {
        setError("Please upload a .jsonl file");
      }
    }
  };

  const handleFileChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    if (e.target.files && e.target.files[0]) {
      const selectedFile = e.target.files[0];
      if (selectedFile.name.endsWith(".jsonl")) {
        setFile(selectedFile);
        setFilename(selectedFile.name);
        setError(null);
      } else {
        setError("Please upload a .jsonl file");
      }
    }
  };

  const handleRemoveFile = (e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setFile(null);
    setFilename("");
    setError(null);
    // Reset the file input element
    const fileInput = document.getElementById(
      "file-upload",
    ) as HTMLInputElement;
    if (fileInput) {
      fileInput.value = "";
    }
  };

  const handleSubmit = async () => {
    if (!file) {
      setError("Please select a file");
      return;
    }

    const finalFilename = filename.trim() || file.name;
    if (!finalFilename.endsWith(".jsonl")) {
      setError("Filename must end with .jsonl");
      return;
    }

    setUploadProgress(0);

    try {
      await uploadMutation.mutateAsync({
        data: {
          file,
          purpose: "batch",
          filename: finalFilename,
          expires_after: {
            anchor: "created_at",
            seconds: expirationSeconds,
          },
        },
        onProgress: setUploadProgress,
      });

      toast.success(`File "${finalFilename}" uploaded successfully`);
      setFile(null);
      setFilename("");
      setExpirationSeconds(2592000);
      setUploadProgress(0);
      onSuccess?.();
      onClose();
    } catch (error) {
      console.error("Failed to upload file:", error);
      setUploadProgress(0);
      setError(
        error instanceof Error
          ? error.message
          : "Failed to upload file. Please try again.",
      );
    }
  };

  const handleClose = () => {
    setFile(null);
    setFilename("");
    setExpirationSeconds(2592000);
    setError(null);
    setUploadProgress(0);
    onClose();
  };

  return (
    <Dialog open={isOpen} onOpenChange={handleClose}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Upload Batch File</DialogTitle>
          <DialogDescription>
            Upload a{" "}
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
            to process multiple requests asynchronously.
          </DialogDescription>
        </DialogHeader>

        <AlertBox variant="error" className="mb-4">
          {error}
        </AlertBox>

        <div className="space-y-6">
          {/* File Upload Area */}
          <div
            className={`relative border-2 border-dashed rounded-lg p-8 text-center transition-colors ${
              dragActive
                ? "border-blue-500 bg-blue-50"
                : file
                  ? "border-green-500 bg-green-50"
                  : "border-gray-300 hover:border-gray-400"
            }`}
            onDragEnter={handleDrag}
            onDragLeave={handleDrag}
            onDragOver={handleDrag}
            onDrop={handleDrop}
          >
            {!file && (
              <input
                type="file"
                id="file-upload"
                accept=".jsonl"
                onChange={handleFileChange}
                className="absolute inset-0 w-full h-full opacity-0 cursor-pointer"
              />
            )}

            {file ? (
              <div className="space-y-2">
                <FileText className="w-12 h-12 mx-auto text-green-600" />
                <div>
                  <p className="text-sm text-green-700">
                    {(file.size / 1024).toFixed(2)} KB
                  </p>
                </div>
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  onClick={handleRemoveFile}
                  className="text-red-600 hover:text-red-700 hover:bg-red-50"
                >
                  <X className="w-4 h-4 mr-1" />
                  Remove
                </Button>
              </div>
            ) : (
              <div className="space-y-2">
                <Upload className="w-12 h-12 mx-auto text-gray-400" />
                <div>
                  <p className="font-medium text-gray-700">
                    Drop your .jsonl file here
                  </p>
                  <p className="text-sm text-gray-500">or click to browse</p>
                </div>
              </div>
            )}
          </div>

          {/* Filename Input */}
          {file && (
            <div className="space-y-2">
              <Label htmlFor="filename">Filename</Label>
              <Input
                id="filename"
                value={filename}
                onChange={(e) => setFilename(e.target.value)}
                placeholder="Enter filename"
                disabled={uploadMutation.isPending}
              />
              <p className="text-xs text-gray-500">
                You can rename the file before uploading. Must end with .jsonl
              </p>
            </div>
          )}

          {/* Upload Progress Bar */}
          {uploadMutation.isPending && (
            <div className="space-y-2">
              <div className="flex justify-between text-sm">
                <span className="text-gray-600">Uploading...</span>
                <span className="text-gray-900 font-medium">{uploadProgress}%</span>
              </div>
              <div className="h-2 bg-gray-200 rounded-full overflow-hidden">
                <div
                  className="h-full bg-blue-600 rounded-full transition-all duration-150 ease-out"
                  style={{ width: `${uploadProgress}%` }}
                />
              </div>
            </div>
          )}

          {/* Expiration Select */}
          <div className="space-y-2">
            <Label htmlFor="expiration">File Expiration</Label>
            <Select
              value={expirationSeconds.toString()}
              onValueChange={(value) => setExpirationSeconds(parseInt(value))}
            >
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {EXPIRATION_PRESETS.map((preset) => (
                  <SelectItem
                    key={preset.seconds}
                    value={preset.seconds.toString()}
                  >
                    {preset.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-xs text-gray-500">
              Files will be automatically deleted after this period
            </p>
          </div>

          {/* Help Text */}
          <div className="bg-blue-50 border border-blue-200 rounded-lg p-3">
            <div className="flex gap-2">
              <AlertCircle className="w-4 h-4 text-blue-600 mt-0.5 shrink-0" />
              <div className="text-sm text-blue-800">
                <p className="font-medium mb-1">JSONL Format Required</p>
                <p className="text-blue-700">
                  Each line should be a valid JSON object representing a batch
                  request.
                  {config?.docs_jsonl_url && (
                    <>
                      {" "}
                      See the{" "}
                      <a
                        href={config.docs_jsonl_url}
                        target="_blank"
                        rel="noopener noreferrer"
                        className="underline hover:text-blue-900"
                      >
                        documentation
                      </a>{" "}
                      for examples.
                    </>
                  )}
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
            disabled={uploadMutation.isPending}
          >
            Cancel
          </Button>
          <Button
            type="button"
            onClick={handleSubmit}
            disabled={!file || uploadMutation.isPending}
          >
            {uploadMutation.isPending ? (
              <>
                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white mr-2"></div>
                Uploading... {uploadProgress}%
              </>
            ) : (
              <>
                <Upload className="w-4 h-4 mr-2" />
                Upload File
              </>
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
