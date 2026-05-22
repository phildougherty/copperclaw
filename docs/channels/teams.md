# teams (Microsoft Teams) channel audit

## Implemented
- deliver: COMPLETE for text/html + edit + reaction in both channel and
  chat targets; files Unsupported.
  `crates/ironclaw-channels/teams/src/adapter.rs:267`
- subscribe: trait-default Ok (Graph subscription managed externally).
- set_typing: trait-default Ok (Graph has no typing indicator on
  channel messages).
- edit_message: action="edit" — PATCH on channel or chat message.
- add_reaction: action="reaction" with shortcode → reactionType map.
- plain_text_fallback: trait-default None (content already split as
  text/html at send time).
- open_dm: trait-default None.

## Gaps
MED:
- Files Unsupported. Graph supports `attachments` with a hosted file
  reference (uploaded to OneDrive/SharePoint first). Not implemented.

LOW:
- Reaction emoji map is hard-coded; emojis outside the map return
  Unsupported. List should be expanded to cover the full Teams
  reaction set (currently like/heart/laugh/surprised/sad/angry).

## Edge cases tested
- [x] channel message returns id
- [x] channel with thread → replies endpoint
- [x] chat uses /chats endpoint
- [x] html content uses html content-type
- [x] files → Unsupported
- [x] malformed platform_id → BadRequest
- [x] edit channel
- [x] edit chat
- [x] edit missing target → BadRequest
- [x] reaction channel
- [x] reaction chat
- [x] reaction unknown emoji → Unsupported
- [x] reaction missing emoji/target → BadRequest
- [x] unknown action → Unsupported
- [x] system without action → BadRequest
- [x] auth / rate / transport propagation

## Fixes in this PR
None.

## Deferred for follow-up
- File attachments via Graph (OneDrive/SharePoint upload + attachment
  reference).
- Expand reaction map.
