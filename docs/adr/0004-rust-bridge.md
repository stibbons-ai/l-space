# The Bridge is written in Rust (tokio)

We considered TypeScript on Bun with Effect 4 as well as Rust. With no SMAPI SOAP server to build, neither language had a technical advantage: the Bridge is HTTP, XML and discovery plumbing. We chose Rust because the compiler gives coding agents a tight feedback loop, and because a Rust frontend can share types with the Bridge without code generation.

## Considered Options

- **TypeScript on Bun + Effect 4**: also good for agent-driven work, but Bun has Node-compatibility gaps that would need workarounds (WebSocket TLS options, unverified SOAP and multicast library support).
