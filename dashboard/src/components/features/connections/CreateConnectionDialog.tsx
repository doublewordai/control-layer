import { useState } from "react";
import { Loader2 } from "lucide-react";
import { toast } from "sonner";
import { useCreateConnection } from "@/api/control-layer/hooks";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function CreateConnectionDialog({ open, onOpenChange }: Props) {
  const createMutation = useCreateConnection();

  const [name, setName] = useState("");
  const [bucket, setBucket] = useState("");
  const [prefix, setPrefix] = useState("");
  const [region, setRegion] = useState("");
  const [accessKeyId, setAccessKeyId] = useState("");
  const [secretAccessKey, setSecretAccessKey] = useState("");
  const [endpointUrl, setEndpointUrl] = useState("");
  const [error, setError] = useState<string | null>(null);

  const resetForm = () => {
    setName("");
    setBucket("");
    setPrefix("");
    setRegion("");
    setAccessKeyId("");
    setSecretAccessKey("");
    setEndpointUrl("");
    setError(null);
  };

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);

    if (!name.trim() || !bucket.trim() || !region.trim() || !accessKeyId.trim() || !secretAccessKey.trim()) {
      setError("Name, bucket, region, access key ID, and secret access key are required.");
      return;
    }

    try {
      await createMutation.mutateAsync({
        provider: "s3",
        name: name.trim(),
        config: {
          bucket: bucket.trim(),
          prefix: prefix.trim() || undefined,
          region: region.trim(),
          access_key_id: accessKeyId.trim(),
          secret_access_key: secretAccessKey.trim(),
          endpoint_url: endpointUrl.trim() || undefined,
        },
      });
      toast.success(`Connection "${name}" created`);
      resetForm();
      onOpenChange(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create connection");
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(val) => {
        if (!val) resetForm();
        onOpenChange(val);
      }}
    >
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>New S3 Connection</DialogTitle>
          <DialogDescription>
            Connect to an Amazon S3 bucket (or S3-compatible service) to sync JSONL files for batch processing.
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={handleSubmit} className="space-y-4">
          {error && (
            <div className="rounded-md bg-red-50 p-3 text-sm text-red-700">
              {error}
            </div>
          )}

          <div className="space-y-2">
            <Label htmlFor="conn-name">Connection Name</Label>
            <Input
              id="conn-name"
              placeholder="e.g. prod-s3-inputs"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </div>

          <div className="grid grid-cols-2 gap-4">
            <div className="space-y-2">
              <Label htmlFor="conn-bucket">Bucket</Label>
              <Input
                id="conn-bucket"
                placeholder="my-bucket"
                value={bucket}
                onChange={(e) => setBucket(e.target.value)}
              />
            </div>
            <div className="space-y-2">
              <Label htmlFor="conn-region">Region</Label>
              <Input
                id="conn-region"
                placeholder="e.g. eu-west-2"
                value={region}
                onChange={(e) => setRegion(e.target.value)}
              />
            </div>
          </div>

          <div className="space-y-2">
            <Label htmlFor="conn-prefix">Prefix (optional)</Label>
            <Input
              id="conn-prefix"
              placeholder="e.g. inputs/batch/"
              value={prefix}
              onChange={(e) => setPrefix(e.target.value)}
            />
            <p className="text-xs text-muted-foreground">
              Only JSONL files under this prefix will be discovered during sync.
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor="conn-access-key">Access Key ID</Label>
            <Input
              id="conn-access-key"
              value={accessKeyId}
              onChange={(e) => setAccessKeyId(e.target.value)}
            />
          </div>

          <div className="space-y-2">
            <Label htmlFor="conn-secret-key">Secret Access Key</Label>
            <Input
              id="conn-secret-key"
              type="password"
              value={secretAccessKey}
              onChange={(e) => setSecretAccessKey(e.target.value)}
            />
            <p className="text-xs text-muted-foreground">
              Use an IAM user with read-only access (s3:GetObject, s3:ListBucket) scoped to this bucket.
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor="conn-endpoint">Custom Endpoint URL (optional)</Label>
            <Input
              id="conn-endpoint"
              placeholder="e.g. http://localhost:9000 for MinIO"
              value={endpointUrl}
              onChange={(e) => setEndpointUrl(e.target.value)}
            />
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => {
                resetForm();
                onOpenChange(false);
              }}
            >
              Cancel
            </Button>
            <Button
              type="submit"
              disabled={createMutation.isPending}
              className="bg-doubleword-background-dark hover:bg-doubleword-neutral-900"
            >
              {createMutation.isPending && (
                <Loader2 className="h-4 w-4 animate-spin mr-2" />
              )}
              Create Connection
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
