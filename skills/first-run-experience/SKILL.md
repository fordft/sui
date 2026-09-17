---
name: first-run-experience
description: Setup and onboarding paths — can a new user reach a working state without knowing internals? Use for install flows, setup forms, config changes, and anything a user hits before their first real task.
cues: [setup, install, first-run, onboard, configure, config, credential, key, provider, login, sign-in, wizard, getting-started]
roles: [lead, worker]
---

# First-run experience

The user arrives knowing the goal, not the implementation. Every field
they see is a question they must answer — remove questions, don't add them.

## Evaluate the path

- Can setup complete without manually editing a config file?
- Are secrets masked during entry and never echoed back?
- Does unavailable optional infrastructure (keychain, browser, network)
  degrade gracefully with a clear next action — or block silently?
- Do errors say what to do next, not just what failed?
- Does a restart preserve what the user chose to persist — and only that?

## Verify the real journey

- Clean environment: no config, no env vars — what does the user see?
- Wrong credential → clear rejection + recovery, not a hung form.
- Headless path (SSH, no keyring, no browser) — every step still possible.
- Interrupt mid-setup and re-enter — no half-written state.

## Do not

- Do not ask the user for values Sui can discover or default sensibly.
- Do not show advanced escape hatches (env vars, manual URLs) as if they
  were the normal path.
- Do not treat "tests pass" as "a human can complete the flow".
