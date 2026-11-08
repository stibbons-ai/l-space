app-name = Ook
starting = Connecting…
busy = Please wait…
bridge = Clacks
retry = Try again

bridge-url-title = Connect to Clacks
bridge-url-intro = Enter the address of Clacks, for example http://192.168.1.10:8080. You only need to do this once.
bridge-url-label = Clacks address
bridge-url-connect = Connect

login-title = Log in
login-library-server = Library Server
login-api-key = Audiobookshelf API key
login-api-key-hint = An Audiobookshelf admin creates API keys under Settings → API Keys.
login-submit = Log in
login-no-library-servers = Clacks has no Library Servers yet. Ask whoever runs Clacks to add one.
login-change-bridge = Change Clacks

settings-title = Settings
settings-library-server = Library Server
settings-account = Account
settings-language = Language
settings-logout = Log out
settings-log-in-elsewhere = Log in to another Library Server
switch-library-server = Switch Library Server

home-title = Home
home-continue-listening = Continue listening
home-continue-listening-none = Nothing in progress. What you have started and not finished shows here.
home-latest-episodes = Latest episodes
home-latest-episodes-none = No new episodes you haven't finished.

libraries-title = Libraries
libraries-none = You can't see any Libraries on your Library Server yet.
library-kind-audiobooks = Audiobooks
library-kind-podcasts = Podcasts
library-empty = There is nothing to listen to in this Library yet.
back = Back

episodes-unfinished = Unfinished
episodes-finished = Finished
episodes-all = All
episodes-none = No episodes here.

item-duration = Duration
item-published = Published
item-progress = Progress
item-chapters = Chapters
item-mark-finished = Mark as finished
item-mark-unfinished = Mark as unfinished

progress-finished = Finished
progress-not-started = Not started
progress-started = Started
progress-left = { $duration } left

duration-hours-minutes = { $hours } h { $minutes } min
duration-hours = { $hours } h
duration-minutes = { $minutes } min
duration-under-minute = under 1 min

groups-title = Groups
groups-none = Clacks hasn't found any Sonos speakers on your network.
groups-refresh = Refresh
group-playing = Playing
group-paused = Paused
group-stopped = Stopped
group-transitioning = Starting…
group-not-answering = Not answering
group-state-unknown = Unknown state
group-pause = Pause
group-resume = Resume
group-back-15 = 15 s back
group-forward-30 = 30 s forward
group-volume = Volume { $volume }
group-volume-down = Quieter
group-volume-up = Louder
play-on = Play on…
playing-on = Playing on { $group }

date = { $month ->
        [1] January
        [2] February
        [3] March
        [4] April
        [5] May
        [6] June
        [7] July
        [8] August
        [9] September
        [10] October
        [11] November
       *[12] December
    } { $day }, { $year }

language-system = System
language-english = English
language-german = Deutsch

error-bridge-url-invalid = That is not a Clacks address. Enter it with http:// or https://, for example http://192.168.1.10:8080.
error-bridge-url-unreachable = Nothing answers at this address. Check it, and that this phone is on your home network or VPN.
error-session-library-server-not-configured = Your Library Server isn't set up on Clacks right now. You stay logged in: once whoever runs Clacks sets it up again, Ook works as before.
error-storage-load = Ook couldn't read what it had saved on this device, so it started afresh. It kept the unreadable file next to the new one.
error-storage-stuck = Ook couldn't read what it had saved on this device and leaves it untouched, so nothing you change now is kept after a restart.
error-storage-save = Ook could not save on this device. You may have to log in again after restarting it.
error-logout-not-saved = You are logged out, but Ook couldn't save that on this device: after a restart you may be logged in again. Log out again then.
error-not-a-bridge = Something answers at this address, but it isn't Clacks. Check the address.
error-bridge-unreachable = Clacks can't be reached. Check that this phone is on your home network or VPN, and that Clacks is running.
error-internal = Something went wrong on Clacks. Please try again later.
error-api-key-rejected = Audiobookshelf rejected this API key: it is wrong, or it was revoked or has expired. Ask your Audiobookshelf admin for a new one.
error-library-server-not-configured = This Library Server is no longer set up on Clacks. Pick another one, or ask whoever runs Clacks.
error-library-server-unreachable = Clacks can't reach this Library Server right now. Please try again later.
error-not-found = This isn't (or is no longer) available to you on your Library Server.
error-session-invalid = Your session has ended. Please log in again.
error-sonos-unreachable = Clacks found your Sonos speakers, but none of them answered just now. Please try again.
error-sonos-search-failed = Clacks can't search your network for Sonos speakers right now. Ask whoever runs Clacks to check its network setup and its logs.
error-group-not-found = That Group is gone: it may have been regrouped in the Sonos app. Pick another one.
error-group-failed = The Group didn't answer, or refused. Check that it is on and on your network, then try again.
error-item-not-playable = Clacks can't play this on a Group yet: only episodes and audiobooks in a single file, in a format Sonos plays.
error-no-handoff = That Group isn't playing what Clacks started: it plays something else now (maybe from the Sonos app), or what Clacks played there ended. Start it again with “Play on…”.
error-unknown = Something unexpected went wrong. Updating Ook may help.
