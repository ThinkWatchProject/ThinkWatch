import { useCallback, useReducer } from 'react';
import type { PermissionDef, PolicyDocument, RoleResponse } from '@/routes/admin/roles/types';
import { policyToPerms } from '@/routes/admin/roles/types';

/**
 * Shape of an in-flight role form. Kept flat rather than nested per-step
 * so the existing handleCreate/handleEdit payload construction stays
 * readable — every field maps directly to what the API expects.
 */
export interface RoleFormState {
  name: string;
  description: string;
  /** Simple (checkbox) vs policy (JSON) editing mode. */
  mode: 'simple' | 'policy';
  perms: Set<string>;
  /** `null` = unrestricted, any Set (including empty) = restrict. */
  models: Set<string> | null;
  /** `null` = unrestricted, any Set = restrict to listed namespaced tools. */
  mcpTools: Set<string> | null;
  policyJson: string;
  policyError: string;
}

export function emptyRoleForm(): RoleFormState {
  return {
    name: '',
    description: '',
    mode: 'simple',
    perms: new Set(),
    models: null,
    mcpTools: null,
    policyJson: '',
    policyError: '',
  };
}

export function fromRoleResponse(role: RoleResponse): RoleFormState {
  return {
    name: role.name,
    description: role.description ?? '',
    mode: role.policy_document ? 'policy' : 'simple',
    perms: new Set(role.permissions),
    models: role.allowed_models === null ? null : new Set(role.allowed_models),
    mcpTools: role.allowed_mcp_tools === null ? null : new Set(role.allowed_mcp_tools),
    policyJson: role.policy_document ? JSON.stringify(role.policy_document, null, 2) : '',
    policyError: '',
  };
}

/// Discriminated-union actions for the reducer. Each one corresponds to a
/// single named field update, plus a `RESET` action for bulk replacement
/// (used when loading an edit target or wiping a finished create form).
type Action =
  | { type: 'name'; value: string }
  | { type: 'description'; value: string }
  | { type: 'mode'; value: RoleFormState['mode'] }
  | { type: 'perms'; value: Set<string> }
  | { type: 'models'; value: Set<string> | null }
  | { type: 'mcpTools'; value: Set<string> | null }
  | { type: 'policyJson'; value: string }
  | { type: 'policyError'; value: string }
  | { type: 'reset'; value: RoleFormState };

function reducer(state: RoleFormState, action: Action): RoleFormState {
  if (action.type === 'reset') return action.value;
  return { ...state, [action.type]: action.value };
}

/**
 * Single source of truth for an in-flight role form. Wraps `useReducer`
 * but exposes per-field setters so the call sites read like a bag of
 * `useState` hooks — no React-Redux ceremony for callers.
 *
 * Returns the same API for both create (empty) and edit (seeded) forms.
 */
export function useRoleForm(initial: RoleFormState = emptyRoleForm()) {
  const [state, dispatch] = useReducer(reducer, initial);

  // Memoize setters so child components passing them as props get
  // referentially stable functions across renders.
  const setName = useCallback((v: string) => dispatch({ type: 'name', value: v }), []);
  const setDescription = useCallback(
    (v: string) => dispatch({ type: 'description', value: v }),
    [],
  );
  const setMode = useCallback(
    (v: RoleFormState['mode']) => dispatch({ type: 'mode', value: v }),
    [],
  );
  const setPerms = useCallback((v: Set<string>) => dispatch({ type: 'perms', value: v }), []);
  const setModels = useCallback(
    (v: Set<string> | null) => dispatch({ type: 'models', value: v }),
    [],
  );
  const setMcpTools = useCallback(
    (v: Set<string> | null) => dispatch({ type: 'mcpTools', value: v }),
    [],
  );
  const setPolicyJson = useCallback(
    (v: string) => dispatch({ type: 'policyJson', value: v }),
    [],
  );
  const setPolicyError = useCallback(
    (v: string) => dispatch({ type: 'policyError', value: v }),
    [],
  );
  const reset = useCallback(
    (next: RoleFormState = emptyRoleForm()) => dispatch({ type: 'reset', value: next }),
    [],
  );

  return {
    ...state,
    setName,
    setDescription,
    setMode,
    setPerms,
    setModels,
    setMcpTools,
    setPolicyJson,
    setPolicyError,
    reset,
  };
}

export type RoleForm = ReturnType<typeof useRoleForm>;

/// Derive the API payload from the current form state. In simple mode the
/// fields come straight from the form; in policy mode we parse the JSON
/// so the saved `allowed_models` / `allowed_mcp_tools` reflect any
/// scope statements the admin added (or removed) directly in the JSON.
export function buildRolePayload(
  form: RoleForm,
  permissions: PermissionDef[],
): {
  permissions: string[];
  allowed_models: string[] | null;
  allowed_mcp_tools: string[] | null;
  policy_document: PolicyDocument | null;
} {
  if (form.mode === 'simple') {
    return {
      permissions: Array.from(form.perms),
      allowed_models: form.models === null ? null : Array.from(form.models),
      allowed_mcp_tools: form.mcpTools === null ? null : Array.from(form.mcpTools),
      policy_document: null,
    };
  }
  // Policy mode: parse the JSON one more time so the scope side-fields
  // mirror whatever Resource constraints the admin actually committed.
  let parsed: PolicyDocument | null = null;
  if (form.policyJson.trim()) {
    try {
      parsed = JSON.parse(form.policyJson) as PolicyDocument;
    } catch {
      parsed = null;
    }
  }
  const fromJson = policyToPerms(form.policyJson, permissions);
  return {
    permissions: [],
    allowed_models: fromJson.models === null ? null : Array.from(fromJson.models),
    allowed_mcp_tools: fromJson.mcpTools === null ? null : Array.from(fromJson.mcpTools),
    policy_document: parsed,
  };
}
