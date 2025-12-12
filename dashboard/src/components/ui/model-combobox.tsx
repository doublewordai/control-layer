import * as React from "react";
import { ChevronsUpDownIcon } from "lucide-react";
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
  queryOptions?: Omit<ModelsQuery, "search" | "limit">; // Additional query options
}

export function ModelCombobox({
  value,
  onValueChange,
  placeholder = "Select a model...",
  searchPlaceholder = "Search models...",
  emptyMessage = "No models found.",
  className,
  filterFn,
  queryOptions,
}: ModelComboboxProps) {
  const [open, setOpen] = React.useState(false);
  const [searchQuery, setSearchQuery] = React.useState("");
  const debouncedSearch = useDebounce(searchQuery, 300);

  const { data: modelsData } = useModels({
    search: debouncedSearch || undefined,
    limit: 20,
    ...queryOptions,
  });

  const models = React.useMemo(() => {
    const allModels = modelsData?.data ?? [];
    return filterFn ? allModels.filter(filterFn) : allModels;
  }, [modelsData, filterFn]);

  const selectedModel = models.find((model) => model.id === value?.id);

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
            {selectedModel?.alias || placeholder}
          </span>
          <ChevronsUpDownIcon className="ml-2 h-4 w-4 shrink-0 opacity-50" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        className="p-0"
        style={{ width: "var(--radix-popover-trigger-width)" }}
      >
        <Command shouldFilter={false}>
          <CommandInput
            placeholder={searchPlaceholder}
            value={searchQuery}
            onValueChange={setSearchQuery}
          />
          <CommandList>
            <CommandEmpty>{emptyMessage}</CommandEmpty>
            <CommandGroup>
              {models.map((model) => (
                <CommandItem
                  key={model.id}
                  value={model.alias}
                  onSelect={() => {
                    onValueChange?.(model);
                    setOpen(false);
                  }}
                >
                  <span className="truncate">{model.alias}</span>
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
