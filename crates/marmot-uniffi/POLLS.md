# Polls

MDK carries [NIP-88](https://github.com/nostr-protocol/nips/blob/master/88.md) poll events inside encrypted MLS
application messages. Poll and vote data never become public relay-query metadata: outer Marmot transport remains kind
445 and MDK omits NIP-88 `relay` tags.

Use `createPoll` with a question, two through ten option labels, `singleChoice` or `multipleChoice`, and an optional Unix
seconds deadline. MDK assigns stable option ids in display order (`"0"` through `"9"`). Questions are limited to 1024
UTF-8 bytes, labels to 256 bytes, and deadlines to at most 30 days after creation. Empty, whitespace-padded, control, and
bidirectional-override text is rejected.

Use `castPollVote` with the poll event id and the complete selected option-id list. It is a replacement, not a delta:
send one id for single choice and one through ten unique ids for multiple choice. The poll must already be a valid local
timeline row in the same group and still be open. An empty selection is not an unvote operation.

`TimelineMessageRecordFfi.poll` is present on valid kind-1068 rows. It contains the ordered options and counts, total
participating authenticated identities, the device's effective sent selection, creator, deadline, and current open
state. Kind-1018 responses do not form timeline rows. For each authenticated author MDK selects the response with the
greatest `(created_at, canonical event id)`; ordinary author deletion or retention expiry falls back to the newest
retained valid response. Projection work considers at most the newest 64 retained responses per author.

Polls are coordination tools, not anonymous or election-grade voting. Every group member receives authenticated voter
identity with each response, and distributed clients/relays do not provide a global sequencer at the closing boundary.
Hosts must not describe the feature as anonymous or use the result for high-stakes elections.

The C surface mirrors this contract as `marmot_create_poll`, `marmot_cast_poll_vote`, and the nullable
`MarmotTimelineMessageRecord.poll`. Recompile C clients against the matching generated header because the timeline record
layout changes.
