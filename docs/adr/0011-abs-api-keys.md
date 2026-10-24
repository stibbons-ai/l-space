# Users authenticate with Audiobookshelf API keys

To log in, an App user picks a Library Server and enters an Audiobookshelf API key. The Bridge checks it with `GET /api/me` and stores it encrypted (ADR 0008). Audiobookshelf's docs steer third-party apps toward its username/password flow, which returns one-hour access tokens and rotating refresh tokens. We chose API keys anyway to avoid single-flight refresh, token rotation, keep-alive refreshes and the login rate limit. The cost is that an Audiobookshelf admin must create a key for every household member. Each key carries that user's full permissions and may never expire.

## Considered Options

- **Username/password with JWT refresh**: the officially recommended flow, but it adds refresh-token rotation, refreshes that must never run concurrently for one user, keep-alive refreshes for users who rarely log in, and a rate limit of 40 logins/refreshes per 10 minutes per IP.
- **OIDC**: needs an admin-maintained allow-list of redirect URIs in Audiobookshelf; deferred.
