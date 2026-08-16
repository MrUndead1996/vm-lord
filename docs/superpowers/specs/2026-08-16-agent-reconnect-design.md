# Agent reconnect and capabilities design

## Goal

Task #92 makes a guest agent survive everything that can end its connection: a
VM that boots before VMLord is listening, a VMLord that restarts, a host that
hangs up mid-session. The agent, not systemd, owns the retry, and it retries on
a bounded backoff so a host that is away for an hour is polled once every half
minute rather than five times a minute. What a reconnected session agrees on --
the protocol revision and the capabilities both peers have -- is negotiated
again from scratch, and the guest now holds the result instead of discarding it.

## Scope

This task contains the guest's reconnect loop, its backoff, the shared rules
for confirming what a host answered a hello with, and the host-side guard that
keeps a guest reconnecting in a tight loop from spinning the accept thread.

It does not add a capability to either side: `CAPABILITY_GPU` is a promise
neither end can keep until the GPU manifest lands in task #94, and announcing
one early would entitle a peer to send messages nothing answers. It adds no
message to the schema, so the protocol revision does not move.

Re-sending the GPU share manifest after a reconnect belongs to #94 as well.
What #92 owes it is the seam: `agent_session::open` hands its caller the
negotiated session, so the host can act once per session rather than once per
VM. Nothing on the reconnect path touches HCS -- no device is re-assigned and
no compute system is modified -- because the partition and its HvSocket service
belong to the VM's run, not to the connection.

## Why the agent retries rather than the unit

The systemd unit already carries `Restart=always`, so a crash brings the agent
back. That is the wrong instrument for a lost connection:

* a fixed `RestartSec` cannot back off, so a VMLord that is closed for an hour
  is polled seven hundred times, each one a unit restart in the journal;
* a process that exits because the host hung up is not a failure, and reporting
  it as one makes a healthy VM look broken to anybody reading `systemctl`;
* the secret is re-read from disk on every attempt, for no gain.

The unit stays as it is. It is what recovers from a crash; the loop below is
what recovers from a host that is not there.

## The guest loop

`vmlord-agent` reads its secret once, then repeats: connect, run a session,
wait, connect again. It leaves the loop only for a reason that cannot change
while the VM runs -- a secret that is missing or unreadable -- because
everything else is something a VMLord restart or upgrade fixes, including a
protocol major this build cannot speak.

The backoff has one rule: **a session that authenticated resets the delay;
anything else advances it.** The delay starts at one second, doubles, and stops
at thirty seconds. That is deliberately not a table of error classes. A failed
connect, a host that hung up during the handshake, a refused version and a tag
the host would not accept are all "the host is not talking to me", and the only
question the loop has to answer is how soon to ask again. A session that got as
far as an accepted challenge proves the host is there, so the next
disconnection is retried immediately rather than at the cap.

Thirty seconds is the bound on how long after a VMLord restart a VM's agent
comes back. It is short enough that a user who reopens VMLord sees agents
appear while they are still looking at the window, and long enough that an
agent waiting out a host that is gone for the evening costs nothing.

## Confirming what the host answered

The host picks the session's revision and capability set; the guest has to
check that what came back is something it offered. Two rules, both in
`vmlord-agent-protocol::handshake` beside the ones the host negotiates with, so
that the two ends cannot disagree about what was agreed:

* `confirm_version` accepts a revision with the guest's own major and a minor no
  higher than the guest's. A higher minor is a host that answered with a
  revision the guest never claimed to speak.
* `confirm_capabilities` accepts a set that is a subset of what the guest
  announced. A capability the guest never offered is not a session it can serve:
  the host would be entitled to send messages this build has no arm for.

Either failure ends the connection rather than being negotiated further. There
is no third round in this protocol, and a peer that answered a hello with
something unofferable is not one more rounds will fix.

The guest keeps the confirmed pair for the life of the session, which is what
#94 will read to decide whether it may act on a GPU manifest at all. Today both
sets are empty and the pair is only logged.

## The host guard

The host's accept loop serves one connection at a time and takes the next one
as soon as a session ends, which is exactly right for an agent that reconnects
on the backoff above. A guest that connects and drops without authenticating --
a broken agent, or something else on the machine that found the service -- would
instead be served as fast as the thread can loop.

So the host applies the same rule from the other side: after a session that
ended before its challenge was answered, the accept loop waits before offering
again, on the same one-to-thirty-second backoff, and a session that
authenticated resets it. The wait is spent in `ACCEPT_POLL`-sized slices with
the running flag checked between them, because stopping a VM must stay bounded
by that poll and not by the backoff.

## What a reconnect does not do

* It does not re-bind the listener. The `AgentListener` belongs to the VM's run
  and is bound to the runtime id HCS gave that run; the connections come and go
  underneath it.
* It does not touch HCS. No modify-compute-system call, no device assignment, no
  configuration write is on this path.
* It does not resume anything. A new connection is a new session: a fresh hello,
  a fresh nonce, a fresh challenge. Freshness is what makes a recorded answer
  worthless, so there is nothing from the previous session worth carrying over.

VMLord restarting is the same story from the host side, and is already what
`initialize` does: it puts the standing offer back up for every VM that is
running, and the agent inside each one connects to it on the next turn of its
loop.

## Tests

* `handshake`: a confirmed revision, a host minor above the guest's, a subset
  capability set, a capability the guest never offered, and an unknown
  capability number.
* Guest session: the confirmed pair is returned, a host that answers with an
  unofferable capability ends the session, and an authenticated session is
  reported as one so the loop can reset its delay.
* Backoff: the first delay, the doubling, the cap, and the reset.
* Host: a session that never authenticated advances the guard, an authenticated
  one resets it, and the wait is cut short when the connection is told to stop.
* `cargo test -p vmlord-agent`, `cargo test-windows` and `cargo check-windows`
  are the final checks.
