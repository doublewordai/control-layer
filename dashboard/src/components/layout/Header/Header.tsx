import {
  useConfig,
  useTransactions,
  useUser,
  useUserBalance,
} from "@/api/control-layer/hooks";
import { useSettings } from "@/contexts/settings/hooks";
import { formatDollars } from "@/utils/money";
import { ArrowLeft, ExternalLink } from "lucide-react";
import { useLocation, useNavigate } from "react-router-dom";

export function Header() {
  const location = useLocation();
  const navigate = useNavigate();

  const isComparisonPage = location.pathname.startsWith("/compare/");
  const { data: currentUser } = useUser("current");
  const { data: config } = useConfig();
  const { isFeatureEnabled } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");

  // Fetch balance and transactions
  const { data: balance = 0 } = useUserBalance(currentUser?.id || "");
  const { data: transactionsData } = useTransactions({
    userId: currentUser?.id || "",
  });

  // Calculate current balance
  // In demo mode, use page_start_balance from transactions response; otherwise use user balance
  const currentBalance =
    isDemoMode && transactionsData?.page_start_balance !== undefined
      ? transactionsData.page_start_balance
      : balance;

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
          <div className="flex items-center gap-2 text-sm">
            <span className="text-gray-600">Balance:</span>
            <span className="font-semibold text-gray-900">
              {formatDollars(currentBalance)}
            </span>
          </div>
          {config?.docs_url && (
            <>
              <div className="w-px h-4 bg-doubleword-neutral-200"></div>
              <a
                href={config.docs_url}
                target="_blank"
                rel="noopener noreferrer"
                className="flex items-center gap-2 text-doubleword-text-tertiary hover:text-doubleword-primary transition-colors font-medium"
              >
                <span>Documentation</span>
                <ExternalLink className="w-3 h-3" />
              </a>
            </>
          )}
        </div>
      </div>
    </div>
  );
}
