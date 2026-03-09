import { useState } from "react";
import { useParams, useNavigate } from "react-router-dom";
import { ArrowLeft, Copy, Check, Play, Code } from "lucide-react";
import { useModel } from "../../../../api/control-layer";
import type { Model } from "../../../../api/control-layer/types";
import { Button } from "../../../ui/button";
import {
  Card,
  CardContent,
} from "../../../ui/card";
import { Markdown } from "../../../ui/markdown";
import { ApiExamples } from "../../../modals";
import { isPlaygroundDenied } from "../../../../utils/modelAccess";
import {
  formatTariffPrice,
  getTariffDisplayName,
  getUserFacingTariffs,
} from "../../../../utils/formatters";
import { IntelligenceBars } from "../IntelligenceIndicator";

const MODEL_TYPE_LABELS: Record<string, string> = {
  CHAT: "Generation",
  EMBEDDINGS: "Embedding",
  RERANKER: "Reranker",
};

function ModelPricing({ model }: { model: Model }) {
  if (!model.tariffs) return null;
  const tiers = getUserFacingTariffs(model.tariffs);
  if (tiers.length === 0) return null;

  return (
    <>
      {/* md+: horizontal layout */}
      <div className="hidden md:flex items-start gap-x-6 gap-y-2 flex-wrap">
        {tiers.map((t) => (
          <div key={t.id} className="text-sm tabular-nums">
            <span className="text-xs text-gray-400">
              {getTariffDisplayName(t.api_key_purpose, t.completion_window)}
            </span>
            <p className="font-medium text-gray-900">
              {formatTariffPrice(t.input_price_per_token)}
              <span className="text-gray-400 font-normal mx-0.5">/</span>
              {formatTariffPrice(t.output_price_per_token)}
              <span className="text-xs text-gray-400 font-normal ml-1">
                per 1M
              </span>
            </p>
          </div>
        ))}
      </div>

      {/* mobile: vertical stack */}
      <div className="md:hidden space-y-1.5">
        {tiers.map((t) => (
          <div
            key={t.id}
            className="flex items-baseline justify-between text-sm tabular-nums"
          >
            <span className="text-xs text-gray-400">
              {getTariffDisplayName(t.api_key_purpose, t.completion_window)}
            </span>
            <span className="font-medium text-gray-900">
              {formatTariffPrice(t.input_price_per_token)}
              <span className="text-gray-400 font-normal mx-0.5">/</span>
              {formatTariffPrice(t.output_price_per_token)}
            </span>
          </div>
        ))}
        <p className="text-[11px] text-gray-400 text-right">
          per 1M · input / output
        </p>
      </div>
    </>
  );
}

export const ModelDetail: React.FC = () => {
  const { modelId } = useParams<{ modelId: string }>();
  const navigate = useNavigate();
  const [aliasCopied, setAliasCopied] = useState(false);
  const [showApiExamples, setShowApiExamples] = useState(false);

  const {
    data: model,
    isLoading,
    error,
  } = useModel(modelId!, { include: "pricing" });

  const playgroundAvailable = model ? !isPlaygroundDenied(model) : false;

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div
          className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue"
          aria-label="Loading"
        />
      </div>
    );
  }

  if (error || !model) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <p className="text-gray-600 font-semibold">
            {error ? `Error: ${(error as Error).message}` : "Model not found"}
          </p>
          <Button
            variant="outline"
            onClick={() => navigate("/models")}
            className="mt-4"
          >
            <ArrowLeft className="mr-2 h-4 w-4" />
            Back to Models
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="p-4 md:p-6">
      {/* Header */}
      <div className="mb-6">
        <div className="flex items-start gap-3 mb-4">
          <button
            onClick={() => navigate("/models")}
            className="p-2 text-gray-500 hover:bg-gray-100 rounded-lg transition-colors shrink-0 mt-0.5"
            aria-label="Back to Models"
          >
            <ArrowLeft className="w-5 h-5" />
          </button>
          <div className="flex-1 min-w-0">
            <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-3">
              <div className="min-w-0">
                <div className="flex items-center gap-3 min-w-0">
                  <h1 className="text-2xl md:text-3xl font-bold text-doubleword-neutral-900 truncate min-w-0">
                    {model.alias}
                  </h1>
                  <button
                    type="button"
                    className="shrink-0 p-1 text-gray-400 hover:text-gray-600 transition-colors"
                    aria-label="Copy model alias"
                    onClick={() => {
                      navigator.clipboard.writeText(model.alias).then(() => {
                        setAliasCopied(true);
                        setTimeout(() => setAliasCopied(false), 1500);
                      });
                    }}
                  >
                    {aliasCopied ? (
                      <Check className="h-4 w-4 text-green-600" />
                    ) : (
                      <Copy className="h-4 w-4" />
                    )}
                  </button>
                </div>
                {model.metadata?.provider && (
                  <p className="text-sm text-muted-foreground mt-1">
                    {model.metadata.provider}
                  </p>
                )}
              </div>
              <div className="flex items-center gap-2">
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setShowApiExamples(true)}
                >
                  <Code className="h-4 w-4 mr-1" />
                  API
                </Button>
                {playgroundAvailable && (
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() =>
                      navigate(
                        `/playground?model=${encodeURIComponent(model.id)}`,
                      )
                    }
                  >
                    <Play className="h-4 w-4 mr-1" />
                    Try it
                  </Button>
                )}
              </div>
            </div>
          </div>
        </div>
      </div>

      <div className="space-y-6">
        <Card className="p-0 gap-0 rounded-lg">
          <CardContent className="px-6 py-5">
            {/* Model info */}
            <div className="flex flex-col lg:flex-row lg:items-start lg:justify-between gap-4 text-sm mb-5 pb-5 border-b">
              {/* Metadata */}
              <div className="flex items-start gap-x-6 gap-y-3 flex-wrap">
                {model.metadata?.provider && (
                  <div>
                    <span className="text-xs text-gray-400">Provider</span>
                    <p className="font-medium text-gray-900">{model.metadata.provider}</p>
                  </div>
                )}
                {model.model_type && (
                  <div>
                    <span className="text-xs text-gray-400">Type</span>
                    <p className="font-medium text-gray-900">
                      {MODEL_TYPE_LABELS[model.model_type] || model.model_type}
                    </p>
                  </div>
                )}
                {model.metadata?.context_window && (
                  <div>
                    <span className="text-xs text-gray-400">Context</span>
                    <p className="font-medium text-gray-900 tabular-nums">
                      {model.metadata.context_window >= 1024
                        ? `${Math.round(model.metadata.context_window / 1024)}k tokens`
                        : `${model.metadata.context_window} tokens`}
                    </p>
                  </div>
                )}
                {model.metadata?.intelligence_index != null && (
                  <div className="hidden md:block">
                    <span className="text-xs text-gray-400">Intelligence</span>
                    <div className="flex items-center gap-1.5 mt-0.5">
                      <IntelligenceBars value={model.metadata.intelligence_index} metadata={model.metadata} />
                    </div>
                  </div>
                )}
                {model.metadata?.released_at && (
                  <div className="hidden md:block">
                    <span className="text-xs text-gray-400">Released</span>
                    <p className="font-medium text-gray-900">
                      {new Date(
                        model.metadata.released_at + "T00:00:00",
                      ).toLocaleDateString("en-US", {
                        month: "short",
                        year: "numeric",
                      })}
                    </p>
                  </div>
                )}
              </div>

              {/* Pricing */}
              <ModelPricing model={model} />
            </div>

            {/* Description */}
            {model.description ? (
              <Markdown className="text-sm text-gray-700">
                {model.description}
              </Markdown>
            ) : (
              <p className="text-sm text-muted-foreground py-6 text-center">
                No description available for this model.
              </p>
            )}
          </CardContent>
        </Card>

      </div>

      <ApiExamples
        isOpen={showApiExamples}
        onClose={() => setShowApiExamples(false)}
        model={model}
      />
    </div>
  );
};

export default ModelDetail;
