// Frontend display types for the requests feature
// Simplified to match http_analytics data (no request/response body content)

export interface RequestsEntry {
  id: string;
  timestamp: string;
  method: string;
  uri: string;
  model?: string;
  status_code?: number;
  duration_ms?: number;
  prompt_tokens?: number;
  completion_tokens?: number;
  total_tokens?: number;
  response_type?: string;
  user_email?: string;
  fusillade_batch_id?: string;
  input_price_per_token?: string;
  output_price_per_token?: string;
  custom_id?: string;
}
