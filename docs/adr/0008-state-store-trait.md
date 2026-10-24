# Bridge state sits behind traits; SQLite is the first implementation

The Bridge persists Library Servers, encrypted Audiobookshelf tokens, App sessions, active Handoffs and Sleep timer deadlines. Code that uses this state depends only on storage traits, never on SQLite, so a later backend replaces only the implementation. A trait with a single implementation is deliberate here: the backend is expected to change, and that change must not ripple into the domain code.

## Consequences

- One shared contract test suite runs against every storage implementation, so a new backend is accepted only if it behaves identically.
