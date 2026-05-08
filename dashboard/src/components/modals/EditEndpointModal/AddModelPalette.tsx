import React, { useMemo, useState } from "react";
import { Plus, ArrowUpRight, Sparkles } from "lucide-react";
import {
  Command,
  CommandInput,
  CommandList,
  CommandEmpty,
  CommandGroup,
  CommandItem,
  CommandSeparator,
} from "../../ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "../../ui/popover";
import { Button } from "../../ui/button";
import type { AvailableModel } from "../../../api/control-layer/types";

export interface AddModelPaletteProps {
  /** Catalog of models discovered from the provider. May be empty (e.g. provider doesn't support /v1/models). */
  catalog: AvailableModel[];
  /** Provider model names already imported (so they appear muted/non-selectable). */
  importedModelNames: Set<string>;
  /** Called when the user picks a catalog entry or confirms a manual add. */
  onAdd: (modelName: string) => void;
  /** Optional className for the trigger button. */
  triggerClassName?: string;
}

const MAX_VISIBLE_RESULTS = 8;

/**
 * Unified add: catalog + manual entry collapsed into one search input. If
 * the user types something the catalog doesn't know, the last option becomes
 * "Add manually: <typed>" — Enter adds it.
 */
export const AddModelPalette: React.FC<AddModelPaletteProps> = ({
  catalog,
  importedModelNames,
  onAdd,
  triggerClassName,
}) => {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");

  const trimmedQuery = query.trim();
  const isImported = trimmedQuery.length > 0 && importedModelNames.has(trimmedQuery);

  // Split the catalog into available vs already-imported. cmdk filters by its own
  // matcher, but we still want to mute already-imported entries.
  const { available, imported } = useMemo(() => {
    const a: AvailableModel[] = [];
    const i: AvailableModel[] = [];
    for (const m of catalog) {
      (importedModelNames.has(m.id) ? i : a).push(m);
    }
    return { available: a, imported: i };
  }, [catalog, importedModelNames]);

  const handleAdd = (modelName: string) => {
    if (!modelName.trim()) return;
    onAdd(modelName.trim());
    setQuery("");
    setOpen(false);
  };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          type="button"
          size="sm"
          variant="default"
          className={triggerClassName}
          aria-label="Add model"
        >
          <Plus className="w-4 h-4 mr-1" />
          Add model
        </Button>
      </PopoverTrigger>
      <PopoverContent
        className="p-0 w-[420px]"
        align="end"
        sideOffset={6}
        onOpenAutoFocus={(e) => {
          // Let cmdk handle focus on the input
          e.preventDefault();
        }}
      >
        <Command
          shouldFilter
          // cmdk needs help knowing which value matches when both catalog and
          // manual-add coexist. We let it filter the catalog list and append a
          // manual-add item that always shows when query is non-empty.
        >
          <CommandInput
            value={query}
            onValueChange={setQuery}
            placeholder={
              catalog.length > 0
                ? `Search ${catalog.length} model${catalog.length === 1 ? "" : "s"} or type a name…`
                : "Type a model name…"
            }
            autoFocus
          />
          <CommandList>
            {/* Show the catalog when we have one. */}
            {available.length > 0 && (
              <CommandGroup
                heading={
                  trimmedQuery.length > 0
                    ? "From provider catalog"
                    : `Provider catalog · ${available.length}`
                }
              >
                {available.slice(0, MAX_VISIBLE_RESULTS * 4).map((model) => (
                  <CommandItem
                    key={model.id}
                    value={model.id}
                    onSelect={() => handleAdd(model.id)}
                  >
                    <ArrowUpRight className="text-muted-foreground" />
                    <span className="flex-1 truncate">{model.id}</span>
                    {model.owned_by && (
                      <span className="text-xs text-muted-foreground truncate ml-2">
                        {model.owned_by}
                      </span>
                    )}
                  </CommandItem>
                ))}
              </CommandGroup>
            )}

            {/* Show already-imported entries as a muted "Did you mean?" group when
                there's a query. Helpful when a user types a name they already added. */}
            {trimmedQuery.length > 0 && imported.length > 0 && (
              <>
                <CommandSeparator />
                <CommandGroup heading="Already imported">
                  {imported.slice(0, MAX_VISIBLE_RESULTS).map((model) => (
                    <CommandItem
                      key={`imported-${model.id}`}
                      value={model.id}
                      disabled
                      className="opacity-60"
                    >
                      <ArrowUpRight />
                      <span className="flex-1 truncate">{model.id}</span>
                      <span className="text-xs text-muted-foreground">
                        already imported
                      </span>
                    </CommandItem>
                  ))}
                </CommandGroup>
              </>
            )}

            {/* Manual-add row: always present when query is non-empty and not
                already imported. cmdk shows it because we set value to the query,
                so it always matches itself. */}
            {trimmedQuery.length > 0 && !isImported && (
              <>
                <CommandSeparator />
                <CommandGroup heading="Manual">
                  <CommandItem
                    value={`__manual__${trimmedQuery}`}
                    onSelect={() => handleAdd(trimmedQuery)}
                    keywords={[trimmedQuery]}
                  >
                    <Sparkles className="text-muted-foreground" />
                    <span className="flex-1 truncate">
                      Add manually: <strong>{trimmedQuery}</strong>
                    </span>
                  </CommandItem>
                </CommandGroup>
              </>
            )}

            <CommandEmpty>
              {catalog.length === 0
                ? "Type a model name and press Enter to add it manually."
                : "No catalog matches. Type to add manually."}
            </CommandEmpty>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
};
