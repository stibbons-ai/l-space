app-name = Ook
starting = Verbinde…
busy = Einen Moment…
bridge = Clacks
retry = Erneut versuchen

bridge-url-title = Mit Clacks verbinden
bridge-url-intro = Gib die Adresse von Clacks ein, zum Beispiel http://192.168.1.10:8080. Das ist nur einmal nötig.
bridge-url-label = Clacks-Adresse
bridge-url-connect = Verbinden

login-title = Anmelden
login-library-server = Library Server
login-api-key = Audiobookshelf-API-Schlüssel
login-api-key-hint = API-Schlüssel legt ein Audiobookshelf-Admin unter Einstellungen → API-Schlüssel an.
login-submit = Anmelden
login-no-library-servers = Clacks hat noch keine Library Server. Bitte wende dich an die Person, die Clacks betreibt, damit sie einen hinzufügt.
login-change-bridge = Clacks ändern

settings-title = Einstellungen
settings-library-server = Library Server
settings-account = Konto
settings-language = Sprache
settings-logout = Abmelden

libraries-title = Bibliotheken
libraries-none = Auf deinem Library Server siehst du noch keine Bibliotheken.
library-kind-audiobooks = Hörbücher
library-kind-podcasts = Podcasts
library-empty = In dieser Bibliothek gibt es noch nichts zum Anhören.
back = Zurück

episodes-unfinished = Nicht beendet
episodes-finished = Beendet
episodes-all = Alle
episodes-none = Hier gibt es keine Folgen.

item-duration = Dauer
item-published = Veröffentlicht
item-progress = Fortschritt
item-chapters = Kapitel

progress-finished = Beendet
progress-not-started = Nicht begonnen
progress-started = Begonnen
progress-left = noch { $duration }

duration-hours-minutes = { $hours } Std. { $minutes } Min.
duration-hours = { $hours } Std.
duration-minutes = { $minutes } Min.
duration-under-minute = unter 1 Min.

groups-title = Gruppen
groups-none = Clacks hat in deinem Netzwerk keine Sonos-Lautsprecher gefunden.
groups-refresh = Aktualisieren
group-playing = Spielt
group-paused = Pausiert
group-stopped = Gestoppt
group-transitioning = Startet…
group-not-answering = Antwortet nicht
group-state-unknown = Unbekannter Zustand
play-on = Abspielen auf…
playing-on = Spielt auf { $group }

date = { $day }. { $month ->
        [1] Januar
        [2] Februar
        [3] März
        [4] April
        [5] Mai
        [6] Juni
        [7] Juli
        [8] August
        [9] September
        [10] Oktober
        [11] November
       *[12] Dezember
    } { $year }

language-system = System
language-english = English
language-german = Deutsch

error-bridge-url-invalid = Das ist keine Clacks-Adresse. Gib sie mit http:// oder https:// ein, zum Beispiel http://192.168.1.10:8080.
error-bridge-url-unreachable = Unter dieser Adresse antwortet nichts. Prüfe sie und ob dieses Telefon in deinem Heimnetz oder VPN ist.
error-session-library-server-not-configured = Dein Library Server ist auf Clacks gerade nicht eingerichtet. Du bleibst angemeldet: Sobald die Person, die Clacks betreibt, ihn wieder einrichtet, funktioniert Ook wie zuvor.
error-storage-load = Ook konnte nicht lesen, was es auf diesem Gerät gespeichert hat, und hat neu begonnen. Die unlesbare Datei liegt neben der neuen.
error-storage-stuck = Ook konnte nicht lesen, was es auf diesem Gerät gespeichert hat, und lässt es unverändert. Was du jetzt änderst, ist nach einem Neustart nicht mehr da.
error-storage-save = Ook konnte nicht auf diesem Gerät speichern. Nach einem Neustart musst du dich eventuell erneut anmelden.
error-logout-not-saved = Du bist abgemeldet, aber Ook konnte das nicht auf diesem Gerät speichern: Nach einem Neustart bist du eventuell wieder angemeldet. Melde dich dann erneut ab.
error-not-a-bridge = Unter dieser Adresse antwortet etwas, aber nicht Clacks. Prüfe die Adresse.
error-bridge-unreachable = Clacks ist nicht erreichbar. Prüfe, ob dieses Telefon in deinem Heimnetz oder VPN ist und ob Clacks läuft.
error-internal = Bei Clacks ist etwas schiefgegangen. Bitte versuche es später noch einmal.
error-api-key-rejected = Audiobookshelf hat diesen API-Schlüssel abgelehnt: Er ist falsch, wurde widerrufen oder ist abgelaufen. Bitte deinen Audiobookshelf-Admin um einen neuen.
error-library-server-not-configured = Dieser Library Server ist auf Clacks nicht mehr eingerichtet. Wähle einen anderen oder wende dich an die Person, die Clacks betreibt.
error-library-server-unreachable = Clacks erreicht diesen Library Server gerade nicht. Bitte versuche es später noch einmal.
error-not-found = Das ist auf deinem Library Server nicht (mehr) für dich verfügbar.
error-session-invalid = Deine Sitzung ist beendet. Bitte melde dich erneut an.
error-sonos-unreachable = Clacks hat deine Sonos-Lautsprecher gefunden, aber gerade antwortet keiner. Bitte versuche es noch einmal.
error-sonos-search-failed = Clacks kann dein Netzwerk gerade nicht nach Sonos-Lautsprechern durchsuchen. Bitte die Person, die Clacks betreibt, die Netzwerkeinrichtung und die Logs von Clacks zu prüfen.
error-group-not-found = Diese Gruppe gibt es nicht mehr: Vielleicht wurde sie in der Sonos-App neu gruppiert. Wähle sie noch einmal aus der Liste.
error-group-failed = Die Gruppe hat nicht angefangen zu spielen. Prüfe, ob sie eingeschaltet und in deinem Netzwerk ist, und versuche es noch einmal.
error-item-not-playable = Clacks kann das noch nicht auf einer Gruppe abspielen: nur Episoden und Hörbücher in einer einzigen Datei, in einem Format, das Sonos abspielt.
error-unknown = Etwas Unerwartetes ist schiefgegangen. Ein Update von Ook kann helfen.
