export interface Provider {
  id: string;
  name: string;
  display_name: string;
  provider_type: string;
  base_url: string;
  is_active: boolean;
  config_json?: { headers?: { key: string; value: string }[] };
  created_at: string;
}

export interface TestResult {
  success: boolean;
  message: string;
  latency_ms?: number;
  model_count?: number;
  models?: string[];
}
