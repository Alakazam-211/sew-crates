## Default Permission

Everything a call UI needs: join, leave, mute, list devices, switch devices, read the call
state and answer a recording's notice (`sew-voice:allow-join`, `allow-leave`, `allow-mute`,
`allow-devices`, `allow-set-device`, `allow-state`, `allow-consent`,
`allow-withdraw-consent`). Every window gets every event (`sew-voice://state`, `participants`,
`speaking`, `errors`, `devices`, `recording`). A window that should only watch a call can list
`sew-voice:allow-state` and `sew-voice:allow-devices` alone instead of this set.

#### This default permission set includes the following:

- `allow-join`
- `allow-leave`
- `allow-mute`
- `allow-devices`
- `allow-set-device`
- `allow-state`
- `allow-consent`
- `allow-withdraw-consent`

## Permission Table

<table>
<tr>
<th>Identifier</th>
<th>Description</th>
</tr>


<tr>
<td>

`sew-voice:allow-call-details`

</td>
<td>

Enables the call_details command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-call-details`

</td>
<td>

Denies the call_details command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-camera`

</td>
<td>

Enables the camera command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-camera`

</td>
<td>

Denies the camera command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-camera-permission`

</td>
<td>

Enables the camera_permission command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-camera-permission`

</td>
<td>

Denies the camera_permission command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-consent`

</td>
<td>

Enables the consent command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-consent`

</td>
<td>

Denies the consent command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-devices`

</td>
<td>

Enables the devices command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-devices`

</td>
<td>

Denies the devices command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-forget-server`

</td>
<td>

Enables the forget_server command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-forget-server`

</td>
<td>

Denies the forget_server command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-join`

</td>
<td>

Enables the join command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-join`

</td>
<td>

Denies the join command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-known-device-keys`

</td>
<td>

Enables the known_device_keys command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-known-device-keys`

</td>
<td>

Denies the known_device_keys command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-leave`

</td>
<td>

Enables the leave command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-leave`

</td>
<td>

Denies the leave command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-mute`

</td>
<td>

Enables the mute command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-mute`

</td>
<td>

Denies the mute command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-preview`

</td>
<td>

Enables the preview command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-preview`

</td>
<td>

Denies the preview command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-request-camera-permission`

</td>
<td>

Enables the request_camera_permission command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-request-camera-permission`

</td>
<td>

Denies the request_camera_permission command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-request-keyframe`

</td>
<td>

Enables the request_keyframe command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-request-keyframe`

</td>
<td>

Denies the request_keyframe command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-ring`

</td>
<td>

Enables the ring command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-ring`

</td>
<td>

Denies the ring command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-ring-ended`

</td>
<td>

Enables the ring_ended command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-ring-ended`

</td>
<td>

Denies the ring_ended command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-screen`

</td>
<td>

Enables the screen command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-screen`

</td>
<td>

Denies the screen command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-screen-sources`

</td>
<td>

Enables the screen_sources command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-screen-sources`

</td>
<td>

Denies the screen_sources command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-set-device`

</td>
<td>

Enables the set_device command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-set-device`

</td>
<td>

Denies the set_device command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-set-video-device`

</td>
<td>

Enables the set_video_device command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-set-video-device`

</td>
<td>

Denies the set_video_device command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-state`

</td>
<td>

Enables the state command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-state`

</td>
<td>

Denies the state command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-video-caps`

</td>
<td>

Enables the video_caps command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-video-caps`

</td>
<td>

Denies the video_caps command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-video-devices`

</td>
<td>

Enables the video_devices command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-video-devices`

</td>
<td>

Denies the video_devices command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-video-sink`

</td>
<td>

Enables the video_sink command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-video-sink`

</td>
<td>

Denies the video_sink command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-video-sink-close`

</td>
<td>

Enables the video_sink_close command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-video-sink-close`

</td>
<td>

Denies the video_sink_close command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-video-sink-update`

</td>
<td>

Enables the video_sink_update command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-video-sink-update`

</td>
<td>

Denies the video_sink_update command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:allow-withdraw-consent`

</td>
<td>

Enables the withdraw_consent command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:deny-withdraw-consent`

</td>
<td>

Denies the withdraw_consent command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`sew-voice:call-details`

</td>
<td>

The call-detail commands, for a call UI that shows more than `sew-voice:default` gives:
`call_details` (the key state and security code, the quality, every media participant with
its engine identity and label, and the ring, for a window opened mid-call), `ring` and
`ring_ended` (an incoming DM call, set from Sew's ring and cleared when it stops). Their events
(`sew-voice://security`, `roster`, `quality`, `incoming`) reach every window. Not in
`sew-voice:default`: grant it next to it, or `sew-voice:allow-call-details`,
`sew-voice:allow-ring` and `sew-voice:allow-ring-ended` one by one.


</td>
</tr>

<tr>
<td>

`sew-voice:device-keys`

</td>
<td>

The device-key commands, for a window that signs the person out: `forget_server` (deletes the
devices of one Sew install, the hardware keys included, and answers the removed keys) and
`known_device_keys` (lists the keys the device store holds). Not in `sew-voice:default`: grant
it, or `sew-voice:allow-forget-server` / `sew-voice:allow-known-device-keys` alone, only to the
window that needs it.


</td>
</tr>

<tr>
<td>

`sew-voice:video`

</td>
<td>

The video commands, for a window that shows or sends video: `video_caps`, `video_devices`,
`set_video_device`, `camera`, `screen_sources`, `screen`, `preview`, `video_sink`,
`video_sink_update`, `video_sink_close`, `request_keyframe`, `camera_permission` and
`request_camera_permission`. The frames reach only the window
that opened the sink (its own channel). Not in `sew-voice:default`: grant it to the call
window, or single `sew-voice:allow-<command>` permissions (a window that only shows video needs
`allow-video-sink`, `allow-video-sink-update`, `allow-video-sink-close` and
`allow-request-keyframe`). The OS asks for the camera and screen recording itself.


</td>
</tr>
</table>
