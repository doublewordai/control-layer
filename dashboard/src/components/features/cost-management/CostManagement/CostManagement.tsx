import { useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { useUser, useAddFunds, useConfig, useCreatePayment, useProcessPayment } from "@/api/control-layer";
import { toast } from "sonner";
import { useSettings } from "@/contexts";
import { TransactionHistory } from "@/components/features/cost-management/CostManagement/TransactionHistory.tsx";
import { AddFundsModal } from "@/components/modals/AddCreditsModal/AddCreditsModal";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { CheckCircle2, XCircle, Loader2 } from "lucide-react";
import type { DisplayUser } from "@/types/display";

export function CostManagement() {
  const [searchParams] = useSearchParams();
  const { isFeatureEnabled, settings } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");
  const { data: config } = useConfig();

  const [showSuccessModal, setShowSuccessModal] = useState(false);
  const [showCancelledModal, setShowCancelledModal] = useState(false);
  const [showAddFundsModal, setShowAddFundsModal] = useState(false);

  // Check if we're filtering by a specific user
  const filterUserId = searchParams.get("user");

  // Fetch current user and display user (the one we're viewing billing for)
  const { data: currentUser, refetch: refetchCurrentUser } = useUser("current");
  const { data: displayUser, refetch: refetchDisplayUser } = useUser(filterUserId || "current");

  const addFundsMutation = useAddFunds();
  const createPaymentMutation = useCreatePayment();
  const processPaymentMutation = useProcessPayment({
    onSuccess: () => {
      setTimeout(() => {
        console.log('Closing modal now');
        setShowSuccessModal(false);
        // Refetch user data to get latest balance
        refetchCurrentUser();
        refetchDisplayUser();
      }, 2000);
    },
    onError: (error) => {
      console.error('Payment processing error:', error);
    }
  });

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

  // Handle return from payment provider
  useEffect(() => {
    if (isDemoMode) return;

    const urlParams = new URLSearchParams(window.location.search);
    const paymentStatus = urlParams.get("payment");
    const sessionId = urlParams.get("session_id");

    if (paymentStatus === "success" && sessionId) {
      // Process payment using the mutation hook
      setShowSuccessModal(true);
      processPaymentMutation.mutate(sessionId);

      // Clean up URL - this prevents re-processing on subsequent renders
      window.history.replaceState({}, "", window.location.pathname);
    } else if (paymentStatus === "cancelled" && sessionId) {
      setShowCancelledModal(true);
      // Clean up URL
      window.history.replaceState({}, "", window.location.pathname);
    } else if (paymentStatus === "success" && !sessionId) {
      // Legacy support for old payment_complete parameter
      toast.success("Payment completed! Your balance has been updated.");
      window.history.replaceState({}, "", window.location.pathname);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isDemoMode]);

  // Auto-close modal when payment processing completes successfully
  useEffect(() => {
    if (processPaymentMutation.isSuccess && showSuccessModal) {
      // Wait a moment to show the success message, then auto-close
      const timer = setTimeout(() => {
        setShowSuccessModal(false);
      }, 2000);
      return () => clearTimeout(timer);
    }
  }, [processPaymentMutation.isSuccess, showSuccessModal]);

  const handleAddFunds = async () => {
    if (isDemoMode) {
      // Demo mode: Call the API (which will be intercepted by MSW)
      const fundAmount = 100.0;
      try {
        await addFundsMutation.mutateAsync({
          source_id: currentUser?.id || "",
          user_id: currentUser?.id || "",
          amount: fundAmount,
          description: "Funds purchase - Demo top up"
        });
        toast.success(`Added $${fundAmount.toFixed(2)}`);
      } catch (error) {
        toast.error("Failed to add funds");
        console.error("Error adding funds:", error);
      }
    } else if (config?.payment_enabled) {
      // Payment processing enabled: Get checkout URL and redirect using the mutation hook
      try {
        const data = await createPaymentMutation.mutateAsync();
        if (data.url) {
          // Navigate to payment provider checkout page
          window.location.href = data.url;
        } else {
          toast.error("Failed to get checkout URL");
        }
      } catch (error) {
        const errorMessage = error instanceof Error ? error.message : "Failed to initiate payment";
        toast.error(errorMessage);
        console.error("Error creating payment:", error);
      }
    } else {
      toast.error("Payment processing is not configured");
    }
  };

  // Determine add funds configuration
  const addFundsConfig = (() => {
    const hasPaymentEnabled = isDemoMode || !!config?.payment_enabled;
    const hasPaymentProvider = !!settings.paymentProviderUrl;

    if (!hasPaymentEnabled && !hasPaymentProvider) {
      // No payment processing configured at all
      return canManageFunds ? {
        type: 'direct' as const,
        onAddFunds: () => setShowAddFundsModal(true)
      } : undefined;
    }

    // Payment processing is configured (either Stripe or external provider)
    if (canManageFunds) {
      // Admin: split button (payment redirect/checkout primary, direct in dropdown)
      return {
        type: 'split' as const,
        onPrimaryAction: hasPaymentProvider ? handleRedirectToPaymentProvider : handleAddFunds,
        onDirectAction: () => setShowAddFundsModal(true)
      };
    } else {
      // Non-admin: simple payment button
      return {
        type: hasPaymentProvider ? 'redirect' as const : 'direct' as const,
        onAddFunds: hasPaymentProvider ? handleRedirectToPaymentProvider : handleAddFunds
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

      {/* Payment Success Modal */}
      <Dialog open={showSuccessModal} onOpenChange={setShowSuccessModal}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              {processPaymentMutation.isPending ? (
                <>
                  <Loader2 className="h-6 w-6 animate-spin text-blue-500" />
                  Processing Payment
                </>
              ) : processPaymentMutation.isError ? (
                <>
                  <XCircle className="h-6 w-6 text-red-500" />
                  Payment Processing Failed
                </>
              ) : (
                <>
                  <CheckCircle2 className="h-6 w-6 text-green-500" />
                  Payment Successful
                </>
              )}
            </DialogTitle>
            <DialogDescription>
              {processPaymentMutation.isPending ? (
                "Processing your payment and updating your account balance..."
              ) : processPaymentMutation.isError ? (
                <div className="space-y-2">
                  <p className="text-red-600">
                    {processPaymentMutation.error instanceof Error
                      ? processPaymentMutation.error.message
                      : "Failed to process payment"}
                  </p>
                  <p className="text-sm text-gray-600">
                    Your payment may have been successful, but we couldn't confirm it yet.
                    If your balance doesn't update within a few minutes, please contact support.
                  </p>
                </div>
              ) : (
                <div className="space-y-2">
                  <p>Thank you for your payment! Your account balance has been updated.</p>
                  <p className="text-sm text-gray-600">
                    You can now use your credits for API requests.
                  </p>
                </div>
              )}
            </DialogDescription>
          </DialogHeader>
          <div className="flex justify-end gap-2 mt-4">
            {processPaymentMutation.isPending ? (
              <Button variant="outline" onClick={() => setShowSuccessModal(false)}>
                Close (processing in background)
              </Button>
            ) : (
              <Button onClick={() => setShowSuccessModal(false)}>
                Close
              </Button>
            )}
          </div>
        </DialogContent>
      </Dialog>

      {/* Payment Cancelled Modal */}
      <Dialog open={showCancelledModal} onOpenChange={setShowCancelledModal}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <XCircle className="h-6 w-6 text-yellow-500" />
              Payment Cancelled
            </DialogTitle>
            <DialogDescription>
              <div className="space-y-2">
                <p>Your payment was cancelled. No charges have been made to your account.</p>
                <p className="text-sm text-gray-600">
                  You can try adding funds again whenever you're ready.
                </p>
              </div>
            </DialogDescription>
          </DialogHeader>
          <div className="flex justify-end gap-2 mt-4">
            <Button variant="outline" onClick={() => setShowCancelledModal(false)}>
              Close
            </Button>
            <Button onClick={() => {
              setShowCancelledModal(false);
              handleAddFunds();
            }}>
              Try Again
            </Button>
          </div>
        </DialogContent>
      </Dialog>
    </div>
  );
}
