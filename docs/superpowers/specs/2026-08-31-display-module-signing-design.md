# Display module signing design

## Purpose

Task #126 makes `vmlord_drm` a signed module. Today it is not, and VMLord's
guests hide that behind a precondition: they boot with Secure Boot disabled and
kernel lockdown `[none]`, so an unsigned DKMS module loads and nothing asks
where it came from. The moment Secure Boot is on -- because a user turned it on,
because a future VMLord turns it on, or because a guest image arrives with it on
-- that module is rejected by the kernel and the desktop is gone with a message
that says only "the module did not load".

This design makes the signature exist, makes VMLord able to say whether it
exists, gives the user the certificate enrollment needs, and tells a rejected
module apart from every other reason a module does not load. It does **not**
enable Secure Boot anywhere, and it does not automate MOK enrollment. See
*Deliberately not in this task*.

## Decisions

* The signing key is **the guest's own MOK**, `/var/lib/shim-signed/mok/MOK.priv`
  and `MOK.der`. All three supported dkms versions already sign with that pair by
  default. VMLord writes no `framework.conf`, ships no key, and holds no private
  key material of any kind.
* The key is **per VM**, created inside the guest, never leaves it, and dies with
  it. Only the DER certificate travels to the host.
* Signing happens **inside `dkms build`**, not in a VMLord step. That is what
  carries it through `AUTOINSTALL=yes` rebuilds after an unattended kernel
  upgrade, with VMLord closed and no agent connected.
* VMLord's two new recipe steps **prepare and verify**, never sign: `SigningKey`
  before the build, `ModuleSignature` after it.
* **A missing or unverifiable signature does not degrade the display.** Secure
  Boot is off, the unsigned module loads, and failing a working desktop over a
  future requirement would be a regression. The failure is in the report and in
  the logs, and nowhere else.
* A module the kernel rejects for its signature gets **its own status code**,
  `display-payload-module-signature-rejected`, and is not retryable.
* The key is **never rotated automatically**. Regeneration invalidates an
  enrollment the user performed by hand, so it happens only when the pair is
  broken, and it forces a re-signing of every version DKMS holds.

## What the distribution already does

Verified against the actual Ubuntu packages for the three supported releases,
not from memory.

**22.04, dkms 2.8.7-2ubuntu2.2.** `build_module` calls `sign_build` after every
build. It runs when `kmodsign` is on `PATH` and `/var/lib/shim-signed/mok/`
exists -- an empty directory shipped by the `shim-signed` package, so the
condition already holds on any UEFI Ubuntu guest. It then runs
`SHIM_NOTRIGGER=y update-secureboot-policy --new-key`, signs every built `.ko`
with `kmodsign sha512` against `/var/lib/shim-signed/mok/MOK.{priv,der}`, and
calls `update-secureboot-policy --enroll-key`.

**24.04, dkms 3.0.11 and 26.04, dkms 3.2.2.** Signing is generic:
`prepare_signing` reads `sign_file`, `mok_signing_key` and `mok_certificate`
from `/etc/dkms/framework.conf` and `framework.conf.d/*.conf`, and on Ubuntu
defaults to the same `/var/lib/shim-signed/mok/MOK.{priv,der}`, generating them
through `update-secureboot-policy --new-key` when they are absent. Signing is
skipped entirely when the running kernel's config has no
`CONFIG_MODULE_SIG_HASH`.

Two consequences shape everything below.

The first is that **the guest's MOK is the one path all three releases agree
on**. A VMLord-owned key elsewhere would need `framework.conf.d` on 3.x and an
edit to dkms's own conffile on 22.04 -- and on 22.04 it would lose anyway: a
`sign_tool` signs the copy under `build/`, while `sign_build` signs the copy
under `module/` that is the one actually installed. The distribution's key wins
on that release no matter what we configure, so the design uses it deliberately
rather than fighting it.

The second is that `update-secureboot-policy --enroll-key` is `mokutil --import`
with a debconf-prompted password, which only stages a request. Completing it
needs MokManager at the UEFI console on the next boot.

## Why enrollment is not automated here

`MokList` is written by MokManager and by nothing else: a boot-services UEFI
variable, filled from an interactive prompt on the firmware console. VMLord's
VMs are created straight through `HcsCreateComputeSystem` and are never
registered with VMMS, so they do not appear in Hyper-V Manager and `vmconnect`
does not attach to them. There is no firmware console for a user to type into,
and no supported way to seed the variable from the host.

The one real path -- moving `Chipset.Uefi.Console` off `"Default"` onto a COM
port and driving MokManager's menus over a named pipe -- is a subsystem, not a
step, and it only pays for itself once VMLord actually enables Secure Boot. This
design therefore treats enrollment as an operator procedure, and spends its
effort on making that procedure possible: the certificate is exported, its
fingerprint is reported, and a rejection says exactly what is missing.

## The recipe steps

`DisplayRecipeStep` gains two variants and `STEPS` becomes twelve, in this
order:

```
Distribution, Payload, BuildDependencies, SigningKey, ModuleSource,
ModuleBuild, ModuleSignature, Initramfs, ModuleLoad, Device, Services,
ServicesStart
```

`SigningKey` sits after `BuildDependencies` because creating the pair may need
`openssl`, and before `ModuleSource` because the pair must exist before anything
builds. `ModuleSignature` sits directly after `ModuleBuild`, on the artifact
that build produced.

### `SigningKey`

Ensures a usable pair and reports what it is.

| Situation | What the step does | State |
|---|---|---|
| Both files present, `MOK.priv` is `0600 root:root` | Reads the certificate, reports both its identities | ok |
| Both present, permissions wider | Repairs to `0600 root:root`, reports both facts | ok |
| Neither present | `SHIM_NOTRIGGER=y DEBIAN_FRONTEND=noninteractive update-secureboot-policy --new-key`; falls back to `openssl req` with `/usr/lib/shim/mok/openssl.cnf` when that binary is absent | ok |
| `MOK.der` missing, `MOK.priv` present | Recreates **both** and records that enrollment must be repeated | ok |
| Creation failed | Reports the failure verbatim | failed |
| The kernel has no `CONFIG_MODULE_SIG_HASH` | Reports that this kernel cannot sign modules | skipped |

A certificate cannot be derived from a private key, so a half pair is a broken
pair: recreating only what is missing would leave a certificate nobody can
enroll matching a key nobody can verify against. Recreating both is the only
honest repair, and saying so in the report is what stops a user from wondering
why an enrollment they already did stopped working.

`--new-key` is a no-op when the pair exists, which is why `SigningKey` running
before every build costs nothing on the usual path. `--enroll-key`, which dkms
calls on 22.04 during the build, stages a `mokutil --import` request that no
MokManager will ever consume here; it fails softly inside the distribution's own
script and is not something this design fights.

The step reports **two identities of one certificate**, because two different
readers need different ones. The SHA-256 of the DER is what a person compares
against the file on the host and what messages quote. The subject key identifier
is what `sign-file` records into the module and what `ModuleSignature` matches
on; it is read from the certificate with `openssl x509 -noout -ext
subjectKeyIdentifier`.

Both exist because `shim-signed`'s `/usr/lib/shim/mok/openssl.cnf` sets
`subjectKeyIdentifier = hash`, alongside `codeSigning` and the module-signing
EKU `1.3.6.1.4.1.2312.16.1.2`. The `openssl req` fallback uses that configuration
file and no other for exactly this reason: a certificate generated without it
would carry no subject key identifier, `sign-file` would fall back to issuer and
serial, and `ModuleSignature` would have nothing to match.

### `ModuleSignature`

Reads `modinfo` of the module the build installed under
`/lib/modules/<kernel>/updates/` and answers one question: is this module signed
by the certificate `SigningKey` reported?

| Situation | State |
|---|---|
| `sig_key` matches the certificate's subject key identifier | ok |
| `sig_key` present and some other key | failed |
| No signature | failed |
| `SigningKey` was skipped | skipped |

**`failed` here changes no display status.** It is a fact recorded for the run,
because with Secure Boot off the module loads regardless and there is nothing
for a user to fix today. What it buys is that the day Secure Boot is turned on,
the report already says whether this guest was ever producing signed modules.

## The certificate on the host

The recipe response gains a field of its own -- the DER bytes of `MOK.der` and
its SHA-256 -- rather than a stage message, because a certificate is data
and a stage message is prose about what happened.

The host writes it beside the VM's state as `display/mok.der` and names that
path in diagnostics. It overwrites on every run: the file is a copy of what the
guest holds now, and a stale copy of a superseded certificate would send a user
to enroll the wrong thing.

The private key is never requested, never sent, never logged, and never written
to the payload share, which is read-only to the guest in any case.

## Diagnosing a rejected module

`DisplayStatusCode` gains one variant:

```rust
/// The module built and the kernel refused its signature.
#[serde(rename = "display-payload-module-signature-rejected")]
PayloadModuleSignatureRejected,
```

It is **not retryable**: no number of retries enrolls a certificate. It is set
when `modprobe` fails and its output carries `Key was rejected by service`
(`EKEYREJECTED`) or `Required key not available` (`ENOKEY`). Every other
`modprobe` failure stays `display-payload-module-not-loaded`, unchanged.

The message names three things a user needs and cannot get from
`display-payload-module-not-loaded`: the Secure Boot state as `mokutil
--sb-state` reports it, the fingerprint of the certificate this guest signs
with, and the fact that it is not enrolled. That is the whole diagnostic value
of the code -- without it, an enrollment problem and a broken build read
identically.

## Lifecycle

**Creation.** Once, on the first display provisioning that reaches
`SigningKey`. Not at VM creation: a headless VM never needs a signing key.

**Rotation.** Never automatic. The pair is regenerated only when it is broken,
and the report says so, because a rotation silently invalidates an enrollment
the user performed by hand.

**Payload update and rollback.** The key is untouched by both. A payload version
and a signing key have unrelated lifetimes: the key lives in `/var/lib`, outside
`/usr/src/vmlord-display-<version>`, outside the payload archive, outside the
share. Both versions DKMS holds are signed, each by its own build with the same
key, so a rollback lands on a module that verifies exactly as the one it
replaced did.

**Regeneration forces a re-sign.** When `SigningKey` creates a new pair while
DKMS already holds installed versions, those versions carry signatures from a
key that no longer exists. The step therefore runs `dkms build --force` and
reinstalls for every version `dkms status` reports for `vmlord-display`, so both
held versions carry the new key. Without this, a rollback after a key
regeneration would land on a module Secure Boot rejects -- the exact failure
this task exists to prevent.

**Kernel upgrade.** Nothing happens, and that is the result. `AUTOINSTALL=yes`
has apt rebuild the module, and dkms signs it with the same guest key, with
VMLord closed and no agent connected. The next recipe run observes the signature
on the new kernel; it does not produce it.

**Destruction.** The key dies with the VM. It is not backed up, not exported,
and not recoverable -- a new VM gets a new key and needs its own enrollment.

## Recovery

| What went wrong | What the user sees | What fixes it |
|---|---|---|
| Secure Boot on, certificate never enrolled | `display-payload-module-signature-rejected`, naming the fingerprint | Enroll `display/mok.der` with `mokutil --import` and complete it in MokManager |
| Guest state file replaced, enrollment lost with the UEFI variables | The same code | Re-enroll the same certificate |
| Key lost or half-present | `SigningKey` reports a regenerated pair; installed versions are re-signed | Re-enroll the new certificate |
| Kernel without `CONFIG_MODULE_SIG_HASH` | Both steps skipped | Nothing -- this kernel cannot sign, and with Secure Boot off it does not need to |

In none of these does the VM stop, the agent session drop, or GPU change state.

## Testing

Everything new except running the commands is a function of text, which is what
the rest of `display_recipe.rs` already is and what makes it testable off a
Hyper-V guest:

* parsing `modinfo` output into "signed by this key id" / "unsigned";
* mapping `modprobe` stderr onto `PayloadModuleSignatureRejected` versus
  `PayloadModuleNotLoaded`, including the case where both phrases are absent;
* parsing `mokutil --sb-state`;
* deciding whether a key pair is complete, half-present or absent, and what
  each answer implies for the report;
* both identities of a known DER -- its SHA-256 and its subject key
  identifier -- and the comparison of the latter against a `modinfo` `sig_key`
  written in `sign-file`'s colon-separated hex;
* the existing "a report names every step even the ones that never ran" test,
  extended to twelve steps.

**Declared unproven:** none of this is exercised against a guest with Secure
Boot actually enabled, because VMLord cannot enable it and the VMs it creates
have no firmware console to enroll a certificate from. What is proven is that
the module is signed, that the signature is by the certificate the host was
handed, and that a rejection maps to its own code. Whether an enrolled
certificate makes the module load is the first thing the enabling task must
test.

## Documentation

* `docs/display-troubleshooting.md` -- a row for
  `display-payload-module-signature-rejected`, and its "Secure Boot and
  networking" section rewritten: the module is signed now, and what is missing
  is the enrollment, not the signature.
* `docs/display-compatibility.md` -- the claim that the module "is not signed
  for Secure Boot; signing is tracked separately" is no longer true. Secure Boot
  still has to be off, for the enrollment reason, and the line should say that
  instead.
* `docs/display-drm-backend.md` -- the paragraph reading "If Secure Boot is ever
  turned on for VMLord VMs, the module needs a MOK-enrolled signature and this
  decision needs revisiting" points at this document for the half that is now
  answered.

## Deliberately not in this task

* **Enabling Secure Boot.** VMLord still sets no Secure Boot template in its
  HCS configuration and still creates VMs that boot with it off. This design is
  the precondition for that task, not that task.
* **Automating MOK enrollment.** It needs a firmware console VMLord's VMs do not
  have. The route is `Chipset.Uefi.Console` on a COM port and a scripted
  MokManager, and it belongs with the task that enables Secure Boot, because
  nothing before then can test it.
* **Signing the payload manifest.** The open item from #113: the catalog entry
  already commits to its archive and `payload.json` by digest, so signing is
  signing that document. It is the host's trust in an artifact, not the kernel's
  trust in a module -- a different key, a different holder, a different failure.
  It stays its own task.
