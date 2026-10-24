# The Bridge owns all Sonos communication

The kickoff brief had the phone App drive Sonos directly. We decided the opposite: the App only ever talks to the Bridge, and the Bridge discovers Groups, starts playback, watches it, and writes Progress to Audiobookshelf. That way the App can be force-quit or leave the house right after a Handoff. The cost is that the Bridge must sit on the household LAN, likely with host networking for multicast discovery.
