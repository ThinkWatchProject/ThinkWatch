export interface Provider {
  id: string;
  name: string;
  display_name: string;
  provider_type: string;
  base_url: string;
  is_active: boolean;
  /**
   * Header values arrive redacted — the server never ships the stored
   * ciphertext. `encrypted` marks the ones that have a saved secret;
   * re-submitting them blank keeps it (see `merge_headers_for_storage`).
   */
  config_json?: { headers?: { key: string; value: string; encrypted?: boolean }[] };
  created_at: string;
}

export interface TestResult {
  success: boolean;
  message: string;
  latency_ms?: number;
  model_count?: number;
  models?: string[];
}
