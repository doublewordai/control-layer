import React from "react";
import { Hash, Trash2 } from "lucide-react";
import type { Model } from "../../../../api/clay/types";
import { Textarea } from "../../../ui/textarea";
import { Button } from "../../../ui/button";

interface SimilarityResult {
  score: number;
  category: string;
}

interface EmbeddingPlaygroundProps {
  selectedModel: Model;
  textA: string;
  textB: string;
  similarityResult: SimilarityResult | null;
  isStreaming: boolean;
  error: string | null;
  onTextAChange: (value: string) => void;
  onTextBChange: (value: string) => void;
  onCompareSimilarity: () => void;
  onClearResult: () => void;
  onKeyDown: (e: React.KeyboardEvent) => void;
}

const EmbeddingPlayground: React.FC<EmbeddingPlaygroundProps> = ({
  selectedModel,
  textA,
  textB,
  similarityResult,
  isStreaming,
  error,
  onTextAChange,
  onTextBChange,
  onCompareSimilarity,
  onClearResult,
  onKeyDown,
}) => {
  return (
    <div className="max-w-6xl mx-auto">
      {!similarityResult ? (
        <div className="flex items-center justify-center h-full">
          <div className="text-center">
            <Hash className="w-16 h-16 text-gray-400 mx-auto mb-4" />
            <p className="text-xl text-gray-600 mb-2">
              Compare text similarity with {selectedModel.alias}
            </p>
            <p className="text-gray-500">
              Enter two texts below to compare their semantic similarity
            </p>
          </div>
        </div>
      ) : (
        <div className="space-y-6">
          <div className="bg-gray-50 rounded-lg p-6">
            <h3
              className="text-lg font-semibold text-gray-900 mb-6"
              role="heading"
              aria-level={3}
            >
              Similarity Results
            </h3>

            {/* Similarity Score Display */}
            <div className="grid grid-cols-1 md:grid-cols-2 gap-6 mb-6">
              <div className="text-center">
                <div className="text-4xl font-bold text-blue-600 mb-2">
                  {(similarityResult.score * 100).toFixed(1)}%
                </div>
                <div
                  className="text-lg font-medium text-gray-700"
                  role="status"
                  aria-label="Similarity category"
                >
                  {similarityResult.category}
                </div>
              </div>

              {/* Visual Similarity Bar */}
              <div className="flex items-center">
                <div className="flex-1">
                  <div className="w-full bg-gray-200 rounded-full h-6">
                    <div
                      className={`h-6 rounded-full transition-all duration-500 ${
                        similarityResult.score >= 0.7
                          ? "bg-green-500"
                          : similarityResult.score >= 0.5
                            ? "bg-yellow-500"
                            : similarityResult.score >= 0.3
                              ? "bg-orange-500"
                              : "bg-red-500"
                      }`}
                      style={{
                        width: `${Math.max(similarityResult.score * 100, 5)}%`,
                      }}
                    />
                  </div>
                  <div className="flex justify-between text-xs text-gray-500 mt-1">
                    <span>Different</span>
                    <span>Identical</span>
                  </div>
                </div>
              </div>
            </div>

            {/* Text Comparison */}
            <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
              <div>
                <h4 className="font-medium text-gray-700 mb-2">Text A</h4>
                <div className="bg-white border rounded-lg p-3 text-sm text-gray-600 max-h-32 overflow-auto">
                  {textA}
                </div>
              </div>
              <div>
                <h4 className="font-medium text-gray-700 mb-2">Text B</h4>
                <div className="bg-white border rounded-lg p-3 text-sm text-gray-600 max-h-32 overflow-auto">
                  {textB}
                </div>
              </div>
            </div>

            <div className="flex gap-2 mt-4">
              <Button
                onClick={onClearResult}
                variant="destructive"
                className="flex items-center gap-2"
                aria-label="Clear results"
              >
                <Trash2 className="w-4 h-4" />
                Clear Result
              </Button>
            </div>
          </div>
        </div>
      )}
      {error && (
        <div className="bg-red-50 border border-red-200 text-red-700 rounded-lg p-4 mt-4">
          <p className="font-medium text-sm">Error</p>
          <p className="text-sm">{error}</p>
        </div>
      )}

      {/* Input Area for Embeddings */}
      <div className="mt-8">
        <div className="space-y-4">
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            <div>
              <label className="block text-sm font-medium text-gray-700 mb-2">
                Text A
              </label>
              <Textarea
                value={textA}
                onChange={(e) => onTextAChange(e.target.value)}
                placeholder="Enter first text to compare..."
                className="text-sm"
                rows={4}
                disabled={isStreaming}
                aria-label="Text A input"
              />
            </div>
            <div>
              <label className="block text-sm font-medium text-gray-700 mb-2">
                Text B
              </label>
              <Textarea
                value={textB}
                onChange={(e) => onTextBChange(e.target.value)}
                onKeyDown={onKeyDown}
                placeholder="Enter second text to compare..."
                className="text-sm"
                rows={4}
                disabled={isStreaming}
                aria-label="Text B input"
              />
            </div>
          </div>
          <div className="flex items-center justify-between">
            <div></div>
            <Button
              onClick={onCompareSimilarity}
              disabled={!textA.trim() || !textB.trim() || isStreaming}
              className="flex items-center gap-2"
              aria-label="Compare similarity"
            >
              <Hash className="w-4 h-4" />
              {isStreaming ? "Comparing..." : "Compare Similarity"}
            </Button>
          </div>
          <div className="text-center">
            <div className="text-sm text-gray-400">
              Enter in Text B to compare • Shift+Enter for new line
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};

export default EmbeddingPlayground;
