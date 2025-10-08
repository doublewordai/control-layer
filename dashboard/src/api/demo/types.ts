// Demo API types for feature-flagged functionality

export interface Request {
  id: string;
  timestamp: string;
  model: string;
  duration_ms: number;
  feedback?: number;
  metadata: {
    email: string;
    organization: string;
    project_id: string;
    environment: string;
    team: string;
    api_version: string;
    client_id: string;
    data_classification: string;
    retention: string;
  };
  request: {
    model: string;
    messages?: Array<{
      role: string;
      content: string;
    }>;
    temperature?: number;
    max_completion_tokens?: number;
  };
  response: {
    created: number;
    model: string;
    choices?: Array<{
      index: number;
      message: {
        role: string;
        content: string;
      };
      finish_reason: string;
    }>;
    usage?: {
      prompt_tokens: number;
      completion_tokens: number;
      total_tokens: number;
    };
  };
}

export interface RequestsQuery {
  limit?: number;
  offset?: number;
  user_id?: string;
  model?: string;
  start_date?: string;
  end_date?: string;
}
