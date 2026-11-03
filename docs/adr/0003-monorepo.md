# One monorepo for the whole stack

The kickoff brief proposed separate `shelfcast-ios`, `shelfcast-android` and `shelfcast-bridge` repos (from the project's original name, Shelfcast). Instead, this `l-space` repo holds the Bridge, the App, and the shared API contract, so a contract change lands as one atomic commit in every component that uses it.
