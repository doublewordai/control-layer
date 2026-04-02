import type { ComponentPropsWithoutRef } from "react";
import { cn } from "../../../../lib/utils";
import onwardsLogo from "../../../../assets/onwards-logo.svg";
import { getCatalogIconInitials } from "./catalogPresentation";

type CatalogIconSpec = {
  src?: string;
  alt: string;
  inverted?: boolean;
};

const ICON_REGISTRY: Record<string, CatalogIconSpec> = {
  anthropic: {
    src: "/endpoints/anthropic.svg",
    alt: "Anthropic",
  },
  google: {
    src: "/endpoints/google.svg",
    alt: "Google",
  },
  openai: {
    src: "/endpoints/openai.svg",
    alt: "OpenAI",
  },
  onwards: {
    src: onwardsLogo,
    alt: "Onwards",
  },
  snowflake: {
    src: "/endpoints/snowflake.png",
    alt: "Snowflake",
  },
};

function normalizeKey(value?: string | null): string | undefined {
  if (!value) return undefined;
  return value.trim().toLowerCase().replace(/\s+/g, "-");
}

function resolveIconSpec(icon?: string, label?: string): CatalogIconSpec | undefined {
  if (!icon) return undefined;
  if (icon.startsWith("https://") || icon.startsWith("/")) {
    return {
      src: icon,
      alt: label || "Catalog icon",
    };
  }
  return ICON_REGISTRY[normalizeKey(icon) ?? ""];
}

export function CatalogIcon({
  icon,
  label,
  size = "md",
  fallback = "initials",
  className,
  ...props
}: {
  icon?: string;
  label: string;
  size?: "sm" | "md";
  fallback?: "initials" | "none";
} & ComponentPropsWithoutRef<"span">) {
  const spec = resolveIconSpec(icon, label);
  if (!spec?.src && fallback === "none") {
    return null;
  }
  const containerClassName = size === "sm" ? "h-5 w-5" : "h-7 w-7";
  const imageClassName = size === "sm" ? "h-5 w-5" : "h-7 w-7";
  const fallbackClassName =
    size === "sm"
      ? "h-5 w-5 rounded-md text-[9px]"
      : "h-7 w-7 rounded-lg text-[9px]";

  return (
    <span
      className={cn(
        "inline-flex shrink-0 items-center justify-center",
        containerClassName,
        className,
      )}
      aria-hidden="true"
      {...props}
    >
      {spec?.src ? (
        <img
          src={spec.src}
          alt={spec.alt}
          className={cn(
            "object-contain",
            imageClassName,
            spec.inverted ? "brightness-0 invert" : undefined,
          )}
        />
      ) : (
        <span
          className={cn(
            "inline-flex items-center justify-center bg-slate-100 font-semibold uppercase tracking-[0.14em] text-slate-500",
            fallbackClassName,
          )}
        >
          {getCatalogIconInitials(label)}
        </span>
      )}
    </span>
  );
}
