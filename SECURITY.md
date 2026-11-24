# Security policy

## Supported versions

Fixes go into the latest release: a vulnerability is fixed in a new release,
whose notes say what it fixes.

## Reporting a vulnerability

Report a vulnerability privately, never in a public issue:

- with GitHub's private vulnerability reporting: **Security → Report a
  vulnerability** on this repository
  ([open a report](https://github.com/vladimirpesic/rusta/security/advisories/new)),
  or
- by email to **<contact@devscriptor.com>**.

Give the Rusta version (`rusta --version`), the platform, and what shows the
problem: the configuration, the tool call the model made, and what Rusta did
with it.

## What is a vulnerability in Rusta

Rusta runs a language model's tool calls on your machine, under the permission
mode, the rules and the built-in guardrails described in
[Permissions and safety](docs/permissions.md). A vulnerability is a way around
them, for example:

- a tool call that runs, edits or deletes without the approval the mode, the
  rules or a guardrail require — the secrets-file and destructive-Git
  guardrails hold even with permissions bypassed;
- a cloned repository's `.rusta.toml` starting a command, redirecting requests
  or loosening permissions in a folder that is not trusted;
- an API key, a token or a session's records readable by another user of the
  machine, or sent anywhere but the provider they are for.

A call that runs because the configuration allows it — `--yes`, a rule that
allows it, an approval for the session — is Rusta doing what it was told.
