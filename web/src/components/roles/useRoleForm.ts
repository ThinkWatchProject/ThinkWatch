import { useState } from 'react';
import type { RoleResponse } from '@/routes/admin/roles/types';

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
    policyJson: role.policy_document
      ? JSON.stringify(role.policy_document, null, 2)
      : '',
    policyError: '',
  };
}

/**
 * One `useState` call per field, bundled into a single object for
 * ergonomic consumption. Not a reducer because most updates are
 * single-field and the scattered setters stay typed.
 *
 * Returns the same API for both create (empty) and edit (seeded) forms
 * so the wizard steps don't need to branch on mode.
 */
export function useRoleForm(initial: RoleFormState = emptyRoleForm()) {
  const [name, setName] = useState(initial.name);
  const [description, setDescription] = useState(initial.description);
  const [mode, setMode] = useState<'simple' | 'policy'>(initial.mode);
  const [perms, setPerms] = useState<Set<string>>(initial.perms);
  const [models, setModels] = useState<Set<string> | null>(initial.models);
  const [mcpTools, setMcpTools] = useState<Set<string> | null>(initial.mcpTools);
  const [policyJson, setPolicyJson] = useState(initial.policyJson);
  const [policyError, setPolicyError] = useState(initial.policyError);

  const reset = (next: RoleFormState = emptyRoleForm()) => {
    setName(next.name);
    setDescription(next.description);
    setMode(next.mode);
    setPerms(next.perms);
    setModels(next.models);
    setMcpTools(next.mcpTools);
    setPolicyJson(next.policyJson);
    setPolicyError(next.policyError);
  };

  return {
    name,
    setName,
    description,
    setDescription,
    mode,
    setMode,
    perms,
    setPerms,
    models,
    setModels,
    mcpTools,
    setMcpTools,
    policyJson,
    setPolicyJson,
    policyError,
    setPolicyError,
    reset,
  };
}

export type RoleForm = ReturnType<typeof useRoleForm>;
