import { describe, it, expect } from 'vitest';
import {
  type PermissionDef,
  type ParsedConstraints,
  permsToPolicy,
  policyToPerms,
} from './types';

// Small permission catalog — enough to exercise the wildcard-expansion
// branches in `policyToPerms` without being exhaustive about the real
// catalog's contents.
const CATALOG: PermissionDef[] = [
  { key: 'ai_gateway:use', resource: 'ai_gateway', action: 'use', dangerous: false },
  { key: 'mcp_gateway:use', resource: 'mcp_gateway', action: 'use', dangerous: false },
  { key: 'api_keys:read', resource: 'api_keys', action: 'read', dangerous: false },
  { key: 'api_keys:create', resource: 'api_keys', action: 'create', dangerous: false },
  { key: 'api_keys:update', resource: 'api_keys', action: 'update', dangerous: false },
  { key: 'api_keys:delete', resource: 'api_keys', action: 'delete', dangerous: true },
  { key: 'providers:read', resource: 'providers', action: 'read', dangerous: false },
  { key: 'providers:update', resource: 'providers', action: 'update', dangerous: false },
  { key: 'users:read', resource: 'users', action: 'read', dangerous: false },
  { key: 'users:update', resource: 'users', action: 'update', dangerous: true },
];

describe('permsToPolicy', () => {
  it('groups permissions by surface into separate Statements', () => {
    const perms = new Set(['ai_gateway:use', 'mcp_gateway:use', 'api_keys:read', 'api_keys:create']);
    const doc = permsToPolicy(perms);
    expect(doc.Version).toBe('2024-01-01');
    const sids = doc.Statement.map((s) => s.Sid);
    expect(sids).toEqual(['AIGateway', 'MCPGateway', 'ConsoleAccess']);
    expect(doc.Statement[0].Action).toEqual(['ai_gateway:use']);
    expect(doc.Statement[1].Action).toEqual(['mcp_gateway:use']);
    expect(doc.Statement[2].Action).toEqual(['api_keys:create', 'api_keys:read']);
  });

  it('attaches model scope as Resource on the AIGateway statement', () => {
    const doc = permsToPolicy(
      new Set(['ai_gateway:use']),
      new Set(['gpt-4o', 'claude-3-5-sonnet']),
    );
    expect(doc.Statement).toHaveLength(1);
    expect(doc.Statement[0].Sid).toBe('AIGateway');
    expect(doc.Statement[0].Resource).toEqual(['model:claude-3-5-sonnet', 'model:gpt-4o']);
  });

  it('attaches MCP tool scope as Resource on the MCPGateway statement', () => {
    const doc = permsToPolicy(
      new Set(['mcp_gateway:use']),
      null,
      new Set(['github__list_issues']),
    );
    expect(doc.Statement).toHaveLength(1);
    expect(doc.Statement[0].Sid).toBe('MCPGateway');
    expect(doc.Statement[0].Resource).toEqual(['mcp_tool:github__list_issues']);
  });

  it('emits an empty Resource array when a scope Set is empty (explicit "none allowed")', () => {
    const doc = permsToPolicy(new Set(['ai_gateway:use']), new Set(), new Set(['mcp_gateway:use']));
    const aiStmt = doc.Statement.find((s) => s.Sid === 'AIGateway');
    expect(aiStmt?.Resource).toEqual([]);
  });

  it('omits scope when models/mcpTools are null (unrestricted)', () => {
    const doc = permsToPolicy(new Set(['ai_gateway:use']), null, null);
    expect(doc.Statement).toHaveLength(1);
    expect(doc.Statement[0].Resource).toBe('*');
  });

  it('attaches Constraints to the AIGateway statement', () => {
    const constraints: ParsedConstraints = {
      ai_gateway: {
        rateLimits: [{ metric: 'requests', window: '1h', maxCount: 100, enabled: true }],
        budgets: [{ period: 'daily', maxTokens: 1000000, enabled: true }],
      },
    };
    const doc = permsToPolicy(new Set(['ai_gateway:use']), null, null, constraints);
    expect(doc.Statement[0].Constraints).toEqual({
      RateLimits: [{ Metric: 'requests', Window: '1h', MaxCount: 100 }],
      Budgets: [{ Period: 'daily', MaxTokens: 1000000 }],
    });
  });

  it('skips disabled constraints during serialization', () => {
    const constraints: ParsedConstraints = {
      ai_gateway: {
        rateLimits: [{ metric: 'requests', window: '1h', maxCount: 100, enabled: false }],
        budgets: [],
      },
    };
    const doc = permsToPolicy(new Set(['ai_gateway:use']), null, null, constraints);
    expect(doc.Statement[0].Constraints).toBeUndefined();
  });

  it('produces only a ConsoleAccess statement for non-gateway perms', () => {
    const doc = permsToPolicy(new Set(['api_keys:read', 'api_keys:create']));
    expect(doc.Statement).toHaveLength(1);
    expect(doc.Statement[0].Sid).toBe('ConsoleAccess');
    expect(doc.Statement[0].Resource).toBe('*');
  });
});

describe('policyToPerms', () => {
  it('returns an empty result for empty input', () => {
    const r = policyToPerms('', CATALOG);
    expect(r.perms.size).toBe(0);
    expect(r.models).toBeNull();
    expect(r.mcpTools).toBeNull();
    expect(r.lossy).toBe(false);
    expect(r.parseError).toBe(false);
  });

  it('flags a parse error on malformed JSON', () => {
    const r = policyToPerms('{ not json', CATALOG);
    expect(r.parseError).toBe(true);
  });

  it('expands Action="*" to every catalog key', () => {
    const doc = JSON.stringify({
      Version: '2024-01-01',
      Statement: [{ Effect: 'Allow', Action: '*', Resource: '*' }],
    });
    const r = policyToPerms(doc, CATALOG);
    expect(r.perms.size).toBe(CATALOG.length);
    expect(r.lossy).toBe(false);
  });

  it('expands prefix:* wildcards to matching catalog keys only', () => {
    const doc = JSON.stringify({
      Version: '2024-01-01',
      Statement: [{ Effect: 'Allow', Action: 'api_keys:*', Resource: '*' }],
    });
    const r = policyToPerms(doc, CATALOG);
    expect([...r.perms].sort()).toEqual([
      'api_keys:create',
      'api_keys:delete',
      'api_keys:read',
      'api_keys:update',
    ]);
  });

  it('marks Deny statements as lossy and skips them', () => {
    const doc = JSON.stringify({
      Version: '2024-01-01',
      Statement: [
        { Effect: 'Allow', Action: 'api_keys:read', Resource: '*' },
        { Effect: 'Deny', Action: '*:update', Resource: '*' },
      ],
    });
    const r = policyToPerms(doc, CATALOG);
    expect(r.perms.has('api_keys:read')).toBe(true);
    expect(r.perms.has('providers:update')).toBe(false);
    expect(r.lossy).toBe(true);
  });

  it('marks actions not present in the catalog as lossy', () => {
    const doc = JSON.stringify({
      Version: '2024-01-01',
      Statement: [{ Effect: 'Allow', Action: ['api_keys:read', 'made_up:action'], Resource: '*' }],
    });
    const r = policyToPerms(doc, CATALOG);
    expect(r.perms.has('api_keys:read')).toBe(true);
    expect(r.lossy).toBe(true);
  });

  it('extracts model and tool scope from Resource prefixes', () => {
    const doc = JSON.stringify({
      Version: '2024-01-01',
      Statement: [
        {
          Sid: 'AIGateway',
          Effect: 'Allow',
          Action: 'ai_gateway:use',
          Resource: ['model:gpt-4o', 'model:claude-3-5-sonnet'],
        },
        {
          Sid: 'MCPGateway',
          Effect: 'Allow',
          Action: 'mcp_gateway:use',
          Resource: ['mcp_tool:github__list_issues'],
        },
      ],
    });
    const r = policyToPerms(doc, CATALOG);
    expect(r.perms.has('ai_gateway:use')).toBe(true);
    expect(r.perms.has('mcp_gateway:use')).toBe(true);
    expect(r.models).not.toBeNull();
    expect([...(r.models ?? [])].sort()).toEqual(['claude-3-5-sonnet', 'gpt-4o']);
    expect([...(r.mcpTools ?? [])]).toEqual(['github__list_issues']);
    expect(r.lossy).toBe(false);
  });

  it('extracts Constraints from Statements', () => {
    const doc = JSON.stringify({
      Version: '2024-01-01',
      Statement: [
        {
          Sid: 'AIGateway',
          Effect: 'Allow',
          Action: 'ai_gateway:use',
          Resource: '*',
          Constraints: {
            RateLimits: [{ Metric: 'requests', Window: '1h', MaxCount: 100 }],
            Budgets: [{ Period: 'daily', MaxTokens: 1000000 }],
          },
        },
        {
          Sid: 'MCPGateway',
          Effect: 'Allow',
          Action: 'mcp_gateway:use',
          Resource: '*',
          Constraints: {
            RateLimits: [{ Metric: 'requests', Window: '1m', MaxCount: 60 }],
          },
        },
      ],
    });
    const r = policyToPerms(doc, CATALOG);
    expect(r.constraints.ai_gateway).toBeDefined();
    expect(r.constraints.ai_gateway!.rateLimits).toHaveLength(1);
    expect(r.constraints.ai_gateway!.rateLimits[0]).toMatchObject({
      metric: 'requests',
      window: '1h',
      maxCount: 100,
    });
    expect(r.constraints.ai_gateway!.budgets).toHaveLength(1);
    expect(r.constraints.ai_gateway!.budgets[0]).toMatchObject({
      period: 'daily',
      maxTokens: 1000000,
    });
    expect(r.constraints.mcp_gateway).toBeDefined();
    expect(r.constraints.mcp_gateway!.rateLimits).toHaveLength(1);
  });

  it('treats non-wildcard Resource statements (without scope prefix) as lossy', () => {
    const doc = JSON.stringify({
      Version: '2024-01-01',
      Statement: [
        { Effect: 'Allow', Action: 'api_keys:read', Resource: 'team:abc' },
      ],
    });
    const r = policyToPerms(doc, CATALOG);
    expect(r.perms.has('api_keys:read')).toBe(false);
    expect(r.lossy).toBe(true);
  });
});

describe('round-trip permsToPolicy -> policyToPerms', () => {
  it('preserves a permissions-only role exactly', () => {
    const original = new Set(['ai_gateway:use', 'api_keys:read', 'api_keys:create']);
    const doc = permsToPolicy(original, null, null);
    const back = policyToPerms(JSON.stringify(doc), CATALOG);
    expect(back.perms).toEqual(original);
    expect(back.models).toBeNull();
    expect(back.mcpTools).toBeNull();
    expect(back.lossy).toBe(false);
  });

  it('preserves perms + model scope + tool scope', () => {
    const perms = new Set(['ai_gateway:use', 'mcp_gateway:use']);
    const models = new Set(['gpt-4o']);
    const tools = new Set(['github__list_issues', 'slack__post_message']);
    const doc = permsToPolicy(perms, models, tools);
    const back = policyToPerms(JSON.stringify(doc), CATALOG);
    expect(back.perms).toEqual(perms);
    expect(back.models).toEqual(models);
    expect(back.mcpTools).toEqual(tools);
    expect(back.lossy).toBe(false);
  });

  it('preserves constraints through round-trip', () => {
    const constraints: ParsedConstraints = {
      ai_gateway: {
        rateLimits: [{ metric: 'requests', window: '1h', maxCount: 100, enabled: true }],
        budgets: [{ period: 'daily', maxTokens: 1000000, enabled: true }],
      },
      mcp_gateway: {
        rateLimits: [{ metric: 'requests', window: '1m', maxCount: 60, enabled: true }],
        budgets: [],
      },
    };
    const doc = permsToPolicy(
      new Set(['ai_gateway:use', 'mcp_gateway:use']),
      null,
      null,
      constraints,
    );
    const back = policyToPerms(JSON.stringify(doc), CATALOG);
    expect(back.constraints.ai_gateway?.rateLimits).toHaveLength(1);
    expect(back.constraints.ai_gateway?.rateLimits[0].maxCount).toBe(100);
    expect(back.constraints.ai_gateway?.budgets).toHaveLength(1);
    expect(back.constraints.mcp_gateway?.rateLimits).toHaveLength(1);
  });
});
