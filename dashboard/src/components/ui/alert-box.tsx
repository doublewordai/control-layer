import * as React from "react";
import { AlertCircle, CheckCircle, Info, AlertTriangle } from "lucide-react";
import { cn } from "../../lib/utils";

export type AlertBoxVariant = "error" | "success" | "info" | "warning";

interface AlertBoxProps {
  variant?: AlertBoxVariant;
  children: React.ReactNode;
  className?: string;
  icon?: React.ReactNode; // Allow custom icon override
}

const variantStyles: Record<
  AlertBoxVariant,
  {
    container: string;
    text: string;
    icon: React.ComponentType<{ className?: string }>;
  }
> = {
  error: {
    container: "bg-red-50 border-red-200",
    text: "text-red-600",
    icon: AlertCircle,
  },
  success: {
    container: "bg-green-50 border-green-200",
    text: "text-green-600",
    icon: CheckCircle,
  },
  info: {
    container: "bg-blue-50 border-blue-200",
    text: "text-blue-600",
    icon: Info,
  },
  warning: {
    container: "bg-yellow-50 border-yellow-200",
    text: "text-yellow-600",
    icon: AlertTriangle,
  },
};

export function AlertBox({
  variant = "info",
  children,
  className,
  icon,
}: AlertBoxProps) {
  if (!children) return null;

  const styles = variantStyles[variant];
  const IconComponent = styles.icon;

  // strip out error if too long or containing HTML tags
  const childString = children.toString();
  const htmlTagRegex = /<\/?[a-zA-Z][^>]*>/;
  if (htmlTagRegex.test(childString) && childString.length > 280) {
    children = "An error occurred. Please try again later.";
  }

  return (
    <div
      className={cn("p-3 border rounded-lg", styles.container, className)}
      role="alert"
    >
      <div className="flex items-start gap-2">
        {icon !== undefined ? (
          icon
        ) : (
          <IconComponent
            className={cn("w-4 h-4 mt-0.5 shrink-0", styles.text)}
          />
        )}
        <div className={cn("text-sm flex-1", styles.text)}>{children}</div>
      </div>
    </div>
  );
}
