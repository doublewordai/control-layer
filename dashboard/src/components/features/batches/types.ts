// Frontend display types for the batches feature
import type {
  FileObject,
  Batch,
  BatchRequest,
  FileRequest,
  BatchStatus,
  BatchAnalytics,
} from "../../../api/control-layer/types";

// Re-export API types for convenience
export type {
  FileObject,
  Batch,
  BatchRequest,
  FileRequest,
  BatchStatus,
  BatchAnalytics,
};

// UI-specific types
export interface BatchWithProgress extends Batch {
  progress: number; // 0-100
  estimatedTimeRemaining?: number; // milliseconds
}
