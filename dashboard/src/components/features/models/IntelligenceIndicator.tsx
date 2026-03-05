import { Brain } from "lucide-react";
import type { ModelMetadata } from "../../../api/control-layer/types";
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../ui/hover-card";
import artificialAnalysisLogo from "../../../assets/artificial-analysis.svg";

const ATTRIBUTION_CONFIG: Record<string, { label: string; url: string; logo?: true }> = {
  artificial_analysis: {
    label: "Artificial Analysis",
    url: "https://artificialanalysis.ai/",
    logo: true,
  },
  mteb: {
    label: "MTEB Leaderboard",
    url: "https://huggingface.co/spaces/mteb/leaderboard",
  },
};

function AttributionFooter({ attribution }: { attribution?: string }) {
  if (!attribution) return null;
  const config = ATTRIBUTION_CONFIG[attribution];
  if (config) {
    return (
      <div className="flex items-center gap-1.5 px-4 py-2.5 border-t">
        {config.logo && (
          <img
            src={artificialAnalysisLogo}
            alt={config.label}
            className="w-4 h-4 rounded-sm"
          />
        )}
        <a
          href={config.url}
          target="_blank"
          rel="noopener noreferrer"
          className="text-xs text-muted-foreground underline hover:text-foreground transition-colors"
        >
          {config.label}
        </a>
      </div>
    );
  }
  return (
    <div className="px-4 py-2.5 border-t">
      <span className="text-xs text-muted-foreground">
        Source: {attribution}
      </span>
    </div>
  );
}

const EVAL_LABELS: Record<string, string> = {
  mmlu_pro: "MMLU Pro",
  gpqa: "GPQA",
  math_500: "MATH-500",
  aime_25: "AIME '25",
  livecodebench: "LiveCodeBench",
  ifbench: "IFBench",
  mteb_multilingual: "MTEB Multilingual",
  mteb_english: "MTEB English",
  mteb_retrieval: "MTEB Retrieval",
  mteb_classification: "MTEB Classification",
  mteb_clustering: "MTEB Clustering",
};

function formatEvalScore(value: number): string {
  if (value <= 1) return `${(value * 100).toFixed(1)}%`;
  return String(value);
}

/**
 * Signal bars indicator for intelligence index.
 * 4 bars with thresholds at 1, 14, 28, 42 — values >=42 fill all bars.
 */
export function IntelligenceBars({
  value,
  metadata,
}: {
  value: number;
  metadata?: ModelMetadata | null;
}) {
  const evaluations = metadata?.extra?.evaluations;
  const attribution = metadata?.attribution;
  const hasHoverContent =
    attribution || (evaluations && Object.keys(evaluations).length > 0);

  const barsAndNumber = (
    <div className="flex items-center gap-1.5">
      <div className="flex items-center gap-1.5 cursor-default">
        <Brain className="w-3.5 h-3.5 text-gray-400" />
        <div className="flex items-end gap-[2px] h-4">
          {[1, 14, 28, 42].map((threshold, i) => (
            <div
              key={threshold}
              className={`w-[4px] rounded-sm ${
                value >= threshold ? "bg-gray-500" : "bg-gray-200"
              }`}
              style={{ height: `${6 + i * 3}px` }}
            />
          ))}
        </div>
      </div>
      <span className="text-xs tabular-nums text-muted-foreground">
        {Math.round(value)}
      </span>
    </div>
  );

  if (!hasHoverContent) return barsAndNumber;

  return (
    <HoverCard openDelay={200} closeDelay={100}>
      <HoverCardTrigger asChild>{barsAndNumber}</HoverCardTrigger>
      <HoverCardContent side="top" align="start" className="w-64 p-0">
        <IntelligenceHoverContent
          value={value}
          attribution={attribution}
          evaluations={evaluations}
        />
      </HoverCardContent>
    </HoverCard>
  );
}

/**
 * MTEB score display for embedding models.
 * Shows "MTEB" label + score, with hover for sub-scores.
 */
export function EmbeddingScore({
  metadata,
}: {
  metadata?: ModelMetadata | null;
}) {
  const evaluations = metadata?.extra?.evaluations;
  const attribution = metadata?.attribution;
  const mtebScore =
    evaluations?.mteb_multilingual ?? evaluations?.mteb_english;
  if (mtebScore == null) return null;

  const label =
    evaluations?.mteb_multilingual != null ? "MTEB" : "MTEB";
  const hasHoverContent =
    attribution || (evaluations && Object.keys(evaluations).length > 1);

  const scoreDisplay = (
    <div className="flex items-center gap-1.5 cursor-default">
      <span className="text-[10px] font-medium text-gray-400 uppercase tracking-wide">
        {label}
      </span>
      <span className="text-xs tabular-nums text-muted-foreground">
        {mtebScore.toFixed(1)}
      </span>
    </div>
  );

  if (!hasHoverContent) return scoreDisplay;

  return (
    <HoverCard openDelay={200} closeDelay={100}>
      <HoverCardTrigger asChild>{scoreDisplay}</HoverCardTrigger>
      <HoverCardContent side="top" align="start" className="w-64 p-0">
        <div>
          <div className="flex items-center gap-3 px-4 pt-4 pb-3">
            <span className="text-2xl font-semibold tabular-nums leading-none">
              {mtebScore.toFixed(1)}
            </span>
            <span className="text-sm text-muted-foreground leading-tight">
              MTEB
              <br />
              Score
            </span>
          </div>
          {evaluations && Object.keys(evaluations).length > 0 && (
            <div className="border-t px-4 py-3 space-y-1.5">
              {Object.entries(evaluations).map(([key, val]) => (
                <div key={key} className="flex items-center justify-between">
                  <span className="text-xs text-muted-foreground">
                    {EVAL_LABELS[key] || key}
                  </span>
                  <span className="text-xs tabular-nums font-medium ml-4 shrink-0">
                    {formatEvalScore(val)}
                  </span>
                </div>
              ))}
            </div>
          )}
          <AttributionFooter attribution={attribution} />
        </div>
      </HoverCardContent>
    </HoverCard>
  );
}

function IntelligenceHoverContent({
  value,
  attribution,
  evaluations,
}: {
  value: number;
  attribution?: string;
  evaluations?: Record<string, number>;
}) {
  return (
    <div>
      {/* Hero index */}
      <div className="flex items-center gap-3 px-4 pt-4 pb-3">
        <span className="text-2xl font-semibold tabular-nums leading-none">
          {Math.round(value)}
        </span>
        <span className="text-sm text-muted-foreground leading-tight">
          Intelligence
          <br />
          Index
        </span>
      </div>

      {/* Evaluations — single column, full width rows */}
      {evaluations && Object.keys(evaluations).length > 0 && (
        <div className="border-t px-4 py-3 space-y-1.5">
          {Object.entries(evaluations).map(([key, val]) => (
            <div key={key} className="flex items-center justify-between">
              <span className="text-xs text-muted-foreground">
                {EVAL_LABELS[key] || key}
              </span>
              <span className="text-xs tabular-nums font-medium ml-4 shrink-0">
                {formatEvalScore(val)}
              </span>
            </div>
          ))}
        </div>
      )}

      <AttributionFooter attribution={attribution} />
    </div>
  );
}

/**
 * Grid of evaluation scores.
 */
export function EvaluationsGrid({
  evaluations,
}: {
  evaluations: Record<string, number>;
}) {
  const entries = Object.entries(evaluations);
  if (entries.length === 0) return null;

  return (
    <div className="space-y-1.5">
      {entries.map(([key, val]) => (
        <div key={key} className="flex items-center justify-between">
          <span className="text-xs text-muted-foreground">
            {EVAL_LABELS[key] || key}
          </span>
          <span className="text-xs tabular-nums font-medium ml-4 shrink-0">
            {formatEvalScore(val)}
          </span>
        </div>
      ))}
    </div>
  );
}
