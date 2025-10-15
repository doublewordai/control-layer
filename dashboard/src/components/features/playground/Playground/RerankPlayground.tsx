import React from "react";
import { ArrowUpDown, Plus, X } from "lucide-react";
import type { Model, RerankResponse } from "../../../../api/waycast/types";
import { Textarea } from "../../../ui/textarea";
import { Button } from "../../../ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from "../../../ui/dialog";

interface RerankPlaygroundProps {
  selectedModel: Model;
  query: string;
  documents: string[];
  rerankResult: RerankResponse | null;
  isStreaming: boolean;
  error: string | null;
  onQueryChange: (value: string) => void;
  onDocumentChange: (index: number, value: string) => void;
  onAddDocument: () => void;
  onRemoveDocument: (index: number) => void;
  onRerank: () => void;
  onClearResult: () => void;
  onKeyDown: (e: React.KeyboardEvent) => void;
}

const RerankPlayground: React.FC<RerankPlaygroundProps> = ({
  selectedModel,
  query,
  documents,
  rerankResult,
  isStreaming,
  error,
  onQueryChange,
  onDocumentChange,
  onAddDocument,
  onRemoveDocument,
  onRerank,
  onClearResult,
  onKeyDown,
}) => {
  const getRelevanceColor = (score: number) => {
    if (score >= 0.8) return "bg-green-500";
    if (score >= 0.6) return "bg-blue-500";
    if (score >= 0.4) return "bg-yellow-500";
    if (score >= 0.2) return "bg-orange-500";
    return "bg-red-500";
  };

  const getRelevanceLabel = (score: number) => {
    if (score >= 0.8) return "Highly Relevant";
    if (score >= 0.6) return "Relevant";
    if (score >= 0.4) return "Moderately Relevant";
    if (score >= 0.2) return "Slightly Relevant";
    return "Not Relevant";
  };

  return (
    <>
      <div className="max-w-7xl mx-auto h-full flex flex-col">
        {/* Simple Header */}
        <div className="mb-6">
          <h2 className="text-xl font-semibold text-gray-900">
            Rerank documents with {selectedModel.alias}
          </h2>
          <p className="text-sm text-gray-500 mt-1">
            Enter a query and documents below to rank them by relevance
          </p>
        </div>

        {error && (
          <div className="bg-red-50 border border-red-200 text-red-700 rounded-lg p-3 mb-4">
            <p className="font-medium text-sm">Error</p>
            <p className="text-sm">{error}</p>
          </div>
        )}

        {/* Input Area - Flex Layout */}
        <div className="flex-1 min-h-0 flex flex-col lg:flex-row gap-8">
          {/* Query Section */}
          <div className="flex-shrink-0 lg:w-80 space-y-4">
            <div className="flex-1">
              <label className="block text-sm font-medium text-gray-700 mb-2">
                Query
              </label>
              <Textarea
                value={query}
                onChange={(e) => onQueryChange(e.target.value)}
                placeholder="Enter your search query..."
                className="text-sm h-24 resize-none"
                disabled={isStreaming}
                aria-label="Query input"
              />
            </div>

            {/* Action Section */}
            <div className="space-y-3">
              <div className="text-xs text-gray-500">
                {documents.length} of 10 documents • Minimum 2 required
              </div>
              <div className="space-y-2">
                <Button
                  onClick={onRerank}
                  disabled={
                    !query.trim() ||
                    documents.length < 2 ||
                    documents.some((doc) => !doc.trim()) ||
                    isStreaming
                  }
                  className="w-full flex items-center justify-center gap-2"
                  aria-label="Rerank documents"
                >
                  <ArrowUpDown className="w-4 h-4" />
                  {isStreaming ? "Reranking..." : "Rerank Documents"}
                </Button>
                <Button
                  onClick={onAddDocument}
                  size="sm"
                  variant="outline"
                  className="w-full flex items-center justify-center gap-1"
                  disabled={isStreaming || documents.length >= 10}
                >
                  <Plus className="w-3 h-3" />
                  Add Document
                </Button>
              </div>
              <div className="text-xs text-gray-400 text-center">
                Shift+Enter for new line
              </div>
            </div>
          </div>

          {/* Documents Section */}
          <div className="flex-1 min-w-0 flex flex-col">
            <label className="block text-sm font-medium text-gray-700 mb-3">
              Documents to Rank
            </label>
            <div className="flex-1 overflow-y-auto space-y-3 pr-2">
              {documents.map((doc, index) => (
                <div key={index} className="relative">
                  <div className="flex items-start gap-2">
                    <div className="flex-shrink-0 mt-2">
                      <div className="w-6 h-6 bg-gray-100 text-gray-600 rounded-full flex items-center justify-center text-xs font-medium">
                        {index + 1}
                      </div>
                    </div>
                    <div className="flex-1">
                      <Textarea
                        value={doc}
                        onChange={(e) =>
                          onDocumentChange(index, e.target.value)
                        }
                        onKeyDown={
                          index === documents.length - 1 ? onKeyDown : undefined
                        }
                        placeholder={`Enter document ${index + 1}...`}
                        className="text-sm resize-none"
                        rows={2}
                        disabled={isStreaming}
                        aria-label={`Document ${index + 1} input`}
                      />
                    </div>
                    {documents.length > 2 && (
                      <Button
                        onClick={() => onRemoveDocument(index)}
                        size="sm"
                        variant="outline"
                        className="flex-shrink-0 text-gray-400 hover:text-red-500 border-none mt-1"
                        disabled={isStreaming}
                      >
                        <X className="w-4 h-4" />
                      </Button>
                    )}
                  </div>
                </div>
              ))}
            </div>
          </div>
        </div>
      </div>

      {/* Results Modal */}
      <Dialog
        open={!!rerankResult}
        onOpenChange={(open) => !open && onClearResult()}
      >
        <DialogContent className="sm:max-w-5xl max-h-[85vh] overflow-hidden flex flex-col">
          <DialogHeader className="flex-shrink-0">
            <DialogTitle className="text-lg">Reranking Results</DialogTitle>
          </DialogHeader>

          {rerankResult && (
            <div className="flex-1 overflow-y-auto pr-2">
              <div className="space-y-4">
                {/* Query Display */}
                <div>
                  <h4 className="text-sm font-medium text-gray-700 mb-2">
                    Query
                  </h4>
                  <div className="bg-gray-50 border rounded-md p-3 text-sm text-gray-700">
                    {query}
                  </div>
                </div>

                {/* Ranked Documents */}
                <div>
                  <h4 className="text-sm font-medium text-gray-700 mb-3">
                    Documents (Ranked by Relevance)
                  </h4>
                  <div className="space-y-3">
                    {rerankResult.results.map((result, index) => (
                      <div
                        key={index}
                        className="bg-white border border-gray-200 rounded-lg p-4 shadow-sm"
                      >
                        {/* Header with rank and score */}
                        <div className="flex items-center gap-3 mb-3">
                          <div className="w-7 h-7 bg-blue-100 text-blue-800 rounded-full flex items-center justify-center text-sm font-semibold">
                            {index + 1}
                          </div>
                          <div className="flex items-center gap-2 flex-1">
                            <span className="text-xs text-gray-500">
                              Original #{result.index + 1}
                            </span>
                            <div className="flex-1 flex items-center gap-2">
                              <div className="flex-1 bg-gray-200 rounded-full h-2 max-w-20">
                                <div
                                  className={`h-2 rounded-full transition-all ${getRelevanceColor(result.relevance_score)}`}
                                  style={{
                                    width: `${Math.max(result.relevance_score * 100, 5)}%`,
                                  }}
                                />
                              </div>
                              <span className="text-xs font-medium text-gray-700 w-12">
                                {(result.relevance_score * 100).toFixed(1)}%
                              </span>
                              <span
                                className={`text-xs px-2 py-1 rounded-full text-white ${getRelevanceColor(result.relevance_score)}`}
                              >
                                {getRelevanceLabel(result.relevance_score)}
                              </span>
                            </div>
                          </div>
                        </div>

                        {/* Document text */}
                        <p className="text-sm text-gray-800 leading-relaxed">
                          {result.document.text}
                        </p>
                      </div>
                    ))}
                  </div>
                </div>

                {/* Usage Info */}
                {rerankResult.usage && (
                  <div className="bg-blue-50 border border-blue-200 rounded-lg p-3">
                    <div className="flex items-center gap-2">
                      <span className="text-sm font-medium text-blue-900">
                        Usage:
                      </span>
                      <span className="text-sm text-blue-700">
                        {rerankResult.usage.total_tokens} tokens
                      </span>
                    </div>
                  </div>
                )}
              </div>
            </div>
          )}

          <DialogFooter className="flex-shrink-0 mt-4">
            <Button
              onClick={onClearResult}
              variant="outline"
              className="flex items-center gap-2"
            >
              Close
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
};

export default RerankPlayground;
