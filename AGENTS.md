# Skyber — instructions for coding agents

**Read [AGENT.md](AGENT.md) first.** It is the detailed project playbook. This file uses the widely discovered `AGENTS.md` filename so tools such as Codex and Kilo find the instructions.

Authoritative project documents:
- [Architecture, scope, gates](docs/PLAN_SKYBER.md)
- [Code quality and CI policy](docs/QUALITY.md)
- [Coding conventions](docs/CODING_STYLE.md)
- [Testing tiers](docs/TESTING.md)

**Non-negotiable reminders:** Skyber uses the **Kyber.tech ultra-low-latency media stack**, **not** CRYSTALS-Kyber and not WebRTC media. Rust core + native Qt/QML. Start with local A1 feasibility, later Windows–Linux A2. Preserve the borderless hover-only floating webcam UX. Separate fast synthetic tests from manual hardware suites. Never invent test results, silently fall back to missing modules, or remove agreed features. See `AGENT.md` for full rules.
