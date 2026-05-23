# teams (Microsoft Teams) channel audit

## Implemented
- deliver: COMPLETE for text/html + edit + reaction in both channel and
  chat targets; channel-target files supported via Graph
  filesFolder + drive upload + attachment-by-reference; chat-target files
  remain Unsupported (delegated-auth limit).
  `crates/ironclaw-channels/teams/src/adapter.rs:334` (top-level deliver);
  `adapter.rs:106` (deliver_chat — the channel/chat split helper).
- subscribe: trait-default Ok (Graph subscription managed externally).
- set_typing: trait-default Ok (Graph has no typing indicator on
  channel messages).
- edit_message: action="edit" — PATCH on channel or chat message.
- add_reaction: action="reaction" with shortcode → reactionType map.
- plain_text_fallback: trait-default None (content already split as
  text/html at send time).
- open_dm: trait-default None.

## Gaps
LOW:
- Reaction emoji map is hard-coded; emojis outside the map return
  Unsupported. List should be expanded to cover the full Teams
  reaction set (currently like/heart/laugh/surprised/sad/angry).
- Channel reply + attachments collapses to a top-level channel post
  with a warning, because the Graph `messages/{id}/replies` endpoint
  doesn't accept `attachments`.
- Files on 1:1 / group chat targets are explicitly Unsupported (the
  bot's app-only auth cannot reach a user's OneDrive). Lift requires
  delegated user auth.

## Edge cases tested
- [x] channel message returns id
- [x] channel with thread → replies endpoint
- [x] chat uses /chats endpoint
- [x] html content uses html content-type
- [x] files on channel target → uploaded via filesFolder → /drives/.../content, attached by reference
- [x] files on chat target → Unsupported with explanation
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
- Channel file attachments via Graph: `get_channel_files_folder`
  resolves the drive + folder ids, `upload_channel_file` PUTs bytes to
  `/drives/{drive}/items/{item}:/{filename}:/content`, and
  `post_channel_message_with_attachments` includes the references on
  the message. The adapter wires the three calls for channel-target
  outbounds with `files`. Chat-target attachments are explicitly
  rejected with an explanation about delegated auth.

## Deferred for follow-up
- 1:1 / group chat attachments (needs delegated user-OneDrive auth).
- Path-style upload tops out at the Graph 4 MB ceiling; large files
  need an upload session.
- Expand reaction map.
