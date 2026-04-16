import { useCallback, useReducer } from 'react';
import type { PermissionDef, PolicyDocument, RoleResponse, ParsedConstraints } from '@/routes/admin/roles/types';
import { policyToPerms, permsToPolicy } from '@/routes/admin/roles/types';

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

export function fromRoleResponse(role: RoleResponse, available?: PermissionDef[]): RoleFormState {
  const doc = role.policy_document;
  const json = JSON.stringify(doc, null, 2);
  if (available) {
    const parsed = policyToPerms(json, available);
    return {
      name: role.name,
      description: role.description ?? '',
      mode: 'policy',
      perms: parsed.perms,
      models: parsed.models,
      mcpTools: parsed.mcpTools,
      policyJson: json,
      policyError: '',
    };
  }
  return {
    name: role.name,
    description: role.description ?? '',
    mode: 'policy',
    perms: new Set(),
    models: null,
    mcpTools: null,
    policyJson: json,
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

/// Derive the API payload from the current form state. Both simple and
/// policy mode produce only `{ policy_document }`. In simple mode the
/// document is synthesized from the form's perms/models/mcpTools/constraints;
/// in policy mode the admin's raw JSON is used as-is.
export function buildRolePayload(
  form: RoleForm,
  _permissions: PermissionDef[],
  constraints?: ParsedConstraints | null,
): {
  policy_document: PolicyDocument;
} {
  if (form.mode === 'simple') {
    return {
      policy_document: permsToPolicy(form.perms, form.models, form.mcpTools, constraints),
    };
  }
  let parsed: PolicyDocument;
  if (form.policyJson.trim()) {
    try {
      parsed = JSON.parse(form.policyJson) as PolicyDocument;
    } catch {
      parsed = { Version: '2024-01-01', Statement: [] };
    }
  } else {
    parsed = { Version: '2024-01-01', Statement: [] };
  }
  return { policy_document: parsed };
}
