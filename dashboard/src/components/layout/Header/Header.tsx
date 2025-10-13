import { ArrowLeft, ExternalLink } from "lucide-react";
import { useLocation, useNavigate } from "react-router-dom";

export function Header() {
  const location = useLocation();
  const navigate = useNavigate();

  const isComparisonPage = location.pathname.startsWith("/compare/");

  return (
    <div className="h-16 bg-white border-b border-doubleword-border fixed top-0 right-0 left-64 z-10">
      <div className="h-full px-8 flex items-center justify-between">
        {isComparisonPage ? (
          <button
            onClick={() => navigate("/models")}
            className="flex items-center gap-2 text-sm text-doubleword-text-tertiary hover:text-doubleword-text-primary transition-colors"
          >
            <ArrowLeft className="w-4 h-4" />
            Back to Models
          </button>
        ) : (
          <div></div>
        )}
        <div className="flex items-center gap-6 text-sm text-doubleword-neutral-600">
          <div className="flex items-center gap-2">
            <span className="text-doubleword-neutral-400">Region:</span>
            <span className="font-medium">UK South</span>
          </div>
          <div className="w-px h-4 bg-doubleword-neutral-200"></div>
          <div className="flex items-center gap-2">
            <span className="text-doubleword-neutral-400">Organization:</span>
            <span className="font-medium">ACME Corp</span>
          </div>
          <div className="w-px h-4 bg-doubleword-neutral-200"></div>
          <a
            href="https://docs.doubleword.ai/control-layer"
            target="_blank"
            rel="noopener noreferrer"
            className="flex items-center gap-2 text-doubleword-text-tertiary hover:text-doubleword-primary transition-colors font-medium"
          >
            <span>Documentation</span>
            <ExternalLink className="w-3 h-3" />
          </a>
        </div>
      </div>
    </div>
  );
}
