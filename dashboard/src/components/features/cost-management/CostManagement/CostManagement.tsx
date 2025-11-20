import { useEffect, useState } from "react";
import { useUser, useAddFunds, useConfig } from "@/api/control-layer";
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

  // Fetch current user
  const { data: user, refetch: refetchUser } = useUser("current");
  const addFundsMutation = useAddFunds();

  const [showSuccessModal, setShowSuccessModal] = useState(false);
  const [showCancelledModal, setShowCancelledModal] = useState(false);
  const [isProcessingPayment, setIsProcessingPayment] = useState(false);
  const [processingError, setProcessingError] = useState<string | null>(null);

  // Handle return from payment provider
  useEffect(() => {
    if (isDemoMode) return;

    const urlParams = new URLSearchParams(window.location.search);
    const paymentStatus = urlParams.get("payment");
    const sessionId = urlParams.get("session_id");

    if (paymentStatus === "success" && sessionId) {
      // Process the payment immediately
      setShowSuccessModal(true);
      setIsProcessingPayment(true);
      setProcessingError(null);

      // Call process_payment endpoint
      fetch(`/admin/api/v1/payments/process/${sessionId}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
      })
        .then(async (response) => {
          if (response.ok) {
            // Payment processed successfully
            await refetchUser();
            setIsProcessingPayment(false);
          } else if (response.status === 402) {
            // Payment not completed yet
            setProcessingError("Payment is still processing. Please check back in a moment.");
            setIsProcessingPayment(false);
          } else {
            const errorData = await response.json().catch(() => ({}));
            setProcessingError(errorData.message || "Failed to process payment");
            setIsProcessingPayment(false);
          }
        })
        .catch((error) => {
          console.error("Error processing payment:", error);
          setProcessingError("Failed to process payment");
          setIsProcessingPayment(false);
        });

      // Clean up URL
      window.history.replaceState({}, "", window.location.pathname);
    } else if (paymentStatus === "cancelled" && sessionId) {
      setShowCancelledModal(true);
      // Clean up URL
      window.history.replaceState({}, "", window.location.pathname);
    } else if (paymentStatus === "success" && !sessionId) {
      // Legacy support for old payment_complete parameter
      toast.success("Payment completed! Your balance has been updated.");
      refetchUser();
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
        const response = await fetch("/admin/api/v1/payments/create_checkout", {
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

      {/* Payment Success Modal */}
      <Dialog open={showSuccessModal} onOpenChange={setShowSuccessModal}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              {isProcessingPayment ? (
                <>
                  <Loader2 className="h-6 w-6 animate-spin text-blue-500" />
                  Processing Payment
                </>
              ) : processingError ? (
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
              {isProcessingPayment ? (
                "Processing your payment and updating your account balance..."
              ) : processingError ? (
                <div className="space-y-2">
                  <p className="text-red-600">{processingError}</p>
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
          {!isProcessingPayment && (
            <div className="flex justify-end gap-2 mt-4">
              <Button onClick={() => setShowSuccessModal(false)}>
                Close
              </Button>
            </div>
          )}
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
