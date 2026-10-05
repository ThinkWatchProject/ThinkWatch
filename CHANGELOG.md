# Changelog

All notable changes to ThinkWatch are recorded here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); from `1.0.0`
onwards versioning follows [SemVer](https://semver.org/spec/v2.0.0.html).

Until `1.0.0`, any `0.y` bump may include breaking changes — see the
notes under each release. Operators upgrading inside the `0.x` line
should read the section for every intermediate version, not just the
target.

## [Unreleased]

### Read before upgrading

- **Rate-limit windows start empty.** Rate-limit counters move to new Redis
  keys (one hash per counter, tagged so that Redis Cluster can run them), and
  the counts from before the upgrade are not carried over: every window starts
  empty and fills from the first request after the upgrade. The old keys
  expire by themselves within two window lengths. Budget counters are kept.
- **Route health starts fresh.** A route's samples, circuit breaker and
  lifetime request count move to new keys for the same reason, so every route
  starts closed with nothing counted. The old lifetime counters never expire;
  `redis-cli --scan --pattern 'route_health:[0-9a-f]*' | xargs redis-cli del`
  removes them (the new keys start `route_health:{`).
- **A key's limits no longer replace its owner's.** A rate limit or budget set
  on an API key used to take the place of the owner's limit for the same
  window or period. Both now apply, each on its own counter: a key's limits can
  narrow what its owner may do through that key, never widen it. A key given a
  higher limit than its owner to give it more room needs the owner's limit
  raised instead.

### Fixed

- **Token limits refuse requests.** A `tokens` rate limit never refused
  anything, and stopped counting once a request would have taken it past its
  limit. A request is now refused once the window's recorded usage reaches the
  limit, and every request's tokens are recorded after it, even past the limit
  — a window can overshoot by what was in flight when it filled.
- **Several request limits at once.** With two or more `requests` limits on a
  user (per minute and per hour, say), every request that passed was counted
  twice, and only one of the limits could refuse; with
  `security.rate_limit_fail_closed` on, every request was refused as
  `rate_limiter_unavailable`.
- **An API key's limits count on that key.** They were counted on its owner's
  counter, which every key of the owner shared, and the usage the console
  reads for a key (`/api/admin/limits/api_key/{id}/usage`) was always 0. Each
  key now has counters of its own, rate limits and budgets alike, for the
  gateway and the MCP gateway, and its usage shows what it used.
- **A refused request counts against nothing.** A request refused by a spent
  budget, or by one rate limit after another had passed, was still counted
  against the request limits. Budgets are now checked first and every rate
  limit in one step, so a refused request leaves every counter as it was.
- **`Retry-After` says when to retry.** A `429` from the gateway's own limits
  said `Retry-After: 30` whatever the limit. It now gives the seconds until the
  window has room for another request, or until a spent budget's period ends
  (the next midnight, Monday or 1st of the month, UTC). A spent budget also
  sends `x-should-retry: false`, so the OpenAI and Anthropic SDKs don't retry it
  by themselves. The body stays in the caller's API format.
- **Redis Cluster.** The rate-limit, route-health and quota scripts touched
  keys of several hash slots, which a Redis Cluster refuses: on a cluster, rate
  limits silently stopped applying (or refused every request with
  `security.rate_limit_fail_closed`), circuit breakers never tripped, and cache
  invalidation reached one node only. Every key a script touches now shares a
  hash tag, pattern deletes scan every node, and the Helm chart's README
  describes a `redis-cluster://` URL.
- **Redis over TLS.** The server was built without TLS for Redis: a
  `rediss://` URL was used as plain TCP, so against a Redis that requires
  TLS — ElastiCache with in-transit encryption, Upstash, Azure Cache for
  Redis and Redis Cloud among them — the server did not start
  (`Failed to connect to Redis: Protocol Error: Expected string.`).
  `rediss://` and `rediss-cluster://` URLs now connect over TLS and check
  the certificate against the system's CAs, as upstream HTTPS does. For a
  Redis whose certificate a private CA signed, `REDIS_CA_CERT` names a PEM
  file with that CA, which is then trusted alone; the Helm chart sets it
  from a Secret given in `redis.caSecret`. The chart's README describes
  both.
- **Several instances starting at once.** Server instances starting together
  against one database — a Helm `replicaCount` above 1, a rolling upgrade, an
  autoscaler adding pods — applied the schema side by side, and all but one
  could exit with `Database migration failed: apply db/schema.sql: … deadlock
  detected` (on an empty database: `duplicate key value violates unique
  constraint "pg_extension_name_index"`). With ClickHouse, the rollups that an
  instance fills from the logs when it finds them empty (`cost_rollup_hourly`,
  `provider_health_5m`, `mcp_server_call_counts`) could be filled by each of
  them, counting every request once per instance on the cost pages, the
  dashboard and the MCP server list. Instances now set up Postgres and
  ClickHouse one at a time, under Postgres advisory locks: the others wait,
  logging `Another instance is setting up the database schema; waiting for it
  to finish`, then find it done. An instance that dies holding a lock releases
  it with its connection.
- **Helm network policy and databases on other ports.** With
  `networkPolicy.enabled`, the server could reach PostgreSQL only on `5432`,
  Redis on `6379` and ClickHouse on `8123`, whatever their `externalUrl` said,
  so a database on another port was blocked — Azure Cache for Redis over TLS
  (`6380`), ClickHouse Cloud (`8443`), a managed Postgres on a port of its own:
  the server could not start, or started without writing to ClickHouse. The
  allowed ports now follow `postgres.externalUrl`, `redis.externalUrl` and
  `clickhouse.externalUrl`: every port a URL names, and the client's default
  for its scheme where it names none. `networkPolicy.extraEgress` adds egress
  rules as written, for ports no URL names (Redis Cluster nodes announcing
  other ports, an upstream or MCP server on a port other than `443`). The
  chart's README describes both.

## [3.1.0] — 2026-10-05

The thinkwatch-core crates move from v0.59.0 to v0.62.0. Two changes reach
the gateway: tool-call inspection gains a built-in rule for ThinkWatch's own
data directory, and an upstream whose base URL already ends in an API
version is no longer sent a second one.

### Read before upgrading

- The new tool-call rule `thinkwatch-data` is on and set to cut off, like
  the other high-risk built-in rules. A deployment whose tool-call
  inspection is in enforce mode starts cutting off answers whose tool call
  reads or changes one of the paths below; one in observe mode only records
  them. Set the rule to record, or switch it off, on the security page to
  keep the previous behaviour.

### Added

- **Tool-call inspection: `thinkwatch-data` (Read or change ThinkWatch's own
  data).** A tool call whose path or command points into ThinkWatch's data
  directory (`~/.thinkwatch`, `%APPDATA%\ThinkWatch`, `/var/lib/thinkwatch`,
  `/etc/thinkwatch`) — where the desktop gateway keeps every upstream key and
  its own protection settings. Text that only mentions the directory, such as
  a document being edited, does not count. The security page lists it with
  the other built-in rules.

### Fixed

- **Upstream base URLs that end in their own API version** (`…/api/paas/v4`,
  `…/api/v3`) now receive requests under that version instead of a second
  `/v1` appended to it, which returned 404. Requests and connection tests
  both use it.

### Changed

- thinkwatch-core crates (tw-bedrock, tw-breaker, tw-dialect, tw-guard)
  v0.59.0 → v0.62.0.

## [3.0.0] — 2026-10-03

The request guards — outbound redaction, tool-call inspection and the
content filter — now share their rule model with the desktop gateway:
the same policy shape, built-in rule catalog, validation, rule view and
sample trial, from thinkwatch-core. Each guard has three modes (off,
observe, and one named for what it does: replace, cut off, enforce), and
the content filter can delete what a rule matches as well as refuse or
record it, and match by code point. Hidden characters become content
filter rules, and the per-model output length guardrail becomes a cap on
the output tokens a request may ask for. Settings saved by an earlier
version are converted at the first start: read the first section before
deploying. It is a major release because setting keys, console API
routes and the `models` table change; the conversion keeps each
deployment's behaviour.

### Read before upgrading

- **Back up `system_settings` and `models` first, and plan for no way
  back to 2.2.** The upgrade converts the guard settings and drops a
  column (next item). Version 2.2 cannot run on the converted database:
  its settings are gone, so it would start on its seeded defaults, and
  it can no longer read the models table. Take a copy before deploying:

  ```sh
  pg_dump --data-only --table=system_settings --table=models "$DATABASE_URL" > thinkwatch-2.2-guards.sql
  ```

- **Stop every replica of 2.2 before the first 3.0 one starts.** A 2.2
  replica still running when 3.0 converts finds its settings gone (it
  then filters and redacts nothing) and cannot rebuild its router; one
  that restarts writes its seeded defaults back. 3.0 does not convert
  those a second time — it removes them at its next start and logs a
  warning — but the 2.2 replica runs on them until then. With the Helm
  chart (release `thinkwatch`, namespace `thinkwatch` here):

  ```sh
  kubectl -n thinkwatch scale deployment/thinkwatch-server --replicas=0
  kubectl -n thinkwatch wait --for=delete pod \
    -l app.kubernetes.io/name=think-watch,app.kubernetes.io/component=server --timeout=5m
  helm upgrade thinkwatch deploy/helm/think-watch -n thinkwatch   # with your usual values
  kubectl -n thinkwatch scale deployment/thinkwatch-server --replicas=<replicas you run>
  ```

  The last command matters when `autoscaling.enabled` is on: the chart
  then leaves the replica count alone, and it stays at 0. With Docker
  Compose, `docker compose stop server` before pulling and starting the
  new image.
- **The old guard settings are converted at the first start, and
  behave as before.** `security.content_filter_patterns`,
  `security.hidden_text`, `security.pii_redactor_patterns` and
  `security.tool_inspection` become `security.content`,
  `security.redact` and `security.inspect_tools`, and are deleted, in
  one transaction during the boot migration. The conversion is recorded
  in `security.legacy_converted` (when, which version, what it
  converted); with that record present, old keys or the old column
  that show up again are removed, never converted, so the policies in
  force are not overwritten. A content rule identical to a built-in
  rule becomes that rule, switched on, any other a custom rule; a list
  with rules in it runs in enforce mode with every built-in rule it did
  not name switched off. The four seeded PII patterns become the
  built-in rules for the same data (`cn-resident-id`, `bank-card`,
  `email`, `cn-mobile-phone`), any other pattern a custom rule whose
  label is its old placeholder prefix. Whatever the old runtime was
  skipping (a rule that did not compile, a built-in rule id it did not
  know, an `output_guardrails` value it could not read) is left out,
  each with a warning in the start-up log. A model's
  `output_guardrails` length cap becomes `max_output_tokens` (below),
  and the column is dropped. To see what was converted, read the three
  keys from Settings or `system_settings` afterwards; the start-up log
  lists them too.
- **Placeholders are written `<<TW_EMAIL_1>>`, not `{{EMAIL_1}}`.** The
  label of a custom rule is upper case letters, digits and
  underscores (an old prefix is converted: `REDACTED-SSN` →
  `REDACTED_SSN`); the built-in identity number and bank card rules
  use `ID_NUMBER` and `CARD_NUMBER`. Anything that looked for the old
  form in answers or logs needs the new one.
- **Redaction searches the whole request**, not only the user's
  messages: the system prompt, earlier answers and tool-call arguments
  are redacted too. Base64 payloads (images, files, signatures) are
  still left alone. Rules run on the request as it is sent, as JSON,
  where a custom pattern's match ends at a quote or a backslash: a
  pattern written to match across a `"` in the decoded text needs
  rewriting.
- **Built-in credential rules start replacing on deployments that were
  redacting.** API keys and tokens with a known prefix, private keys,
  JWTs and connection-string passwords are built-in rules that ship
  switched on. A deployment whose PII list had patterns in it runs
  redaction in enforce mode after the upgrade, so these values are now
  replaced as well. Switch the ones you do not want off on the
  console's security page. With an empty PII list, redaction
  converts to observe mode: it records what it finds and changes
  nothing.
- **The built-in rules that replace the seeded PII patterns are
  stricter.** An identity number has to have a real province code, a
  real date of birth and a matching check digit; a card number a known
  network's prefix and length and a valid Luhn digit, and published
  test card numbers do not count. Numbers the old regexes took for
  them — any 18 digits, any 16 — are no longer replaced. A deployment
  that relied on the looser match can add its old regex back as a
  custom rule.
- **Tool calls are judged as the client receives them, and two built-in
  rules are new.** Inspection now reads a tool call converted to the
  caller's format and with redacted values restored — what the client
  would run — where it used to read the placeholders. The new
  `secret-to-unknown-host` rule cuts (in enforce mode) a call that sends
  a recognised API key or private key to a host that is neither local
  nor the key's own provider; `upload-file-to-host` records a call that
  uploads a local file to an outside host. A deployment running
  tool-call inspection in enforce mode starts cutting the first; add it
  to `disable` if that is not wanted.
- **"Warn" and "log" are one action now, "record only"**, and hidden
  characters are content filter rules: `unicode-tags` and
  `bidi-controls`, plus `zero-width` and `private-use`, which ship off.
  `security.hidden_text: block` converts to those two rules refusing,
  `warn` and `log` to recording, `off` to switching them off. Recording
  writes an audit event: `hidden_text: log` used to reach only the
  application log, and now writes `gateway.content_flagged`.
- **A deployment with no content rules starts recording.** An empty
  content filter list converts to observe mode with the built-in rules
  that ship on, so requests matching them (`ignore previous
  instructions` and the like) write `gateway.content_flagged` events
  where 2.2 wrote nothing. Nothing on the wire changes.
- **The output length guardrail is replaced by a model's maximum output
  tokens — check each model's after upgrading.** A cap of N bytes on
  the answer converts to `ceil(N / 4)` output tokens, stored as
  converted. The answer is no longer measured or cut: a request asking
  for more tokens than the cap is lowered to it, in whichever field its
  API uses (a Chat request that sets both `max_tokens` and
  `max_completion_tokens` has both lowered), and the upstream stops
  there; one asking for less keeps its own. A request that sets no limit
  is held to the cap only when the cap is within what the gateway knows
  the model's family to take (32,000 tokens for Claude models, 8,192 for
  others); a Chat request is then given `max_completion_tokens` on
  OpenAI's own endpoint, whose reasoning models refuse `max_tokens`, and
  `max_tokens` elsewhere. Above that figure, the request goes out
  without a limit rather than with one the model could refuse, and the
  model's own default applies: a 100,000-byte cap on a non-Claude model
  converts to 25,000 tokens, so its requests that set no limit are not
  capped at all. To hold every request to a cap, have the clients send
  a limit, or set the cap to that family figure or below. Reasoning
  (thinking) tokens count towards the cap on the APIs that bill them as
  output, so a cap that fit an answer can cut short a model that thinks
  first. An Anthropic request with extended thinking has its thinking
  budget lowered below the cap too, or thinking turned off when the cap
  is 1,024 tokens or less, the smallest budget Anthropic takes. The
  model API's `output_guardrails` field is gone: a request that still
  sets one (anything but `null` or `[]`) is refused with `400`, so a
  script cannot believe answers are still capped. `max_output_tokens`
  (1 to 2147483647, `null` for no limit) replaces it.
- **A new installation observes by default.** Every guard starts in
  observe mode, with only the built-in rules that rarely misfire
  switched on (personal data such as e-mail addresses and phone
  numbers ships off). Nothing is refused, replaced or deleted until a
  guard is switched to its third mode.
- **A content filter refusal is `403`**, with the error type of the
  caller's API (`permission_error` for OpenAI-style APIs). Keyword and
  regex rules used to refuse with `400`. In `gateway_logs`, its
  `error_type` is `PolicyBlocked`, where it was `TransformError`.
- **A rule that deletes text changes what is stored.** The request a
  content rule stripped goes upstream, is redacted and is captured as
  the stripped one: the audit log's request body is what was sent, not
  what the caller typed.
- **Changing a guard policy takes `settings:write` and the guard's own
  permission**: `pii_redactor:write` for `security.redact`,
  `content_filter:write` for `security.content` and
  `security.inspect_tools`, through `PATCH /api/admin/settings`. 2.2
  checked `settings:write` on the server and the guard permission in
  the console; both are checked on the server now. Trying a sample
  takes `pii_redactor:read` or `content_filter:read`. Reading the
  policies — `GET /api/admin/security`, which the console's security
  page loads — takes `settings:read`. The seeded `admin` and
  `super_admin` roles hold all of them.
- **Console API changes.** `GET /api/admin/security` lists each guard's
  mode and every rule, and `POST /api/admin/security/{guard}/test` tries
  a sample; they replace `/api/admin/settings/content-filter/test`,
  `/content-filter/presets`, `/pii-redactor/test`,
  `/tool-inspection/rules` and `/tool-inspection/test`, which are gone.
- **Audit events.** Every rule that matches writes one event per
  request: `gateway.content_flagged`, `gateway.content_stripped` and
  `gateway.content_blocked`; `gateway.redaction_flagged` and
  `gateway.redaction_replaced`; `gateway.tool_call_flagged` and
  `gateway.tool_call_blocked` as before. `gateway.hidden_text_flagged`
  and `gateway.hidden_text_blocked` are gone; hidden characters are
  content events. **No event carries the request's text**: a content
  event names the rule, the outcome, how many matches and whether they
  were in a tool result; a redaction event names the rule and counts
  the values and their occurrences, and only a built-in rule's lists a
  few in masked form (`sk-an…7f9c`) — a custom rule's values are not
  written at all. A request writes at most 20 events per guard (the
  rules that changed it, or matched most, first), each with the number
  of rules that matched (`rules_in_request`). With
  `audit.body_redact_pii` on, captured bodies are redacted with the
  outbound redaction rules, built-in ones included, whatever the
  redaction mode.
- **Metrics renamed.** `gateway_hidden_text_total` is gone: hidden
  characters count in `gateway_content_matched_total{outcome,custom}`
  with every content rule. `pii_pattern_invalid_total{pattern}` is now
  `guard_policy_invalid_total{guard}` (a stored policy with a rule that
  does not compile; the rule is left out), next to
  `guard_policy_unreadable_total{guard}` (a stored policy that cannot be
  read; the guard runs on its factory policy). Redaction counts values
  in `gateway_redaction_found_total{kind,outcome}`.

### Added

- **Deleting what a content rule matches.** A content rule can refuse
  the request, delete the matched text from the caller's messages and
  tool results and send the rest, or only record. Text deleted joins
  back what it separated, so the request is checked again afterwards.
- **Code point rules.** A content rule can match characters by code
  point (`U+200B`, `U+E0000–U+E007F`), for invisible characters a
  keyword cannot be written for.
- **Every rule visible and switchable**, built-in and custom, in each
  guard, with what it does in the third mode and what it did out of the
  box; a sample can be tried against one rule, an unsaved one, or all
  of them.

### Changed

- **Core crates at ThinkWatch-Core v0.59.0.** `tw-dialect`, `tw-guard`,
  `tw-breaker` and `tw-bedrock` move from v0.55.0; the shared guard model
  described above comes with them.

### Fixed

- **A credential in a matched tool call no longer reaches the audit
  log.** The excerpt of a tool call that inspection cut or recorded is
  masked with the redaction rules before it is written; 2.2 stored the
  matched arguments as they were, a key the model wrote in them
  included.
- **A request's audit events and its log row carry the same id** when
  the caller sends no `x-trace-id`. The log row of a request that went
  through used to carry a second, unrelated id.

## [2.2.0] — 2026-10-01

This release fixes authorization. The gateways never checked
`ai_gateway:use` or `mcp_gateway:use`, so a user with no role, or with
only roles that grant neither, could call every model and MCP tool; a
role granted at the scope of one team widened model and tool access
platform-wide; and an API key whose allow-list came out empty could
call any model. The gateways now require the permission and count only
the roles that grant it. The seeded `team_manager` role now works when
granted at team scope, and seven permissions that nothing ever checked
are retired. Some users lose gateway access on upgrade: read the first
section before deploying. There is one manual database script and no
schema, setting, environment variable or Helm value change.

### Read before upgrading

- **Users without a role that grants gateway use lose gateway access.**
  A request to the AI gateway is refused with `403` unless a role held
  by the key's owner grants `ai_gateway:use`, and a request to the MCP
  gateway unless one grants `mcp_gateway:use`. The body names the
  missing permission, in the error format of the caller's API. The
  built-in `developer`, `admin`, `super_admin` and `team_manager` roles
  grant both; `viewer` grants neither. Three groups of users are
  refused after the upgrade where 2.1.0 let them through:
  - users with no role at all;
  - users whose roles are only `viewer`, or custom roles without these
    actions;
  - users whose only gateway role is assigned at team scope (see the
    third item below).

  Find them before upgrading. This query lists every active user with a
  live key for a gateway that none of their roles will grant (it counts
  global assignments and roles attached to the user's teams, which is
  what the gateways now read; it does not account for `Deny`
  statements):

  ```sql
  WITH grants AS (
    SELECT id AS role_id,
           jsonb_path_exists(policy_document,
             '$.Statement[*] ? (@.Effect == "Allow").Action[*] ? (@ == "*" || @ == "ai_gateway:*" || @ == "ai_gateway:use")') AS ai,
           jsonb_path_exists(policy_document,
             '$.Statement[*] ? (@.Effect == "Allow").Action[*] ? (@ == "*" || @ == "mcp_gateway:*" || @ == "mcp_gateway:use")') AS mcp
      FROM rbac_roles
  ),
  held AS (
    SELECT user_id, role_id FROM rbac_role_assignments WHERE scope_kind = 'global'
    UNION
    SELECT tm.user_id, tra.role_id
      FROM team_members tm JOIN team_role_assignments tra USING (team_id)
  ),
  access AS (
    SELECT h.user_id, bool_or(g.ai) AS ai, bool_or(g.mcp) AS mcp
      FROM held h JOIN grants g USING (role_id) GROUP BY h.user_id
  )
  SELECT u.email, s.surface
    FROM api_keys k
    JOIN users u ON u.id = k.user_id
    CROSS JOIN LATERAL unnest(k.surfaces) AS s(surface)
    LEFT JOIN access a ON a.user_id = u.id
   WHERE k.is_active AND k.deleted_at IS NULL
     AND u.is_active AND u.deleted_at IS NULL
     AND NOT COALESCE(CASE s.surface WHEN 'ai_gateway' THEN a.ai
                                     WHEN 'mcp_gateway' THEN a.mcp END, false)
   GROUP BY u.email, s.surface
   ORDER BY u.email, s.surface;
  ```

  Give each of them `developer` (or a custom role with the actions)
  either at global scope (Users → edit the user's roles, scope
  *Global*), or by attaching the role to a team they belong to (Teams →
  the team → Roles), which every member of that team inherits. The
  change takes effect within a minute; each user's permissions are
  cached for 60 seconds.

  If SSO users are meant to use the gateway from their first sign-in,
  set *Default Role for New Users* (`auth.default_role`) in Settings to
  `developer`. It is empty by default, and it applies only to accounts
  created after it is set; existing users need the grant above.
- **A role that does not grant gateway use no longer widens model or
  tool access.** Model and MCP tool scopes are now the union over the
  roles that grant `ai_gateway:use` / `mcp_gateway:use` only. A role
  without those actions (such as `viewer`) used to count as
  "unrestricted", so adding it to a user limited to some models opened
  every model to them. Users relying on that lose the extra models.
- **A role assigned at team scope no longer grants gateway access.** An
  assignment with scope `team:<id>` administers that team from the
  console (team roster, team limits); it no longer contributes models,
  MCP tools or gateway use, which it used to do for every request the
  user made, member of the team or not. Roles attached to a team itself
  (Teams → Roles), which every member inherits, still count. A user
  whose only gateway role was a team-scoped `developer` or
  `team_manager` needs that role at global scope, or attached to their
  team. The role assignment editor now says this under the scope
  picker.
- **An empty model allow-list allows nothing.** A key whose
  `allowed_models` is `[]`, or whose list shares no model with what its
  owner's roles grant, used to call any model; it now calls none, as
  `allowed_mcp_tools: []` already did on the MCP gateway. The console
  never saves `[]` (clearing the picker sends `null`), so only keys
  written through the API are affected. To find them:
  `SELECT id, name, user_id FROM api_keys WHERE allowed_models = '{}' AND is_active AND deleted_at IS NULL;`
  Set such a key's list to `null` to leave it bounded by its owner's
  roles only. Model entries still match by prefix, and a key narrowed to
  `gpt-4o-mini` under a role granting `gpt-4o` keeps `gpt-4o-mini`.
- **Run `db/release_migrations/2026-09-30_retire_unchecked_permissions.sql`
  once, after deploying.** The server does not apply it. It adds
  `teams:read` to the `team_manager` role and removes the seven retired
  permissions (see *Changed*) from every role, system and custom. It is
  idempotent and changes no access for any other role, since nothing
  checked the removed keys. Without it, the server still starts and
  works, but:
  - `team_manager` keeps the old `team:read` it cannot use, so a team
    manager who is not a member of the team they manage still gets
    `403` opening it, its roster or its roles;
  - every start logs a warning listing the roles that still name
    retired permissions.

  *Reset to defaults* on a system role in the console has the same
  effect for that one role, but does not clean custom roles.
- **A team-scoped `rate_limits:write` now takes effect.** It used to
  require global scope for every subject, so the seeded `team_manager`
  granted at team scope could not touch any limit. It now covers the
  rate limits and budget caps of users in that team and of the API keys
  they own, never the holder's own user or keys; role subjects still
  need global scope. Anyone holding `team_manager` (or a custom role
  with `rate_limits:write`) at team scope can now change, lift or
  delete the limits of every member of that team, administrators
  included. Review team-scoped assignments of these roles before
  upgrading.

### Security

- **Gateway use is checked, and a missing grant means no access.** See
  the first three items above: a user with no role, or only roles
  without gateway use, could call every model and MCP tool; a
  `viewer`-style role widened a restricted user to every model; a role
  granted at the scope of any team, even one the user was not a member
  of, widened model and tool access platform-wide. An explicit `Deny`
  on `ai_gateway:use` or `mcp_gateway:use` now closes that gateway.
  Action wildcards (`ai_gateway:*`, `*`) grant it, as they already did
  for console permissions.
- **A key narrowed inside a prefix grant is no longer unrestricted.**
  The key's allow-list was intersected with its owner's role grants
  entry by entry, as literal strings. A key limited to `gpt-4o-mini`
  under a role granting `gpt-4o` (a prefix) came out with an empty list,
  which the gateway read as "no restriction", so the key could call
  every model. The intersection now keeps an entry of either side that
  the other side covers, by prefix for models and by `<server>__*`
  pattern for MCP tools, and an empty result allows nothing.

### Fixed

- **The seeded `team_manager` role works at team scope.** It granted
  `team:read`, but the team handlers check `teams:read`, so a team
  manager got `403` listing teams and opening the team, its roster or
  its roles. It now grants `teams:read`, and opening a team accepts
  `teams:read` scoped to that team, where it used to require global
  scope. Existing installations need the release migration above.
- **Bulk disable and delete work on API keys' limits.** The bulk
  disable and delete routes for rate-limit rules and budget caps
  rejected every row stored for an API key (`api_key_lineage`, the kind
  a key's limits are stored under) as an unknown subject kind.
- **The console's effective-permissions preview shows real gateway
  access.** It now counts only roles that grant gateway use and skips
  team-scoped assignments, as the gateways do, where it used to count
  every assigned role.

### Changed

- **Seven permissions that nothing checked are retired**: `team:read`,
  `team:write`, `logs:read_own`, `logs:read_team`,
  `audit_logs:read_own`, `audit_logs:read_team` and
  `audit_logs:read_all`. Every log endpoint, audit logs included, is
  gated on `logs:read_all` at global scope, and no own- or team-filtered
  log view exists, so these grants never did anything. They are gone
  from the permission catalog, the role editor and the seeded roles.
  Roles that still name them load, and the server logs a warning at
  start instead of refusing to boot.
- **Deleting one rate-limit rule or budget cap is bound to the subject
  in the path.** `DELETE` on
  `/api/admin/limits/{kind}/{id}/rules/{rule_id}` and
  `/api/admin/limits/{kind}/{id}/budgets/{cap_id}` now answers `404` when the rule or cap belongs to a different
  subject; it used to delete any row id once the path's subject was
  authorized.
- **Creating or editing a key with an explicit allow-list checks it
  against gateway-granting roles only.** An `allowed_models` or
  `allowed_mcp_tools` entry is refused with `400` when no role of the
  owner that grants the gateway covers it, so a user without a gateway
  role can no longer save a non-empty list.

## [2.1.0] — 2026-09-30

Amazon Bedrock becomes a provider you can run from the console. It
authenticates with a Bedrock API key as well as access keys or the
instance role, lists its models for import and for the route editor,
and has a working Test Connection. Requests converted for Bedrock now
keep their prompt-cache breakpoints and Claude's thinking settings. The
route editor takes any upstream model name, and a route the provider
refused can be created anyway. ThinkWatch-Core moves to v0.55.0, whose
Bedrock layer this edition now shares with the desktop gateway. No
database, setting, environment variable or Helm value changes.

### Read before upgrading

- **Test Connection with a saved provider's secrets needs
  `providers:update`.** A test that names a saved provider
  (`provider_id`, which is what the Edit dialog sends) used to take only
  `providers:create`. A custom role that has `providers:create` without
  `providers:update` can no longer test existing providers; the built-in
  admin role has both. A test with values typed into the request still
  takes `providers:create`. See *Security* below.
- **Refusing a route the provider does not serve answers
  `model_not_served`.** `POST /api/admin/models/{model_id}/routes` still
  answers `400` when the import probe found no API the provider serves
  the model on, but `error.type` is now `model_not_served`, where it was
  `bad_request`. Scripts that matched on `bad_request` for this case need
  the new value. The request takes a new optional `"force": true` to
  create the route anyway.
- **`error_type` in `gateway_logs` for failed requests is always the
  error's tag.** An upstream HTTP error used to log its debug text, such
  as `ProviderHttpError { status: 502, message: "…" }`, and an upstream
  rate limit a cut-off `UpstreamRateLimited { retry_after_secs: Some`.
  They now log `ProviderHttpError` and `UpstreamRateLimited`, as the
  metric labels and streamed requests already did. Dashboards or log
  forwarder queries that grouped by those strings see them merge into
  one value each.
- **The server log now carries the upstream's reply to a 401 or 403**,
  as it already did for other upstream errors. For Bedrock that reply
  names the AWS account and the IAM principal. Callers still see only
  "Authentication failed with upstream", and failover, breakers and
  `gateway_logs` treat these errors as before.
- **Prompt caching now works on Bedrock routes, and is billed at cache
  prices.** Requests converted for Bedrock used to lose their
  `cache_control` breakpoints, so Claude on Bedrock re-read the whole
  prompt at full price every turn. Breakpoints now go out as Converse
  `cachePoint` blocks, to Claude and Nova models only (others reject
  them), and the cache reads and writes Bedrock reports come back into
  usage, one-hour writes included. Traffic with repeated prompts, such as
  Claude Code, costs much less on Bedrock routes than it did; writes cost
  a little more, at `cache_write_weight` / `cache_write_1h_weight`.
- **Claude's thinking reaches Bedrock.** A request converted for Claude
  on Bedrock now carries its thinking and effort settings, in the form
  Anthropic's API uses, along with the sampling limits thinking imposes
  (`temperature` 1, no `top_k`, `top_p` at least 0.95). Thinking used to
  be dropped on the way to Bedrock. Other Bedrock models still get no
  thinking.

### Added

- **Bedrock providers can authenticate with a Bedrock API key.** Pick
  *Bedrock API Key* as the authentication mode and paste the key AWS
  generated. It is kept like any other provider's API key, as an
  encrypted `Authorization: Bearer` header, and replaced in the
  provider's Edit dialog. A Bedrock provider that sends an
  `Authorization` header is not SigV4-signed; one without it is signed
  as before, with access keys or the instance role. Use a long-term key:
  a short-term one expires within 12 hours.
- **Bedrock models can be imported, and are suggested in the route
  editor.** A Bedrock provider now lists what it can be routed to: the
  region's foundation models that can be invoked on demand and answer in
  text, and the inference profiles AWS defines, such as
  `us.anthropic.claude-sonnet-4-5-20250929-v1:0`. A model that is only
  served through an inference profile, as most current models are, is
  listed under its profiles' ids and not its own. Whether the account
  may call a model is still checked one model at a time when it is
  imported. The provider's credential needs
  `bedrock:ListFoundationModels` and `bedrock:ListInferenceProfiles`;
  the `AmazonBedrockLimitedAccess` policy a long-term API key is created
  with allows both.
- **Test Connection for Bedrock providers**, however they authenticate:
  with an API key, access keys or the instance role. It lists the models
  above, so a wrong credential or a missing permission shows up before
  any traffic does.
- **Create anyway, for a route the provider refused.** When the route
  editor's save is refused because the provider does not serve the
  model, the error now offers *Create anyway*: the refusal can be
  stale, or be about the probe's request rather than the model. The
  route is created, and its `model_route.created` audit row records the
  refusal it overrode in a new `refusal_overridden` key.

### Changed

- **The route editor's upstream model field takes any model name.** For
  a provider that lists its models, the field used to turn into a list
  to pick from, so a model the listing leaves out could not be routed to
  from the console. It now suggests the provider's models as you type,
  and takes whatever is typed.
- **Bedrock instance-role credentials are cached.** They took three
  IMDSv2 round trips per request; they are now kept until five minutes
  before they expire, and a burst of requests at expiry makes one trip.
- **Chat Completions requests can switch reasoning with `thinking`.**
  When a request is converted for another format, `thinking.type`
  (`enabled` / `disabled`, as DeepSeek, GLM and Kimi write it) now turns
  reasoning on or off; `disabled` wins over `reasoning_effort`.
- **Core crates at ThinkWatch-Core v0.55.0.** `tw-dialect`, `tw-guard`
  and `tw-breaker` move from v0.43.0, and `tw-bedrock` joins them:
  SigV4 signing, eventstream unframing, Bedrock's addresses, region
  checks and model catalog now come from core. Where credentials come
  from (provider keys, the instance role) stays in this repository.
  `tw-breaker` is unchanged.

### Fixed

- **Bedrock providers could not be created or edited in the console.**
  The region was checked as a URL, so saving failed with
  `400 Invalid URL`. A Bedrock provider's `base_url` is now checked as an
  AWS region such as `us-east-1`, and anything else is refused, since the
  host is built from it. A provider saved with a URL there never reached
  Bedrock; set its region in the Edit dialog.
- **Bedrock models the account may not call were imported anyway.**
  Bedrock refuses such a model (model access not granted, or an IAM or
  organization policy that denies it) with a 403, and the import probe
  read every 403 as a credential problem that says nothing about the
  model. The route was created and failed on first use. Now, when the
  region's control plane accepts the same credential, the refusal is
  recorded as the model's, with AWS's reason, and the model is skipped
  on import like any other refused one. Once access is granted,
  re-check the provider's models.
- **A Bedrock provider whose stored secret key would not decrypt signed
  with an empty secret**, and every request failed with AWS's
  `SignatureDoesNotMatch`. It now refuses its requests with the reason
  until the keys are saved again, rather than falling back to the
  instance role, which would call AWS as a different identity. An
  access key ID saved without a secret is refused the same way.
- **A Bedrock model id that is an ARN could not be routed.** Its `/`
  went into the request path unescaped, adding a path segment AWS could
  not route. It is now escaped.
- **A base URL ending in its version segment doubled it.** A provider
  written as `https://api.openai.com/v1` sent requests, and the model
  listing, to `/v1/v1/…` and got 404s. A trailing version segment
  (`v1`, `v1beta`, …) that the request path starts with is now written
  once. Base URLs without one are unchanged.
- **Typing into a provider's API key field and clearing it again wiped
  the saved key on save.** The field sent `Bearer ` with nothing after
  it. A cleared field now keeps the saved key, as a blank one always did.

### Security

- **Testing a connection with a saved provider's secrets takes
  `providers:update`.** The test fills each header left blank with the
  saved provider's secret and sends it to the URL in the request, and
  `providers:create` was enough to ask for it. So a user allowed only to
  create providers could send any saved API key to a server of their
  own. A test that names a saved provider (`provider_id`) now takes
  `providers:update`, which lets a user point that provider elsewhere
  anyway; a test without one still takes `providers:create`. A user with
  `providers:update` alone can now use Test Connection in the Edit
  dialog, which used to answer `403`.

## [2.0.0] — 2026-09-24

Callers now get errors in their own API's format, and an upstream that
refuses a request no longer takes a model's other routes down with it.
The gateway also speaks two more client protocols: Gemini, and the
Responses API over a WebSocket. Cached input is billed at cache prices,
and a request with no usage report is billed on an estimate instead of
at zero. The TOTP requirement, which never took effect before, is now
enforced. This is a major release because error bodies, the
content-filter preset ids and the TOTP behaviour all change in ways a
client or a script can notice.

### Read before upgrading

- **Check `security.totp_required` before you upgrade.** In 1.x this
  setting never took effect: it is stored as a boolean and was read as a
  string, so it always read as off. From 2.0.0 it is enforced by the
  server. Find out what it is set to:

  ```sql
  SELECT value FROM system_settings WHERE key = 'security.totp_required';
  ```

  If it is `true`, every console user without TOTP, super admins
  included, is held at a TOTP setup screen on their next request, and
  sessions that are already open are held too. Until they set up TOTP,
  every console and admin endpoint answers 403
  `totp_enrollment_required`, except `/api/auth/me`, logout,
  `register-key` and the TOTP status/setup/verify-setup calls. Setting
  up TOTP releases the session straight away. API keys are not affected:
  gateway, MCP and console `tw-` key traffic keeps working. While the
  setting is on, `POST /api/auth/totp/disable` is refused with 400. The
  setting must now be a JSON boolean; a string such as `"true"` is
  refused on save.
- **Gateway error bodies follow each client API's own format.** Status
  codes and `Retry-After` are unchanged. Code that reads the error
  `type` needs updating:
  - **Chat Completions and Responses** keep the
    `{"error": {"message", "type", …}}` shape, but `type` is now
    OpenAI's value for the status, not a ThinkWatch tag:
    `authentication_error` (401), `permission_error` (403),
    `not_found_error` (404), `rate_limit_error` (429),
    `invalid_request_error` (other 4xx) and `server_error` (5xx). The
    old tags are gone: `rate_limited`, `policy_blocked`,
    `provider_http_error`, `provider_error`, `provider_timeout`,
    `transform_error`, `network_error` and `auth_error`. A policy block
    is now `permission_error` with status 403.
  - **Anthropic Messages** clients get Anthropic's body,
    `{"type": "error", "error": {"type", "message"}}`, with Anthropic's
    type names (`rate_limit_error`, `overloaded_error`, `api_error`, …).
  - **Gemini** clients get Google's body,
    `{"error": {"code", "message", "status"}}`.
  - **Once a stream has started**, a Responses client gets a
    `response.failed` event, where before it got a Chat-style error
    frame that SDKs skip, so the stream just stopped. An Anthropic
    client gets an `error` event whose type follows the status.
  - The `error_type` field in `gateway_logs` and the metric labels are
    unchanged.
- **Only upstream failures fail over or count against a route's
  breaker.** In 1.x every non-2xx except 401, 403 and 429 was retried on
  the model's other routes and counted as a failure on each of them, so
  one malformed request could open the breakers on all of a model's
  routes.
  - **Tried on another route and counted against this one:** 5xx, 408,
    429, 401 and 403 (the upstream refused the gateway's own
    credential), timeouts, network errors and unreadable responses.
  - **Returned to the caller straight away, and counted as the upstream
    working:** every other 4xx.
  - **What the caller sees:** such a 4xx comes back with its own status
    and the upstream's reason. In 1.x it came back as a 502 after every
    route had been tried.
  - An upstream 5xx comes back with the upstream's status (500, 503, …)
    rather than a blanket 502.
  - An upstream timeout is now 504.
  - Streams follow the same rule. A stream cut by tool-call inspection
    no longer counts against the route.
- **Cached input is billed at cache prices.** In 1.x cache reads and
  writes were billed, and debited from budgets and weighted rate limits,
  as full-price input. `models` gains three weights, `cache_read_weight`,
  `cache_write_weight` and `cache_write_1h_weight`. When a weight is
  unset, it is `input_weight` times Anthropic's ratio: 0.1× for a read,
  1.25× for a write and 2× for a one-hour write. What this changes:
  - Traffic with many cache reads (Claude Code, for instance) costs much
    less than it did.
  - Traffic that writes to the cache costs a little more.
  - Older OpenAI models discount cache reads less (0.5× or 0.25×). Set
    the weights on those models yourself.
  - `input_tokens` in the log is still the whole input. The log detail
    gains `cache_read_tokens`, `cache_write_tokens` and `cache_write_1h`.
- **Output length limits now apply to streams.** In 1.x, `max_length`
  output guardrails checked only whole responses, so streamed answers
  were never checked. Now the frame that would cross the limit is not
  sent, and the stream ends with an error in the caller's format. A
  response served from the cache is also checked against the limit in
  force. If you set a limit, streamed answers that used to go through
  can now be cut off.
- **Content-filter preset groups are renamed.** The groups are now
  `injection`, `persona` and `chinese`; they used to be `basic`,
  `strict` and `chinese`. This matters only if you call the presets
  endpoint by group id. Rules you have already added are copies and are
  not affected. Other changes to the filter:
  - A rule with an empty pattern is now refused on save.
  - Each text part of a message is scanned separately, so a pattern no
    longer matches across two parts.
  - The engine is now shared with ThinkWatch-Core's `tw-guard`. The
    stored format and the admin API are unchanged.
- **Requests with no usage report are billed on an estimate.** In 1.x
  such a request was billed at zero. This happens when an upstream
  ignores the request for usage, or when the caller leaves before the
  final chunk arrives. The estimate is:
  - input: about four bytes of the request per token, not counting
    images and files;
  - output: the answer that actually arrived.

  Estimated rows carry `usage_estimated: true` in their detail and count
  in `gateway_usage_estimated_total`. A request with no answer at all is
  still billed at zero.
- **Clients that leave early are now logged.** In 1.x a client that
  disconnected before its response existed left no `gateway_logs` row at
  all. That covers leaving during auth, limits or routing, or while
  waiting for a whole (not streamed) answer. Such a request now writes
  one row: status 499, `stream_outcome: client_cancelled`,
  `cancelled_before: response`, no tokens and no cost. Expect more 499
  rows in dashboards and log forwarders. A new counter,
  `gateway_cancelled_before_response_total`, counts them.

### Database changes

Both apply on their own at startup, as every schema change does, and
both are additive:

- `models` gains three nullable columns: `cache_read_weight`,
  `cache_write_weight` and `cache_write_1h_weight`
  (`ALTER TABLE … ADD COLUMN IF NOT EXISTS`, `CHECK (>= 0)`).
- `system_settings` gets an `auth.default_role` row, seeded empty (no
  role). Existing rows are left alone (`ON CONFLICT DO NOTHING`).

Neither is irreversible. A 1.1.0 server runs against the upgraded
database: it ignores the new columns and the new setting. What a 1.1.0
server cannot do is price cache tokens from the weights.

### Added

- **Gemini clients.** New endpoints:
  - `POST /v1beta/models/{model}:generateContent` and
    `:streamGenerateContent`, also served under `/v1/models/…`;
  - `GET /v1beta/models`, which lists models in Gemini's format.

  These requests get the same limits, budgets, filters, routing with
  failover, format conversion, inspection, billing and audit as every
  other endpoint. A Gemini upstream gets the request as it was sent.
  A stream comes back as SSE with `alt=sse`, and as Gemini's JSON array
  without it. `:countTokens` and `:embedContent` are refused with 400.
- **The Responses API over a WebSocket.** Connect to `GET /v1/responses`
  with `Upgrade: websocket`.
  - Each `response.create` frame is handled like a streamed
    `POST /v1/responses`, with its own limits, routing, billing and
    audit row.
  - Turns on one connection run in order. A refused turn fails with
    `response.failed`, and the connection stays open.
  - The connection keeps its latest response. That lets a turn continue
    from it with `previous_response_id`, even with `store: false`,
    which is how Codex works, and against any upstream format.
  - A new counter, `gateway_responses_ws_connections_total`, counts
    connections.
- **More places to put an API key.** Gateway keys are also accepted in
  `x-api-key` (Anthropic SDKs), `x-goog-api-key` and `?key=` (Gemini
  SDKs), as well as `Authorization: Bearer`. Headers are checked first.
  A key given in the query string is never sent upstream.
- **Hidden-text audit events show what the text says.** Each item in
  `found` gains `revealed`, the ASCII that the hidden tag characters
  spell.
- **`auth.default_role` can be set.** It is the role that newly
  registered users and SSO users get. In 1.x, setting it through the
  admin API reported success but changed nothing, because the setting
  row did not exist.
- **Model editor** fields for the three cache weights. Each placeholder
  shows the value used when the field is left empty.

### Changed

- **Default output length for upstreams that require `max_tokens`.** When
  the caller sets none, the gateway now sends 32000 for Claude models and
  8192 for other models. It used to send 4096, which cut Claude answers
  short.
- **More upstreams count as the vendor's own endpoint.** DeepSeek,
  Moonshot, Zhipu/Z.ai, DashScope, xAI and `*.amazonaws.com` are now
  recognised, and the check reads the parsed host. A relay URL such as
  `https://relay/api.openai.com` no longer passes as official. Official
  endpoints are stricter about request parameters, so the gateway drops
  or renames some parameters before sending to them.
- **Hidden-text scanning uses ThinkWatch-Core's `tw-guard`.** Same
  scope, same actions, and nothing is stripped.
- **Requests forwarded in their own format lose ThinkWatch's reasoning
  signatures.** A `tw1.` signature written by an earlier format
  conversion is removed, because Anthropic rejects it. The upstream's
  own signatures are kept.
- **Core crates: `tw-dialect`, `tw-guard` and `tw-breaker` at
  ThinkWatch-Core v0.43.0.** The code only this edition used (at-rest
  crypto, SigV4 signing, the gateway error type) moved into this
  repository. It works the same, and stored secrets decrypt as before.
- **The server's SQL moved from the request handlers into repository
  modules** (catalog, dashboard, limits, log forwarding, identity,
  access, MCP). Every statement is unchanged. New integration tests cover
  these endpoints and pass on both the old and the new code.
- **CI runs on pull requests into `dev`**, including the whole
  integration suite against Postgres, Redis and ClickHouse.

### Fixed

- **Revoking a user's default MCP connection always failed with a 500**,
  and the account could not be revoked. The newest remaining account is
  now made the default.

## [1.1.0] — 2026-09-24

The gateway stops rebuilding every request as a chat-shaped message. A
request whose route speaks the caller's own format goes out as the
caller sent it; one that crosses formats is converted by
[ThinkWatch-Core](https://github.com/ThinkWatchProject/ThinkWatch-Core),
the same layer the desktop edition uses. Tools, tool choice, system
prompt blocks, `metadata` and `cache_control` now reach the upstream,
where they used to be dropped. Two new checks guard what goes in and
out: tool calls an upstream returns, and invisible characters in what
a caller sends.

### Read before upgrading

- **Anthropic routes record more prompt tokens for the same work.**
  Prompt tokens now count the same for every upstream: plain input plus
  cache reads and cache writes, which is OpenAI's definition.
  Anthropic's own `input_tokens` leaves the cached part out, so on those
  routes prompt tokens, cost and budget use all go up. The price model
  still charges every prompt token at one rate.
- **Requests in the upstream's own format are forwarded as sent.** Every
  field the caller sends reaches the upstream, along with the caller's
  `anthropic-beta` and `anthropic-version` headers. Only the model name
  changes, and PII is swapped for placeholders. An OpenAI-compatible
  upstream that rejects fields it does not know may now refuse requests
  that used to succeed, because those fields were stripped before. Try
  your upstreams with the clients you actually run.
- **Two checks are on by default, and neither blocks anything yet.**
  - Tool-call inspection starts in `observe` mode.
  - The hidden-character check starts in `warn` mode.
  - Both write audit events, so expect new entries in the audit log and
    in anything subscribed to it.
  - Nothing is refused until you switch to `enforce` or `block`.
- **The response cache starts cold.** The cache key now covers the whole
  request, so entries written by 1.0.2 are never hit again. They expire
  on their own.
- **One PII value gets one placeholder.** Within a request, the same
  e-mail address is `{{EMAIL_1}}` wherever it appears. It used to get a
  new number each time, so a model saw one person as several. Saving a
  PII pattern now also requires the placeholder prefix to be letters,
  digits or underscores.

### Added

- **Tool-call inspection** (`security.tool_inspection`).
  - **Why:** an upstream writes the response, so it can hand the caller
    a tool call the model never made, such as `bash("curl … | sh")`
    appended to an ordinary answer. An agent set to auto-approve then
    runs it.
  - **Rules:** a built-in set of dangerous-command rules. Each can be
    switched off or given a different action, and you can add your own.
  - **Modes:** `off`, `observe` (records hits and changes nothing on the
    wire) and `enforce`.
  - **Enforce on a stream:** the stream is cut at the frame that would
    complete a matching call, and the refusal arrives in the caller's
    format.
  - **Enforce on a whole response or a cache hit:** refused with 403
    (`policy_blocked`). A refused answer is neither cached nor billed.
  - **Audit and metrics:** every hit is an audit event
    (`gateway.tool_call_flagged` or `gateway.tool_call_blocked`) and
    counts in `gateway_tool_call_flagged_total`.
  - **Admin API:** `GET /api/admin/settings/tool-inspection/rules` and
    `POST /api/admin/settings/tool-inspection/test`.
  - **Console:** a card on the security page, plus a sandbox tab.
- **Hidden-character check** (`security.hidden_text`: `off`, `log`,
  `warn` or `block`).
  - **What it looks for:**
    - Unicode tag characters, which carry an instruction invisibly into
      the model's context;
    - bidirectional overrides, which make text read differently on
      screen than it is.
  - **Where:** the caller's messages and the tool results inside them.
    The system prompt and the model's own turns are not checked.
  - **Not flagged:** zero-width joiners (emoji), the zero-width
    non-joiner (Persian) and Cyrillic.
  - **Actions:** `warn` writes `gateway.hidden_text_flagged`; `block`
    refuses with 403 and writes `gateway.hidden_text_blocked`.
  - **Console:** a card on the security page.
- **Tool-call arguments get their PII back.** A model asked to e-mail
  `a@example.com` used to call the tool with `{{EMAIL_1}}` as the
  address.

### Changed

- **One pipeline for `/v1/chat/completions`, `/v1/messages` and
  `/v1/responses`.** A same-format request is forwarded as sent. A
  cross-format request is converted, and the gateway logs what the
  target format cannot carry.
- **Content filtering and PII detection read tool results too.** That is
  where an injected instruction, or customer data pulled in by a tool,
  usually sits.
- **Usage is read off the upstream's own bytes.** A streamed response is
  no longer held in memory for an accounting pass at the end.
- **Chat streams are billed on the upstream's actual usage.** They are
  always sent asking for it. A caller who did not ask for the usage
  chunk still does not get one. 1.0.2 estimated the count for these.
- **Streams send their headers at once.** A caller who leaves while the
  upstream is still thinking is recorded as cancelled.
- **Connectivity tests use the live encoder.** A route's test request is
  built by the same encoder as real traffic, so a passing test means
  forwarding works.
- **The web console loads data through TanStack Query.**
  - Signing out, including from another tab, clears everything cached.
  - After a change, screens refresh in place.
  - Polling pauses while the tab is hidden.
- **Core crates come from one pinned tag** (ThinkWatch-Core v0.40.0),
  declared once at the workspace root.

### Fixed

- **Requests lost their tools, tool choice and non-text content** on the
  way upstream (ThinkWatch-Core#50). Claude Code's system prompt, sent
  as an array, was dropped whole, and so was every `cache_control`
  breakpoint. Each cached prefix was billed as full-price input.
- **The response cache could serve the wrong answer.** Its key covered
  only model, messages and `max_tokens`, so two requests that differed
  only in tools, `response_format`, `seed` and so on shared one entry.
- **A tripped route stayed out until its Redis key expired**, roughly
  four cooldowns. It now gets probed once the cooldown is over, and a
  success closes it.
- **The dashboard showed every AI provider's breaker as `Closed`.** It
  now shows the real state of the provider's routes, reporting the
  worst one.
- **The PII "try patterns" endpoint misreported labels.** A pattern
  named `CUSTOM_EMAIL` was reported as `CUSTOM`.
- **`:latest` could point at a `main` build rather than the release**,
  which is what happened for v1.0.2's server image. Only the release
  workflow sets `:latest` now.
- **The web image hung for six hours.** Its frontend was built under
  QEMU for arm64, where Node crashed and the step never returned. It is
  now built once, natively. The image contents are unchanged.

### Security

- Refreshed the web console's lockfile to clear 56 Dependabot alerts
  (1 critical, 23 high). All were transitive, and none of them reached
  the shipped bundle.

## [1.0.2] — 2026-09-13

The shared gateway layer moves out into its own repository, and OIDC
learns to accept an ID token whose `aud` carries more than the client
ID. Mostly a fix release otherwise — the deploy and auth items below are
the ones worth reading before you upgrade.

### Added

- **`OIDC_ADDITIONAL_TRUSTED_AUDIENCES`** — a comma-separated allowlist
  of extra ID-token audiences to trust. Some IdPs put something besides
  the client ID in `aud`; Zitadel includes the parent project ID, and
  `openidconnect`'s stock verifier rejects every non-client-ID audience,
  so login failed outright. Leave it unset and verification is
  byte-for-byte what it was. Set, it widens exactly one check: the extra
  audiences on a multi-audience token are matched against this list as
  exact strings — no wildcards, no prefixes. The client ID must still
  appear in `aud` regardless. Documented in `.env.example`. Thanks to
  @DaniW42 for the report and the patch (#12).
- **Automatic upstream protocol detection** — the gateway learns an
  upstream's dialect and relearns it when the upstream changes, instead
  of relying on a static guess. Protocol is now a property of the route.
- **Reverse-proxy identification** — the server recognizes its own
  reverse proxy, which is what makes the dashboard WebSocket and the API
  docs work behind nginx.

### Changed

- **`azp` is validated whenever it is present**, per OIDC Core 3.1.3.7
  step 5, which has no audience-count precondition. Previously it was
  only checked when a multi-audience token made it mandatory, so a
  single-audience token naming this client with `azp` pointing at a
  *different* client was accepted — the IdP stating plainly that the
  token was authorized for somebody else. **This can reject a token that
  1.0.1 accepted.** If your IdP sets `azp` to something other than your
  client ID, logins will start failing and the message will say so;
  that is the IdP to fix, not this setting.
- **The shared gateway layer now comes from
  [ThinkWatch-Core](https://github.com/ThinkWatchProject/ThinkWatch-Core)**
  as a git dependency rather than living in this tree. Six crates —
  types, protocol, provider, resilience, crypto, and the JSON secret
  envelope — are one implementation shared with the desktop edition
  instead of two copies drifting apart. No runtime or API change; it
  affects you only if you build from source, where the build now needs
  network access to resolve that dependency. The published images are
  unaffected.
- **Stored secrets are redacted on read and decrypted on use.** Provider
  credentials no longer travel in plaintext through code paths that
  merely display or list them.

### Fixed

- **Deploy** — nginx broke the API docs three separate ways; the
  dashboard WebSocket wasn't proxied; the prod stack's healthchecks were
  wrong; and the dev stack could overwrite production's ClickHouse
  credentials. The last one is the reason to read this list.
- **Auth** — the console logged itself out after every token refresh.
  Rate-limit counters could be created without an expiry and sit in
  Redis forever.
- **Models** — the gateway no longer offers models the upstream refuses,
  no longer imports models it won't serve, and reports every dialect
  that was refused rather than only the last. Provider `base_url`
  trailing slashes are normalized.
- **Analytics** — spend is attributed to a person, not a UUID.
- **Audit** — a syslog forwarder formatting fix that had CI red on
  clippy 1.98 (#11, thanks @DaniW42). Two more instances of the same
  lint, plus a `result_large_err` false positive in the MCP lifecycle
  stages, measured rather than boxed: the `Ok` variant is 296 bytes
  against the `Err`'s 152, so boxing saves nothing and adds an
  allocation.
- **UI** — the brand mark stays inside the collapsed sidebar rail.

### Contributing

`CONTRIBUTING.md` and a PR template now state the branch contract that
had only lived in `docs/operations/release.md`: routine work targets
`dev`, and `main` is the release line. A workflow comments on PRs opened
against `main` from anything other than `dev` or a `hotfix/*` branch,
because GitHub pre-fills the base with the default branch and walks
contributors into it.

## [1.0.1] — 2026-05-27

Release-pipeline validation. **No product change** — the published
binary, REST surface, MCP wire shapes, and audit semantics are
identical to `v1.0.0`. Operators pinning `1.0.0` have no reason to
bump; those tracking `:latest` move forward.

### Changed

- **Release workflow** — arm64 image builds now run on a native
  arm64 runner (`ubuntu-24.04-arm`) instead of QEMU emulation.
  v1.0.0's server image build took 1h24m; this should drop to
  ~10 min. Multi-arch manifest assembled by a new merge job via
  `docker buildx imagetools create`.
- **Node 24 opt-in** — workflow sets
  `FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true` so all `actions/*` +
  `docker/*` run on Node 24 ahead of GitHub's 2026-06-02 forced
  cutover.

## [1.0.0] — 2026-05-27

Stability commitment. No code delta since `0.5.0` — this tag marks
the point at which the API surface becomes a [SemVer](https://semver.org/spec/v2.0.0.html)
commitment.

### Changed

- **Versioning policy** — from this tag onwards every breaking
  change (REST routes, MCP wire shapes, audit-row JSON keys,
  database schema, public Rust APIs in published crates) requires
  a major bump. Operators chasing the `:latest` tag on the GHCR
  images can do so without surprise.

### Notes

- Docker images cut at this tag receive `:latest` for the first
  time — the release workflow suppresses `:latest` on `0.x` and
  pre-release tags. Pin the version in production rather than
  tracking `:latest` unless you have a controlled rollback path.

## [0.5.0] — 2026-05-26

First public beta. The product surface is stable enough to deploy
against, but the API contract is **not yet committed** — expect
breaking changes in `0.6.x` and beyond as the run-up to `1.0.0`
narrows the surface. Operators running this version against real
traffic should pin the image digest and read every subsequent
release note.

### Highlights

- **AI gateway** (`:3000`) — OpenAI / Anthropic / Google / Azure
  OpenAI / AWS Bedrock with weighted multi-route failover,
  circuit breakers, semantic response cache (Redis-backed), SSE
  streaming with PII restore on the wire.
- **MCP gateway** — per-user OAuth + static-token + admin-shared
  credential modes, response cache with prefix-based invalidation,
  per-server circuit breakers keyed by UUID (so a rename or
  recreate doesn't inherit stale state).
- **Audit pipeline** — bodies up to `audit.body_max_bytes` land
  inline in ClickHouse; oversize bodies offload to S3-compatible
  object storage (RustFS / MinIO / AWS S3). Request and response
  bodies are PII-redacted before write; the bucket lifecycle rule
  is administered from the admin UI
  (`audit.body_s3_lifecycle_days`).
- **RBAC + identity** — JWT access tokens, refresh tokens, OIDC
  SSO, TOTP, recovery codes. Permissions evaluated on every
  request from a Redis-cached + DB-backed policy. Per-API-key
  limits + budgets enforced on the gateway hot path.
- **Observability** — `/metrics` (Prometheus exposition, bearer-
  protected), `/api/health` (PG + Redis + ClickHouse + S3 deep
  probe), per-request `x-trace-id`, structured tracing.
- **Admin console** (`:3001`) — React 19 + TypeScript + i18n
  (en/zh, perfect parity). Dashboard, traces, cost analytics,
  RBAC editor, MCP server CRUD, settings PATCH.

### Quality

- 675 unit + integration tests, gated on `make precommit`.
- Five rounds of systematic bug audits (≈ 45 bugs fixed, ≈ 800
  lines of legacy compat scrubbed) preceded this tag — see
  commits `b0b5820 → dbfe9da` for the full series.

### Operations

- Helm chart ships an opt-in `ServiceMonitor`
  (`metrics.serviceMonitor.enabled=true`) gated on the
  auto-generated `METRICS_BEARER_TOKEN` secret. Pair with
  `kube-prometheus-stack` for `/metrics` scraping.
- `deploy/grafana/dashboards/` — starter overview dashboard JSON
  plus a metric reference + minimal alert rule set in the README.
- `docs/operations/secret-rotation.md` — JWT_SECRET online
  rotation and ENCRYPTION_KEY offline re-encrypt procedures.
- `docs/operations/backup-restore.md` — PG + ClickHouse + S3
  procedures with restore-order gotchas, cross-version compat
  notes, and a quarterly DR drill template.

### Known limitations for `0.5.x`

- **API surface NOT frozen** — REST routes, MCP wire shapes,
  audit-row JSON keys, and database schema may change in any
  `0.x` bump. SemVer kicks in at `1.0.0`.
- **No online ENCRYPTION_KEY rotation** — the documented
  procedure requires a brief downtime window. Online dual-key
  rotation is queued for `1.x`.

### Upgrade path from `0.1.0`

The `0.1.0` series was never published; deployments running
unreleased builds should: stop the gateway, run `db/schema.sql`
against PostgreSQL, restart against this tag. The schema is
idempotent end-to-end, so the apply is safe to repeat.

[Unreleased]: https://github.com/ThinkWatchProject/ThinkWatch/compare/v3.1.0...HEAD
[3.1.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v3.1.0
[3.0.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v3.0.0
[2.2.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v2.2.0
[2.1.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v2.1.0
[2.0.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v2.0.0
[1.1.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v1.1.0
[1.0.2]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v1.0.2
[1.0.1]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v1.0.1
[1.0.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v1.0.0
[0.5.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v0.5.0
