# 0001: Match one pinned Home Assistant release, its APIs and its files

Status: accepted (2026-11-13)

## Context

hars is only useful if the existing Home Assistant ecosystem works with it:
the official frontend, the companion apps, and people's existing installs.
Home Assistant changes its APIs, storage shapes and behaviour every month.
Tracking "latest" would mean chasing a moving target with no fixed answer
to test against.

## Decision

- hars matches **one pinned release** of Home Assistant core (2026.6.1), the
  frontend it ships (20260527.4) and one companion app version. The pins are
  listed in [ARCHITECTURE.md](../ARCHITECTURE.md#home-assistant-compatibility).
- It speaks the same REST and WebSocket APIs and reads and writes the same
  `.storage` files, so `hars import` copies an install's files byte for byte
  after validating them, and logins and app sessions carry over.
- Where behaviour is observable (states, attributes, timing, errors), hars
  reproduces HA's, including its quirks. Differences are deliberate and
  written down.

## Consequences

- The frontend and apps work unchanged; no fork to maintain.
- Moving a pin is a project: regenerate the vectors and translation tables
  against the new release and rerun the browser and app checks.
- Behaviour HA has but hars doesn't is visible; the import report lists it.
