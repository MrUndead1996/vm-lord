#!/usr/bin/env python3
"""VMLord's conversion of an AppSandbox guest, run once per step.

Fixed and root-owned. Every name, path and key it acts on comes out of
input.json beside it, so nothing a person chose is ever part of a command.

Each step is idempotent and each has a check that can be run again on its own:
the host records what it did, and the guest is asked again anyway.
"""

import hashlib
import json
import os
import pwd
import shutil
import subprocess
import sys

HOME = "/var/lib/vmlord/convert"

# Where the agent goes, what its unit is called and what the unit says are all
# VMLord's, not this program's: they come from vmlord-seed and
# vmlord-agent-protocol through input.json, so that a change to the unit or to
# the secret's path reaches an imported guest and a created one alike. A copy
# here would be a copy that falls behind while verify-agent-files still passes.

HERE = os.path.dirname(os.path.abspath(__file__))


def fail(message):
    print(message, file=sys.stderr)
    raise SystemExit(1)


def load_input():
    with open(os.path.join(HOME, "input.json"), "r", encoding="utf-8") as handle:
        return json.load(handle)


def digest(path):
    hasher = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 16), b""):
            hasher.update(block)
    return hasher.hexdigest()


def check_manifest(directory):
    with open(os.path.join(directory, "manifest.json"), "r", encoding="utf-8") as handle:
        manifest = json.load(handle)
    for entry in manifest["files"]:
        path = os.path.join(directory, entry["name"])
        if not os.path.isfile(path):
            fail("the bundle is missing %s" % entry["name"])
        if digest(path) != entry["sha256"]:
            fail("%s is not what the manifest binds it to" % entry["name"])
    return manifest


def systemctl(*arguments):
    return subprocess.call(["systemctl"] + list(arguments))


def write_root_file(path, text, mode):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)
    os.chmod(path, mode)
    os.chown(path, 0, 0)


# --- steps -----------------------------------------------------------------


def install_bundle():
    # The upload arrived with the modes of the account that wrote it, so the
    # secret that authenticates this VM's agent to the host is readable by that
    # account's group and possibly by everyone. Narrow it where it lies, before
    # anything else, so that the window is as short as this program can make it.
    staged_secret = os.path.join(HERE, "agent.secret")
    if os.path.isfile(staged_secret):
        os.chmod(staged_secret, 0o600)
    check_manifest(HERE)
    if os.path.realpath(HERE) == os.path.realpath(HOME):
        fail("the bundle is already installed; it is not staged from %s" % HOME)
    if os.path.isdir(HOME):
        shutil.rmtree(HOME)
    os.makedirs(os.path.dirname(HOME), exist_ok=True)
    shutil.copytree(HERE, HOME)
    for name in os.listdir(HOME):
        path = os.path.join(HOME, name)
        os.chown(path, 0, 0)
        os.chmod(path, 0o600)
    os.chown(HOME, 0, 0)
    os.chmod(HOME, 0o700)
    # Nothing needs the staged copy again, and leaving it behind would leave
    # the agent secret on a disk /tmp-or-not makes no promises about.
    shutil.rmtree(HERE, ignore_errors=True)


def verify_bundle():
    check_manifest(HOME)


def deploy_vmlord_key():
    values = load_input()
    account = pwd.getpwnam(values["guest_username"])
    ssh_directory = os.path.join(account.pw_dir, ".ssh")
    keys = os.path.join(ssh_directory, "authorized_keys")
    os.makedirs(ssh_directory, exist_ok=True)
    lines = []
    if os.path.isfile(keys):
        with open(keys, "r", encoding="utf-8") as handle:
            lines = [line.rstrip("\n") for line in handle if line.strip()]
    if values["vmlord_public_key"] not in lines:
        lines.append(values["vmlord_public_key"])
    with open(keys, "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines) + "\n")
    os.chmod(ssh_directory, 0o700)
    os.chmod(keys, 0o600)
    os.chown(ssh_directory, account.pw_uid, account.pw_gid)
    os.chown(keys, account.pw_uid, account.pw_gid)


def verify_vmlord_key():
    values = load_input()
    account = pwd.getpwnam(values["guest_username"])
    keys = os.path.join(account.pw_dir, ".ssh", "authorized_keys")
    if not os.path.isfile(keys):
        fail("%s does not exist" % keys)
    with open(keys, "r", encoding="utf-8") as handle:
        if values["vmlord_public_key"] not in handle.read().splitlines():
            fail("VMLord's key is not in %s" % keys)
    if os.stat(keys).st_mode & 0o777 != 0o600:
        fail("%s is not readable by its owner alone" % keys)


def install_agent():
    values = load_input()
    agent = values["agent_binary_path"]
    os.makedirs(os.path.dirname(agent), exist_ok=True)
    shutil.copyfile(os.path.join(HOME, "vmlord-agent"), agent)
    os.chmod(agent, 0o755)
    os.chown(agent, 0, 0)
    with open(os.path.join(HOME, "agent.secret"), "r", encoding="utf-8") as handle:
        write_root_file(values["agent_secret_path"], handle.read().strip() + "\n", 0o600)
    write_root_file(values["agent_unit_path"], values["agent_unit_text"], 0o644)
    systemctl("daemon-reload")
    if systemctl("enable", values["agent_unit_name"]) != 0:
        fail("the VMLord agent unit could not be enabled")


def verify_agent_files():
    values = load_input()
    installed = (
        (values["agent_binary_path"], 0o755),
        (values["agent_secret_path"], 0o600),
        (values["agent_unit_path"], 0o644),
    )
    for path, mode in installed:
        if not os.path.isfile(path):
            fail("%s is missing" % path)
        if os.stat(path).st_mode & 0o777 != mode:
            fail("%s does not have the permissions VMLord installed it with" % path)
    if systemctl("is-enabled", "--quiet", values["agent_unit_name"]) != 0:
        fail("%s is not enabled" % values["agent_unit_name"])


def disable_appsandbox_units():
    # A unit a given guest never had makes systemctl return non-zero, which is
    # an answer rather than a failure: the check below is what decides.
    for unit in load_input()["appsandbox_units"]:
        systemctl("disable", "--now", unit)
    systemctl("daemon-reload")


def verify_appsandbox_units_disabled():
    for unit in load_input()["appsandbox_units"]:
        if systemctl("is-active", "--quiet", unit) == 0:
            fail("%s is still running" % unit)
        if systemctl("is-enabled", "--quiet", unit) == 0:
            fail("%s is still enabled" % unit)


def validate_replacements():
    """The gate the removal waits behind.

    Nothing of AppSandbox's is deleted until what replaces it is installed,
    enabled and accepted by systemd itself.
    """
    verify_agent_files()
    unit = load_input()["agent_unit_path"]
    if subprocess.call(["systemd-analyze", "verify", unit]) != 0:
        fail("systemd does not accept %s" % unit)


def remove_obsolete_files():
    for path in load_input()["obsolete_paths"]:
        if os.path.isfile(path) or os.path.islink(path):
            os.remove(path)
    systemctl("daemon-reload")


def verify_obsolete_files_removed():
    for path in load_input()["obsolete_paths"]:
        if os.path.exists(path):
            fail("%s is still there" % path)


def request_shutdown():
    # A normal shutdown and not a reset: the guest has just written units,
    # keys and a secret, and the next boot is the one that has to find them.
    if systemctl("poweroff") != 0:
        fail("the guest refused a normal shutdown")


STEPS = {
    "install-bundle": install_bundle,
    "verify-bundle": verify_bundle,
    "deploy-vmlord-key": deploy_vmlord_key,
    "verify-vmlord-key": verify_vmlord_key,
    "install-agent": install_agent,
    "verify-agent-files": verify_agent_files,
    "disable-appsandbox-units": disable_appsandbox_units,
    "verify-appsandbox-units-disabled": verify_appsandbox_units_disabled,
    "validate-replacements": validate_replacements,
    "remove-obsolete-files": remove_obsolete_files,
    "verify-obsolete-files-removed": verify_obsolete_files_removed,
    "request-shutdown": request_shutdown,
}


def main():
    if len(sys.argv) != 2 or sys.argv[1] not in STEPS:
        fail("usage: vmlord-convert <step>")
    if os.geteuid() != 0:
        fail("vmlord-convert must run as root")
    STEPS[sys.argv[1]]()


if __name__ == "__main__":
    main()
