import { Zap, Layers } from "lucide-react";
import { useUpdateOrganization } from "@/api/control-layer/hooks";
import type { Modality, Organization } from "@/api/control-layer/types";
import { Switch } from "@/components/ui/switch";
import { Label } from "@/components/ui/label";

interface ModalitySettingsProps {
  organization: Organization;
  /** Only owners may change these; everyone else sees the current state. */
  canEdit: boolean;
}

const MODALITIES: {
  value: Modality;
  label: string;
  description: string;
  icon: typeof Zap;
}[] = [
  {
    value: "realtime",
    label: "Realtime inference",
    description:
      "Synchronous requests to the chat completions, responses and embeddings endpoints, including flex and background requests.",
    icon: Zap,
  },
  {
    value: "batch",
    label: "Batch API",
    description:
      "Uploading files and creating batches. Existing batches can still be read and cancelled.",
    icon: Layers,
  },
];

/**
 * Owner-controlled switches for the product surfaces an organization may use.
 * A surface that is off is refused at the API for every key the organization
 * owns, whichever member created the key.
 */
export function ModalitySettings({
  organization,
  canEdit,
}: ModalitySettingsProps) {
  const updateOrg = useUpdateOrganization();
  const disabled = new Set<Modality>(organization.disabled_modalities ?? []);

  const setEnabled = async (modality: Modality, enabled: boolean) => {
    const next = new Set(disabled);
    if (enabled) {
      next.delete(modality);
    } else {
      next.add(modality);
    }
    await updateOrg.mutateAsync({
      id: organization.id,
      data: { disabled_modalities: Array.from(next) },
    });
  };

  return (
    <div className="bg-white rounded-lg border border-gray-200 p-6">
      <h4 className="text-lg font-medium text-gray-900 mb-1">Modalities</h4>
      <p className="text-sm text-gray-500 mb-4">
        {canEdit
          ? "Switch off a product surface for everyone in this organization. Requests to a disabled surface are refused for every key the organization owns."
          : "Product surfaces available to this organization. Only an owner can change these."}
      </p>
      <div className="divide-y divide-gray-200">
        {MODALITIES.map(({ value, label, description, icon: Icon }) => {
          const enabled = !disabled.has(value);
          const id = `modality-${value}`;
          return (
            <div
              key={value}
              className="flex items-center justify-between py-4 first:pt-0 last:pb-0"
            >
              <div className="flex items-center gap-3">
                <div className="p-2 bg-gray-100 rounded-lg">
                  <Icon className="w-4 h-4 text-gray-600" />
                </div>
                <div>
                  <Label
                    htmlFor={id}
                    className="text-sm font-medium text-gray-900"
                  >
                    {label}
                  </Label>
                  <p className="text-xs text-gray-500 mt-0.5">{description}</p>
                </div>
              </div>
              <div className="flex items-center gap-2 shrink-0">
                {!enabled && (
                  <span className="text-xs text-red-600 font-medium">
                    Disabled
                  </span>
                )}
                <Switch
                  id={id}
                  checked={enabled}
                  onCheckedChange={(checked) => void setEnabled(value, checked)}
                  disabled={!canEdit || updateOrg.isPending}
                  aria-label={label}
                />
              </div>
            </div>
          );
        })}
      </div>
      {updateOrg.isError && (
        <p className="text-xs text-red-600 mt-3">
          Could not save the change. Please try again.
        </p>
      )}
    </div>
  );
}
