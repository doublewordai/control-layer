import { useMemo, useState } from "react";
import { ImageOff } from "lucide-react";
import { Dialog, DialogContent, DialogTitle } from "./dialog";
import { InfoTip } from "./info-tip";
import { extractDwImageShas, dwImageUrl } from "../../utils/dwImage";

/**
 * Renders thumbnails for any `dw-img://<sha256>` image references found in a
 * request/response body. Each thumbnail resolves through the management-API
 * image endpoint (302 → short-lived signed URL), so the user sees the original
 * image they submitted rather than the opaque token. Renders nothing when the
 * body contains no image references.
 */

function DwImageThumb({ sha256 }: { sha256: string }) {
  const [open, setOpen] = useState(false);
  const [errored, setErrored] = useState(false);
  const src = dwImageUrl(sha256);
  const token = `dw-img://${sha256}`;

  if (errored) {
    return (
      <div
        className="flex h-20 w-20 flex-col items-center justify-center gap-1 rounded-md border border-doubleword-border bg-doubleword-neutral-50 text-doubleword-neutral-400"
        title={token}
      >
        <ImageOff className="h-4 w-4" />
        <span className="text-[10px]">unavailable</span>
      </div>
    );
  }

  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="h-20 w-20 overflow-hidden rounded-md border border-doubleword-border bg-doubleword-neutral-50 transition-colors hover:border-doubleword-neutral-400"
        aria-label={`View stored image ${sha256.slice(0, 12)}`}
        title={token}
      >
        <img
          src={src}
          alt={`Stored image ${sha256.slice(0, 12)}`}
          loading="lazy"
          className="h-full w-full object-contain"
          onError={() => setErrored(true)}
        />
      </button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="sm:max-w-2xl">
          <DialogTitle className="sr-only">Stored image preview</DialogTitle>
          <img
            src={src}
            alt={`Stored image ${sha256}`}
            className="mx-auto max-h-[70vh] w-auto object-contain"
          />
          <p className="break-all text-center font-mono text-xs text-doubleword-neutral-500">
            {token}
          </p>
        </DialogContent>
      </Dialog>
    </>
  );
}

interface DwImageGalleryProps {
  /** A request/response body — either a JSON string or a parsed value. */
  body: unknown;
  className?: string;
}

export function DwImageGallery({ body, className = "" }: DwImageGalleryProps) {
  const shas = useMemo(() => extractDwImageShas(body), [body]);
  if (shas.length === 0) return null;

  return (
    <div className={className}>
      <div className="mb-2 flex items-center gap-1.5">
        <h4 className="text-xs font-semibold text-doubleword-neutral-700">
          {shas.length === 1 ? "Image" : `Images (${shas.length})`}
        </h4>
        <InfoTip className="w-72">
          <p className="text-xs text-doubleword-neutral-600">
            Stored in your control plane. The original image was saved to your
            object storage and is referenced here by content hash; this preview
            is served through a short-lived signed link.
          </p>
        </InfoTip>
      </div>
      <div className="flex flex-wrap gap-2">
        {shas.map((sha) => (
          <DwImageThumb key={sha} sha256={sha} />
        ))}
      </div>
    </div>
  );
}
