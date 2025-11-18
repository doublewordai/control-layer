import { useEffect } from "react";
import { useUser, useAddFunds, useConfig } from "@/api/control-layer";
import { toast } from "sonner";
import { useSettings } from "@/contexts";
import { TransactionHistory } from "@/components/features/cost-management/CostManagement/TransactionHistory.tsx";

export function CostManagement() {
  const { isFeatureEnabled } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");
  const { data: config } = useConfig();

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
      try {
        await addFundsMutation.mutateAsync({
          user_id: user?.id || "",
          amount: fundAmount,
          description: "Funds purchase - Demo top up",
        });
        toast.success(`Added $${fundAmount.toFixed(2)}`);
      } catch (error) {
        toast.error("Failed to add funds");
        console.error("Error adding funds:", error);
      }
    } else if (config?.payment_enabled) {
      // Payment processing enabled: Get checkout URL and redirect
      try {
        const response = await fetch("/admin/api/v1/create_checkout", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
        });

        if (response.ok) {
          const data = await response.json();
          if (data.url) {
            // Navigate to Stripe checkout page
            window.location.href = data.url;
          } else {
            toast.error("Failed to get checkout URL");
          }
        } else {
          const errorData = await response.json().catch(() => ({}));
          toast.error(errorData.message || "Failed to initiate checkout");
        }
      } catch (error) {
        toast.error("Failed to add funds");
        console.error("Error creating checkout:", error);
      }
    } else {
      toast.error("Payment processing is not configured");
    }
  };

  // Only show Add Funds button if in demo mode or payment processing is enabled
  const canAddFunds = isDemoMode || !!config?.payment_enabled;

  return (
    <div className="p-6">
      <div className="mb-8">
        <h1 className="text-3xl font-bold text-doubleword-neutral-900">
          Cost Management
        </h1>
        <p className="text-doubleword-neutral-600 mt-2">
          Monitor your credit balance and transaction history
        </p>
      </div>

      {user && (
        <TransactionHistory
          userId={user.id}
          onAddFunds={canAddFunds ? handleAddFunds : undefined}
          isAddingFunds={addFundsMutation.isPending}
          showCard={false}
        />
      )}
    </div>
  );
}
