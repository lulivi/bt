# bt — Better Together

A shared radio: listeners in different places hear the same songs at the same time, from a queue they build together out of YouTube links.

## Language

**Song**:
A YouTube video, identified by its video ID.
_Avoid_: Track, video

**Queue**:
The ordered, shared list of Entries every Listener sees and plays from.
_Avoid_: Playlist

**Entry**:
One place in the Queue that refers to a Song; the same Song can appear in several Entries.
_Avoid_: Track, item

**Current entry**:
The Entry the Radio is broadcasting (or would be, if paused); there is none when the Radio is idle.
_Avoid_: Now playing

**Radio**:
The single continuous broadcast every Listener hears; it never stops, it falls silent when paused or idle.
_Avoid_: Player, station

**Listener**:
A person with a browser connected to the Radio.
_Avoid_: User, client
