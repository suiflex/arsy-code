# MCP and ACP adapters

## Problem and existing approaches

MCP 2026-07-28 standardizes JSON-RPC integrations among hosts, clients, and servers, with tools/resources/prompts/elicitation and optional tasks, skills, and apps (**D**). ACP v1 standardizes agent/client session, permission, filesystem, terminal, and update flows (**D**). Neither is a sufficient internal domain model.

## MCP design

ARSY is both client and optional server.

```mermaid
flowchart LR
  MS[MCP server] --> MC[MCP client adapter] --> B[Capability bus]
  B --> SS[MCP server adapter] --> EX[External MCP client]
```

Support stdio and Streamable HTTP first; legacy SSE only behind compatibility demand. Connections are managed through [`arsy mcp`](36-cli-tui.md), whose `test` subcommand negotiates capabilities without invoking a tool. Negotiate capabilities, correlate requests, bound messages/timeouts, support cancellation/progress, authenticate HTTP, and require policy for sampling/elicitation/tool effects. Resources become external-resource references and artifacts; annotations remain untrusted. MCP Apps render in a sandboxed UI origin with a mediated bridge.

Two connections are declared before any configuration file is read:
`fluxguard` and `probelm`, which ship in the same archive as `arsy` and are
enabled when they sit beside the binary. Each is a connection like any other —
same transport, same policy, same trust ceiling as the operator's own layer —
and each can be switched off. See
[distribution](34-distribution.md#bundled-fluxguard-and-probelm).

## Connection lifecycle

A connection that drops is retried with bounded backoff and an explicit attempt limit, mirroring the
language-server rule in [code intelligence](11-code-intelligence.md). When the limit is exhausted the
connection is marked failed and stays failed; it is never retried indefinitely.

Reconnecting never widens authority. Identity, trust label, rate limit, body cap, and capability
ceiling are re-applied on every reconnect, and the server re-negotiates from scratch. A tool,
resource, or prompt that appears after a reconnect and falls outside the ceiling is rejected with an
`ARSY-POL-*` diagnostic rather than silently accepted, because otherwise disconnecting and
reconnecting would be a way to acquire capability.

Authentication failure and a policy-disabled connection are both excluded from automatic retry. Each
requires an explicit `arsy mcp reconnect`, so a rejected credential cannot become a retry loop.

Requests in flight when a connection drops fail with an `ARSY-PRT-*` diagnostic and are retried only
when the operation is idempotent, under the retry rules in [diagnostics](33-diagnostics.md).

Refresh is re-discovery without tearing the connection down, for servers whose tool, resource, or
prompt list changes while connected. It invalidates the discovery cache for that connection and
nothing else; the session, its correlations, and its in-flight work are untouched. Refreshed entries
pass the same ceiling check as newly discovered ones.

Both reconnect and refresh append an event recording the connection, the trigger, and what changed.

## ACP design

Map `initialize`, authentication, `session/new|load|prompt|cancel`, updates, permission requests, filesystem, and terminal methods to protocol projections and operations. Honor absolute paths and 1-based lines at the adapter, then canonicalize internally. Advertise only implemented capabilities. Extension `_meta` and underscore methods never alter core authority.

## Failure, security, performance

Malformed messages, duplicate IDs, reconnects, cancellation races, hostile schemas, OAuth confusion, and server impersonation are tested. Each connection has an identity, trust label, rate limit, body cap, and capability ceiling. External servers cannot recursively trigger unbounded sampling or elicitation. Connection pools and lazy discovery prevent startup fan-out.

## Decision

MCP and ACP are versioned bidirectional edge adapters. Their raw request types stop at the adapter boundary.
