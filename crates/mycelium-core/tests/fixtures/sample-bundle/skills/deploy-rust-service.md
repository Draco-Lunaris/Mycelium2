---
type: Skill
title: Deploy Rust Service
description: Skill for deploying rust services with cargo and systemd
tags:
  - skill
  - rust
---

# Instructions

1. Build with `cargo build --release`.
2. Copy the binary to the target host.
3. Install the systemd unit.

Related decision: [auth model](/decisions/auth-model.md).