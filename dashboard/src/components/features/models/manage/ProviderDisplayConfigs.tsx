import React, { useEffect, useState } from "react";
import { ArrowLeft, Plus, Save, Trash2 } from "lucide-react";
import { useNavigate } from "react-router-dom";
import {
  useCreateProviderDisplayConfig,
  useDeleteProviderDisplayConfig,
  useProviderDisplayConfigs,
  useUpdateProviderDisplayConfig,
} from "../../../../api/control-layer";
import type { ProviderDisplayConfig } from "../../../../api/control-layer/types";
import { Button } from "../../../ui/button";
import { Input } from "../../../ui/input";
import { Label } from "../../../ui/label";

type EditableProvider = {
  draft_id: string;
  provider_key: string;
  display_name: string;
  icon: string;
  sort_order: number;
  configured: boolean;
  model_count: number;
};

function createDraftId(): string {
  return `provider-draft-${Math.random().toString(36).slice(2, 10)}`;
}

function toDraft(provider: ProviderDisplayConfig): EditableProvider {
  return {
    draft_id: provider.provider_key,
    provider_key: provider.provider_key,
    display_name: provider.display_name,
    icon: provider.icon || "",
    sort_order:
      provider.sort_order === Number.MAX_SAFE_INTEGER ? 0 : provider.sort_order,
    configured: provider.configured,
    model_count: provider.model_count,
  };
}

const ProviderDisplayConfigs: React.FC = () => {
  const navigate = useNavigate();
  const { data: providers = [], isLoading } = useProviderDisplayConfigs();
  const createProvider = useCreateProviderDisplayConfig();
  const updateProvider = useUpdateProviderDisplayConfig();
  const deleteProvider = useDeleteProviderDisplayConfig();

  const [drafts, setDrafts] = useState<EditableProvider[]>([]);

  useEffect(() => {
    setDrafts(providers.map(toDraft));
  }, [providers]);

  const isSaving =
    createProvider.isPending ||
    updateProvider.isPending ||
    deleteProvider.isPending;

  const updateDraft = (index: number, updater: (draft: EditableProvider) => EditableProvider) => {
    setDrafts((current) =>
      current.map((draft, currentIndex) =>
        currentIndex === index ? updater(draft) : draft,
      ),
    );
  };

  const handleSave = async (draft: EditableProvider) => {
    const providerKey = draft.provider_key.trim().toLowerCase();
    const displayName = draft.display_name.trim();
    const icon = draft.icon.trim();

    if (!providerKey || !displayName) return;

    if (!draft.configured) {
      await createProvider.mutateAsync({
        provider_key: providerKey,
        display_name: displayName,
        icon: icon || undefined,
        sort_order: draft.sort_order,
      });
      return;
    }

    await updateProvider.mutateAsync({
      providerKey,
      data: {
        display_name: displayName,
        icon: icon || null,
        sort_order: draft.sort_order,
      },
    });
  };

  return (
    <div className="p-4 md:p-6">
      <div className="mb-6 flex items-center justify-between gap-4">
        <div className="flex items-center gap-3">
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => navigate("/models/manage")}
          >
            <ArrowLeft className="mr-2 h-4 w-4" />
            Back
          </Button>
          <div>
            <h1 className="text-2xl md:text-3xl font-bold text-doubleword-neutral-900">
              Manage Providers
            </h1>
            <p className="text-sm text-muted-foreground">
              Configure provider-level icons and ordering for the public models page.
            </p>
          </div>
        </div>
        <Button
          type="button"
          variant="outline"
          onClick={() =>
            setDrafts((current) => [
              ...current,
              {
                draft_id: createDraftId(),
                provider_key: "",
                display_name: "",
                icon: "",
                sort_order: 0,
                configured: false,
                model_count: 0,
              },
            ])
          }
          className="gap-2"
        >
          <Plus className="h-4 w-4" />
          Add Provider
        </Button>
      </div>

      <div className="space-y-4">
        {isLoading ? (
          <div className="rounded-lg border bg-white p-6 text-sm text-muted-foreground">
            Loading providers...
          </div>
        ) : drafts.length === 0 ? (
          <div className="rounded-lg border bg-white p-6 text-sm text-muted-foreground">
            No providers found yet.
          </div>
        ) : (
          drafts.map((draft, index) => (
            <div key={draft.draft_id} className="rounded-xl border bg-white p-5">
              <div className="grid gap-4 md:grid-cols-[1.2fr_1.2fr_2fr_120px_auto] md:items-end">
                <div>
                  <Label>Provider Key</Label>
                  <Input
                    value={draft.provider_key}
                    onChange={(e) =>
                      updateDraft(index, (current) => ({
                        ...current,
                        provider_key: e.target.value,
                      }))
                    }
                    disabled={draft.configured}
                    className="mt-1"
                    placeholder="openai"
                  />
                </div>
                <div>
                  <Label>Display Name</Label>
                  <Input
                    value={draft.display_name}
                    onChange={(e) =>
                      updateDraft(index, (current) => ({
                        ...current,
                        display_name: e.target.value,
                      }))
                    }
                    className="mt-1"
                    placeholder="OpenAI"
                  />
                </div>
                <div>
                  <Label>Icon</Label>
                  <Input
                    value={draft.icon}
                    onChange={(e) =>
                      updateDraft(index, (current) => ({
                        ...current,
                        icon: e.target.value,
                      }))
                    }
                    className="mt-1"
                    placeholder="https://..., /asset.svg, or built-in key"
                  />
                </div>
                <div>
                  <Label>Sort Order</Label>
                  <Input
                    type="number"
                    value={draft.sort_order}
                    onChange={(e) =>
                      updateDraft(index, (current) => ({
                        ...current,
                        sort_order: e.target.value ? parseInt(e.target.value, 10) : 0,
                      }))
                    }
                    className="mt-1"
                  />
                </div>
                <div className="flex gap-2">
                  <Button
                    type="button"
                    onClick={() => void handleSave(draft)}
                    disabled={isSaving || !draft.provider_key.trim() || !draft.display_name.trim()}
                    className="gap-2"
                  >
                    <Save className="h-4 w-4" />
                    Save
                  </Button>
                  {draft.configured ? (
                    <Button
                      type="button"
                      variant="outline"
                      onClick={() => void deleteProvider.mutateAsync(draft.provider_key)}
                      disabled={isSaving}
                      className="gap-2 text-red-700 hover:text-red-800"
                    >
                      <Trash2 className="h-4 w-4" />
                    </Button>
                  ) : (
                    <Button
                      type="button"
                      variant="outline"
                      onClick={() =>
                        setDrafts((current) =>
                          current.filter((_, currentIndex) => currentIndex !== index),
                        )
                      }
                      className="gap-2"
                    >
                      <Trash2 className="h-4 w-4" />
                    </Button>
                  )}
                </div>
              </div>
            </div>
          ))
        )}
      </div>
    </div>
  );
};

export default ProviderDisplayConfigs;
