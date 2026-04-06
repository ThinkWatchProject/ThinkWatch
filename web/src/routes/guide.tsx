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
  Server,
  MonitorSmartphone,
  Workflow,
  Bot,
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
      <pre className="max-w-full overflow-x-auto rounded-lg border bg-muted/50 p-4 font-mono text-sm leading-relaxed">
        <code className="whitespace-pre-wrap break-all">{code}</code>
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

function InfoBox({ children }: { children: React.ReactNode }) {
  return (
    <div className="rounded-lg border border-blue-200 bg-blue-50 p-3 text-sm text-blue-800 dark:border-blue-900 dark:bg-blue-950 dark:text-blue-200">
      {children}
    </div>
  );
}

// ===========================================================================
// MCP Gateway — AI Prompt tab
// ===========================================================================

function McpPromptTab({ mcpUrl }: { mcpUrl: string }) {
  const { t } = useTranslation();
  const prompt = `I need to configure my AI tool's MCP (Model Context Protocol) client to connect to an MCP gateway called ThinkWatch.

Here is the information you need:

MCP Endpoint: ${mcpUrl}/mcp
Transport: Streamable HTTP (protocol version 2025-03-26)
API Key: (ask the user, it starts with "tw-")

The MCP server configuration should be:
{
  "type": "streamableHttp",
  "url": "${mcpUrl}/mcp",
  "headers": {
    "Authorization": "Bearer <API_KEY>"
  }
}

Configuration for specific tools:
- Claude Desktop: add to mcpServers in claude_desktop_config.json (macOS: ~/Library/Application Support/Claude/claude_desktop_config.json, Windows: %APPDATA%\\Claude\\claude_desktop_config.json)
- Claude Code CLI: run \`claude mcp add think-watch --transport streamable-http "${mcpUrl}/mcp" --header "Authorization: Bearer <key>"\`
- Cursor: add to mcpServers in .cursor/mcp.json (project) or ~/.cursor/mcp.json (global)
- Cline: add to mcpServers in cline_mcp_settings.json or via Cline Settings > MCP Servers
- VS Code / Copilot: add to mcpServers in .vscode/mcp.json

Please detect which tool I am using and help me configure it step by step.`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Bot className="h-5 w-5" />
        <h3 className="text-lg font-semibold">{t('guide.mcpPromptTitle')}</h3>
        <Badge variant="secondary">{t('guide.recommended')}</Badge>
      </div>
      <p className="text-sm text-muted-foreground">{t('guide.mcpPromptDesc')}</p>
      <StepList
        steps={[
          t('guide.promptStep.copy'),
          t('guide.promptStep.paste'),
          t('guide.promptStep.follow'),
        ]}
      />
      <CodeBlock code={prompt} />
      <InfoBox>{t('guide.promptTip')}</InfoBox>
    </div>
  );
}

// ===========================================================================
// AI Gateway tab contents
// ===========================================================================

function ClaudeCodeTab({ gatewayUrl }: { gatewayUrl: string }) {
  const { t } = useTranslation();
  const code = `# Set ThinkWatch as Anthropic API proxy
export ANTHROPIC_BASE_URL=${gatewayUrl}
export ANTHROPIC_API_KEY=tw-your-api-key-here

# Then use claude normally
claude`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Terminal className="h-5 w-5" />
        <h3 className="text-lg font-semibold">Claude Code</h3>
        <Badge variant="outline">Anthropic CLI</Badge>
      </div>
      <p className="text-sm text-muted-foreground">{t('guide.claudeCodeDesc')}</p>
      <StepList
        steps={[
          t('guide.aiStep.createKey'),
          t('guide.aiStep.claudeCodeBaseUrl'),
          t('guide.aiStep.claudeCodeApiKey'),
          t('guide.aiStep.claudeCodeRun'),
        ]}
      />
      <CodeBlock code={code} />
    </div>
  );
}

function CursorTab({ gatewayUrl }: { gatewayUrl: string }) {
  const { t } = useTranslation();
  const code = `// In Cursor Settings > Models > OpenAI API Key
// Base URL: ${gatewayUrl}/v1
// API Key: tw-your-api-key-here`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <MousePointerClick className="h-5 w-5" />
        <h3 className="text-lg font-semibold">Cursor</h3>
        <Badge variant="outline">IDE</Badge>
      </div>
      <p className="text-sm text-muted-foreground">{t('guide.cursorDesc')}</p>
      <StepList
        steps={[
          t('guide.aiStep.createKey'),
          t('guide.aiStep.cursorOpen'),
          t('guide.aiStep.cursorBaseUrl', { url: `${gatewayUrl}/v1` }),
          t('guide.aiStep.cursorApiKey'),
          t('guide.aiStep.cursorSelectModel'),
        ]}
      />
      <CodeBlock code={code} />
      <InfoBox>{t('guide.cursorEndpointNote')}</InfoBox>
    </div>
  );
}

function ContinueTab({ gatewayUrl }: { gatewayUrl: string }) {
  const { t } = useTranslation();
  const code = `// ~/.continue/config.json
{
  "models": [{
    "title": "ThinkWatch Proxy",
    "provider": "openai",
    "model": "gpt-4o",
    "apiBase": "${gatewayUrl}/v1",
    "apiKey": "tw-your-api-key-here"
  }]
}`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Puzzle className="h-5 w-5" />
        <h3 className="text-lg font-semibold">Continue</h3>
        <Badge variant="outline">VS Code Extension</Badge>
      </div>
      <p className="text-sm text-muted-foreground">{t('guide.continueDesc')}</p>
      <StepList
        steps={[
          t('guide.aiStep.createKey'),
          t('guide.aiStep.continueOpen'),
          t('guide.aiStep.continueAdd'),
          t('guide.aiStep.continueApiBase', { url: `${gatewayUrl}/v1` }),
          t('guide.aiStep.continueApiKey'),
          t('guide.aiStep.continueChooseModel'),
        ]}
      />
      <CodeBlock code={code} />
    </div>
  );
}

function ClineTab({ gatewayUrl }: { gatewayUrl: string }) {
  const { t } = useTranslation();
  const code = `// Cline Settings > API Provider > OpenAI Compatible
// Base URL: ${gatewayUrl}/v1
// API Key: tw-your-api-key-here
// Model: gpt-4o (or any model configured in ThinkWatch)`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Code2 className="h-5 w-5" />
        <h3 className="text-lg font-semibold">Cline</h3>
        <Badge variant="outline">VS Code Extension</Badge>
      </div>
      <p className="text-sm text-muted-foreground">{t('guide.clineDesc')}</p>
      <StepList
        steps={[
          t('guide.aiStep.createKey'),
          t('guide.aiStep.clineOpen'),
          t('guide.aiStep.clineBaseUrl', { url: `${gatewayUrl}/v1` }),
          t('guide.aiStep.clineApiKey'),
          t('guide.aiStep.clineModel'),
        ]}
      />
      <CodeBlock code={code} />
    </div>
  );
}

function OpenAiSdkTab({ gatewayUrl }: { gatewayUrl: string }) {
  const { t } = useTranslation();
  const code = `from openai import OpenAI

client = OpenAI(
    base_url="${gatewayUrl}/v1",
    api_key="tw-your-api-key-here",
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
      <p className="text-sm text-muted-foreground">{t('guide.openaiSdkDesc')}</p>
      <StepList
        steps={[
          t('guide.aiStep.openaiSdkInstall'),
          t('guide.aiStep.createKey'),
          t('guide.aiStep.openaiSdkBaseUrl', { url: `${gatewayUrl}/v1` }),
          t('guide.aiStep.openaiSdkApiKey'),
          t('guide.aiStep.openaiSdkModel'),
        ]}
      />
      <CodeBlock code={code} />
    </div>
  );
}

function AnthropicSdkTab({ gatewayUrl }: { gatewayUrl: string }) {
  const { t } = useTranslation();
  const code = `import anthropic

client = anthropic.Anthropic(
    base_url="${gatewayUrl}",
    api_key="tw-your-api-key-here",
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
      <p className="text-sm text-muted-foreground">{t('guide.anthropicSdkDesc')}</p>
      <StepList
        steps={[
          t('guide.aiStep.anthropicSdkInstall'),
          t('guide.aiStep.createKey'),
          t('guide.aiStep.anthropicSdkBaseUrl', { url: gatewayUrl }),
          t('guide.aiStep.anthropicSdkApiKey'),
          t('guide.aiStep.anthropicSdkModel'),
        ]}
      />
      <CodeBlock code={code} />
      <InfoBox>
        <strong>{t('guide.note')}:</strong> {t('guide.anthropicSdkNote')}
      </InfoBox>
    </div>
  );
}

function CurlTab({ gatewayUrl }: { gatewayUrl: string }) {
  const { t } = useTranslation();
  const openaiCode = `# OpenAI-compatible format
curl ${gatewayUrl}/v1/chat/completions \\
  -H "Authorization: Bearer tw-your-api-key-here" \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello"}]
  }'`;

  const anthropicCode = `# Anthropic Messages format
curl ${gatewayUrl}/v1/messages \\
  -H "Authorization: Bearer tw-your-api-key-here" \\
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
      <p className="text-sm text-muted-foreground">{t('guide.curlDesc')}</p>
      <h4 className="text-sm font-medium">{t('guide.curlOpenaiFormat')}</h4>
      <CodeBlock code={openaiCode} />
      <h4 className="text-sm font-medium">{t('guide.curlAnthropicFormat')}</h4>
      <CodeBlock code={anthropicCode} />
    </div>
  );
}

// ===========================================================================
// MCP Gateway tab contents
// ===========================================================================

function McpClaudeDesktopTab({ mcpUrl }: { mcpUrl: string }) {
  const { t } = useTranslation();
  const code = `// claude_desktop_config.json
// macOS: ~/Library/Application Support/Claude/claude_desktop_config.json
// Windows: %APPDATA%\\Claude\\claude_desktop_config.json
{
  "mcpServers": {
    "think-watch": {
      "type": "streamableHttp",
      "url": "${mcpUrl}/mcp",
      "headers": {
        "Authorization": "Bearer tw-your-api-key-here"
      }
    }
  }
}`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <MonitorSmartphone className="h-5 w-5" />
        <h3 className="text-lg font-semibold">{t('guide.claudeDesktop')}</h3>
        <Badge variant="outline">Desktop App</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        {t('guide.claudeDesktopDesc')}
      </p>
      <StepList
        steps={[
          t('guide.mcpStep.openConfig'),
          t('guide.mcpStep.addServer'),
          t('guide.mcpStep.setApiKey'),
          t('guide.mcpStep.restartApp'),
        ]}
      />
      <CodeBlock code={code} />
      <InfoBox>
        <strong>{t('guide.mcpProtocol')}:</strong> Streamable HTTP — {t('guide.mcpProtocolDesc')}
      </InfoBox>
    </div>
  );
}

function McpClaudeCodeTab({ mcpUrl }: { mcpUrl: string }) {
  const { t } = useTranslation();
  const code = `# Add ThinkWatch as an MCP server in Claude Code
claude mcp add think-watch \\
  --transport streamable-http \\
  "${mcpUrl}/mcp" \\
  --header "Authorization: Bearer tw-your-api-key-here"

# List registered MCP servers
claude mcp list

# Remove if needed
claude mcp remove think-watch`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Terminal className="h-5 w-5" />
        <h3 className="text-lg font-semibold">Claude Code (MCP)</h3>
        <Badge variant="outline">CLI</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        {t('guide.claudeCodeMcpDesc')}
      </p>
      <StepList
        steps={[
          t('guide.mcpStep.runAdd'),
          t('guide.mcpStep.verifyList'),
          t('guide.mcpStep.useTools'),
        ]}
      />
      <CodeBlock code={code} />
    </div>
  );
}

function McpCursorTab({ mcpUrl }: { mcpUrl: string }) {
  const { t } = useTranslation();
  const code = `// .cursor/mcp.json (project-level) or ~/.cursor/mcp.json (global)
{
  "mcpServers": {
    "think-watch": {
      "type": "streamableHttp",
      "url": "${mcpUrl}/mcp",
      "headers": {
        "Authorization": "Bearer tw-your-api-key-here"
      }
    }
  }
}`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <MousePointerClick className="h-5 w-5" />
        <h3 className="text-lg font-semibold">Cursor (MCP)</h3>
        <Badge variant="outline">IDE</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        {t('guide.cursorMcpDesc')}
      </p>
      <StepList
        steps={[
          t('guide.mcpStep.createMcpJson'),
          t('guide.mcpStep.addServerEntry'),
          t('guide.mcpStep.setApiKey'),
          t('guide.mcpStep.restartApp'),
        ]}
      />
      <CodeBlock code={code} />
    </div>
  );
}

function McpClineTab({ mcpUrl }: { mcpUrl: string }) {
  const { t } = useTranslation();
  const code = `// VS Code Settings > Cline > MCP Servers
// Or in cline_mcp_settings.json:
{
  "mcpServers": {
    "think-watch": {
      "type": "streamableHttp",
      "url": "${mcpUrl}/mcp",
      "headers": {
        "Authorization": "Bearer tw-your-api-key-here"
      }
    }
  }
}`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Code2 className="h-5 w-5" />
        <h3 className="text-lg font-semibold">Cline (MCP)</h3>
        <Badge variant="outline">VS Code Extension</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        {t('guide.clineMcpDesc')}
      </p>
      <StepList
        steps={[
          t('guide.mcpStep.openClineSettings'),
          t('guide.mcpStep.addServerEntry'),
          t('guide.mcpStep.setApiKey'),
          t('guide.mcpStep.toolsAppear'),
        ]}
      />
      <CodeBlock code={code} />
    </div>
  );
}

function McpSdkTab({ mcpUrl }: { mcpUrl: string }) {
  const { t } = useTranslation();
  const tsCode = `import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";

const client = new Client({ name: "my-app", version: "1.0.0" });

const transport = new StreamableHTTPClientTransport(
  new URL("${mcpUrl}/mcp"),
  {
    requestInit: {
      headers: {
        Authorization: "Bearer tw-your-api-key-here",
      },
    },
  }
);

await client.connect(transport);

// List available tools
const tools = await client.listTools();
console.log(tools);

// Call a tool
const result = await client.callTool({
  name: "tool-name",
  arguments: { key: "value" },
});`;

  const pyCode = `from mcp import ClientSession
from mcp.client.streamable_http import streamablehttp_client

async with streamablehttp_client(
    "${mcpUrl}/mcp",
    headers={"Authorization": "Bearer tw-your-api-key-here"},
) as (read, write, _):
    async with ClientSession(read, write) as session:
        await session.initialize()

        # List available tools
        tools = await session.list_tools()
        print(tools)

        # Call a tool
        result = await session.call_tool("tool-name", arguments={"key": "value"})`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Braces className="h-5 w-5" />
        <h3 className="text-lg font-semibold">MCP SDK</h3>
        <Badge variant="outline">TypeScript / Python</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        {t('guide.mcpSdkDesc')}
      </p>
      <h4 className="text-sm font-medium">TypeScript (Node.js)</h4>
      <CodeBlock code={tsCode} />
      <h4 className="text-sm font-medium">Python</h4>
      <CodeBlock code={pyCode} />
    </div>
  );
}

function McpCurlTab({ mcpUrl }: { mcpUrl: string }) {
  const { t } = useTranslation();
  const initCode = `# Initialize MCP session
curl -X POST ${mcpUrl}/mcp \\
  -H "Authorization: Bearer tw-your-api-key-here" \\
  -H "Content-Type: application/json" \\
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {
      "protocolVersion": "2025-03-26",
      "capabilities": {},
      "clientInfo": { "name": "curl-test", "version": "1.0.0" }
    }
  }'`;

  const toolsCode = `# List available tools (use Mcp-Session-Id from init response)
curl -X POST ${mcpUrl}/mcp \\
  -H "Authorization: Bearer tw-your-api-key-here" \\
  -H "Content-Type: application/json" \\
  -H "Mcp-Session-Id: <session-id>" \\
  -d '{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "tools/list",
    "params": {}
  }'`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Terminal className="h-5 w-5" />
        <h3 className="text-lg font-semibold">cURL (MCP)</h3>
        <Badge variant="outline">Command Line</Badge>
      </div>
      <p className="text-sm text-muted-foreground">
        {t('guide.mcpCurlDesc')}
      </p>
      <h4 className="text-sm font-medium">{t('guide.mcpInitSession')}</h4>
      <CodeBlock code={initCode} />
      <h4 className="text-sm font-medium">{t('guide.mcpListTools')}</h4>
      <CodeBlock code={toolsCode} />
      <InfoBox>
        <strong>Mcp-Session-Id:</strong> {t('guide.mcpSessionIdNote')}
      </InfoBox>
    </div>
  );
}

// ===========================================================================
// Main page
// ===========================================================================

export function GuidePage() {
  const { t } = useTranslation();
  const gatewayUrl = useGatewayUrl();
  const mcpUrl = gatewayUrl; // MCP endpoint is on the gateway port

  return (
    <div className="min-w-0 space-y-6">
      {/* Header */}
      <div>
        <h1 className="flex items-center gap-2 text-2xl font-bold">
          <BookOpen className="h-6 w-6" />
          {t('guide.title')}
        </h1>
        <p className="mt-1 text-muted-foreground">{t('guide.subtitle')}</p>
      </div>

      {/* URL overview cards */}
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
            <p className="mt-2 text-xs text-muted-foreground">{t('guide.gatewayUrlDesc')}</p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="pb-2">
            <CardTitle className="flex items-center gap-2 text-sm font-medium">
              <Workflow className="h-4 w-4" />
              {t('guide.mcpEndpoint')}
            </CardTitle>
          </CardHeader>
          <CardContent>
            <div className="flex items-center gap-2">
              <code className="flex-1 rounded bg-muted px-3 py-2 font-mono text-sm">
                {mcpUrl}/mcp
              </code>
              <CopyButton text={`${mcpUrl}/mcp`} />
            </div>
            <p className="mt-2 text-xs text-muted-foreground">{t('guide.mcpEndpointDesc')}</p>
          </CardContent>
        </Card>
      </div>

      {/* ============================================================= */}
      {/* Top-level tabs: AI Gateway / MCP Gateway                       */}
      {/* ============================================================= */}
      <Tabs defaultValue="ai-gateway">
        <TabsList className="w-full grid grid-cols-2">
          <TabsTrigger value="ai-gateway" className="gap-2">
            <Globe className="h-4 w-4" />
            {t('guide.aiGatewaySection')}
          </TabsTrigger>
          <TabsTrigger value="mcp-gateway" className="gap-2">
            <Server className="h-4 w-4" />
            {t('guide.mcpGatewaySection')}
          </TabsTrigger>
        </TabsList>

        {/* AI Gateway */}
        <TabsContent value="ai-gateway" className="mt-4 space-y-4">
          <p className="text-sm text-muted-foreground">{t('guide.aiGatewayDesc')}</p>

          {/* Supported endpoints */}
          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-sm font-medium">{t('guide.supportedEndpoints')}</CardTitle>
            </CardHeader>
            <CardContent className="space-y-1 text-sm">
              <p>
                <Badge variant="outline" className="mr-2 font-mono text-xs">POST</Badge>
                {t('guide.openaiEndpoint')}
              </p>
              <p>
                <Badge variant="outline" className="mr-2 font-mono text-xs">POST</Badge>
                {t('guide.anthropicEndpoint')}
              </p>
              <p>
                <Badge variant="outline" className="mr-2 font-mono text-xs">POST</Badge>
                {t('guide.responsesEndpoint')}
              </p>
              <p>
                <Badge variant="outline" className="mr-2 font-mono text-xs">GET</Badge>
                {t('guide.modelsEndpoint')}
              </p>
            </CardContent>
          </Card>

          {/* AI Gateway tool tabs */}
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
        </TabsContent>

        {/* MCP Gateway */}
        <TabsContent value="mcp-gateway" className="mt-4 space-y-4">
          <p className="text-sm text-muted-foreground">{t('guide.mcpGatewayDesc')}</p>

          {/* MCP endpoint info */}
          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-sm font-medium">{t('guide.mcpInfo')}</CardTitle>
            </CardHeader>
            <CardContent className="space-y-1 text-sm">
              <p>
                <Badge variant="outline" className="mr-2 font-mono text-xs">POST</Badge>
                {t('guide.mcpStreamableEndpoint')}
              </p>
              <p>
                <Badge variant="outline" className="mr-2 font-mono text-xs">DELETE</Badge>
                {t('guide.mcpSessionEndpoint')}
              </p>
              <p className="mt-2 text-xs text-muted-foreground">{t('guide.mcpVersionNote')}</p>
            </CardContent>
          </Card>

          {/* MCP tool tabs */}
          <Card>
            <CardContent className="pt-6">
              <Tabs defaultValue="mcp-prompt">
                <TabsList className="flex-wrap">
                  <TabsTrigger value="mcp-prompt">
                    <Bot className="mr-1 h-3.5 w-3.5" />
                    {t('guide.aiPrompt')}
                  </TabsTrigger>
                  <TabsTrigger value="mcp-claude-desktop">{t('guide.claudeDesktop')}</TabsTrigger>
                  <TabsTrigger value="mcp-claude-code">Claude Code</TabsTrigger>
                  <TabsTrigger value="mcp-cursor">Cursor</TabsTrigger>
                  <TabsTrigger value="mcp-cline">Cline</TabsTrigger>
                  <TabsTrigger value="mcp-sdk">{t('guide.mcpSdk')}</TabsTrigger>
                  <TabsTrigger value="mcp-curl">cURL</TabsTrigger>
                </TabsList>

                <TabsContent value="mcp-prompt" className="mt-4">
                  <McpPromptTab mcpUrl={mcpUrl} />
                </TabsContent>
                <TabsContent value="mcp-claude-desktop" className="mt-4">
                  <McpClaudeDesktopTab mcpUrl={mcpUrl} />
                </TabsContent>
                <TabsContent value="mcp-claude-code" className="mt-4">
                  <McpClaudeCodeTab mcpUrl={mcpUrl} />
                </TabsContent>
                <TabsContent value="mcp-cursor" className="mt-4">
                  <McpCursorTab mcpUrl={mcpUrl} />
                </TabsContent>
                <TabsContent value="mcp-cline" className="mt-4">
                  <McpClineTab mcpUrl={mcpUrl} />
                </TabsContent>
                <TabsContent value="mcp-sdk" className="mt-4">
                  <McpSdkTab mcpUrl={mcpUrl} />
                </TabsContent>
                <TabsContent value="mcp-curl" className="mt-4">
                  <McpCurlTab mcpUrl={mcpUrl} />
                </TabsContent>
              </Tabs>
            </CardContent>
          </Card>
        </TabsContent>
      </Tabs>
    </div>
  );
}
