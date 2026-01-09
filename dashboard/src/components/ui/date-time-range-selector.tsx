import * as React from "react";
import { CalendarIcon } from "lucide-react";
import { type DateRange } from "react-day-picker";
import { format } from "date-fns";

import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Calendar } from "@/components/ui/calendar";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { Separator } from "@/components/ui/separator";

interface DateTimeRangeSelectorProps {
  value?: {
    from: Date;
    to: Date;
  };
  onChange?: (range: { from: Date; to: Date } | undefined) => void;
  className?: string;
}

export function DateTimeRangeSelector({
  value,
  onChange,
  className,
}: DateTimeRangeSelectorProps) {
  // Default to last 24 hours
  const getDefaultRange = () => {
    const now = new Date();
    const from = new Date(now.getTime() - 24 * 60 * 60 * 1000);
    return { from, to: now };
  };

  const [range, setRange] = React.useState<DateRange | undefined>(
    value ? { from: value.from, to: value.to } : getDefaultRange(),
  );
  const [startTime, setStartTime] = React.useState(() => {
    if (value) {
      return `${value.from.getHours().toString().padStart(2, "0")}:${value.from
        .getMinutes()
        .toString()
        .padStart(2, "0")}`;
    }
    const from = getDefaultRange().from;
    return `${from.getHours().toString().padStart(2, "0")}:${from
      .getMinutes()
      .toString()
      .padStart(2, "0")}`;
  });
  const [endTime, setEndTime] = React.useState(() => {
    if (value) {
      return `${value.to.getHours().toString().padStart(2, "0")}:${value.to
        .getMinutes()
        .toString()
        .padStart(2, "0")}`;
    }
    const to = getDefaultRange().to;
    return `${to.getHours().toString().padStart(2, "0")}:${to
      .getMinutes()
      .toString()
      .padStart(2, "0")}`;
  });
  const [open, setOpen] = React.useState(false);

  React.useEffect(() => {
    if (value) {
      setRange({ from: value.from, to: value.to });
      setStartTime(
        `${value.from.getHours().toString().padStart(2, "0")}:${value.from
          .getMinutes()
          .toString()
          .padStart(2, "0")}`,
      );
      setEndTime(
        `${value.to.getHours().toString().padStart(2, "0")}:${value.to
          .getMinutes()
          .toString()
          .padStart(2, "0")}`,
      );
    }
  }, [value]);

  const handleQuickSelect = (preset: string) => {
    const now = new Date();
    let from: Date;
    let to: Date = new Date();

    switch (preset) {
      case "1h":
        from = new Date(now.getTime() - 60 * 60 * 1000);
        break;
      case "6h":
        from = new Date(now.getTime() - 6 * 60 * 60 * 1000);
        break;
      case "12h":
        from = new Date(now.getTime() - 12 * 60 * 60 * 1000);
        break;
      case "1d":
        from = new Date(now.getTime() - 24 * 60 * 60 * 1000);
        break;
      case "7d":
        from = new Date(now.getTime() - 7 * 24 * 60 * 60 * 1000);
        break;
      case "30d":
        from = new Date(now.getTime() - 30 * 24 * 60 * 60 * 1000);
        break;
      case "today":
        from = new Date(
          now.getFullYear(),
          now.getMonth(),
          now.getDate(),
          0,
          0,
          0,
        );
        to = new Date(
          now.getFullYear(),
          now.getMonth(),
          now.getDate(),
          23,
          59,
          59,
        );
        break;
      case "yesterday": {
        const yesterday = new Date(now.getTime() - 24 * 60 * 60 * 1000);
        from = new Date(
          yesterday.getFullYear(),
          yesterday.getMonth(),
          yesterday.getDate(),
          0,
          0,
          0,
        );
        to = new Date(
          yesterday.getFullYear(),
          yesterday.getMonth(),
          yesterday.getDate(),
          23,
          59,
          59,
        );
        break;
      }
      case "thisWeek": {
        const dayOfWeek = now.getDay();
        const diff = now.getDate() - dayOfWeek + (dayOfWeek === 0 ? -6 : 1);
        from = new Date(now.setDate(diff));
        from.setHours(0, 0, 0, 0);
        to = new Date();
        break;
      }
      case "thisMonth":
        from = new Date(now.getFullYear(), now.getMonth(), 1, 0, 0, 0);
        to = new Date();
        break;
      default:
        return;
    }

    setRange({ from, to });
    setStartTime(
      `${from.getHours().toString().padStart(2, "0")}:${from
        .getMinutes()
        .toString()
        .padStart(2, "0")}`,
    );
    setEndTime(
      `${to.getHours().toString().padStart(2, "0")}:${to
        .getMinutes()
        .toString()
        .padStart(2, "0")}`,
    );

    onChange?.({ from, to });
    setOpen(false);
  };

  const handleApply = () => {
    if (range?.from && range?.to) {
      const [startHour, startMinute] = startTime.split(":").map(Number);
      const [endHour, endMinute] = endTime.split(":").map(Number);

      const from = new Date(range.from);
      from.setHours(startHour, startMinute, 0, 0);

      const to = new Date(range.to);
      to.setHours(endHour, endMinute, 59, 999);

      onChange?.({ from, to });
      setOpen(false);
    }
  };

  const formatDateRange = () => {
    if (range?.from && range?.to) {
      if (range.from.toDateString() === range.to.toDateString()) {
        return `${format(range.from, "MMM d, yyyy")} ${startTime} - ${endTime}`;
      }
      return `${format(range.from, "MMM d")} ${startTime} - ${format(
        range.to,
        "MMM d, yyyy",
      )} ${endTime}`;
    }
    return "Select date & time";
  };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="outline"
          className={cn(
            "justify-between font-normal",
            !range && "text-muted-foreground",
            className,
          )}
        >
          <span className="flex items-center gap-2">
            <CalendarIcon className="h-4 w-4" />
            {formatDateRange()}
          </span>
        </Button>
      </PopoverTrigger>
      <PopoverContent className="w-auto p-0 min-w-fit" align="end">
        <div className="flex">
          <div className="p-3 min-w-80">
            <Calendar
              mode="range"
              selected={range}
              onSelect={setRange}
              numberOfMonths={1}
              className="rounded-md w-full"
              disabled={(date) => date > new Date()}
              captionLayout="dropdown"
              classNames={{
                today: "",
              }}
            />
            <Separator className="my-3" />
            <div className="space-y-3">
              <div className="flex items-center gap-3">
                <div className="flex-1">
                  <Label htmlFor="start-time" className="text-xs">
                    Start Time
                  </Label>
                  <Input
                    id="start-time"
                    type="time"
                    value={startTime}
                    onChange={(e) => setStartTime(e.target.value)}
                    className="h-8"
                  />
                </div>
                <div className="flex-1">
                  <Label htmlFor="end-time" className="text-xs">
                    End Time
                  </Label>
                  <Input
                    id="end-time"
                    type="time"
                    value={endTime}
                    onChange={(e) => setEndTime(e.target.value)}
                    className="h-8"
                  />
                </div>
              </div>
              <div className="flex gap-2">
                <Button
                  onClick={handleApply}
                  disabled={!range?.from || !range?.to}
                  className="flex-1"
                  size="sm"
                >
                  Apply
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => {
                    const defaultRange = getDefaultRange();
                    setRange(defaultRange);
                    setStartTime(
                      `${defaultRange.from.getHours().toString().padStart(2, "0")}:${defaultRange.from
                        .getMinutes()
                        .toString()
                        .padStart(2, "0")}`,
                    );
                    setEndTime(
                      `${defaultRange.to.getHours().toString().padStart(2, "0")}:${defaultRange.to
                        .getMinutes()
                        .toString()
                        .padStart(2, "0")}`,
                    );
                    onChange?.(defaultRange);
                  }}
                  className="px-3"
                >
                  Reset
                </Button>
              </div>
            </div>
          </div>
          <Separator orientation="vertical" />
          <div className="p-3 space-y-2 min-w-[120px]">
            <p className="text-xs font-medium text-muted-foreground mb-2">
              Quick Select
            </p>
            <Button
              variant="outline"
              size="sm"
              className="w-full"
              onClick={() => handleQuickSelect("1h")}
            >
              1h
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="w-full"
              onClick={() => handleQuickSelect("1d")}
            >
              24h
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="w-full"
              onClick={() => handleQuickSelect("7d")}
            >
              7d
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="w-full"
              onClick={() => handleQuickSelect("30d")}
            >
              30d
            </Button>
            <Separator className="my-2" />
            <Button
              variant="outline"
              size="sm"
              className="w-full"
              onClick={() => handleQuickSelect("today")}
            >
              Today
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="w-full"
              onClick={() => handleQuickSelect("yesterday")}
            >
              Yesterday
            </Button>
          </div>
        </div>
      </PopoverContent>
    </Popover>
  );
}
