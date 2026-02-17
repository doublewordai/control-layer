import React, { useState, useEffect } from "react";
import { Plus, Pencil, Trash2, Check, X } from "lucide-react";
import { Button } from "@/components";
import { Input } from "../../../ui/input";
import { Label } from "../../../ui/label";
import { formatTariffPrice } from "@/utils";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "../../../ui/table";
import type {
  ModelTariff,
  TariffDefinition,
  TariffApiKeyPurpose,
} from "@/api/control-layer";

interface TariffFormData {
  name: string;
  input_price_per_million: string;
  output_price_per_million: string;
  api_key_purpose: TariffApiKeyPurpose | "none";
  completion_window: string; // Priority like "Standard (24h)", "High (1h)", etc.
}

// Internal representation with temporary IDs for editing
interface TariffEdit extends TariffDefinition {
  _tempId: string;
  valid_from?: string; // ISO date string, only for existing tariffs
}

interface ModelTariffTableProps {
  tariffs: ModelTariff[];
  onChange: (tariffs: TariffDefinition[]) => void;
  isLoading?: boolean;
  readOnly?: boolean;
  availableSLAs?: string[]; // Available priorities like ["Standard (24h)", "High (1h)"]
}

const EMPTY_FORM: TariffFormData = {
  name: "",
  input_price_per_million: "",
  output_price_per_million: "",
  api_key_purpose: "none",
  completion_window: "",
};

const API_KEY_PURPOSE_LABELS: Record<TariffApiKeyPurpose | "none", string> = {
  realtime: "Realtime",
  batch: "Batch",
  playground: "Playground",
  none: "None (fallback)",
};

export const ModelTariffTable: React.FC<ModelTariffTableProps> = ({
  tariffs,
  onChange,
  isLoading = false,
  readOnly = false,
  availableSLAs = ["Standard (24h)"], // Default to standard priority if not provided
}) => {
  // Local state: convert ModelTariff[] to TariffEdit[] for editing
  const [localTariffs, setLocalTariffs] = useState<TariffEdit[]>([]);
  const [isAdding, setIsAdding] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [formData, setFormData] = useState<TariffFormData>(EMPTY_FORM);
  const [errors, setErrors] = useState<{ [key: string]: string }>({});

  // Initialize local state from props (only active tariffs)
  useEffect(() => {
    const activeTariffs = tariffs.filter((t) => t.is_active);
    setLocalTariffs(
      activeTariffs.map((t) => ({
        name: t.name,
        input_price_per_token: t.input_price_per_token,
        output_price_per_token: t.output_price_per_token,
        api_key_purpose: t.api_key_purpose,
        completion_window: t.completion_window,
        valid_from: t.valid_from,
        _tempId: t.id,
      })),
    );
  }, [tariffs]);

  // Notify parent whenever local state changes
  const updateTariffs = (newTariffs: TariffEdit[]) => {
    setLocalTariffs(newTariffs);
    // Convert to TariffDefinition[] (remove _tempId and valid_from)
    onChange(
      newTariffs.map(({ _tempId, valid_from: _valid_from, ...def }) => def),
    );
  };

  // Generate default name based on purpose and priority
  const getDefaultName = (
    purpose: TariffApiKeyPurpose | "none",
    priority?: string,
  ): string => {
    if (purpose === "none") return "";
    if (purpose === "batch" && priority) {
      return priority; // Priority already includes display name like "Standard (24h)"
    }
    return API_KEY_PURPOSE_LABELS[purpose];
  };

  const validateForm = (): boolean => {
    const newErrors: { [key: string]: string } = {};

    if (!formData.name.trim()) {
      newErrors.name = "Name is required";
    }

    const inputPrice = Number(formData.input_price_per_million);
    if (
      formData.input_price_per_million &&
      (isNaN(inputPrice) || inputPrice < 0)
    ) {
      newErrors.input_price = "Must be a valid positive number";
    }

    const outputPrice = Number(formData.output_price_per_million);
    if (
      formData.output_price_per_million &&
      (isNaN(outputPrice) || outputPrice < 0)
    ) {
      newErrors.output_price = "Must be a valid positive number";
    }

    // Batch tariffs must have a completion_window
    if (formData.api_key_purpose === "batch" && !formData.completion_window) {
      newErrors.completion_window = "Priority is required for batch tariffs";
    }

    setErrors(newErrors);
    return Object.keys(newErrors).length === 0;
  };

  const handleStartAdd = () => {
    setIsAdding(true);
    setFormData(EMPTY_FORM);
    setErrors({});
  };

  const handleCancelAdd = () => {
    setIsAdding(false);
    setFormData(EMPTY_FORM);
    setErrors({});
  };

  const handleSaveAdd = () => {
    if (!validateForm()) return;

    const inputPrice = Number(formData.input_price_per_million) / 1000000 || 0;
    const outputPrice =
      Number(formData.output_price_per_million) / 1000000 || 0;

    const newTariff: TariffEdit = {
      name: formData.name,
      input_price_per_token: inputPrice.toString(),
      output_price_per_token: outputPrice.toString(),
      api_key_purpose:
        formData.api_key_purpose === "none"
          ? undefined
          : formData.api_key_purpose,
      completion_window: formData.completion_window || undefined,
      _tempId: `temp-${Date.now()}-${Math.random()}`,
    };

    updateTariffs([...localTariffs, newTariff]);
    setIsAdding(false);
    setFormData(EMPTY_FORM);
  };

  const handleStartEdit = (tariff: TariffEdit) => {
    setEditingId(tariff._tempId);
    setFormData({
      name: tariff.name,
      input_price_per_million: (
        parseFloat(tariff.input_price_per_token) * 1000000
      ).toString(),
      output_price_per_million: (
        parseFloat(tariff.output_price_per_token) * 1000000
      ).toString(),
      api_key_purpose: tariff.api_key_purpose || "none",
      completion_window: tariff.completion_window || "",
    });
    setErrors({});
  };

  const handleCancelEdit = () => {
    setEditingId(null);
    setFormData(EMPTY_FORM);
    setErrors({});
  };

  const handleSaveEdit = (tempId: string) => {
    if (!validateForm()) return;

    const inputPrice = Number(formData.input_price_per_million) / 1000000 || 0;
    const outputPrice =
      Number(formData.output_price_per_million) / 1000000 || 0;

    const updatedTariffs = localTariffs.map((t) =>
      t._tempId === tempId
        ? {
            ...t,
            name: formData.name,
            input_price_per_token: inputPrice.toString(),
            output_price_per_token: outputPrice.toString(),
            api_key_purpose:
              formData.api_key_purpose === "none"
                ? undefined
                : formData.api_key_purpose,
            completion_window: formData.completion_window || undefined,
          }
        : t,
    );

    updateTariffs(updatedTariffs);
    setEditingId(null);
    setFormData(EMPTY_FORM);
  };

  const handleDelete = (tempId: string) => {
    updateTariffs(localTariffs.filter((t) => t._tempId !== tempId));
  };

  // Get available API key purposes for dropdown (exclude already-used ones, except "none")
  // For batch purpose, allow multiple tariffs (one per SLA)
  const getAvailablePurposes = (excludeTempId?: string) => {
    const usedPurposes = new Set(
      localTariffs
        .filter(
          (t) => t._tempId !== excludeTempId && t.api_key_purpose !== undefined,
        )
        .map((t) => t.api_key_purpose),
    );

    return Object.entries(API_KEY_PURPOSE_LABELS).filter(([value]) => {
      // Always allow "none"
      if (value === "none") return true;
      // Allow batch even if one exists (can have multiple with different SLAs)
      if (value === "batch") return true;
      // For other purposes, exclude if already used
      return !usedPurposes.has(value as TariffApiKeyPurpose);
    });
  };

  const renderRow = (tariff: TariffEdit) => {
    const isEditing = editingId === tariff._tempId;

    if (isEditing) {
      return (
        <TableRow key={tariff._tempId}>
          <TableCell>
            <Select
              value={formData.api_key_purpose}
              onValueChange={(value) => {
                const purpose = value as TariffApiKeyPurpose | "none";
                const newName = getDefaultName(
                  purpose,
                  formData.completion_window,
                );
                setFormData({
                  ...formData,
                  api_key_purpose: purpose,
                  name: newName,
                });
              }}
            >
              <SelectTrigger className="w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {getAvailablePurposes(tariff._tempId).map(([value, label]) => (
                  <SelectItem key={value} value={value}>
                    {label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </TableCell>
          <TableCell>
            <Input
              value={formData.name}
              onChange={(e) =>
                setFormData({ ...formData, name: e.target.value })
              }
              placeholder="Tariff name"
              className={errors.name ? "border-red-500" : ""}
            />
            {errors.name && (
              <p className="text-xs text-red-500 mt-1">{errors.name}</p>
            )}
          </TableCell>
          <TableCell>
            <Input
              value={formData.input_price_per_million}
              onChange={(e) =>
                setFormData({
                  ...formData,
                  input_price_per_million: e.target.value,
                })
              }
              placeholder="0.00"
              className={errors.input_price ? "border-red-500" : ""}
            />
            {errors.input_price && (
              <p className="text-xs text-red-500 mt-1">{errors.input_price}</p>
            )}
          </TableCell>
          <TableCell>
            <Input
              value={formData.output_price_per_million}
              onChange={(e) =>
                setFormData({
                  ...formData,
                  output_price_per_million: e.target.value,
                })
              }
              placeholder="0.00"
              className={errors.output_price ? "border-red-500" : ""}
            />
            {errors.output_price && (
              <p className="text-xs text-red-500 mt-1">{errors.output_price}</p>
            )}
          </TableCell>
          <TableCell>
            {formData.api_key_purpose === "batch" ? (
              <>
                <Select
                  value={formData.completion_window}
                  onValueChange={(value) => {
                    const newName = getDefaultName(
                      formData.api_key_purpose,
                      value,
                    );
                    setFormData({
                      ...formData,
                      completion_window: value,
                      name: newName,
                    });
                  }}
                >
                  <SelectTrigger
                    className={`w-full ${errors.completion_window ? "border-red-500" : ""}`}
                  >
                    <SelectValue placeholder="Select Priority" />
                  </SelectTrigger>
                  <SelectContent>
                    {availableSLAs.map((sla) => (
                      <SelectItem key={sla} value={sla}>
                        {sla}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                {errors.completion_window && (
                  <p className="text-xs text-red-500 mt-1">
                    {errors.completion_window}
                  </p>
                )}
              </>
            ) : (
              <span className="text-sm text-gray-400 italic">N/A</span>
            )}
          </TableCell>
          <TableCell>
            {tariff.valid_from ? (
              <span className="text-sm text-gray-600">
                {new Date(tariff.valid_from).toLocaleString()}
              </span>
            ) : (
              <span className="text-sm text-gray-400 italic">New</span>
            )}
          </TableCell>
          <TableCell>
            <div className="flex gap-1">
              <Button
                size="sm"
                variant="ghost"
                onClick={() => handleSaveEdit(tariff._tempId)}
                disabled={isLoading}
              >
                <Check className="h-4 w-4" />
              </Button>
              <Button
                size="sm"
                variant="ghost"
                onClick={handleCancelEdit}
                disabled={isLoading}
              >
                <X className="h-4 w-4" />
              </Button>
            </div>
          </TableCell>
        </TableRow>
      );
    }

    return (
      <TableRow key={tariff._tempId}>
        <TableCell>
          {tariff.api_key_purpose
            ? tariff.api_key_purpose === "batch" && tariff.completion_window
              ? `${API_KEY_PURPOSE_LABELS[tariff.api_key_purpose]} (${tariff.completion_window.charAt(0).toUpperCase() + tariff.completion_window.slice(1)})`
              : API_KEY_PURPOSE_LABELS[tariff.api_key_purpose]
            : API_KEY_PURPOSE_LABELS.none}
        </TableCell>
        <TableCell>{tariff.name}</TableCell>
        <TableCell>{formatTariffPrice(tariff.input_price_per_token)}</TableCell>
        <TableCell>
          {formatTariffPrice(tariff.output_price_per_token)}
        </TableCell>
        <TableCell>
          {tariff.completion_window ? (
            <span className="text-sm text-gray-600">
              {tariff.completion_window.charAt(0).toUpperCase() + tariff.completion_window.slice(1)}
            </span>
          ) : (
            <span className="text-sm text-gray-400 italic">N/A</span>
          )}
        </TableCell>
        <TableCell>
          {tariff.valid_from ? (
            <span className="text-sm text-gray-600">
              {new Date(tariff.valid_from).toLocaleString()}
            </span>
          ) : (
            <span className="text-sm text-gray-400 italic">New</span>
          )}
        </TableCell>
        <TableCell>
          {!readOnly && (
            <div className="flex gap-1">
              <Button
                size="sm"
                variant="ghost"
                onClick={() => handleStartEdit(tariff)}
                disabled={isLoading}
              >
                <Pencil className="h-4 w-4" />
              </Button>
              <Button
                size="sm"
                variant="ghost"
                onClick={() => handleDelete(tariff._tempId)}
                disabled={isLoading}
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            </div>
          )}
        </TableCell>
      </TableRow>
    );
  };

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <Label>Model Tariffs</Label>
        {!readOnly && !isAdding && (
          <Button
            size="sm"
            variant="outline"
            onClick={handleStartAdd}
            disabled={isLoading}
          >
            <Plus className="h-4 w-4 mr-1" />
            Add Tariff
          </Button>
        )}
      </div>

      <div className="border rounded-md">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="w-[180px]">API Key Purpose</TableHead>
              <TableHead className="w-[180px]">Name</TableHead>
              <TableHead className="w-20">Input (per 1M)</TableHead>
              <TableHead className="w-20">Output (per 1M)</TableHead>
              <TableHead className="w-20">Priority</TableHead>
              <TableHead className="w-20">Valid From</TableHead>
              <TableHead className="w-20">Actions</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {localTariffs.length === 0 && !isAdding && (
              <TableRow>
                <TableCell colSpan={7} className="text-center text-gray-500">
                  No tariffs configured. Add one to get started.
                </TableCell>
              </TableRow>
            )}
            {localTariffs.map(renderRow)}
            {isAdding && (
              <TableRow>
                <TableCell>
                  <Select
                    value={formData.api_key_purpose}
                    onValueChange={(value) => {
                      const purpose = value as TariffApiKeyPurpose | "none";
                      const newName = getDefaultName(
                        purpose,
                        formData.completion_window,
                      );
                      setFormData({
                        ...formData,
                        api_key_purpose: purpose,
                        name: newName,
                      });
                    }}
                  >
                    <SelectTrigger className="w-full">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {getAvailablePurposes().map(([value, label]) => (
                        <SelectItem key={value} value={value}>
                          {label}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </TableCell>
                <TableCell>
                  <Input
                    value={formData.name}
                    onChange={(e) =>
                      setFormData({ ...formData, name: e.target.value })
                    }
                    placeholder="e.g., Standard Pricing"
                    className={errors.name ? "border-red-500" : ""}
                  />
                  {errors.name && (
                    <p className="text-xs text-red-500 mt-1">{errors.name}</p>
                  )}
                </TableCell>
                <TableCell>
                  <Input
                    value={formData.input_price_per_million}
                    onChange={(e) =>
                      setFormData({
                        ...formData,
                        input_price_per_million: e.target.value,
                      })
                    }
                    placeholder="3.00"
                    className={errors.input_price ? "border-red-500" : ""}
                  />
                  {errors.input_price && (
                    <p className="text-xs text-red-500 mt-1">
                      {errors.input_price}
                    </p>
                  )}
                </TableCell>
                <TableCell>
                  <Input
                    value={formData.output_price_per_million}
                    onChange={(e) =>
                      setFormData({
                        ...formData,
                        output_price_per_million: e.target.value,
                      })
                    }
                    placeholder="15.00"
                    className={errors.output_price ? "border-red-500" : ""}
                  />
                  {errors.output_price && (
                    <p className="text-xs text-red-500 mt-1">
                      {errors.output_price}
                    </p>
                  )}
                </TableCell>
                <TableCell>
                  {formData.api_key_purpose === "batch" ? (
                    <>
                      <Select
                        value={formData.completion_window}
                        onValueChange={(value) => {
                          const newName = getDefaultName(
                            formData.api_key_purpose,
                            value,
                          );
                          setFormData({
                            ...formData,
                            completion_window: value,
                            name: newName,
                          });
                        }}
                      >
                        <SelectTrigger
                          className={`w-full ${errors.completion_window ? "border-red-500" : ""}`}
                        >
                          <SelectValue placeholder="Select Priority" />
                        </SelectTrigger>
                        <SelectContent>
                          {availableSLAs.map((sla) => (
                            <SelectItem key={sla} value={sla}>
                              {sla}
                            </SelectItem>
                          ))}
                        </SelectContent>
                      </Select>
                      {errors.completion_window && (
                        <p className="text-xs text-red-500 mt-1">
                          {errors.completion_window}
                        </p>
                      )}
                    </>
                  ) : (
                    <span className="text-sm text-gray-400 italic">N/A</span>
                  )}
                </TableCell>
                <TableCell>
                  <span className="text-sm text-gray-400 italic">New</span>
                </TableCell>
                <TableCell>
                  <div className="flex gap-1">
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={handleSaveAdd}
                      disabled={isLoading}
                    >
                      <Check className="h-4 w-4" />
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={handleCancelAdd}
                      disabled={isLoading}
                    >
                      <X className="h-4 w-4" />
                    </Button>
                  </div>
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </div>

      <p className="text-sm text-gray-500">
        Prices are in dollars per million tokens. Different purposes allow you
        to charge different rates for realtime, batch, and playground usage.
      </p>
    </div>
  );
};
