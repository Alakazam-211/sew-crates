//! Path templates for the routes named in the contracts, in axum 0.8 syntax.
//!
//! `GET` reads and `POST` mutates; nothing else is served. Every route is listed here.

/// Client API base.
pub const API_BASE: &str = "/api/v1";

/// `POST`: exchange a K2 pass for a session.
pub const AUTH_K2: &str = "/api/v1/auth/k2";
/// `POST`: renew a K2 session with a fresh pass.
pub const AUTH_RENEW: &str = "/api/v1/auth/renew";
/// `POST`: standalone password login.
pub const AUTH_LOGIN: &str = "/api/v1/auth/login";
/// `POST`: revoke the current session.
pub const AUTH_LOGOUT: &str = "/api/v1/auth/logout";
/// `POST`: set a standalone password with a one-time setup token.
pub const AUTH_PASSWORD_SET: &str = "/api/v1/auth/password/set";

/// `GET`: the signed-in member.
pub const ME: &str = "/api/v1/me";
/// `GET`: client-listener health, public.
pub const HEALTH: &str = "/api/v1/health";

/// `GET`: the websocket gateway.
pub const GATEWAY: &str = "/api/v1/gateway";
/// `POST`: mint a single-use gateway ticket.
pub const GATEWAY_TICKET: &str = "/api/v1/gateway/ticket";

/// `GET`: resolve a chat address.
pub const RESOLVE: &str = "/api/v1/resolve";

/// `GET` list, `POST` create: messages in a channel.
pub const CHANNEL_MESSAGES: &str = "/api/v1/channels/{id}/messages";
/// `GET`: messages by `ids=`.
pub const MESSAGES_BY_IDS: &str = "/api/v1/messages";
/// `POST`: edit a message.
pub const MESSAGE_EDIT: &str = "/api/v1/messages/{id}/edit";
/// `POST`: delete a message.
pub const MESSAGE_DELETE: &str = "/api/v1/messages/{id}/delete";
/// `POST`: the person an assistant posted for approves or declines the post.
pub const MESSAGE_ON_BEHALF_APPROVAL: &str = "/api/v1/messages/{id}/on-behalf-approval";
/// `GET`: a message's edit history.
pub const MESSAGE_HISTORY: &str = "/api/v1/messages/{id}/history";

/// `POST`: archive a channel.
pub const CHANNEL_ARCHIVE: &str = "/api/v1/channels/{id}/archive";
/// `POST`: re-sync a channel's overwrites with its category.
pub const CHANNEL_SYNC: &str = "/api/v1/channels/{id}/sync";
/// `POST`: move the read cursor.
pub const CHANNEL_ACK: &str = "/api/v1/channels/{id}/ack";
/// `GET`: channel change-log catch-up.
pub const CHANNEL_CHANGES: &str = "/api/v1/channels/{id}/changes";
/// `POST`: start an upload.
pub const CHANNEL_UPLOADS: &str = "/api/v1/channels/{id}/uploads";
/// `POST`: join a voice channel.
pub const CHANNEL_VOICE_JOIN: &str = "/api/v1/channels/{id}/voice/join";

/// `GET`: member change-log catch-up.
pub const ME_CHANGES: &str = "/api/v1/me/changes";

// ---- Read state, unread, threads inbox, catch-up ----

/// `POST`: move the channel cursor back (mark unread).
pub const CHANNEL_MARK_UNREAD: &str = "/api/v1/channels/{id}/mark_unread";
/// `POST`: move a thread cursor forward.
pub const THREAD_ACK: &str = "/api/v1/threads/{id}/ack";
/// `POST`: move a thread cursor back (mark unread).
pub const THREAD_MARK_UNREAD: &str = "/api/v1/threads/{id}/mark_unread";
/// `GET`: the caller's channel read states.
pub const ME_READ_STATES: &str = "/api/v1/me/read_states";
/// `POST`: bulk ack ("mark all read").
pub const ME_READ_STATES_ACK: &str = "/api/v1/me/read_states/ack";
/// `GET`: the caller's thread read states by root id.
pub const ME_THREAD_READS: &str = "/api/v1/me/thread_reads";
/// `GET`: the threads inbox list.
pub const ME_THREADS: &str = "/api/v1/me/threads";
/// `GET`: the threads inbox totals.
pub const ME_THREADS_SUMMARY: &str = "/api/v1/me/threads/summary";
/// `GET`: READY's sync section over HTTP.
pub const ME_SYNC: &str = "/api/v1/me/sync";

/// `GET` read, `POST` set: install org policies.
pub const INSTALL_POLICIES: &str = "/api/v1/install/policies";

// ---- Spaces, categories, channels, membership, DMs ----

/// `POST`: set a member's install role (install owner only).
pub const INSTALL_MEMBER_ROLE: &str = "/api/v1/install/members/{id}/role";
/// `POST`: make a member a guest or a guest a member (standalone only).
pub const INSTALL_MEMBER_GUEST: &str = "/api/v1/install/members/{id}/guest";
/// `GET`: every guest with its spaces and channels (install owner and admins).
pub const INSTALL_GUESTS: &str = "/api/v1/install/guests";
/// `GET` list, `POST` create: spaces.
pub const SPACES: &str = "/api/v1/spaces";
/// `GET`: one space.
pub const SPACE: &str = "/api/v1/spaces/{id}";
/// `POST`: update a space.
pub const SPACE_UPDATE: &str = "/api/v1/spaces/{id}/update";
/// `POST`: join an open space.
pub const SPACE_JOIN: &str = "/api/v1/spaces/{id}/join";
/// `POST`: leave a space.
pub const SPACE_LEAVE: &str = "/api/v1/spaces/{id}/leave";
/// `GET`: space members.
pub const SPACE_MEMBERS: &str = "/api/v1/spaces/{id}/members";
/// `POST`: add space members.
pub const SPACE_MEMBERS_ADD: &str = "/api/v1/spaces/{id}/members/add";
/// `POST`: remove a space member.
pub const SPACE_MEMBERS_REMOVE: &str = "/api/v1/spaces/{id}/members/remove";
/// `POST`: transfer space ownership.
pub const SPACE_TRANSFER: &str = "/api/v1/spaces/{id}/transfer";
/// `GET` list, `POST` create: a space's categories.
pub const SPACE_CATEGORIES: &str = "/api/v1/spaces/{id}/categories";
/// `POST`: reorder a space's categories.
pub const SPACE_CATEGORIES_REORDER: &str = "/api/v1/spaces/{id}/categories/reorder";
/// `POST`: update a category.
pub const CATEGORY_UPDATE: &str = "/api/v1/categories/{id}/update";
/// `POST`: delete a category.
pub const CATEGORY_DELETE: &str = "/api/v1/categories/{id}/delete";
/// `GET` list, `POST` create: a space's channels.
pub const SPACE_CHANNELS: &str = "/api/v1/spaces/{id}/channels";
/// `POST`: reorder a space's channels.
pub const SPACE_CHANNELS_REORDER: &str = "/api/v1/spaces/{id}/channels/reorder";
/// `GET`: one channel or DM.
pub const CHANNEL: &str = "/api/v1/channels/{id}";
/// `POST`: update a channel.
pub const CHANNEL_UPDATE: &str = "/api/v1/channels/{id}/update";
/// `POST`: unarchive a channel.
pub const CHANNEL_UNARCHIVE: &str = "/api/v1/channels/{id}/unarchive";
/// `POST`: join a channel.
pub const CHANNEL_JOIN: &str = "/api/v1/channels/{id}/join";
/// `POST`: leave a channel.
pub const CHANNEL_LEAVE: &str = "/api/v1/channels/{id}/leave";
/// `GET`: channel members.
pub const CHANNEL_MEMBERS: &str = "/api/v1/channels/{id}/members";
/// `POST`: add channel members.
pub const CHANNEL_MEMBERS_ADD: &str = "/api/v1/channels/{id}/members/add";
/// `POST`: remove a channel member.
pub const CHANNEL_MEMBERS_REMOVE: &str = "/api/v1/channels/{id}/members/remove";
/// `GET`: every channel and DM the caller belongs to.
pub const ME_CHANNELS: &str = "/api/v1/me/channels";
/// `POST`: find or create a DM or group DM.
pub const DMS: &str = "/api/v1/dms";

// ---- Messaging

/// `POST`: forward a message into another channel or thread.
pub const MESSAGE_FORWARD: &str = "/api/v1/messages/{id}/forward";
/// `POST`: add a reaction.
pub const MESSAGE_REACTIONS_ADD: &str = "/api/v1/messages/{id}/reactions/add";
/// `POST`: remove a reaction.
pub const MESSAGE_REACTIONS_REMOVE: &str = "/api/v1/messages/{id}/reactions/remove";
/// `GET`: who reacted with one emoji.
pub const MESSAGE_REACTORS: &str = "/api/v1/messages/{id}/reactors";
/// `POST`: pin a message.
pub const MESSAGE_PIN: &str = "/api/v1/messages/{id}/pin";
/// `POST`: unpin a message.
pub const MESSAGE_UNPIN: &str = "/api/v1/messages/{id}/unpin";
/// `GET`: a channel's pinned messages.
pub const CHANNEL_PINS: &str = "/api/v1/channels/{id}/pins";
/// `GET`: a thread's replies (`{id}` = the root).
pub const THREAD_MESSAGES: &str = "/api/v1/messages/{id}/replies";
/// `POST`: follow a thread (`{id}` = the root).
pub const THREAD_FOLLOW: &str = "/api/v1/threads/{id}/follow";
/// `POST`: unfollow a thread.
pub const THREAD_UNFOLLOW: &str = "/api/v1/threads/{id}/unfollow";
/// `POST`: delete a whole thread (MANAGE_THREADS).
pub const THREAD_DELETE: &str = "/api/v1/threads/{id}/delete";

// ---- Roles, overwrites, moderation, policies ----

/// `GET` list, `POST` create: a space's roles.
pub const SPACE_ROLES: &str = "/api/v1/spaces/{id}/roles";
/// `POST`: reorder a space's roles.
pub const SPACE_ROLES_REORDER: &str = "/api/v1/spaces/{id}/roles/reorder";
/// `POST`: edit a role.
pub const ROLE_EDIT: &str = "/api/v1/roles/{id}/edit";
/// `POST`: delete a role.
pub const ROLE_DELETE: &str = "/api/v1/roles/{id}/delete";
/// `GET`: a role's holders.
pub const ROLE_MEMBERS: &str = "/api/v1/roles/{id}/members";
/// `GET`: one space member (roles, timeout).
pub const SPACE_MEMBER: &str = "/api/v1/spaces/{id}/members/{member_id}";
/// `POST`: assign roles to a space member.
pub const SPACE_MEMBER_ROLES_ADD: &str = "/api/v1/spaces/{id}/members/{member_id}/roles/add";
/// `POST`: remove roles from a space member.
pub const SPACE_MEMBER_ROLES_REMOVE: &str = "/api/v1/spaces/{id}/members/{member_id}/roles/remove";
/// `POST`: time a space member out.
pub const SPACE_MEMBER_TIMEOUT: &str = "/api/v1/spaces/{id}/members/{member_id}/timeout";
/// `POST`: end a space member's timeout.
pub const SPACE_MEMBER_TIMEOUT_CLEAR: &str =
    "/api/v1/spaces/{id}/members/{member_id}/timeout/clear";
/// `GET`: a page of a space's bans; `POST`: ban a member.
pub const SPACE_BANS: &str = "/api/v1/spaces/{id}/bans";
/// `GET`: one ban.
pub const SPACE_BAN: &str = "/api/v1/spaces/{id}/bans/{member_id}";
/// `POST`: lift a ban (does not re-add).
pub const SPACE_BAN_DELETE: &str = "/api/v1/spaces/{id}/bans/{member_id}/delete";
/// `GET`: a channel's effective overwrites.
pub const CHANNEL_OVERWRITES: &str = "/api/v1/channels/{id}/overwrites";
/// `POST`: set one channel overwrite.
pub const CHANNEL_OVERWRITES_SET: &str = "/api/v1/channels/{id}/overwrites/set";
/// `POST`: delete one channel overwrite.
pub const CHANNEL_OVERWRITES_DELETE: &str = "/api/v1/channels/{id}/overwrites/delete";
/// `GET`: a category's overwrites.
pub const CATEGORY_OVERWRITES: &str = "/api/v1/categories/{id}/overwrites";
/// `POST`: set one category overwrite.
pub const CATEGORY_OVERWRITES_SET: &str = "/api/v1/categories/{id}/overwrites/set";
/// `POST`: delete one category overwrite.
pub const CATEGORY_OVERWRITES_DELETE: &str = "/api/v1/categories/{id}/overwrites/delete";
/// `POST`: set a channel's slow mode.
pub const CHANNEL_SLOW_MODE_SET: &str = "/api/v1/channels/{id}/slow_mode/set";

// ---- Files. The upload create route is `CHANNEL_UPLOADS`.

/// `POST` a chunk, `GET` the committed offset: the signed upload data URL (no Bearer).
pub const UPLOAD_DATA: &str = "/api/v1/uploads/{id}/data";
/// `GET`: an upload, with a fresh data URL (the uploader only).
pub const UPLOAD: &str = "/api/v1/uploads/{id}";
/// `POST`: abort an upload (the uploader only).
pub const UPLOAD_ABORT: &str = "/api/v1/uploads/{id}/abort";
/// `POST`: remove attachments from a message (author or moderator).
pub const MESSAGE_ATTACHMENTS_REMOVE: &str = "/api/v1/messages/{id}/attachments/remove";
/// `GET`: a channel's files.
pub const CHANNEL_FILES: &str = "/api/v1/channels/{id}/files";
/// `GET`: one file as JSON (Bearer), or with `exp` and `sig` its bytes (signed; `HEAD` too).
pub const FILE: &str = "/api/v1/files/{id}";
/// `GET`/`HEAD`: a file's thumbnail (signed).
pub const FILE_THUMB: &str = "/api/v1/files/{id}/thumb";
/// `GET`/`HEAD`: a file's large preview (signed).
pub const FILE_PREVIEW: &str = "/api/v1/files/{id}/preview";
/// `GET`/`HEAD`: a video's poster frame (signed).
pub const FILE_POSTER: &str = "/api/v1/files/{id}/poster";
// ---- Custom emoji, space icons, GIFs, link previews, secrets ----

/// `GET` list, `POST` create (raw image; `?name=&aliases=`): a space's custom emoji.
pub const SPACE_EMOJI: &str = "/api/v1/spaces/{id}/emoji";
/// `GET`: one custom emoji.
pub const EMOJI: &str = "/api/v1/emoji/{id}";
/// `POST`: rename a custom emoji or replace its aliases.
pub const EMOJI_EDIT: &str = "/api/v1/emoji/{id}/edit";
/// `POST`: delete a custom emoji.
pub const EMOJI_DELETE: &str = "/api/v1/emoji/{id}/delete";
/// `GET` (signed, no Bearer): a custom emoji image; `{variant}` = `full` | `still`.
pub const EMOJI_IMAGE: &str = "/api/v1/emoji-images/{id}/{variant}";
/// `POST` (raw image): set a space's icon.
pub const SPACE_ICON: &str = "/api/v1/spaces/{id}/icon";
/// `POST`: clear a space's icon.
pub const SPACE_ICON_CLEAR: &str = "/api/v1/spaces/{id}/icon/clear";
/// `GET` (signed, no Bearer): a space icon; `{size}` = `512` | `64`.
pub const SPACE_ICON_IMAGE: &str = "/api/v1/space-icons/{id}/{icon_id}/{size}";
/// `GET`: GIF search (when `gifs.enabled`).
pub const GIFS_SEARCH: &str = "/api/v1/gifs/search";
/// `GET`: trending GIFs (when `gifs.enabled`).
pub const GIFS_TRENDING: &str = "/api/v1/gifs/trending";
/// `GET` (signed, no Bearer): a proxied GIF thumbnail.
pub const GIFS_MEDIA: &str = "/api/v1/gifs/media/{token}";
/// `POST`: fetch a picked GIF into a pending upload of the channel.
pub const CHANNEL_GIFS_ATTACH: &str = "/api/v1/channels/{id}/gifs/attach";
/// `POST`: turn a message's link previews off.
pub const MESSAGE_PREVIEWS_SUPPRESS: &str = "/api/v1/messages/{id}/previews/suppress";
/// `GET` (signed, no Bearer): a stored link-preview image.
pub const PREVIEW_IMAGE: &str = "/api/v1/previews/{id}/image";
/// `GET`: install secrets' status (install owner or admin; values never returned).
pub const INSTALL_SECRETS: &str = "/api/v1/install/secrets";
/// `POST`: set an install secret.
pub const INSTALL_SECRETS_SET: &str = "/api/v1/install/secrets/set";
/// `POST`: clear an install secret.
pub const INSTALL_SECRETS_CLEAR: &str = "/api/v1/install/secrets/clear";

// ---- Search (POST reads keep query text out of URLs) ----

/// `POST`: search messages (a read; `Idempotency-Key` is accepted and ignored).
pub const SEARCH_MESSAGES: &str = "/api/v1/search/messages";
/// `POST`: search file names (a read; `Idempotency-Key` is accepted and ignored).
pub const SEARCH_FILES: &str = "/api/v1/search/files";

/// Mgmt API base (Unix socket only).
pub const MGMT_BASE: &str = "/mgmt/v1";
/// `GET`: liveness.
pub const MGMT_HEALTH: &str = "/mgmt/v1/health";
/// `GET`: status.
pub const MGMT_STATUS: &str = "/mgmt/v1/status";
/// `GET`: Prometheus metrics.
pub const MGMT_METRICS: &str = "/mgmt/v1/metrics";
/// `POST`: provision a principal.
pub const MGMT_PROVISION: &str = "/mgmt/v1/principals/provision";
/// `POST`: update a principal's profile.
pub const MGMT_UPDATE: &str = "/mgmt/v1/principals/update";
/// `POST`: deprovision a principal.
pub const MGMT_DEPROVISION: &str = "/mgmt/v1/principals/deprovision";
/// `POST`: give a local member a K2 identity, keeping its member id.
pub const MGMT_PRINCIPALS_RELINK: &str = "/mgmt/v1/principals/relink";
/// `POST`: switch the install's identity mode from standalone to K2, once.
pub const MGMT_IDENTITY_SWITCH: &str = "/mgmt/v1/identity/switch";
/// `POST`: revoke sessions.
pub const MGMT_SESSIONS_REVOKE: &str = "/mgmt/v1/sessions/revoke";
/// `POST`: set the install owner.
pub const MGMT_OWNER_SET: &str = "/mgmt/v1/owner/set";
/// `POST`: create a Space with an owner, managers and an optional App.
pub const MGMT_SPACES_CREATE: &str = "/mgmt/v1/spaces/create";
/// `POST`: bind a Space to an App, once.
pub const MGMT_SPACES_BIND: &str = "/mgmt/v1/spaces/bind";
/// `GET`, `POST`: a Space's settings (`private`).
pub const MGMT_SPACE_SETTINGS: &str = "/mgmt/v1/spaces/{space_id}/settings";
/// `GET`: a Space's App links (`space_apps`).
pub const MGMT_SPACE_APPS: &str = "/mgmt/v1/spaces/{space_id}/apps";
/// `POST`: link a participant App to a Space (Sew 0.8.0).
pub const MGMT_SPACE_APPS_LINK: &str = "/mgmt/v1/spaces/{space_id}/apps/link";
/// `POST`: remove a participant App's link to a Space (Sew 0.8.0).
pub const MGMT_SPACE_APPS_UNLINK: &str = "/mgmt/v1/spaces/{space_id}/apps/unlink";
/// `POST`: one person's private channels and level in an App-governed Space.
pub const MGMT_SPACE_MEMBER_CHANNELS: &str =
    "/mgmt/v1/spaces/{space_id}/members/{external_id}/channels";
/// `POST`: the bulk form of [`MGMT_SPACE_MEMBER_CHANNELS`], 1–500 items.
pub const MGMT_SPACE_MEMBERS_CHANNELS: &str = "/mgmt/v1/spaces/{space_id}/members/channels";
/// `POST`: unbind a Space from its App; it stays as a read-only orphan.
pub const MGMT_SPACES_UNBIND: &str = "/mgmt/v1/spaces/unbind";
/// `POST`: release an orphaned Space from its old App, so another App may bind it.
pub const MGMT_SPACES_RELEASE: &str = "/mgmt/v1/spaces/release";
/// `GET`, `POST`: the install's home client, one App id or none.
pub const MGMT_HOME_CLIENT: &str = "/mgmt/v1/home-client";
/// `GET`, `POST`: one App's settings (`hidden_from_home`, `display_name`, `origin`).
pub const MGMT_APP_SETTINGS: &str = "/mgmt/v1/apps/{app_id}/settings";
/// `GET`: every Space link of one App, both roles, paged (Sew 0.8.0).
pub const MGMT_APP_SPACES: &str = "/mgmt/v1/apps/{app_id}/spaces";
/// `POST`: add principals to a Space.
pub const MGMT_SPACE_MEMBERS_ADD: &str = "/mgmt/v1/spaces/members/add";
/// `POST`: remove a principal from a Space.
pub const MGMT_SPACE_MEMBERS_REMOVE: &str = "/mgmt/v1/spaces/members/remove";
/// `POST`: create a channel in an App-bound Space, idempotent on the slug.
pub const MGMT_CHANNELS_CREATE: &str = "/mgmt/v1/channels/create";
/// `POST`: bulk channel ops (ensure, rename, archive, unarchive) in a Space an App
/// governs, answered per op.
pub const MGMT_CHANNELS_BULK: &str = "/mgmt/v1/channels/bulk";
/// `GET`: every voice state in a Space's voice channels, with the feed cursor.
pub const MGMT_SPACE_VOICE_STATES: &str = "/mgmt/v1/spaces/{space_id}/voice/states";
/// `GET`: a Space's voice changes after a cursor (long-poll).
pub const MGMT_SPACE_VOICE_CHANGES: &str = "/mgmt/v1/spaces/{space_id}/voice/changes";
/// `POST`: close (or reopen) a voice room.
pub const MGMT_CHANNEL_CLOSING: &str = "/mgmt/v1/channels/{channel_id}/closing";
/// `POST`: K2 reports the media path (the TURN lane) down or up.
pub const MGMT_VOICE_MEDIA_PATH: &str = "/mgmt/v1/voice/media-path";
/// `GET` and `POST`: an App's quotas and their usage.
pub const MGMT_APP_QUOTAS: &str = "/mgmt/v1/apps/{app_id}/quotas";
/// `POST`: create the hidden Meetings Space and its retention, once (idempotent).
pub const MGMT_MEET_INIT: &str = "/mgmt/v1/meet/init";
/// `POST`: pin an assistant to a person.
pub const MGMT_AGENTS_PIN: &str = "/mgmt/v1/agents/pin";
/// `POST`: unpin an assistant.
pub const MGMT_AGENTS_UNPIN: &str = "/mgmt/v1/agents/unpin";
/// `GET`: an agent's delivery feed (long-poll).
pub const MGMT_AGENT_DELIVERIES: &str = "/mgmt/v1/agents/{member_id}/deliveries";
/// `GET`: the person notification feed (rings, missed calls, mentions, DMs, replies),
/// long-polled by K2 for App push.
pub const MGMT_PERSON_NOTIFICATIONS: &str = "/mgmt/v1/people/{external_id}/notifications";
/// `POST`: start a backup job.
pub const MGMT_BACKUP: &str = "/mgmt/v1/backup";
/// `GET`: the complete bundles, newest first.
pub const MGMT_BACKUPS: &str = "/mgmt/v1/backups";
/// `GET`: one bundle as a stable tar.
pub const MGMT_BACKUP_ARCHIVE: &str = "/mgmt/v1/backups/{id}/archive";
/// `POST`: start an export job.
pub const MGMT_EXPORT: &str = "/mgmt/v1/export";
/// `GET`: job status.
pub const MGMT_JOB: &str = "/mgmt/v1/jobs/{id}";
/// `GET`: jobs, newest first.
pub const MGMT_JOBS: &str = "/mgmt/v1/jobs";
/// `POST`: requeue a dead job.
pub const MGMT_JOB_RETRY: &str = "/mgmt/v1/jobs/{id}/retry";
/// `GET`: principals, paged by member id.
pub const MGMT_PRINCIPALS: &str = "/mgmt/v1/principals";
/// `POST`: create a standalone local user.
pub const MGMT_LOCAL_USERS: &str = "/mgmt/v1/local/users";
/// `POST`: issue a setup link, or bootstrap the first owner.
pub const MGMT_LOCAL_SETUP_LINK: &str = "/mgmt/v1/local/setup-link";
/// `POST`: reactivate a local user keeping its profile.
pub const MGMT_LOCAL_REACTIVATE: &str = "/mgmt/v1/local/users/reactivate";
/// `POST`: set a local user's install role.
pub const MGMT_LOCAL_ROLE: &str = "/mgmt/v1/local/users/role";
/// `POST`: make a local user a guest or a member (standalone only).
pub const MGMT_LOCAL_USERS_GUEST: &str = "/mgmt/v1/local/users/guest";
/// `GET` read, `POST` set: org policies over mgmt.
pub const MGMT_POLICIES: &str = "/mgmt/v1/policies";
/// `POST`: start a master-key re-wrap job.
pub const MGMT_KEYS_ROTATE_MASTER: &str = "/mgmt/v1/keys/rotate-master";
/// `POST`: start an audit chain check.
pub const MGMT_AUDIT_VERIFY: &str = "/mgmt/v1/audit/verify";
/// `GET`: install secrets' status over mgmt.
pub const MGMT_SECRETS: &str = "/mgmt/v1/secrets";
/// `POST`: set an install secret over mgmt.
pub const MGMT_SECRETS_SET: &str = "/mgmt/v1/secrets/set";
/// `POST`: clear an install secret over mgmt.
pub const MGMT_SECRETS_CLEAR: &str = "/mgmt/v1/secrets/clear";

// ---- People

/// `GET`: one member (with custom fields the caller may see).
pub const MEMBER: &str = "/api/v1/members/{id}";
/// `GET`: members by `ids=` or `username=`.
pub const MEMBERS: &str = "/api/v1/members";
/// `POST`: an admin resets another member's avatar or status.
pub const MEMBER_PROFILE_RESET: &str = "/api/v1/members/{id}/profile/reset";
/// `POST`: edit the caller's profile.
pub const ME_PROFILE: &str = "/api/v1/me/profile";
/// `POST`: upload the caller's avatar (raw image body).
pub const ME_AVATAR: &str = "/api/v1/me/avatar";
/// `POST`: remove the caller's avatar.
pub const ME_AVATAR_CLEAR: &str = "/api/v1/me/avatar/clear";
/// `GET`: a signed avatar (`?exp=&sig=`, no Bearer).
pub const AVATAR: &str = "/api/v1/avatars/{member_id}/{avatar_id}/{size}";
/// `POST`: set the caller's status.
pub const ME_STATUS: &str = "/api/v1/me/status";
/// `POST`: clear the caller's status.
pub const ME_STATUS_CLEAR: &str = "/api/v1/me/status/clear";
/// `GET` read, `POST` set: the caller's presence settings.
pub const ME_PRESENCE: &str = "/api/v1/me/presence";
/// `POST`: set or remove the caller's DND schedule.
pub const ME_DND_SCHEDULE: &str = "/api/v1/me/dnd/schedule";
/// `POST`: end DND now.
pub const ME_DND_END: &str = "/api/v1/me/dnd/end";
/// `GET`: presence by `ids=`.
pub const PRESENCE: &str = "/api/v1/presence";
/// `GET`: the people directory.
pub const DIRECTORY: &str = "/api/v1/directory";
/// `GET` list, `POST` create: custom profile field definitions.
pub const INSTALL_PROFILE_FIELDS: &str = "/api/v1/install/profile-fields";
/// `POST`: update a custom field definition.
pub const INSTALL_PROFILE_FIELD_UPDATE: &str = "/api/v1/install/profile-fields/{id}/update";
/// `POST`: delete a custom field definition.
pub const INSTALL_PROFILE_FIELD_DELETE: &str = "/api/v1/install/profile-fields/{id}/delete";
/// `POST`: reorder custom field definitions.
pub const INSTALL_PROFILE_FIELDS_REORDER: &str = "/api/v1/install/profile-fields/reorder";
/// `GET` list, `POST` create: user groups.
pub const GROUPS: &str = "/api/v1/groups";
/// `GET`: one user group.
pub const GROUP: &str = "/api/v1/groups/{id}";
/// `POST`: update a user group.
pub const GROUP_UPDATE: &str = "/api/v1/groups/{id}/update";
/// `POST`: delete a user group.
pub const GROUP_DELETE: &str = "/api/v1/groups/{id}/delete";
/// `GET`: a user group's members.
pub const GROUP_MEMBERS: &str = "/api/v1/groups/{id}/members";
/// `POST`: add user group members.
pub const GROUP_MEMBERS_ADD: &str = "/api/v1/groups/{id}/members/add";
/// `POST`: remove user group members.
pub const GROUP_MEMBERS_REMOVE: &str = "/api/v1/groups/{id}/members/remove";
/// `GET`: mention targets for autocomplete in a channel.
pub const CHANNEL_MENTIONABLES: &str = "/api/v1/channels/{id}/mentionables";
/// `POST`: K2 pushes an avatar (raw image body, `?external_id=&force=`).
pub const MGMT_PRINCIPAL_AVATAR: &str = "/mgmt/v1/principals/avatar";
/// `POST`: K2 clears a pushed avatar.
pub const MGMT_PRINCIPAL_AVATAR_CLEAR: &str = "/mgmt/v1/principals/avatar/clear";
/// `GET`: every custom profile field definition.
pub const MGMT_PROFILE_FIELDS: &str = "/mgmt/v1/profile-fields";
/// `POST`: create or update a custom field definition by key.
pub const MGMT_PROFILE_FIELDS_UPSERT: &str = "/mgmt/v1/profile-fields/upsert";
/// `POST`: rebuild the search index from Postgres.
pub const MGMT_SEARCH_REBUILD: &str = "/mgmt/v1/search/rebuild";

// ---- Retention, legal holds. Compliance-flag holders only.

/// `GET`: retention policy rows.
pub const COMPLIANCE_RETENTION: &str = "/api/v1/compliance/retention";
/// `GET`: what applies to one channel (`?channel_id=`).
pub const COMPLIANCE_RETENTION_EFFECTIVE: &str = "/api/v1/compliance/retention/effective";
/// `POST`: start a preview (dry run) of a proposal.
pub const COMPLIANCE_RETENTION_PREVIEW: &str = "/api/v1/compliance/retention/preview";
/// `GET`: one preview.
pub const COMPLIANCE_RETENTION_PREVIEW_GET: &str = "/api/v1/compliance/retention/previews/{id}";
/// `POST`: set a policy (from a preview when shorter).
pub const COMPLIANCE_RETENTION_SET: &str = "/api/v1/compliance/retention/set";
/// `POST`: cancel a pending policy.
pub const COMPLIANCE_RETENTION_CANCEL: &str = "/api/v1/compliance/retention/{id}/cancel";
/// `POST`: queue a retention run now.
pub const COMPLIANCE_RETENTION_RUN: &str = "/api/v1/compliance/retention/run";
/// `GET`: retention runs, newest first.
pub const COMPLIANCE_RETENTION_RUNS: &str = "/api/v1/compliance/retention/runs";
/// `GET` list, `POST` create: legal holds.
pub const COMPLIANCE_HOLDS: &str = "/api/v1/compliance/holds";
/// `GET`: one legal hold.
pub const COMPLIANCE_HOLD: &str = "/api/v1/compliance/holds/{id}";
/// `POST`: widen a legal hold.
pub const COMPLIANCE_HOLD_UPDATE: &str = "/api/v1/compliance/holds/{id}/update";
/// `POST`: release a legal hold.
pub const COMPLIANCE_HOLD_RELEASE: &str = "/api/v1/compliance/holds/{id}/release";
/// `GET`: every channel, private channels and DMs included (metadata only).
pub const COMPLIANCE_CHANNELS: &str = "/api/v1/compliance/channels";
/// Mgmt twin of [`COMPLIANCE_RETENTION`].
pub const MGMT_COMPLIANCE_RETENTION: &str = "/mgmt/v1/compliance/retention";
/// Mgmt twin of [`COMPLIANCE_RETENTION_EFFECTIVE`].
pub const MGMT_COMPLIANCE_RETENTION_EFFECTIVE: &str = "/mgmt/v1/compliance/retention/effective";
/// Mgmt twin of [`COMPLIANCE_RETENTION_PREVIEW`].
pub const MGMT_COMPLIANCE_RETENTION_PREVIEW: &str = "/mgmt/v1/compliance/retention/preview";
/// Mgmt twin of [`COMPLIANCE_RETENTION_PREVIEW_GET`].
pub const MGMT_COMPLIANCE_RETENTION_PREVIEW_GET: &str =
    "/mgmt/v1/compliance/retention/previews/{id}";
/// Mgmt twin of [`COMPLIANCE_RETENTION_SET`].
pub const MGMT_COMPLIANCE_RETENTION_SET: &str = "/mgmt/v1/compliance/retention/set";
/// Mgmt twin of [`COMPLIANCE_RETENTION_CANCEL`].
pub const MGMT_COMPLIANCE_RETENTION_CANCEL: &str = "/mgmt/v1/compliance/retention/{id}/cancel";
/// Mgmt twin of [`COMPLIANCE_RETENTION_RUN`].
pub const MGMT_COMPLIANCE_RETENTION_RUN: &str = "/mgmt/v1/compliance/retention/run";
/// Mgmt twin of [`COMPLIANCE_RETENTION_RUNS`].
pub const MGMT_COMPLIANCE_RETENTION_RUNS: &str = "/mgmt/v1/compliance/retention/runs";
/// Mgmt twin of [`COMPLIANCE_HOLDS`].
pub const MGMT_COMPLIANCE_HOLDS: &str = "/mgmt/v1/compliance/holds";
/// Mgmt twin of [`COMPLIANCE_HOLD`].
pub const MGMT_COMPLIANCE_HOLD: &str = "/mgmt/v1/compliance/holds/{id}";
/// Mgmt twin of [`COMPLIANCE_HOLD_UPDATE`].
pub const MGMT_COMPLIANCE_HOLD_UPDATE: &str = "/mgmt/v1/compliance/holds/{id}/update";
/// Mgmt twin of [`COMPLIANCE_HOLD_RELEASE`].
pub const MGMT_COMPLIANCE_HOLD_RELEASE: &str = "/mgmt/v1/compliance/holds/{id}/release";
/// Mgmt twin of [`COMPLIANCE_CHANNELS`].
pub const MGMT_COMPLIANCE_CHANNELS: &str = "/mgmt/v1/compliance/channels";
// ---- Drafts, saved, scheduled, reminders, polls, bookmarks ----
/// `POST`: save the caller's draft in a channel (or one of its threads).
pub const CHANNEL_DRAFT: &str = "/api/v1/channels/{id}/draft";
/// `POST`: delete the caller's draft in a channel (or one of its threads).
pub const CHANNEL_DRAFT_DELETE: &str = "/api/v1/channels/{id}/draft/delete";
/// `GET`: the caller's drafts.
pub const ME_DRAFTS: &str = "/api/v1/me/drafts";
/// `POST`: save a message for later.
pub const MESSAGE_SAVE: &str = "/api/v1/messages/{id}/save";
/// `POST`: remove a message from the caller's saved items.
pub const MESSAGE_UNSAVE: &str = "/api/v1/messages/{id}/unsave";
/// `POST`: change a saved item's state or note.
pub const SAVED_EDIT: &str = "/api/v1/saved/{id}/edit";
/// `POST`: delete a saved item.
pub const SAVED_DELETE: &str = "/api/v1/saved/{id}/delete";
/// `GET`: the caller's saved items.
pub const ME_SAVED: &str = "/api/v1/me/saved";
/// `POST`: schedule a message in a channel.
pub const CHANNEL_SCHEDULED: &str = "/api/v1/channels/{id}/scheduled";
/// `POST`: edit or reschedule a scheduled message.
pub const SCHEDULED_EDIT: &str = "/api/v1/scheduled/{id}/edit";
/// `POST`: send a scheduled message now.
pub const SCHEDULED_SEND_NOW: &str = "/api/v1/scheduled/{id}/send_now";
/// `POST`: cancel a scheduled message.
pub const SCHEDULED_DELETE: &str = "/api/v1/scheduled/{id}/delete";
/// `GET`: the caller's scheduled messages.
pub const ME_SCHEDULED: &str = "/api/v1/me/scheduled";
/// `GET` list, `POST` create: the caller's reminders.
pub const ME_REMINDERS: &str = "/api/v1/me/reminders";
/// `POST`: edit a reminder.
pub const REMINDER_EDIT: &str = "/api/v1/reminders/{id}/edit";
/// `POST`: snooze a reminder.
pub const REMINDER_SNOOZE: &str = "/api/v1/reminders/{id}/snooze";
/// `POST`: mark a reminder done.
pub const REMINDER_COMPLETE: &str = "/api/v1/reminders/{id}/complete";
/// `POST`: delete a reminder.
pub const REMINDER_DELETE: &str = "/api/v1/reminders/{id}/delete";
/// `POST`: vote in a message's poll (the set replaces the caller's selection).
pub const MESSAGE_POLL_VOTE: &str = "/api/v1/messages/{id}/poll/vote";
/// `POST`: close a message's poll.
pub const MESSAGE_POLL_CLOSE: &str = "/api/v1/messages/{id}/poll/close";
/// `GET`: who voted for one option of a non-anonymous poll.
pub const MESSAGE_POLL_VOTERS: &str = "/api/v1/messages/{id}/poll/voters";
/// `GET` list, `POST` create: a channel's bookmarks.
pub const CHANNEL_BOOKMARKS: &str = "/api/v1/channels/{id}/bookmarks";
/// `POST`: reorder a channel's bookmarks.
pub const CHANNEL_BOOKMARKS_REORDER: &str = "/api/v1/channels/{id}/bookmarks/reorder";
/// `POST`: edit a bookmark.
pub const BOOKMARK_EDIT: &str = "/api/v1/bookmarks/{id}/edit";
/// `POST`: delete a bookmark.
pub const BOOKMARK_DELETE: &str = "/api/v1/bookmarks/{id}/delete";

// ---- Notifications and the email digest.

/// `GET`: the caller's notification settings.
pub const ME_NOTIFY: &str = "/api/v1/me/notify";
/// `POST`: change the caller's notification settings.
pub const ME_NOTIFY_SET: &str = "/api/v1/me/notify/set";
/// `GET`: the caller's explicit channel and space prefs.
pub const ME_NOTIFY_PREFS: &str = "/api/v1/me/notify/prefs";
/// `GET`: the caller's prefs for a channel or DM.
pub const CHANNEL_NOTIFY: &str = "/api/v1/channels/{id}/notify";
/// `POST`: set the caller's level or mute for a channel or DM.
pub const CHANNEL_NOTIFY_SET: &str = "/api/v1/channels/{id}/notify/set";
/// `GET`: the caller's level for a space.
pub const SPACE_NOTIFY: &str = "/api/v1/spaces/{id}/notify";
/// `POST`: set the caller's level for a space.
pub const SPACE_NOTIFY_SET: &str = "/api/v1/spaces/{id}/notify/set";
/// `GET` (a confirm page) and `POST` (one-click, RFC 8058): a signed digest unsubscribe link.
pub const DIGEST_UNSUBSCRIBE: &str = "/api/v1/digest/unsubscribe/{id}";
/// `POST`: send a test mail through `[mail.smtp]`.
pub const MGMT_MAIL_TEST: &str = "/mgmt/v1/mail/test";
// ---- Compliance flag, audit API, exports ----
/// `POST`: grant or revoke a member's compliance flag (install owner).
pub const INSTALL_MEMBER_COMPLIANCE: &str = "/api/v1/install/members/{id}/compliance";
/// `GET`: active compliance-flag holders (owner or flag).
pub const COMPLIANCE_OFFICERS: &str = "/api/v1/compliance/officers";
/// `GET` list, `POST` create: eDiscovery exports.
pub const COMPLIANCE_EXPORTS: &str = "/api/v1/compliance/exports";
/// `POST`: estimate an eDiscovery scope.
pub const COMPLIANCE_EXPORTS_ESTIMATE: &str = "/api/v1/compliance/exports/estimate";
/// `GET`: one eDiscovery export.
pub const COMPLIANCE_EXPORT: &str = "/api/v1/compliance/exports/{id}";
/// `POST`: cancel a queued or running eDiscovery export.
pub const COMPLIANCE_EXPORT_CANCEL: &str = "/api/v1/compliance/exports/{id}/cancel";
/// `POST`: delete an eDiscovery archive.
pub const COMPLIANCE_EXPORT_DELETE: &str = "/api/v1/compliance/exports/{id}/delete";
/// `POST`: a 1 h signed download link bound to the caller.
pub const COMPLIANCE_EXPORT_LINK: &str = "/api/v1/compliance/exports/{id}/link";
/// `GET`: the archive bytes (Bearer, or the signed link; `Range` supported).
pub const COMPLIANCE_EXPORT_ARCHIVE: &str = "/api/v1/compliance/exports/{id}/archive";
/// `GET`: audit rows the caller may read.
pub const AUDIT: &str = "/api/v1/audit";
/// `GET`: the audit chain's head and last verification.
pub const AUDIT_CHAIN: &str = "/api/v1/audit/chain";
/// `POST`: start an audit chain check.
pub const AUDIT_VERIFY: &str = "/api/v1/audit/verify";
/// `GET`: every audit row (mgmt service).
pub const MGMT_AUDIT: &str = "/mgmt/v1/audit";
/// `POST`: grant or revoke a principal's compliance flag.
pub const MGMT_PRINCIPALS_COMPLIANCE: &str = "/mgmt/v1/principals/compliance";
/// `GET`: exports of both kinds.
pub const MGMT_EXPORTS: &str = "/mgmt/v1/exports";
/// `GET`: one export.
pub const MGMT_EXPORT_GET: &str = "/mgmt/v1/exports/{id}";
/// `GET`: an export's archive bytes (`Range` supported).
pub const MGMT_EXPORT_ARCHIVE: &str = "/mgmt/v1/exports/{id}/archive";
/// `POST`: cancel an export.
pub const MGMT_EXPORT_CANCEL: &str = "/mgmt/v1/exports/{id}/cancel";
/// `POST`: delete an export's archive.
pub const MGMT_EXPORT_DELETE: &str = "/mgmt/v1/exports/{id}/delete";
/// Mgmt twin of [`COMPLIANCE_EXPORTS`].
pub const MGMT_COMPLIANCE_EXPORTS: &str = "/mgmt/v1/compliance/exports";
/// Mgmt twin of [`COMPLIANCE_EXPORTS_ESTIMATE`].
pub const MGMT_COMPLIANCE_EXPORTS_ESTIMATE: &str = "/mgmt/v1/compliance/exports/estimate";
/// Mgmt twin of [`COMPLIANCE_EXPORT`].
pub const MGMT_COMPLIANCE_EXPORT: &str = "/mgmt/v1/compliance/exports/{id}";
/// Mgmt twin of [`COMPLIANCE_EXPORT_CANCEL`].
pub const MGMT_COMPLIANCE_EXPORT_CANCEL: &str = "/mgmt/v1/compliance/exports/{id}/cancel";
/// Mgmt twin of [`COMPLIANCE_EXPORT_DELETE`].
pub const MGMT_COMPLIANCE_EXPORT_DELETE: &str = "/mgmt/v1/compliance/exports/{id}/delete";
/// Mgmt twin of [`COMPLIANCE_EXPORT_ARCHIVE`].
pub const MGMT_COMPLIANCE_EXPORT_ARCHIVE: &str = "/mgmt/v1/compliance/exports/{id}/archive";

// ---- Voice channels ----
// (`CHANNEL_VOICE_JOIN` above is the join; its body carries the `e2ee` object.)

/// `GET`: the caller's voice state (`{state}`).
pub const ME_VOICE: &str = "/api/v1/me/voice";
/// `POST`: leave voice.
pub const ME_VOICE_LEAVE: &str = "/api/v1/me/voice/leave";
/// `POST`: set self-mute / self-deafen.
pub const ME_VOICE_STATE: &str = "/api/v1/me/voice/state";
/// `POST`: one aggregate call-stats sample of the caller's current call (while
/// `calls.stats_beacon` is on).
pub const ME_VOICE_STATS: &str = "/api/v1/me/voice/stats";
/// `POST` (Bearer `sewcc_` only): renew the call-scoped credential.
pub const ME_VOICE_CREDENTIAL: &str = "/api/v1/me/voice/credential";
/// `POST`: server-mute / server-deafen a space member.
pub const SPACE_MEMBER_VOICE: &str = "/api/v1/spaces/{id}/members/{member_id}/voice";
/// `POST`: move a space member to another voice channel, or disconnect them (`null`).
pub const SPACE_MEMBER_VOICE_MOVE: &str = "/api/v1/spaces/{id}/members/{member_id}/voice/move";
/// `GET`: voice states in a space's viewable channels.
pub const SPACE_VOICE_STATES: &str = "/api/v1/spaces/{id}/voice/states";
/// `GET`: voice states in one voice channel.
pub const CHANNEL_VOICE_STATES: &str = "/api/v1/channels/{id}/voice/states";

// ---- Call key delivery (the key join itself is `CHANNEL_VOICE_JOIN`) ----

/// `POST`: upload 1–2 KeyPackages; answers single-use refs.
pub const ME_VOICE_KEY_PACKAGES: &str = "/api/v1/me/voice/keys/key_packages";
/// `POST`: key join after a move, with the Sew-minted `join_id`.
pub const ME_VOICE_KEYS_JOIN: &str = "/api/v1/me/voice/keys/join";
/// `POST`: state report (create, re-bind, rebuild).
pub const CALL_KEYS_STATE: &str = "/api/v1/calls/{call_id}/keys/state";
/// `POST`: the designated committer's commit.
pub const CALL_KEYS_COMMIT: &str = "/api/v1/calls/{call_id}/keys/commit";
/// `POST`: every remote key of an epoch is installed.
pub const CALL_KEYS_READY: &str = "/api/v1/calls/{call_id}/keys/ready";
/// `POST`: re-add with a fresh inline KeyPackage.
pub const CALL_KEYS_INVALID: &str = "/api/v1/calls/{call_id}/keys/invalid";
/// `POST`: a fresh KeyPackage for a resetting call.
pub const CALL_KEYS_KEY_PACKAGE: &str = "/api/v1/calls/{call_id}/keys/key_package";
/// `POST`: the caller's sender key sealed to the recorder.
pub const CALL_KEYS_RELEASE: &str = "/api/v1/calls/{call_id}/keys/release";
/// `POST`: an immediate rekey (consent withdrawn).
pub const CALL_KEYS_REKEY: &str = "/api/v1/calls/{call_id}/keys/rekey";
/// `POST`: a roster, slot or engine report.
pub const CALL_KEYS_REPORT: &str = "/api/v1/calls/{call_id}/keys/report";
/// `POST`: enrol a device key.
pub const DEVICES: &str = "/api/v1/devices";
/// `POST`: renew a device certificate.
pub const DEVICE_RENEW: &str = "/api/v1/devices/{id}/renew";
/// `GET`: the caller's devices.
pub const ME_DEVICES: &str = "/api/v1/me/devices";
/// `POST`: revoke a device.
pub const DEVICE_REVOKE: &str = "/api/v1/devices/{id}/revoke";
/// `GET`: revoked serials, paged (`?after=&limit=`).
pub const DEVICES_CRL: &str = "/api/v1/devices/crl";

// ---- DM calls ----

/// `POST`: start or join the DM's call (body `CallJoinRequest`).
pub const CHANNEL_CALL_JOIN: &str = "/api/v1/channels/{id}/call/join";
/// `POST`: decline the caller's ring (body `CallDeclineRequest`).
pub const CHANNEL_CALL_DECLINE: &str = "/api/v1/channels/{id}/call/decline";
/// `POST`: ring members again (body `CallRingRequest`).
pub const CHANNEL_CALL_RING: &str = "/api/v1/channels/{id}/call/ring";
/// `GET`: the DM's active call.
pub const CHANNEL_CALL: &str = "/api/v1/channels/{id}/call";
/// `GET`: active calls in the caller's DMs.
pub const ME_CALLS: &str = "/api/v1/me/calls";
/// `GET`: one call, active or ended, by its `calls.id` (`{call_id}` shares the key routes'
/// segment name).
pub const CALL_BY_ID: &str = "/api/v1/calls/{call_id}";

// ---- Recording ----

/// `POST`: start recording the channel's live call (body `StartRecordingRequest`).
pub const CHANNEL_RECORDING_START: &str = "/api/v1/channels/{id}/recording/start";
/// `GET`: the channel's active recording (`ActiveRecording`).
pub const CHANNEL_RECORDING: &str = "/api/v1/channels/{id}/recording";
/// `GET`: the channel's recordings, newest first (`Page<Recording>`).
pub const CHANNEL_RECORDINGS: &str = "/api/v1/channels/{id}/recordings";
/// `GET`: one recording.
pub const RECORDING: &str = "/api/v1/recordings/{id}";
/// `POST`: stop a recording (body `{}`).
pub const RECORDING_STOP: &str = "/api/v1/recordings/{id}/stop";
/// `POST`: answer the in-room consent prompt (body `ConsentRequest`).
pub const RECORDING_CONSENT: &str = "/api/v1/recordings/{id}/consent";

// ---- Recording audio ----

/// `POST`: change a recording's audio keep days (body `AudioRetentionRequest`).
pub const RECORDING_AUDIO_RETENTION: &str = "/api/v1/recordings/{id}/audio/retention";
/// `GET`: the audio listing (`RecordingAudio`; `client_unseal` playback adds keys and URLs).
pub const RECORDING_AUDIO: &str = "/api/v1/recordings/{id}/audio";
/// `GET`: one speaker's segments (`Page<AudioSegment>`, `?after=<segment_id>`).
pub const RECORDING_AUDIO_TRACK: &str = "/api/v1/recordings/{id}/audio/tracks/{member_id}";
/// `GET`/`HEAD`: `client_unseal` playback only, recorder-sealed bytes of a segment or mix part
/// (signed, no Bearer).
pub const RECORDING_AUDIO_FILE: &str = "/api/v1/recordings/{id}/audio/files/{link_id}";
/// `POST`: a compliance download grant (body `DownloadRequest`, `201 RecordingDownload`).
pub const COMPLIANCE_RECORDING_DOWNLOADS: &str = "/api/v1/compliance/recordings/{id}/downloads";
/// `GET`: a grant's segments (`Page<DownloadSegment>`, `?after=<segment_id>`).
pub const COMPLIANCE_RECORDING_DOWNLOAD_SEGMENTS: &str =
    "/api/v1/compliance/recordings/{id}/downloads/{download_id}/segments";
/// `GET`/`HEAD`: recorder-sealed bytes for a grant (Bearer plus a viewer-bound signature,
/// `?d=<download_id>&exp=&sig=`).
pub const COMPLIANCE_RECORDING_FILE: &str = "/api/v1/compliance/recordings/{id}/files/{link_id}";

// ---- Meetings for members ----

/// `POST`: create a meeting (body `CreateMeetingRequest`, answers `MeetingCreated`); `GET`:
/// the caller's meetings (`?scope=upcoming|past&cursor=&limit=`, `MeetingPage`).
pub const MEETINGS: &str = "/api/v1/meetings";
/// `GET`: one meeting (`Meeting`; a masked 404 unless the caller is in its channel).
pub const MEETING: &str = "/api/v1/meetings/{id}";
/// `POST`: change a meeting (hosts; body `UpdateMeetingRequest`).
pub const MEETING_UPDATE: &str = "/api/v1/meetings/{id}/update";
/// `POST`: cancel a scheduled meeting (hosts; body `{}`).
pub const MEETING_CANCEL: &str = "/api/v1/meetings/{id}/cancel";
/// `GET`: the meeting's attendees, emails included (the organiser only).
pub const MEETING_ATTENDEES: &str = "/api/v1/meetings/{id}/attendees";
/// `POST`: remove one attendee (the organiser only; body `RemoveMeetingAttendeeRequest`).
pub const MEETING_ATTENDEES_REMOVE: &str = "/api/v1/meetings/{id}/attendees/remove";
/// `GET` a meeting's active link (hosts; the secret is unsealed for them).
pub const MEETING_LINK: &str = "/api/v1/meetings/{id}/link";
/// `POST` revoke the active link and make a new one (hosts).
pub const MEETING_LINK_ROTATE: &str = "/api/v1/meetings/{id}/link/rotate";
/// `POST` revoke the active link (hosts).
pub const MEETING_LINK_REVOKE: &str = "/api/v1/meetings/{id}/link/revoke";
/// `POST` what a link's page may show before a name is typed (public; no session).
pub const MEETINGS_PEEK: &str = "/api/v1/meetings/peek";
/// `POST` redeem a link for a link-guest session (public; no session).
pub const MEETINGS_REDEEM: &str = "/api/v1/meetings/redeem";
/// `POST` which meeting a link names, for a signed-in caller.
pub const MEETINGS_RESOLVE: &str = "/api/v1/meetings/resolve";
/// `GET` a meeting's waiting lobby entries (hosts; `MeetingLobby`).
pub const MEETING_LOBBY: &str = "/api/v1/meetings/{id}/lobby";
/// `POST` let a waiting entry in (hosts; body `{}`, answers `MeetingLobbyEntry`).
pub const MEETING_LOBBY_ADMIT: &str = "/api/v1/meetings/{id}/lobby/{entry}/admit";
/// `POST` do not let a waiting entry in (hosts; body `{}`).
pub const MEETING_LOBBY_DENY: &str = "/api/v1/meetings/{id}/lobby/{entry}/deny";
/// `POST` let every waiting entry in, up to the link's `max_guests_inside` (hosts).
pub const MEETING_LOBBY_ADMIT_ALL: &str = "/api/v1/meetings/{id}/lobby/admit_all";
/// `POST` knock with a link as a signed-in member (body `MeetingLinkRef`; never by
/// meeting id alone).
pub const MEETINGS_KNOCK: &str = "/api/v1/meetings/knock";
/// `GET` the caller's own lobby entries (the guest page's polling fallback).
pub const MEETINGS_LOBBY_ME: &str = "/api/v1/meetings/lobby/me";
/// `POST` end a live meeting for everyone (hosts; body `{}`, answers `Meeting`).
pub const MEETING_END: &str = "/api/v1/meetings/{id}/end";
/// `POST` lock or unlock a meeting (hosts; body `LockMeetingRequest`, answers
/// `Meeting`).
pub const MEETING_LOCK: &str = "/api/v1/meetings/{id}/lock";
/// `POST` remove someone from a meeting (hosts; body `RemoveMeetingMemberRequest`).
pub const MEETING_REMOVE: &str = "/api/v1/meetings/{id}/remove";
/// `GET` a meeting channel's people (`{id}` is the channel; `after`/`limit`
/// paging; answers `MeetingPersonPage`): everyone in it, link guests included, may read the
/// names of everyone else in it.
pub const CHANNEL_PEOPLE: &str = "/api/v1/channels/{id}/people";
/// `POST` withdraw the caller's waiting knock in meeting `{id}` (body `{}`, answers
/// `{ok: true}` whether or not the caller was waiting).
pub const MEETING_LOBBY_WITHDRAW: &str = "/api/v1/meetings/{id}/lobby/me/withdraw";

/// Fills `{call_id}` in a call key route with the call's `r_…` room.
pub fn with_call_id(template: &str, call_id: &str) -> String {
    template.replace("{call_id}", call_id)
}

/// Fills `{id}` or `{member_id}` in a template with an id.
pub fn with_id(template: &str, id: crate::id::Id) -> String {
    template
        .replace("{id}", &id.to_string())
        .replace("{member_id}", &id.to_string())
}

/// Fills `{id}` with `id` and `{member_id}` with `member` (the space-member routes).
pub fn with_ids(template: &str, id: crate::id::Id, member: crate::id::Id) -> String {
    template
        .replace("{id}", &id.to_string())
        .replace("{member_id}", &member.to_string())
}
