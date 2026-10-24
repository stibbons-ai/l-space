# ABS auth findings (as of 2026-10-24)

Source code was read at master commit `edb1f7e` (2026-10-23), which is the v2.37.2 release commit. In the links below, S = https://github.com/advplyr/audiobookshelf/blob/edb1f7efc92ecacdef51aafb4d9d5054ce10604f

## 5. Latest version
- **v2.37.2**, published 2026-10-23T20:18Z. https://github.com/advplyr/audiobookshelf/releases/tag/v2.37.2
- Earlier releases: v2.37.1 (2026-09-29) and v2.37.0 (2026-09-28). The v2.37.0 release added "API keys can authenticate websocket connections". v2.37.1 extended the OIDC callback cookie to 10 minutes and cleaned up abandoned OIDC mobile sessions. https://github.com/advplyr/audiobookshelf/releases/tag/v2.37.1, https://github.com/advplyr/audiobookshelf/releases/tag/v2.37.0
- CVE-2026-73085 was fixed in 2.36.0. Before that version, refresh-type JWTs were accepted as bearer tokens. Current code rejects `type:'refresh'` outside `/auth/refresh`. https://app.opencve.io/cve/CVE-2026-73085, S/server/auth/TokenManager.js (`isBearerAccessTokenPayload`)

## 1. Auth model
- Since v2.26.0, ABS uses JWT access tokens plus rotating refresh tokens, with sessions stored in the database. https://github.com/advplyr/audiobookshelf/discussions/4460
- **Current defaults in code:**
  - Access token: **1 h** (`ACCESS_TOKEN_EXPIRY`, in seconds).
  - Refresh token: **30 days** (`REFRESH_TOKEN_EXPIRY`).
  - Grace period for a rotated-out refresh token: **10 min** (`REFRESH_TOKEN_GRACE_PERIOD`). During the grace period the old token returns the current refresh token and a new access token.
  - Source: S/server/auth/TokenManager.js
  - The 2025 announcement gave the initial defaults as 12 h and 7 d, and said they would change. Those numbers are now stale.
- **Login:** `POST /login` with JSON `{username,password}`.
  - With header `x-return-tokens: true`: the response contains `user.accessToken` and `user.refreshToken`.
  - Without it: the refresh token is set only as the httpOnly cookie `refresh_token` (sameSite=lax, path /).
  - The response still contains the legacy `user.token`.
  - Sources: S/server/Auth.js (routes `/login` and `handleLoginSuccess`), https://github.com/advplyr/audiobookshelf/discussions/4460#discussioncomment-13735737
- **Refresh:** `POST /auth/refresh`.
  - Send the refresh token in header `x-refresh-token` (or in the cookie).
  - If you used the header, the response body includes the new `user.refreshToken`.
  - The refresh token **rotates on every use**, so the Bridge must persist the new one each time. Failure returns 401 with `{error}`.
  - Rate limited per IP: 40 requests per 10 min by default (`RATE_LIMIT_AUTH_MAX`, `RATE_LIMIT_AUTH_WINDOW`).
  - Sources: S/server/Auth.js, discussion #4460.
- **Logout:** `POST /logout` with `x-refresh-token` deletes that session.
- **Legacy non-expiring tokens:**
  - These still authenticate as of v2.37.2. `jwtAuthCheck` accepts JWTs without `exp` and only sets a `user.isOldToken` flag.
  - `/login` still returns `user.token`, which comes from the deprecated `generateAccessToken` with no expiry.
  - The announcement said the old system would be removed "no earlier than September 30, 2025". No removal is present in the code.
  - Rotating `JWT_SECRET_KEY` invalidates these tokens.
  - **No removal date has been announced (UNVERIFIED that none exists elsewhere). Do not depend on them.**
  - Sources: S/server/auth/TokenManager.js (`jwtAuthCheck`, `generateAccessToken` @deprecated), discussion #4460.

## 2. API keys
- **Who can create them:** admin only, through `/api/api-keys` (GET/POST/PATCH/DELETE) or Settings → Users → API Keys. Only root can create a key for the root user. https://audiobookshelf.org/docs/documentation/server-management/api-keys/, S/server/controllers/ApiKeyController.js, S/server/routers/ApiRouter.js
- **User binding:** each key acts on behalf of one selected user and inherits that user's permissions. It is not further scoped.
- **Expiry:** optional (`expiresIn` in seconds, or none). Expired keys are auto-deactivated when next used. A key can also be deactivated without deleting it. S/server/models/ApiKey.js, TokenManager.js
- **Stream URLs:** yes, keys work as `?token=`. The passport-jwt strategy extracts the token from `Authorization: Bearer` OR the query param `token` (`ExtractJwt.fromUrlQueryParameter('token')`), and the same `jwtAuthCheck` handles `type:'api'`. S/server/Auth.js line ~126
- **Caveat:** the docs say keys are "designed for server-to-server… For third-party apps, use the standard JWT authentication flow instead", and they require an admin to mint them per user.

## 3. Token expiry during a stream
- Auth runs once, as Express middleware, before the handler: `/api` → `authMiddleware`. `GET /api/items/:id/file/:fileid` then calls `res.sendFile(path)`. S/server/Server.js (~l.354), S/server/routers/ApiRouter.js l.124, S/server/controllers/LibraryItemController.js (~l.1003)
- There is no re-check while the response body is being sent, so **an in-flight response keeps streaming. Only new requests (each new Range request) get 401 once `exp` has passed.** [INFERENCE from the code; not tested]
- The practical consequence:
  - A 1 h access token in a Sonos URL breaks the first seek or reconnect after an hour.
  - Use either a long-lived API key, or a Bridge-proxied URL that holds its own token and refreshes it.

## 4. OIDC for a third-party client
This is the flow used by the mobile app. Source: S/server/auth/OidcAuthStrategy.js (`getAuthorizationUrl`, `isValidRedirectUri`, `handleMobileRedirect`), S/server/Auth.js
1. The client generates a PKCE pair. It opens a browser at `GET /auth/openid?response_type=code&redirect_uri=<URI>&code_challenge=<S256>&code_challenge_method=S256&state=<optional>`. Any of the parameters `redirect_uri`, `response_type=code` or `code_challenge` puts the request in mobile flow. Only `code` is allowed as the response type.
2. `redirect_uri` must be on the whitelist in server setting `authOpenIDMobileRedirectURIs`.
   - Default: `['audiobookshelf://oauth']`.
   - A single `'*'` allows any URI.
   - An admin sets it in Settings → Authentication ("Allowed Mobile Redirect URIs"); this is stored in server settings and changed through MiscController.
   - Sources: S/server/objects/settings/ServerSettings.js, S/server/controllers/MiscController.js
3. ABS registers `<host>/auth/openid/mobile-redirect` with the IdP as the redirect. After the IdP returns, ABS 302s to `<redirect_uri>?code=…&state=…`.
4. The client calls `GET /auth/openid/callback?code=…&state=…&code_verifier=…`. It must send the same cookie jar as step 1, because ABS keeps OIDC state in the express session. Cookie `auth_method=openid-mobile` makes the auth API-based.
   - The response is JSON shaped like the `/login` response, including `user.accessToken` and `user.refreshToken`.
   - The callback cookie lasts 10 min (v2.37.1).
5. Refresh afterwards uses `/auth/refresh` with `x-refresh-token`, as in section 1.
- Note: the IdP's allowed redirect list must include `https://<abs>/auth/openid/mobile-redirect` (UNVERIFIED in the docs; inferred from the code).
- For the Bridge: use an https redirect_uri served by the Bridge, or a custom scheme for the App, and have the ABS admin add it to the whitelist.
- The old api.audiobookshelf.org docs describe this flow but are marked "out-of-date and no longer maintained". https://api.audiobookshelf.org/
