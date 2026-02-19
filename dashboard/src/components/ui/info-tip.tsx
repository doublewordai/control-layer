import { useState } from "react";
import { Info } from "lucide-react";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "./popover";

interface InfoTipProps {
  children: React.ReactNode;
  className?: string;
}

/**
 * A mobile-friendly info tooltip that shows on hover (desktop) and tap (mobile).
 * Uses Popover internally instead of HoverCard to support touch devices.
 */
export function InfoTip({ children, className = "w-80" }: InfoTipProps) {
  const [open, setOpen] = useState(false);

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="inline-flex items-center"
          onPointerEnter={(e) => {
            if (e.pointerType === "mouse") setOpen(true);
          }}
          onPointerLeave={(e) => {
            if (e.pointerType === "mouse") setOpen(false);
          }}
        >
          <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
        </button>
      </PopoverTrigger>
      <PopoverContent
        className={`${className} px-3 py-2`}
        sideOffset={5}
        onOpenAutoFocus={(e) => e.preventDefault()}
      >
        {children}
      </PopoverContent>
    </Popover>
  );
}
