# Display audio design

## Purpose

Task #124 gives a display session sound: what plays inside the guest is heard on
the host. Everything the session needs to exist is already built -- #118 settled
the protocol, #115 made the guest capture and listen, #117 and #119 made the
window interactive, #121 wired Connect, #125 added a fourth channel and proved
that adding one is routine -- and none of it carries a sample.

The task is not a small addition for the same reason the clipboard was not: the
display stack is deliberately session-blind, and sound is not. Frames come off a
DRM device and input goes to uinput devices, both reachable by a system account;
audio is produced by applications inside a logged-in user's PipeWire graph. So
audio needs a component the stack does not have, and a route out of that graph
that does not link a system C library -- `libasound` and `libpipewire` are both
out, because the guest binaries are built for `x86_64-unknown-linux-musl` with
no C toolchain and AGENTS.md forbids spending that.

AppSandbox is the precedent to answer to. `tools/linux/agent/appsandbox-audio.c`
captures the ALSA loopback and ships PCM over vsock port 4; `vm_display_idd.c`
renders it with WASAPI, mutes from the window's system menu and rebuilds its
session when the host's default endpoint changes. This task matches that
coverage. It does not carry a microphone: host capture into the guest is the
other half of the problem -- a second direction, a second loopback in the guest,
Windows microphone consent -- and AppSandbox has no such thing either.

## What the spike established

The route rests on kernel behaviour that no document promises, so it was proved
against a running guest -- the `test` VM, Ubuntu 26.04, kernel 7.0.0-30 -- before
this design was written.

* **`snd-aloop` is already there.** It belongs to
  `linux-modules-<version>-generic`, the base modules package, not to
  `linux-modules-extra`; `modprobe snd-aloop` brings up a `Loopback` card with
  `pcmC0D0p/c` and `pcmC0D1p/c`. The recipe gains a `modules-load.d` entry and
  no apt step.
* **Group membership is enough.** `/dev/snd/*` is `root:audio 0660` plus an ACL
  for the user of the active seat. A system daemon in the `audio` group can open
  the loopback; unlike the clipboard, this needs nothing from logind and nothing
  from the user's session.
* **The VM has no other sound card.** `Loopback` is the only one, so it becomes
  PipeWire's default sink without a policy fight.
* **Whichever half opens first pins the other.** With the playback half closed,
  the capture half accepts anything -- S16 through FLOAT, 8 kHz to 768 kHz, 1 to
  32 channels. With `speaker-test` holding playback at S32/48k/2ch, capture
  narrows to exactly that. The reverse holds too: with capture opened at
  S16_LE/48k/2ch, the playback half narrowed to S16_LE/48k/2ch. A daemon that
  starts at boot therefore chooses the format, and PipeWire adapts.
* **Silence flows by itself.** With no playback open, three seconds of capture
  produced exactly 1 152 000 bytes of zeros: `snd-aloop` clocks silence from a
  timer. The stream does not stall between sounds, so suppression is a test for
  zeros rather than a guess about liveness.
* **The raw ioctls work.** A throwaway musl binary with hand-written
  `SNDRV_PCM_IOCTL_*` requests and hand-written `snd_pcm_hw_params`,
  `snd_pcm_sw_params` and `snd_xferi` structures drove the device end to end:
  `PVERSION` reported 2.0.18, `HW_REFINE` narrowed to the requested
  S16_LE/48000/2ch with a 480-frame period and a 1920-frame buffer, `HW_PARAMS`,
  `SW_PARAMS`, `PREPARE` and `START` were accepted, and 200 reads of 480 frames
  took 2000 ms with no short read and no xrun. The first read waited 10.4 ms and
  the worst 11.0 ms -- the period, and nothing on top of it. Two seconds of
  capture cost 4.1 ms of system time when silent and 3.2 ms of user time when
  carrying a sine, about 0.2% of a core.

`READI_FRAMES` is `_IOR`, not `_IOW`; the first attempt encoded it as a write
and the kernel answered `ENOTTY`. It is written down because it is the kind of
detail that costs an afternoon twice.

Ubuntu 22.04 and 24.04 were not exercised. `snd-aloop` has been in the kernel
for far longer than either, and the release matrix is #128's business.

## Decisions

### A fifth channel, parallel to the four that exist

Audio must not delay a frame or a keystroke, which the task states. That rules
out both the control channel (64 KiB payload cap, and a period ahead of a `Pong`
is a session that looks frozen) and any sharing with frames.

So the session gets a fifth socket:

| | port | service GUID |
| --- | --- | --- |
| control | `VMLD` `0x564D_4C44` | `564D4C44-FACB-11E6-BD58-64006A7986D3` |
| frame | `VMLF` `0x564D_4C46` | `564D4C46-…` |
| input | `VMLI` `0x564D_4C49` | `564D4C49-…` |
| clipboard | `VMLC` `0x564D_4C43` | `564D4C43-…` |
| audio | `VMLS` `0x564D_4C53` | `564D4C53-FACB-11E6-BD58-64006A7986D3` |

`S` for sound: `VMLA` is the agent's own service, `564D4C41`.

`Channel::Audio = 5` joins the record header's channel byte, and the entry is
listed on every VM for the reason #121 gave for the others: a service table
entry is the partition's permission for a service to exist, not a claim that
anything is listening.

No new cryptography. `keys::channel_key` is parameterised by the channel byte,
so the audio key is derived from the session key and the transcript exactly as a
frame key is, and the bind is the same three records. `Session`'s per-channel
array grows by one slot.

### Three record types, and a header field that does the fourth's work

| record | direction | payload |
| --- | --- | --- |
| `AudioFormat` | guest → host | sample rate, channels, sample format, frames per period |
| `AudioData` | guest → host | one period of interleaved PCM, raw |
| `AudioError` | guest → host | why there is no stream, or why one stopped |

`AudioData` carries codec-free bytes rather than a message from the schema,
which the frame channel already does for keyframes and tile deltas. Its stream
position -- the number of frames captured before it -- travels in the header's
existing `base` field, the way a tile delta carries the sequence it builds on.

That choice removes a record. A gap in the stream, whether it came from
suppressed silence or from dropped periods, is a jump in `base`, so the host
learns both that audio is missing and exactly how much of it without being told.
`base` is 32 bits and wraps after about 24.8 hours at 48 kHz, so the host
compares positions with wrapping arithmetic; that is correct for any gap shorter
than 12 hours, which is every gap that can occur.

The channel is one-directional once bound. Mute lives on the host and changes
nothing the guest needs to know, so there is nothing for the guest to read.

`AUDIO_MAX_PAYLOAD` is 16 KiB. A 480-frame period of S32 stereo is 3840 bytes;
the rest is room for a longer period the guest did not choose.

### The format is whatever got pinned, and the daemon says which

The daemon is a system service and starts at boot, so in the ordinary case it
opens the capture half first and pins **S16_LE, 48 kHz, stereo, 480-frame
periods**: 192 KB/s on the wire, one record every 10 ms, and a capture latency
equal to the period. PipeWire adapts, converting from float as it already does.

The other order happens too -- the daemon is restarted while a desktop is
playing -- and there it does not argue: it reads what `HW_REFINE` reports,
takes it, and announces it. This is the whole of the "format negotiation" the
task asks for: not a conversation between two peers, but an honest statement of
what the kernel allowed. The host accepts any of it and lets WASAPI convert,
initialising with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` rather than matching the
endpoint's mix format itself.

A format that changes mid-session -- the device was reopened -- is a second
`AudioFormat` record. The host rebuilds its renderer around it and treats the
change as a gap.

### Silence is not sent, and the queue is bounded

A period of nothing but zeros is not transmitted. Suppression starts after ten
consecutive silent periods (100 ms), so a quiet passage does not make the stream
flap, and an idle desktop costs nothing at all instead of 1.5 Mbit/s.

The guest holds a ring of 200 ms (twenty periods). The ALSA reader never blocks
on the socket: when the host cannot keep up the oldest periods are dropped and
`base` records how many. This is what makes "audio does not block display frames
or input" a structural property rather than a scheduling promise -- there is no
backlog to build, because audio waits for nothing.

The host mirrors it. WASAPI runs in shared mode, and a period that does not fit
the endpoint's buffer is dropped rather than queued. Late audio is worse than
absent audio.

### The host rebuilds the endpoint, not the channel

The renderer registers an `IMMNotificationClient` and acts on three
notifications: `OnDefaultDeviceChanged`, because the user switched output or
plugged in headphones; `OnDeviceStateChanged`, because the endpoint in use was
disabled or unplugged; and `OnDeviceAdded`, because a host that had no audio
endpoint at all may acquire one. Notifications arrive on someone else's thread,
so they set an atomic flag that the audio thread reads between periods.

AppSandbox restarts the whole channel here: its `audio_restart` flag breaks the
loop, the socket closes and an outer loop reconnects. That is a consequence of
its renderer and its socket living in the same loop with no way to restart one
without the other, not a decision. This design rebuilds only the endpoint and
keeps the channel bound: a device change on the host is an event the guest
neither knows about nor should, the wire format does not change with it, and a
rebind costs a round trip that can fail. Concretely -- stop and release
`IAudioRenderClient`, `IAudioClient` and `IMMDevice`, activate the new default
endpoint, initialise it with the same `WAVEFORMATEX`, start it. Buffered PCM is
discarded, because there is nothing worth splicing across a device switch, and
the discontinuity reaches the host as an ordinary jump in `base`.

If the new endpoint cannot be initialised, or there is no endpoint at all, the
renderer parks: it keeps reading records and discarding them, reports through
`diagnostic!` once rather than per period, and revives on the next notification.
The channel stays up. A host whose user has no working output is not a reason to
tear down a guest's stream.

### Mute is host-side and instant

A system-menu item beside the ones the window already has. A muted renderer
keeps reading records and discarding them instead of pushing them into WASAPI,
which is what makes unmuting immediate and keeps it from disturbing the pinned
format or the guest's pacing. The state is remembered per VM alongside the
window state the viewer already saves, and starts unmuted.

### The guest end is a system daemon

`vmlord-display-audio`, a fourth musl binary beside the broker, the session and
the clipboard daemon. A system unit ordered `After=vmlord-display-broker.service`,
running as the existing `vmlord-display` account with `SupplementaryGroups=audio`
and the same hardening the session unit carries.

It takes its channel key from a third broker socket,
`/run/vmlord/display-audio.sock`, authorised by the peer's uid against that
system account -- the socket's own permissions, not the logind lookup the
clipboard needs, because unlike a clipboard an audio stream does not belong to
whoever is at the screen.

It binds the vsock channel **before** it opens ALSA. A guest with no
`snd-aloop`, or one whose loopback is busy, then answers `AudioError` with a
reason instead of never appearing, and the reason reaches the host's
diagnostics.

The payload gains, beside the binary and its unit: a `modules-load.d` entry for
`snd-aloop`; a `modprobe.d` file with `options snd-aloop index=0
pcm_substreams=1`, because the default two cables show up in GNOME as two
identical outputs; and a WirePlumber rule hiding the loopback's capture side, so
that the loop is never offered to the user as a microphone.

### The capability says what the build has, not what is playing

`CAPABILITY_AUDIO` is announced by a guest whose payload ships this daemon,
whether or not anything is playing and whether or not anyone has logged in. The
reasoning is the clipboard's: a session commonly opens at the GDM login screen,
a capability cannot be renegotiated once the handshake settles it, and tying the
announcement to a running stream would mean audio never works for anyone who
connected before logging in.

### No sample reaches a log

Every line about audio carries the format, a frame count, a stream position and
an outcome. None carries a sample, at any level, on either side, and the test
for it is part of this task.

## Components

| where | what changes |
| --- | --- |
| `display-protocol` | `Channel::Audio`, `AudioRecord`, the three messages, `CAPABILITY_AUDIO`, `AUDIO_MAX_PAYLOAD`, a fourth per-channel slot in `Session` |
| `display-protocol` | a portable `audio` module: silence suppression, the ring and its drop accounting, stream-position arithmetic, the format description -- no ALSA, no WASAPI, no socket in it |
| `display-services` | `alsa/uapi.rs`, the kernel's PCM ABI written out beside `drm/uapi.rs`, for the same reason |
| `display-services` | `vmlord-display-audio`: the capture loop, the vsock bind, the format announcement |
| `display-services` | the broker's third socket and the audio key in what it sends |
| `payloads/display` | the unit, `modules-load.d`, `modprobe.d`, the WirePlumber rule |
| `agent` | the new binary and files in the install lists, and the recipe stage that enables the unit |
| `display-viewer` | the audio thread, the WASAPI renderer, the `IMMNotificationClient`, the mute item and its saved state |
| `platform` | the fifth service table entry, `audio_port` in the launch parameters, the audio key in the hand-over |
| `docs` | architecture, compatibility, user guide, troubleshooting |

The portable module is where the rules live, on purpose: it is the only way the
limits are testable without a guest, a sound card or a window.

## Testing

* The portable module: the suppression threshold, the ring's drop accounting,
  stream positions across the 32-bit wrap, format encode and decode. Pure unit
  tests.
* The records: golden encodings, malformed payloads and a fuzz target, beside
  the ones the four existing channels have.
* The ALSA ABI: the sizes and field offsets of `snd_pcm_hw_params`,
  `snd_pcm_sw_params` and `snd_xferi` against the kernel's layout. This is what
  catches an ABI slip before a guest does.
* The bind: an audio socket proving itself with a frame key must fail, which is
  what `channel_key`'s domain separation exists for.
* No-logging: a capture of the log during a stream contains no sample bytes.
* End to end on the `test` VM, by hand: sound in GNOME; an idle desktop sending
  nothing; mute and unmute; **the host's default output changed while sound is
  playing**; the only endpoint removed and returned; the daemon killed and
  restarted; PipeWire restarted; the session reconnected; and the display's
  frame rate measured under audio to confirm it is unaffected.

## Out of scope

* Microphone: host capture into the guest.
* Per-application volume, or any mixing beyond the host's own.
* Audio at the GDM login screen -- nothing plays there, and the daemon needs no
  session to be running, so this is a statement about what is tested rather than
  a restriction in the code.
* Compression. 192 KB/s of PCM is cheaper than a codec on both ends.
* Surround: the daemon pins stereo, and a guest that pins more is carried but
  not sought.
* The Ubuntu 22.04 and 24.04 legs of the release matrix, which #128 owns.
