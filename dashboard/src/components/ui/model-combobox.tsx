import * as React from "react";
import { ChevronsUpDownIcon, Layers, Server } from "lucide-react";
import { useModels } from "../../api/control-layer";
import { useDebounce } from "../../hooks/useDebounce";
import { cn } from "../../lib/utils";
import { Button } from "./button";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "./command";
import { Popover, PopoverContent, PopoverTrigger } from "./popover";
import type { Model, ModelsQuery } from "../../api/control-layer/types";

interface ModelComboboxProps {
  value: Model | null;
  onValueChange?: (value: Model) => void;
  placeholder?: React.ReactNode;
  searchPlaceholder?: string;
  emptyMessage?: string;
  className?: string;
  filterFn?: (model: Model) => boolean; // Optional filter function for models
  queryOptions?: Omit<ModelsQuery, "search" | "limit" | "include">; // Additional query options
  /**
   * Width of the popover. Defaults to matching the trigger button width.
   * Pass a value like "min-w-[28rem]" to expand the dropdown beyond the
   * trigger — useful when the trigger is narrow but model names can be long.
   */
  popoverClassName?: string;
}

const RESULT_LIMIT = 50;

export function ModelCombobox({
  value,
  onValueChange,
  placeholder = "Select a model...",
  searchPlaceholder = "Search by alias, model name, or endpoint…",
  emptyMessage = "No models found.",
  className,
  filterFn,
  queryOptions,
  popoverClassName,
}: ModelComboboxProps) {
  const [open, setOpen] = React.useState(false);
  const [searchQuery, setSearchQuery] = React.useState("");
  const debouncedSearch = useDebounce(searchQuery, 300);

  // Always include endpoint info so the dropdown can show which endpoint a
  // model is hosted on — critical when many endpoints expose similarly-named
  // models (e.g. multiple "gpt-4o" deployments).
  const { data: modelsData, isLoading } = useModels({
    search: debouncedSearch || undefined,
    limit: RESULT_LIMIT,
    include: "endpoints",
    ...queryOptions,
  });

  const models = React.useMemo(() => {
    const allModels = modelsData?.data ?? [];
    return filterFn ? allModels.filter(filterFn) : allModels;
  }, [modelsData, filterFn]);

  // The currently-selected model may not be in the latest result set (e.g.
  // user searched for something else after picking). Fall back to the value
  // prop directly so we can still render its label.
  const selectedModel = models.find((m) => m.id === value?.id) ?? value;

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="outline"
          role="combobox"
          aria-expanded={open}
          aria-label="Select model"
          className={cn("justify-between text-left", className)}
        >
          <span className="truncate">
            {selectedModel ? (
              <ModelTriggerLabel model={selectedModel} />
            ) : (
              placeholder
            )}
          </span>
          <ChevronsUpDownIcon className="ml-2 h-4 w-4 shrink-0 opacity-50" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        className={cn("p-0", popoverClassName)}
        style={
          popoverClassName
            ? undefined
            : { width: "var(--radix-popover-trigger-width)" }
        }
      >
        {/* shouldFilter is enabled so cmdk does fuzzy matching across the
            visible result set. The server still pre-filters via `search`, so
            for very large catalogs the user types -> server narrows ->
            cmdk fuzzy-refines. Best of both worlds. */}
        <Command>
          <CommandInput
            placeholder={searchPlaceholder}
            value={searchQuery}
            onValueChange={setSearchQuery}
          />
          <CommandList>
            <CommandEmpty>
              {isLoading ? "Loading…" : emptyMessage}
            </CommandEmpty>
            <CommandGroup>
              {models.map((model) => (
                <CommandItem
                  key={model.id}
                  // Including alias + model_name + endpoint name in the
                  // matchable value lets cmdk find a model by any of them,
                  // not just by alias.
                  value={cmdkValueFor(model)}
                  onSelect={() => {
                    onValueChange?.(model);
                    setOpen(false);
                  }}
                  className="flex flex-col items-start gap-0.5 py-2"
                >
                  <ModelOptionLabel model={model} />
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}

function cmdkValueFor(model: Model): string {
  // cmdk treats this as the matchable string; include every field a user
  // might type. Prefix with id to keep values unique across rows.
  const endpointName = model.endpoint?.name ?? "";
  const kind = model.is_composite ? "virtual" : "hosted";
  return `${model.id}|${model.alias}|${model.model_name}|${endpointName}|${kind}`;
}

const ModelTriggerLabel: React.FC<{ model: Model }> = ({ model }) => {
  const subtitle = subtitleFor(model);
  return (
    <span className="flex flex-col leading-tight overflow-hidden">
      <span className="truncate">{model.alias}</span>
      {subtitle && (
        <span className="truncate text-[11px] text-muted-foreground">
          {subtitle}
        </span>
      )}
    </span>
  );
};

const ModelOptionLabel: React.FC<{ model: Model }> = ({ model }) => {
  const subtitle = subtitleFor(model);
  return (
    <>
      <div className="flex items-center gap-2 w-full min-w-0">
        <span className="truncate font-medium">{model.alias}</span>
        {model.is_composite ? (
          <span className="ml-auto inline-flex items-center gap-1 text-[10px] uppercase tracking-wide px-1.5 py-0.5 rounded bg-violet-50 text-violet-700 border border-violet-200 shrink-0">
            <Layers className="h-3 w-3" /> virtual
          </span>
        ) : null}
      </div>
      {subtitle && (
        <span className="text-xs text-muted-foreground truncate w-full">
          {subtitle}
        </span>
      )}
    </>
  );
};

function subtitleFor(model: Model): string | null {
  // Show: model_name (the provider name) + endpoint name. Skip redundant
  // bits — if alias === model_name, only show the endpoint.
  const parts: string[] = [];
  if (model.is_composite) {
    if (model.alias !== model.model_name) parts.push(model.model_name);
  } else {
    if (model.alias !== model.model_name) parts.push(model.model_name);
    if (model.endpoint?.name) {
      parts.push(`on ${model.endpoint.name}`);
    } else if (!model.endpoint && model.hosted_on) {
      // include=endpoints didn't return the endpoint (rare) — skip rather
      // than show a UUID.
    }
  }
  return parts.length > 0 ? parts.join(" · ") : null;
}

// Re-export the icon for callers that want to wrap their own triggers.
export { Server as ModelEndpointIcon };
