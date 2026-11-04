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
</table>
