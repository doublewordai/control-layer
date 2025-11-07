import { useState } from "react";
import { Upload, X, FileText, AlertCircle } from "lucide-react";
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
import { useUploadFile } from "../../../api/control-layer/hooks";
import { toast } from "sonner";

interface UploadFileModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess?: () => void;
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
}: UploadFileModalProps) {
  const [file, setFile] = useState<File | null>(null);
  const [expirationSeconds, setExpirationSeconds] = useState<number>(2592000); // 30 days default
  const [dragActive, setDragActive] = useState(false);

  const uploadMutation = useUploadFile();

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
      } else {
        toast.error("Please upload a .jsonl file");
      }
    }
  };

  const handleFileChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    if (e.target.files && e.target.files[0]) {
      const selectedFile = e.target.files[0];
      if (selectedFile.name.endsWith(".jsonl")) {
        setFile(selectedFile);
      } else {
        toast.error("Please upload a .jsonl file");
      }
    }
  };

  const handleSubmit = async () => {
    if (!file) {
      toast.error("Please select a file");
      return;
    }

    try {
      await uploadMutation.mutateAsync({
        file,
        purpose: "batch",
        expires_after: {
          anchor: "created_at",
          seconds: expirationSeconds,
        },
      });

      toast.success(`File "${file.name}" uploaded successfully`);
      setFile(null);
      setExpirationSeconds(2592000);
      onSuccess?.();
      onClose();
    } catch (error) {
      console.error("Failed to upload file:", error);
      toast.error(
        error instanceof Error
          ? error.message
          : "Failed to upload file. Please try again.",
      );
    }
  };

  const handleClose = () => {
    setFile(null);
    setExpirationSeconds(2592000);
    onClose();
  };

  return (
    <Dialog open={isOpen} onOpenChange={handleClose}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Upload Batch File</DialogTitle>
        </DialogHeader>

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
            <input
              type="file"
              id="file-upload"
              accept=".jsonl"
              onChange={handleFileChange}
              className="absolute inset-0 w-full h-full opacity-0 cursor-pointer"
            />

            {file ? (
              <div className="space-y-2">
                <FileText className="w-12 h-12 mx-auto text-green-600" />
                <div>
                  <p className="font-medium text-green-900">{file.name}</p>
                  <p className="text-sm text-green-700">
                    {(file.size / 1024).toFixed(2)} KB
                  </p>
                </div>
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  onClick={(e) => {
                    e.stopPropagation();
                    setFile(null);
                  }}
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
                  <p className="text-sm text-gray-500">
                    or click to browse
                  </p>
                </div>
              </div>
            )}
          </div>

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
              <AlertCircle className="w-4 h-4 text-blue-600 mt-0.5 flex-shrink-0" />
              <div className="text-sm text-blue-800">
                <p className="font-medium mb-1">JSONL Format Required</p>
                <p className="text-blue-700">
                  Each line should be a valid JSON object representing a batch
                  request. See the{" "}
                  <a
                    href="https://docs.doubleword.ai"
                    target="_blank"
                    rel="noopener noreferrer"
                    className="underline hover:text-blue-900"
                  >
                    documentation
                  </a>{" "}
                  for examples.
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
                Uploading...
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