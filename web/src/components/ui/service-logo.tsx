import { Server } from 'lucide-react';
import { Avatar, AvatarFallback } from '@/components/ui/avatar';
import { cn } from '@/lib/utils';

interface ServiceLogoProps {
  /** Service name — matches loosely, case-insensitive. */
  service: string;
  className?: string;
}

/**
 * Small lettermark for a named service. Composes shadcn `Avatar` with a
 * per-service tint on the fallback — no trademark risk (uses initials,
 * not real logos) while still carrying visual identity for brands like
 * OpenAI / Anthropic / GitHub.
 */
export function ServiceLogo({ service, className }: ServiceLogoProps) {
  const spec = resolve(service.toLowerCase());

  return (
    <Avatar className={cn('h-6 w-6 rounded', className)}>
      {spec ? (
        <AvatarFallback
          className={cn('rounded font-mono text-[10px] font-bold', spec.className)}
          title={service}
        >
          {spec.letter}
        </AvatarFallback>
      ) : (
        <AvatarFallback className="rounded bg-muted text-muted-foreground">
          <Server className="h-3.5 w-3.5" />
        </AvatarFallback>
      )}
    </Avatar>
  );
}

function resolve(key: string): { letter: string; className: string } | null {
  // Tokenize on any non-alphanumeric separator (hyphen / dot / slash /
  // underscore / space) so brand checks match whole segments instead
  // of substrings. The substring approach would have e.g. flagged a
  // future "hexagon" MCP server as Exa, or "awsm-mcp" as Bedrock —
  // the segments around the brand keyword would mask the false
  // positive. Token-set lookups make every rule a `===` check
  // without ballooning the rule list.
  const tokens = new Set(key.split(/[^a-z0-9]+/i).filter(Boolean));
  const has = (...names: string[]) => names.some((n) => tokens.has(n));

  // AI providers — first-party APIs
  if (has('openai', 'gpt')) return { letter: 'O', className: 'bg-emerald-500/15 text-emerald-500' };
  if (has('anthropic', 'claude')) return { letter: 'A', className: 'bg-amber-500/15 text-amber-500' };
  if (has('google', 'gemini')) return { letter: 'G', className: 'bg-blue-500/15 text-blue-500' };
  if (has('azure')) return { letter: 'Az', className: 'bg-cyan-500/15 text-cyan-500' };
  if (has('bedrock', 'aws')) return { letter: 'A', className: 'bg-violet-500/15 text-violet-500' };

  // AI providers — aggregators + open-weight gateways. Listed before
  // the "Dev tooling" block so e.g. "openrouter" doesn't accidentally
  // share a prefix with a future rule.
  if (has('openrouter')) return { letter: 'OR', className: 'bg-rose-500/15 text-rose-500' };
  if (has('deepseek')) return { letter: 'DS', className: 'bg-blue-600/15 text-blue-600' };
  if (has('moonshot', 'kimi')) return { letter: 'K', className: 'bg-violet-500/15 text-violet-500' };
  if (has('mistral')) return { letter: 'Mi', className: 'bg-orange-500/15 text-orange-500' };
  if (has('groq')) return { letter: 'Gq', className: 'bg-red-500/15 text-red-500' };
  if (has('perplexity')) return { letter: 'Pp', className: 'bg-teal-500/15 text-teal-500' };
  if (has('fireworks')) return { letter: 'Fw', className: 'bg-amber-600/15 text-amber-600' };
  if (has('together')) return { letter: 'Tg', className: 'bg-blue-500/15 text-blue-500' };
  if (has('xai', 'grok')) return { letter: 'X', className: 'bg-neutral-500/15 text-foreground' };
  if (has('cohere')) return { letter: 'Co', className: 'bg-pink-500/15 text-pink-500' };

  // MCP servers — search / retrieval
  if (has('exa')) return { letter: 'Ex', className: 'bg-indigo-500/15 text-indigo-500' };
  if (has('brave')) return { letter: 'Bv', className: 'bg-orange-500/15 text-orange-500' };
  if (has('tavily')) return { letter: 'Tv', className: 'bg-cyan-500/15 text-cyan-500' };

  // Dev tooling
  if (has('github')) return { letter: 'GH', className: 'bg-neutral-500/15 text-foreground' };
  if (has('gitlab')) return { letter: 'GL', className: 'bg-orange-500/15 text-orange-500' };
  if (has('linear')) return { letter: 'L', className: 'bg-indigo-500/15 text-indigo-500' };
  if (has('sentry')) return { letter: 'S', className: 'bg-purple-500/15 text-purple-500' };
  if (has('jira', 'atlassian')) return { letter: 'J', className: 'bg-blue-600/15 text-blue-600' };
  if (has('vercel')) return { letter: 'V', className: 'bg-neutral-500/15 text-foreground' };
  if (has('figma')) return { letter: 'Fg', className: 'bg-fuchsia-500/15 text-fuchsia-500' };
  if (has('stripe')) return { letter: 'St', className: 'bg-violet-500/15 text-violet-500' };

  // Data stores
  if (has('postgres')) return { letter: 'Pg', className: 'bg-sky-500/15 text-sky-500' };
  if (has('mysql')) return { letter: 'My', className: 'bg-orange-600/15 text-orange-600' };
  if (has('redis')) return { letter: 'R', className: 'bg-red-500/15 text-red-500' };
  if (has('mongo')) return { letter: 'M', className: 'bg-green-500/15 text-green-500' };
  if (has('supabase')) return { letter: 'Sb', className: 'bg-emerald-500/15 text-emerald-500' };

  // Messaging
  if (has('slack')) return { letter: 'Sl', className: 'bg-fuchsia-500/15 text-fuchsia-500' };
  if (has('discord')) return { letter: 'D', className: 'bg-indigo-500/15 text-indigo-500' };

  // Docs / Knowledge
  if (has('microsoft')) return { letter: 'MS', className: 'bg-sky-500/15 text-sky-500' };
  if (has('cloudflare')) return { letter: 'CF', className: 'bg-orange-500/15 text-orange-500' };
  if (has('notion')) return { letter: 'N', className: 'bg-neutral-500/15 text-foreground' };
  if (has('wikipedia')) return { letter: 'W', className: 'bg-stone-500/15 text-stone-500' };
  if (has('arxiv')) return { letter: 'ar', className: 'bg-red-600/15 text-red-600' };
  if (has('mdn')) return { letter: 'M', className: 'bg-neutral-500/15 text-foreground' };

  return null;
}
