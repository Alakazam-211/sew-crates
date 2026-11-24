## Default Permission

Everything a shell's UI needs: the saved servers (`servers`, `add_server`, `confirm_server`,
`remove_server`, `server_info`, `set_active`), sign-in (`sign_in`, `pair`, `sign_in_door`,
`sign_in_door_set_password`, `pair_confirm`, `pair_decline`), sign-out and the device list
(`sign_out`, `devices`, `revoke_device`), `open` (a server's page in the calling window, signed
in), `notify` (a notification and the app badge), and `pending_pair` and `pending_deep_links`
(what a `pair_request`, `pair_refused` or `deep_link` signal is about; the bundled UI only).
Every window gets every event (`sew-app://servers`, `signed_out`, `warning`, `deep_link`,
`pair_request`, `pair_refused`) as a signal with no server, account, link or code in it.
From a saved server's live site the commands act only on that server and its accounts, and
`servers` lists only those; `add_server` and `confirm_server` answer only the bundled UI.
Not in it: `handoff`, which answers a code for the page's cookie (`sew-app:handoff`); the API
transport for a bundled UI (`api`, `gateway_open`, `gateway_send`, `gateway_close`,
`upload_bytes`: `sew-app:transport`); and `pick_files` with `upload_file`, which read a file
the person picked in the file dialog (`sew-app:upload-file`).

#### This default permission set includes the following:

- `allow-servers`
- `allow-add-server`
- `allow-confirm-server`
- `allow-remove-server`
- `allow-server-info`
- `allow-set-active`
- `allow-sign-in`
- `allow-pair`
- `allow-sign-in-door`
- `allow-sign-in-door-set-password`
- `allow-pair-confirm`
- `allow-pair-decline`
- `allow-sign-out`
- `allow-devices`
- `allow-revoke-device`
- `allow-open`
- `allow-notify`
- `allow-pending-pair`
- `allow-pending-deep-links`

## Permission Table

<table>
<tr>
<th>Identifier</th>
<th>Description</th>
</tr>


<tr>
<td>

`sew-app:allow-add-server`

</td>
<td>

Enables the add_server command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-add-server`

</td>
<td>

Denies the add_server command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-api`

</td>
<td>

Enables the api command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-api`

</td>
<td>

Denies the api command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-confirm-server`

</td>
<td>

Enables the confirm_server command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-confirm-server`

</td>
<td>

Denies the confirm_server command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-devices`

</td>
<td>

Enables the devices command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-devices`

</td>
<td>

Denies the devices command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-gateway-close`

</td>
<td>

Enables the gateway_close command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-gateway-close`

</td>
<td>

Denies the gateway_close command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-gateway-open`

</td>
<td>

Enables the gateway_open command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-gateway-open`

</td>
<td>

Denies the gateway_open command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-gateway-send`

</td>
<td>

Enables the gateway_send command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-gateway-send`

</td>
<td>

Denies the gateway_send command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-handoff`

</td>
<td>

Enables the handoff command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-handoff`

</td>
<td>

Denies the handoff command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-notify`

</td>
<td>

Enables the notify command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-notify`

</td>
<td>

Denies the notify command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-open`

</td>
<td>

Enables the open command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-open`

</td>
<td>

Denies the open command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-pair`

</td>
<td>

Enables the pair command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-pair`

</td>
<td>

Denies the pair command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-pair-confirm`

</td>
<td>

Enables the pair_confirm command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-pair-confirm`

</td>
<td>

Denies the pair_confirm command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-pair-decline`

</td>
<td>

Enables the pair_decline command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-pair-decline`

</td>
<td>

Denies the pair_decline command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-pending-deep-links`

</td>
<td>

Enables the pending_deep_links command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-pending-deep-links`

</td>
<td>

Denies the pending_deep_links command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-pending-pair`

</td>
<td>

Enables the pending_pair command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-pending-pair`

</td>
<td>

Denies the pending_pair command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-pick-files`

</td>
<td>

Enables the pick_files command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-pick-files`

</td>
<td>

Denies the pick_files command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-remove-server`

</td>
<td>

Enables the remove_server command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-remove-server`

</td>
<td>

Denies the remove_server command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-revoke-device`

</td>
<td>

Enables the revoke_device command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-revoke-device`

</td>
<td>

Denies the revoke_device command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-server-info`

</td>
<td>

Enables the server_info command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-server-info`

</td>
<td>

Denies the server_info command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-servers`

</td>
<td>

Enables the servers command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-servers`

</td>
<td>

Denies the servers command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-set-active`

</td>
<td>

Enables the set_active command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-set-active`

</td>
<td>

Denies the set_active command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-sign-in`

</td>
<td>

Enables the sign_in command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-sign-in`

</td>
<td>

Denies the sign_in command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-sign-in-door`

</td>
<td>

Enables the sign_in_door command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-sign-in-door`

</td>
<td>

Denies the sign_in_door command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-sign-in-door-set-password`

</td>
<td>

Enables the sign_in_door_set_password command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-sign-in-door-set-password`

</td>
<td>

Denies the sign_in_door_set_password command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-sign-out`

</td>
<td>

Enables the sign_out command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-sign-out`

</td>
<td>

Denies the sign_out command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-upload-bytes`

</td>
<td>

Enables the upload_bytes command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-upload-bytes`

</td>
<td>

Denies the upload_bytes command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:allow-upload-file`

</td>
<td>

Enables the upload_file command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:deny-upload-file`

</td>
<td>

Denies the upload_file command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-app:handoff`

</td>
<td>

`handoff`: a single-use 60 s code that the calling page trades for the App's cookie session
(`POST /sew/native/handoff/redeem`). The plugin answers it only to a page on the account's own
server. Grant it only to the remote capability of the App's own site, never to a window that
loads other pages.


</td>
</tr>

<tr>
<td>

`sew-app:transport`

</td>
<td>

The API transport, for an App's bundled UI: `api` (the App's client API through the server's
native lane, with the device bearer kept in the app), `gateway_open`, `gateway_send` and
`gateway_close` (the gateway socket held in the app and relayed over a channel, its ticket put in
by the app), and `upload_bytes` (raw bytes to an upload or an image route). The page never gets
the bearer or a ticket. Grant it to the bundled UI's own capability; a server's live site has its
cookie session and doesn't need it. The `sewfile:` scheme needs no permission: it answers only
the bundled UI's pages.


</td>
</tr>

<tr>
<td>

`sew-app:upload-file`

</td>
<td>

`pick_files` and `upload_file`: `pick_files` opens the native file dialog (desktop) and
answers a handle for each file the person picked; `upload_file` reads the file of such a
handle in Rust and posts its bytes to an upload or an image route. It takes only handles the
dialog gave the same page this session, never a path, and doesn't follow a link. Grant it only
to the app's bundled UI, never to a server's live site.


</td>
</tr>
</table>
