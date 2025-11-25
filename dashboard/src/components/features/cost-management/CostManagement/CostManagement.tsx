import { useEffect, useState } from "react";
import { useUser, useAddFunds, useConfig, useCreatePayment, useProcessPayment } from "@/api/control-layer";
import { toast } from "sonner";
import { useSettings } from "@/contexts";
import { TransactionHistory } from "@/components/features/cost-management/CostManagement/TransactionHistory.tsx";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { CheckCircle2, XCircle, Loader2 } from "lucide-react";

export function CostManagement() {
  const { isFeatureEnabled } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");
  const { data: config } = useConfig();

  const [showSuccessModal, setShowSuccessModal] = useState(false);
  const [showCancelledModal, setShowCancelledModal] = useState(false);

  // Fetch current user
  const { data: user } = useUser("current");
  const addFundsMutation = useAddFunds();
  const createPaymentMutation = useCreatePayment();
  const processPaymentMutation = useProcessPayment({
    onSuccess: () => {
      setTimeout(() => {
        setShowSuccessModal(false);
      }, 2000);
    },
  });

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
          source_id: user?.id || "",
          user_id: user?.id || "",
          amount: fundAmount,
          description: "Funds purchase - Demo top up"
        });
        toast.success(`Added $${fundAmount.toFixed(2)}`);
      } catch (error) {
        toast.error("Failed to add funds");
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
        toast.error("Failed to transfer to payment provider.");
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
                  <p>Your payment has been captured but not yet applied to your account.</p>
                  <p className="text-sm text-gray-600">
                    Your balance should update automatically within a few minutes. If it doesn't, please contact support.
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
