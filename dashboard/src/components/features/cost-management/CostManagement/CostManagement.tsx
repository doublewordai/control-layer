import { useState } from "react";
import { useSearchParams } from "react-router-dom";
import { useUser } from "@/api/control-layer";
import { useSettings } from "@/contexts";
import { TransactionHistory } from "@/components/features/cost-management/CostManagement/TransactionHistory.tsx";
import { AddFundsModal } from "@/components/modals/AddCreditsModal/AddCreditsModal";
import type { DisplayUser } from "@/types/display";

export function CostManagement() {
  const [searchParams] = useSearchParams();
  const [showAddFundsModal, setShowAddFundsModal] = useState(false);
  const { settings } = useSettings();

  // Check if we're filtering by a specific user
  const filterUserId = searchParams.get("user");

  // Fetch current user and display user (the one we're viewing billing for)
  const { data: currentUser, refetch: refetchCurrentUser } = useUser("current");
  const { data: displayUser, refetch: refetchDisplayUser } = useUser(filterUserId || "current");

  // Check if user has permission to add funds (PlatformManager or BillingManager)
  const canManageFunds = currentUser?.roles?.some(
    (role) => role === "PlatformManager" || role === "BillingManager"
  );

  const handleAddFundsSuccess = () => {
    // Refetch user data to get latest balance
    refetchCurrentUser();
    refetchDisplayUser();
  };

  // Handle redirect to payment provider
  const handleRedirectToPaymentProvider = () => {
    const paymentProviderUrl = settings.paymentProviderUrl;
    if (!paymentProviderUrl) {
      return;
    }

    // Build callback URL to return to this page
    const callbackUrl = `${window.location.origin}${window.location.pathname}`;

    // Redirect to payment provider with callback URL
    const redirectUrl = `${paymentProviderUrl}?callback=${encodeURIComponent(callbackUrl)}`;
    window.location.href = redirectUrl;
  };

  // Determine add funds configuration
  const addFundsConfig = (() => {
    const hasPaymentProvider = !!settings.paymentProviderUrl;

    if (!hasPaymentProvider) {
      // No payment provider configured
      return canManageFunds ? {
        type: 'direct' as const,
        onAddFunds: () => setShowAddFundsModal(true)
      } : undefined;
    }

    // Payment provider configured
    if (canManageFunds) {
      // Admin: split button (redirect primary, direct in dropdown)
      return {
        type: 'split' as const,
        onPrimaryAction: handleRedirectToPaymentProvider,
        onDirectAction: () => setShowAddFundsModal(true)
      };
    } else {
      // Non-admin: simple redirect button
      return {
        type: 'redirect' as const,
        onAddFunds: handleRedirectToPaymentProvider
      };
    }
  })();

  return (
    <div className="p-6">
      {currentUser && displayUser && (
        <>
          <TransactionHistory
            userId={filterUserId || currentUser.id}
            addFundsConfig={addFundsConfig}
            showCard={false}
            filterUserId={filterUserId || undefined}
          />
          {canManageFunds && (
            <AddFundsModal
              isOpen={showAddFundsModal}
              onClose={() => setShowAddFundsModal(false)}
              targetUser={displayUser as DisplayUser}
              onSuccess={handleAddFundsSuccess}
            />
          )}
        </>
      )}
    </div>
  );
}
