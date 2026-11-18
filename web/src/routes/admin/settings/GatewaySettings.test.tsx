import { beforeAll, expect, it, vi } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { api, apiPost } from '@/lib/api';
import { renderWithQueryClient } from '@/test/render';
import { HostedWebSettings } from './HostedWebSettings';
import { ModelDiscoverySettings } from './ModelDiscoverySettings';

vi.mock('@/lib/api', () => ({ api: vi.fn(), apiPost: vi.fn() }));

beforeAll(() => {
  Object.assign(Element.prototype, {
    hasPointerCapture: () => false,
    setPointerCapture: () => {},
    releasePointerCapture: () => {},
    scrollIntoView: () => {},
  });
});

it('saves selected search sources in order and grants access using a rotated key’s lineage', async () => {
  const persist = vi.fn().mockResolvedValue(undefined);
  vi.mocked(api).mockResolvedValue({
    data: [
      { id: 'new', lineage_id: 'original', key_prefix: 'tw-console', name: 'Developer CLI', surfaces: ['ai_gateway'], is_active: true },
      { id: 'old', lineage_id: 'original', key_prefix: 'tw-old', name: 'Developer CLI', surfaces: ['ai_gateway'], is_active: true },
      { id: 'mcp', lineage_id: 'mcp', key_prefix: 'tw-mcp', name: 'MCP only', surfaces: ['mcp_gateway'], is_active: true },
    ],
    total: 3,
  });
  const user = userEvent.setup();
  renderWithQueryClient(<HostedWebSettings initial={{ execution: { max_results: 12 } }} persist={persist} />);

  await user.click(screen.getByRole('switch', { name: 'Run web tools in ThinkWatch' }));
  expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
  await user.click(screen.getByRole('switch', { name: 'Use automatic source order' }));
  await user.click(screen.getByRole('checkbox', { name: 'Google Docs' }));
  await user.click(screen.getByRole('button', { name: 'Move Bing up' }));
  await user.click(await screen.findByRole('button', { name: 'Authorize Developer CLI (tw-console)' }));
  expect(screen.queryByText(/MCP only|tw-old/)).not.toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: 'Save' }));

  await waitFor(() => expect(persist).toHaveBeenCalledWith({
    enabled_lineages: ['original'],
    execution: {
      mode: 'core', engines: ['bing', 'duckduckgo'], max_results: 12,
      allowed_domains: [], blocked_domains: [],
    },
  }));
});

it('edits model aliases and provider capabilities using controls', async () => {
  const persist = vi.fn().mockResolvedValue(undefined);
  vi.mocked(api).mockImplementation(async (url) => {
    if (url === '/api/admin/models/ids') return [{ model_id: 'coding' }];
    if (url === '/api/admin/providers') return [{ id: 'provider-id', name: 'Engineering' }];
    if (url === '/api/admin/providers/provider-id/remote-models') return [{ id: 'upstream-code' }];
    throw new Error('Unexpected request');
  });
  const user = userEvent.setup();
  renderWithQueryClient(<ModelDiscoverySettings initial={{}} persist={persist} />);
  await user.click(screen.getByRole('button', { name: 'Add alias' }));
  await user.type(screen.getByRole('textbox', { name: 'Alias name' }), 'fast');
  await waitFor(() => expect(api).toHaveBeenCalledWith('/api/admin/models/ids', expect.anything()));
  await user.click(screen.getByRole('combobox', { name: 'Target model' }));
  await user.click(await screen.findByRole('option', { name: 'coding' }));
  await user.click(screen.getByRole('checkbox', { name: 'Hide the original in model lists' }));

  await user.click(screen.getByRole('button', { name: 'Add override' }));
  await waitFor(() => expect(api).toHaveBeenCalledWith('/api/admin/providers', expect.anything()));
  await user.click(screen.getByRole('combobox', { name: 'Provider' }));
  await user.click(await screen.findByRole('option', { name: 'Engineering' }));
  await user.click(screen.getByRole('combobox', { name: 'Upstream model ID' }));
  await user.click(await screen.findByRole('option', { name: 'upstream-code' }));
  await user.type(screen.getByRole('spinbutton', { name: 'Context window (tokens)' }), '128000');
  await user.click(screen.getByRole('button', { name: 'Save' }));

  await waitFor(() => expect(persist).toHaveBeenCalledWith({
    aliases: { fast: { target: 'coding', hide_original: true } },
    profiles: { 'provider-id': { 'upstream-code': { context_window: 128000 } } },
  }));
});

it('tests the saved search settings and shows sources, warnings, and failures', async () => {
  vi.mocked(api).mockResolvedValue({ data: [], total: 0 });
  vi.mocked(apiPost).mockResolvedValueOnce({
    sources: [{ url: 'https://example.org/', title: 'Example guide', text: 'Page excerpt', engine: 'bing' }],
    elapsed_ms: 21, cached: true, truncated: false,
    warnings: [{ text: 'DuckDuckGo timed out' }],
  }).mockRejectedValueOnce(new Error('All configured sources failed'));
  const user = userEvent.setup();
  renderWithQueryClient(<HostedWebSettings initial={{ execution: { mode: 'core' }, enabled_lineages: ['key'] }}
    persist={vi.fn()} />);

  await user.type(screen.getByRole('searchbox', { name: 'Search query' }), 'example guide');
  await user.click(screen.getByRole('button', { name: 'Run test' }));
  expect(await screen.findByRole('link', { name: 'Example guide' })).toHaveAttribute('href', 'https://example.org/');
  expect(apiPost).toHaveBeenCalledWith('/api/admin/settings/web-tools/test', { action: 'search', query: 'example guide' });
  expect(screen.getByText('DuckDuckGo timed out')).toBeInTheDocument();
  expect(screen.getByRole('status')).toHaveTextContent('21 ms');
  expect(screen.getByRole('status')).toHaveTextContent('Cached');

  await user.click(screen.getByRole('button', { name: 'Run test' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('All configured sources failed');
  expect(screen.queryByRole('link', { name: 'Example guide' })).toBeNull();
});

it('keeps stale alias edits until the administrator chooses to load the latest settings', async () => {
  const persist = vi.fn().mockRejectedValue(Object.assign(new Error('Conflict'), { status: 409 }));
  const loadLatest = vi.fn().mockResolvedValue({ aliases: { newer: { target: 'coding' } } });
  vi.mocked(api).mockResolvedValue([{ model_id: 'coding' }]);
  const user = userEvent.setup();
  renderWithQueryClient(<ModelDiscoverySettings initial={{ aliases: { fast: { target: 'coding' } } }}
    persist={persist} loadLatest={loadLatest} />);

  const name = screen.getByRole('textbox', { name: 'Alias name' });
  await user.clear(name);
  await user.type(name, 'my-draft');
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByText(/Another administrator changed/)).toBeInTheDocument();
  expect(name).toHaveValue('my-draft');
  expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
  expect(loadLatest).not.toHaveBeenCalled();
  await user.click(screen.getByRole('button', { name: 'Discard edits and load latest' }));
  await waitFor(() => expect(screen.getByRole('textbox', { name: 'Alias name' })).toHaveValue('newer'));
});
