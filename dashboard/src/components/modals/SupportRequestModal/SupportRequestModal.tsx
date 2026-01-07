import React, { useState, useEffect } from "react";
import { Mail } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
  DialogDescription,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { Textarea } from "../../ui/textarea";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../ui/select";

export type SupportSubject =
  | "Model/Feature Request"
  | "Help Running Batches"
  | "General Feedback"
  | "Other";

const SUPPORT_SUBJECTS: SupportSubject[] = [
  "Model/Feature Request",
  "Help Running Batches",
  "General Feedback",
  "Other",
];

const SUPPORT_EMAIL = "support@doubleword.ai";

interface SupportRequestModalProps {
  isOpen: boolean;
  onClose: () => void;
  defaultSubject?: SupportSubject;
}

export const SupportRequestModal: React.FC<SupportRequestModalProps> = ({
  isOpen,
  onClose,
  defaultSubject,
}) => {
  const [subject, setSubject] = useState<SupportSubject | "">("");
  const [message, setMessage] = useState("");

  // Reset form and apply default subject when modal opens
  useEffect(() => {
    if (isOpen) {
      setSubject(defaultSubject ?? "");
      setMessage("");
    }
  }, [isOpen, defaultSubject]);

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!subject || !message.trim()) {
      return;
    }

    const mailtoLink = `mailto:${SUPPORT_EMAIL}?subject=${encodeURIComponent(subject)}&body=${encodeURIComponent(message)}`;
    window.location.href = mailtoLink;
    onClose();
  };

  const isValid = subject !== "" && message.trim().length > 0;

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Contact Support</DialogTitle>
          <DialogDescription>
            Send us a support message at support@doubleword.ai and we'll get
            back to you as soon as possible.
          </DialogDescription>
        </DialogHeader>

        <form
          id="support-request-form"
          onSubmit={handleSubmit}
          className="space-y-4"
        >
          <div>
            <label
              htmlFor="subject"
              className="block text-sm font-medium text-gray-700 mb-2"
            >
              Subject *
            </label>
            <Select
              value={subject}
              onValueChange={(value) => setSubject(value as SupportSubject)}
            >
              <SelectTrigger className="w-full" aria-label="Select subject">
                <SelectValue placeholder="Select a subject" />
              </SelectTrigger>
              <SelectContent>
                {SUPPORT_SUBJECTS.map((subj) => (
                  <SelectItem key={subj} value={subj}>
                    {subj}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>

          <div>
            <label
              htmlFor="message"
              className="block text-sm font-medium text-gray-700 mb-2"
            >
              Message *
            </label>
            <Textarea
              id="message"
              value={message}
              onChange={(e) => setMessage(e.target.value)}
              rows={5}
              className="w-full"
              placeholder="Describe your request or feedback..."
            />
          </div>
        </form>

        <DialogFooter>
          <Button type="button" variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" form="support-request-form" disabled={!isValid}>
            <Mail className="w-4 h-4 mr-2" />
            Open in Email
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
