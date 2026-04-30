import * as React from "react";
import DOMPurify, { type Config } from "dompurify";

import { cn } from "@/lib/utils";

// Profile of tags/attributes we are willing to render from a same-origin
// content source (e.g. dashboard/public/bootstrap.js). The bootstrap banner
// content uses an inline <style> block plus a small subset of layout/SVG
// markup, so we explicitly allow those tags and strip everything else.
const SANITIZE_CONFIG: Config = {
  ADD_TAGS: ["style"],
  // FORCE_BODY ensures a leading <style> in the input is parsed into the
  // sanitized output instead of being dropped at the document boundary.
  FORCE_BODY: true,
  // DOMPurify already strips javascript: URLs and inline event handlers by
  // default; we additionally forbid known data-exfiltration vectors.
  FORBID_TAGS: ["script", "iframe", "object", "embed"],
  FORBID_ATTR: ["onerror", "onload", "onclick", "onmouseover"],
};

export interface SafeHTMLProps
  extends Omit<React.HTMLAttributes<HTMLDivElement>, "dangerouslySetInnerHTML"> {
  html: string;
}

export const SafeHTML = React.forwardRef<HTMLDivElement, SafeHTMLProps>(
  ({ html, className, ...props }, ref) => {
    const sanitized = React.useMemo(
      () => DOMPurify.sanitize(html, SANITIZE_CONFIG) as string,
      [html],
    );

    return (
      <div
        ref={ref}
        className={cn(className)}
        dangerouslySetInnerHTML={{ __html: sanitized }}
        {...props}
      />
    );
  },
);
SafeHTML.displayName = "SafeHTML";
