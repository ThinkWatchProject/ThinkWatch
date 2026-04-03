import { useState, useCallback, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import {
  BookOpen,
  Copy,
  Check,
  Terminal,
  Globe,
  Code2,
  MousePointerClick,
  Puzzle,
  Braces,
} from 'lucide-react';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function useGatewayUrl(): string {
  return useMemo(() => {
    const { protocol, hostname } = window.location;
    return `${protocol}//${hostname}:3000`;
  }, []);
}

function CopyButton({ text }: { text: string }) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);

  const copy = useCallback(() => {
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }, [text]);

  return (
    <Button variant="outline" size="sm" onClick={copy} className="gap-1.5">
      {copied ? (
        <>
          <Check className="h-3.5 w-3.5" />
          {t('guide.copied')}
        </>
      ) : (
        <>
          <Copy className="h-3.5 w-3.5" />
          {t('guide.copyCode')}
        </>
      )}
    </Button>
  );
}

function CodeBlock({ code }: { code: string }) {
  return (
    <div className="relative">
      <div className="absolute right-3 top-3">
        <CopyButton text={code} />
      </div>
      <pre className="overflow-x-auto rounded-lg border bg-muted/50 p-4 font-mono text-sm leading-relaxed">
        <code>{code}</code>
      </pre>
    </div>
  );
}

function StepList({ steps }: { steps: string[] }) {
  const { t } = useTranslation();
  return (
    <div className="space-y-1">
      <p className="text-sm font-medium text-muted-foreground">{t('guide.steps')}</p>
      <ol className="list-decimal space-y-1 pl-5 text-sm">
        {steps.map((step, i) => (
          <li key={i}>{step}</li>
        ))}
      </ol>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Tab content builders
// ---------------------------------------------------------------------------

function ClaudeCodeTab({ gatewayUrl }: { gatewayUrl: string }) {
  const code = `# Set AgentBastion as Anthropic API proxy
export ANTHROPIC_BASE_URL=${gatewayUrl}
export ANTHROPIC_API_KEY=ab-your-api-key-here

# Then use claude normally
claude`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Terminal className="h-5 w-5" />
        <h3 className="text-lg font-semibold">Claude Code</h3>
        <Badge variant="outline">Anthropic CLI</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        Claude Code connects using the Anthropic native format. Set environment variables to route
        all requests through AgentBastion.
      </p>
      <StepList
        steps={[
          'Set ANTHROPIC_BASE_URL to your AgentBastion gateway URL',
          'Set ANTHROPIC_API_KEY to your ab- prefixed API key',
          'Run claude as usual — all requests are proxied through the gateway',
        ]}
      />
      <CodeBlock code={code} />
      <div className="rounded-lg border border-blue-200 bg-blue-50 p-3 text-sm text-blue-800 dark:border-blue-900 dark:bg-blue-950 dark:text-blue-200">
        <strong>Endpoint:</strong> /v1/messages (Anthropic native format).
        AgentBastion translates internally and supports all Claude models.
      </div>
    </div>
  );
}

function CursorTab({ gatewayUrl }: { gatewayUrl: string }) {
  const code = `// In Cursor Settings > Models > OpenAI API Key
// Base URL: ${gatewayUrl}/v1
// API Key: ab-your-api-key-here`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <MousePointerClick className="h-5 w-5" />
        <h3 className="text-lg font-semibold">Cursor</h3>
        <Badge variant="outline">IDE</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        Cursor uses the OpenAI-compatible API format. Configure it in the editor settings.
      </p>
      <StepList
        steps={[
          'Open Cursor Settings',
          'Navigate to Models > OpenAI API Key',
          `Set Base URL to: ${gatewayUrl}/v1`,
          'Set API Key to your ab- prefixed API key',
          'Select any model configured in AgentBastion',
        ]}
      />
      <CodeBlock code={code} />
      <div className="rounded-lg border border-blue-200 bg-blue-50 p-3 text-sm text-blue-800 dark:border-blue-900 dark:bg-blue-950 dark:text-blue-200">
        <strong>Endpoint:</strong> /v1/chat/completions (OpenAI-compatible).
        Supports all models routed through AgentBastion.
      </div>
    </div>
  );
}

function ContinueTab({ gatewayUrl }: { gatewayUrl: string }) {
  const code = `// ~/.continue/config.json
{
  "models": [{
    "title": "AgentBastion Proxy",
    "provider": "openai",
    "model": "gpt-4o",
    "apiBase": "${gatewayUrl}/v1",
    "apiKey": "ab-your-api-key-here"
  }]
}`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Puzzle className="h-5 w-5" />
        <h3 className="text-lg font-semibold">Continue</h3>
        <Badge variant="outline">VS Code Extension</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        Continue connects as an OpenAI-compatible provider. Edit the config file to point at
        AgentBastion.
      </p>
      <StepList
        steps={[
          'Open ~/.continue/config.json (or use the Continue settings UI)',
          'Add a new model entry with provider set to "openai"',
          `Set apiBase to: ${gatewayUrl}/v1`,
          'Set apiKey to your ab- prefixed API key',
          'Choose any model name configured in AgentBastion',
        ]}
      />
      <CodeBlock code={code} />
    </div>
  );
}

function ClineTab({ gatewayUrl }: { gatewayUrl: string }) {
  const code = `// Cline Settings > API Provider > OpenAI Compatible
// Base URL: ${gatewayUrl}/v1
// API Key: ab-your-api-key-here
// Model: gpt-4o (or any model configured in AgentBastion)`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Code2 className="h-5 w-5" />
        <h3 className="text-lg font-semibold">Cline</h3>
        <Badge variant="outline">VS Code Extension</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        Cline supports OpenAI-compatible providers. Configure it through the extension settings.
      </p>
      <StepList
        steps={[
          'Open Cline Settings in VS Code',
          'Select API Provider > OpenAI Compatible',
          `Set Base URL to: ${gatewayUrl}/v1`,
          'Set API Key to your ab- prefixed API key',
          'Enter a model name (e.g. gpt-4o) configured in AgentBastion',
        ]}
      />
      <CodeBlock code={code} />
    </div>
  );
}

function OpenAiSdkTab({ gatewayUrl }: { gatewayUrl: string }) {
  const code = `from openai import OpenAI

client = OpenAI(
    base_url="${gatewayUrl}/v1",
    api_key="ab-your-api-key-here",
)

response = client.chat.completions.create(
    model="gpt-4o",
    messages=[{"role": "user", "content": "Hello!"}],
)`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Braces className="h-5 w-5" />
        <h3 className="text-lg font-semibold">OpenAI SDK</h3>
        <Badge variant="outline">Python</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        Use the official OpenAI Python SDK with AgentBastion as a drop-in replacement.
      </p>
      <StepList
        steps={[
          'Install the OpenAI SDK: pip install openai',
          `Set base_url to: ${gatewayUrl}/v1`,
          'Set api_key to your ab- prefixed API key',
          'Use any model configured in AgentBastion',
        ]}
      />
      <CodeBlock code={code} />
    </div>
  );
}

function AnthropicSdkTab({ gatewayUrl }: { gatewayUrl: string }) {
  const code = `import anthropic

client = anthropic.Anthropic(
    base_url="${gatewayUrl}",
    api_key="ab-your-api-key-here",
)

message = client.messages.create(
    model="claude-sonnet-4-20250514",
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello!"}],
)`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Braces className="h-5 w-5" />
        <h3 className="text-lg font-semibold">Anthropic SDK</h3>
        <Badge variant="outline">Python</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        Use the official Anthropic Python SDK by pointing it at AgentBastion.
      </p>
      <StepList
        steps={[
          'Install the Anthropic SDK: pip install anthropic',
          `Set base_url to: ${gatewayUrl} (no /v1 suffix)`,
          'Set api_key to your ab- prefixed API key',
          'Use any Claude model configured in AgentBastion',
        ]}
      />
      <CodeBlock code={code} />
      <div className="rounded-lg border border-blue-200 bg-blue-50 p-3 text-sm text-blue-800 dark:border-blue-900 dark:bg-blue-950 dark:text-blue-200">
        <strong>Note:</strong> The Anthropic SDK uses the base URL without the /v1 suffix, unlike
        OpenAI-compatible clients.
      </div>
    </div>
  );
}

function CurlTab({ gatewayUrl }: { gatewayUrl: string }) {
  const openaiCode = `# OpenAI-compatible format
curl ${gatewayUrl}/v1/chat/completions \\
  -H "Authorization: Bearer ab-your-api-key-here" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello"}]
  }'`;

  const anthropicCode = `# Anthropic Messages format
curl ${gatewayUrl}/v1/messages \\
  -H "Authorization: Bearer ab-your-api-key-here" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "claude-sonnet-4-20250514",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "Hello"}]
  }'`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Terminal className="h-5 w-5" />
        <h3 className="text-lg font-semibold">cURL</h3>
        <Badge variant="outline">Command Line</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        Test the gateway directly with cURL. Both OpenAI-compatible and Anthropic message formats
        are supported.
      </p>
      <h4 className="text-sm font-medium">OpenAI-compatible format</h4>
      <CodeBlock code={openaiCode} />
      <h4 className="text-sm font-medium">Anthropic Messages format</h4>
      <CodeBlock code={anthropicCode} />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Main page
// ---------------------------------------------------------------------------

export function GuidePage() {
  const { t } = useTranslation();
  const gatewayUrl = useGatewayUrl();

  return (
    <div className="space-y-6">
      {/* Header */}
      <div>
        <h1 className="flex items-center gap-2 text-2xl font-bold">
          <BookOpen className="h-6 w-6" />
          {t('guide.title')}
        </h1>
        <p className="mt-1 text-muted-foreground">{t('guide.subtitle')}</p>
      </div>

      {/* Gateway URL + info cards */}
      <div className="grid gap-4 sm:grid-cols-2">
        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="flex items-center gap-2 text-sm font-medium">
              <Globe className="h-4 w-4" />
              {t('guide.gatewayUrl')}
            </CardTitle>
          </CardHeader>
          <CardContent>
            <div className="flex items-center gap-2">
              <code className="flex-1 rounded bg-muted px-3 py-2 font-mono text-sm">
                {gatewayUrl}
              </code>
              <CopyButton text={gatewayUrl} />
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="flex items-center gap-2 text-sm font-medium">
              <BookOpen className="h-4 w-4" />
              {t('guide.supportedEndpoints')}
            </CardTitle>
          </CardHeader>
          <CardContent className="space-y-1 text-sm">
            <p>
              <Badge variant="outline" className="mr-2 font-mono text-xs">
                POST
              </Badge>
              {t('guide.openaiEndpoint')}
            </p>
            <p>
              <Badge variant="outline" className="mr-2 font-mono text-xs">
                POST
              </Badge>
              {t('guide.anthropicEndpoint')}
            </p>
            <p>
              <Badge variant="outline" className="mr-2 font-mono text-xs">
                GET
              </Badge>
              {t('guide.modelsEndpoint')}
            </p>
          </CardContent>
        </Card>
      </div>

      {/* API key reminder */}
      <div className="rounded-lg border border-amber-200 bg-amber-50 p-3 text-sm text-amber-800 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-200">
        {t('guide.apiKeyNote')}
      </div>

      {/* Tool tabs */}
      <Card>
        <CardContent className="pt-6">
          <Tabs defaultValue="claude-code">
            <TabsList className="flex-wrap">
              <TabsTrigger value="claude-code">{t('guide.claudeCode')}</TabsTrigger>
              <TabsTrigger value="cursor">{t('guide.cursor')}</TabsTrigger>
              <TabsTrigger value="continue">{t('guide.continue')}</TabsTrigger>
              <TabsTrigger value="cline">{t('guide.cline')}</TabsTrigger>
              <TabsTrigger value="openai-sdk">{t('guide.openaiSdk')}</TabsTrigger>
              <TabsTrigger value="anthropic-sdk">{t('guide.anthropicSdk')}</TabsTrigger>
              <TabsTrigger value="curl">{t('guide.curl')}</TabsTrigger>
            </TabsList>

            <TabsContent value="claude-code" className="mt-4">
              <ClaudeCodeTab gatewayUrl={gatewayUrl} />
            </TabsContent>
            <TabsContent value="cursor" className="mt-4">
              <CursorTab gatewayUrl={gatewayUrl} />
            </TabsContent>
            <TabsContent value="continue" className="mt-4">
              <ContinueTab gatewayUrl={gatewayUrl} />
            </TabsContent>
            <TabsContent value="cline" className="mt-4">
              <ClineTab gatewayUrl={gatewayUrl} />
            </TabsContent>
            <TabsContent value="openai-sdk" className="mt-4">
              <OpenAiSdkTab gatewayUrl={gatewayUrl} />
            </TabsContent>
            <TabsContent value="anthropic-sdk" className="mt-4">
              <AnthropicSdkTab gatewayUrl={gatewayUrl} />
            </TabsContent>
            <TabsContent value="curl" className="mt-4">
              <CurlTab gatewayUrl={gatewayUrl} />
            </TabsContent>
          </Tabs>
        </CardContent>
      </Card>
    </div>
  );
}
