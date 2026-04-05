# Licensing

ThinkWatch is distributed under the [Business Source License 1.1](LICENSE)
as source-available software.

## Summary

| Item | Rule |
|------|------|
| Non-production use | Permitted at no charge |
| Free production use | Permitted up to both `10,000,000` Billable Tokens and `10,000` MCP Tool Calls per UTC calendar month |
| Commercial trigger | A separate commercial license is required if either threshold is exceeded |
| Future open-source conversion | Each released version converts to `GPL-2.0-or-later` on the earlier of its Change Date or the fourth anniversary of its first public release |

## Usage-Based Commercial Terms

Commercial access is priced by monthly usage rather than by seat count.

### Billable Tokens

| Included in Billable Tokens | Excluded from Billable Tokens |
|-----------------------------|-------------------------------|
| All input tokens processed for production traffic | Local development, CI, test, staging, or demo traffic |
| All output tokens generated for production traffic | Internal evaluation traffic that does not serve end-user production requests |
| Aggregated usage across all workspaces, teams, environments, and customers operated by the same legal entity | — |

### MCP Tool Calls

| Included in MCP Tool Calls | Excluded from MCP Tool Calls |
|----------------------------|------------------------------|
| Each production invocation of an MCP tool routed through the Licensed Work | Internal health checks |
| Calls initiated by human users, agents, automations, or background workflows | Tool discovery and catalog refresh operations such as `tools/list` |
| Aggregated usage across all workspaces, teams, environments, and customers operated by the same legal entity | Local development, CI, test, staging, or demo traffic |

### Production Usage Volume

| Term | Definition |
|------|------------|
| Production Usage Volume | The total Billable Tokens and total MCP Tool Calls processed during a UTC calendar month |

### Pricing Model

| Tier | Billable Tokens / month | MCP Tool Calls / month | List Price |
|------|--------------------------|-------------------------|------------|
| `Starter` | `0` to `10,000,000` | `0` to `10,000` | Included under the Additional Use Grant in [LICENSE](LICENSE) |
| `Growth` | `10,000,001` to `100,000,000` | `10,001` to `100,000` | `USD 499 / month` |
| `Scale` | `100,000,001` to `1,000,000,000` | `100,001` to `1,000,000` | `USD 1,999 / month` |
| `Enterprise` | `1,000,000,001` to `10,000,000,000` | `1,000,001` to `10,000,000` | `USD 6,999 / month` |
| `Custom` | Above `10,000,000,000` | Above `10,000,000` | Custom commercial agreement |

Embedded, OEM, and managed-service offerings may require a custom commercial
agreement even when usage is otherwise metered monthly.

### Tier Determination

| Scenario | Applicable Tier |
|----------|-----------------|
| The monthly tier is determined by the higher tier reached by either Billable Tokens or MCP Tool Calls | Highest matching tier across both metrics |
| `8,000,000` Billable Tokens and `25,000` MCP Tool Calls | `Growth` |
| `220,000,000` Billable Tokens and `80,000` MCP Tool Calls | `Scale` |

## Compliance

| Requirement | Rule |
|-------------|------|
| Exceeding free thresholds | You must obtain a commercial license before continuing production use of the Licensed Work |
