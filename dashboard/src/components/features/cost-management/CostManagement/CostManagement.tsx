import { useEffect } from "react";
import { useSearchParams } from "react-router-dom";
import { useUser, useAddFunds } from "@/api/control-layer";
import { toast } from "sonner";
import { useSettings } from "@/contexts";
import { TransactionHistory } from "@/components/features/cost-management/CostManagement/TransactionHistory.tsx";

export function CostManagement() {
  const { isFeatureEnabled, settings } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");
  const [searchParams] = useSearchParams();

  // Check if we're filtering by a specific user
  const filterUserId = searchParams.get("user");

  // Fetch current user
  const { data: user, refetch: refetchUser } = useUser("current");
  const addFundsMutation = useAddFunds();

  // Handle return from payment provider
  useEffect(() => {
    if (isDemoMode) return;

    const urlParams = new URLSearchParams(window.location.search);
    const paymentComplete = urlParams.get("payment_complete");

    if (paymentComplete === "true") {
      // Show success message
      toast.success("Payment completed! Your balance has been updated.");

      // Refetch user data to get latest balance
      refetchUser();

      // Clean up URL
      window.history.replaceState({}, "", window.location.pathname);
    }
  }, [isDemoMode, refetchUser]);

  const handleAddFunds = async () => {
    if (isDemoMode) {
      // Demo mode: Call the API (which will be intercepted by MSW)
      const fundAmount = 100.0;
      const targetUserId = filterUserId || user?.id || "";
      try {
        await addFundsMutation.mutateAsync({
          source_id: user?.id || "",
          user_id: targetUserId,
          amount: fundAmount,
          description: "Funds purchase - Demo top up"
        });
        toast.success(`Added $${fundAmount.toFixed(2)}`);
      } catch (error) {
        toast.error("Failed to add funds");
        console.error("Error adding funds:", error);
      }
    } else {
      // API mode: Redirect to payment provider
      const paymentProviderUrl = settings.paymentProviderUrl;

      // Build callback URL to return to this page
      const callbackUrl = `${window.location.origin}${window.location.pathname}?payment_complete=true`;

      // Redirect to payment provider with callback URL
      const redirectUrl = `${paymentProviderUrl}?callback=${encodeURIComponent(callbackUrl)}`;
      window.location.href = redirectUrl;
    }
  };

  // Only show Add Funds button if in demo mode or payment provider is configured
  const canAddFunds = isDemoMode || !!settings.paymentProviderUrl;

  return (
    <div className="p-6">
      {user && (
        <TransactionHistory
          userId={filterUserId || user.id}
          onAddFunds={canAddFunds ? handleAddFunds : undefined}
          isAddingFunds={addFundsMutation.isPending}
          showCard={false}
          filterUserId={filterUserId || undefined}
        />
      )}
    </div>
  );
}
