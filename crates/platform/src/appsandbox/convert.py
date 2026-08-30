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
import zipfile

HOME = "/var/lib/vmlord/convert"
AGENT = "/usr/local/lib/vmlord/vmlord-agent"
AGENT_SECRET = "/etc/vmlord/agent.secret"
AGENT_UNIT = "/etc/systemd/system/vmlord-agent.service"
PAYLOADS = "/var/lib/vmlord/payloads"

AGENT_UNIT_TEXT = (
    "[Unit]\n"
    "Description=VMLord guest agent\n"
    "ConditionPathExists=/etc/vmlord/agent.secret\n"
    "\n"
    "[Service]\n"
    "ExecStart=/usr/local/lib/vmlord/vmlord-agent\n"
    "User=root\n"
    "Restart=always\n"
    "RestartSec=5\n"
    "\n"
    "[Install]\n"
    "WantedBy=multi-user.target\n"
)

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


def payload_directory(kind, payload_id):
    return os.path.join(PAYLOADS, kind, payload_id)


def install_payload(kind, key):
    payload_id = load_input()[key]
    target = payload_directory(kind, payload_id)
    if os.path.isdir(target):
        shutil.rmtree(target)
    os.makedirs(target)
    with zipfile.ZipFile(os.path.join(HOME, kind + "-payload.zip")) as archive:
        for member in archive.namelist():
            resolved = os.path.realpath(os.path.join(target, member))
            if not resolved.startswith(os.path.realpath(target) + os.sep):
                fail("%s escapes its payload directory" % member)
        archive.extractall(target)
    os.chmod(target, 0o755)


def check_payload(kind, key):
    target = payload_directory(kind, load_input()[key])
    if not os.path.isdir(target) or not os.listdir(target):
        fail("the %s payload is not installed at %s" % (kind, target))


# --- steps -----------------------------------------------------------------


def install_bundle():
    check_manifest(HERE)
    if os.path.realpath(HERE) != os.path.realpath(HOME):
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
    os.makedirs(os.path.dirname(AGENT), exist_ok=True)
    shutil.copyfile(os.path.join(HOME, "vmlord-agent"), AGENT)
    os.chmod(AGENT, 0o755)
    os.chown(AGENT, 0, 0)
    with open(os.path.join(HOME, "agent.secret"), "r", encoding="utf-8") as handle:
        write_root_file(AGENT_SECRET, handle.read().strip() + "\n", 0o600)
    write_root_file(AGENT_UNIT, AGENT_UNIT_TEXT, 0o644)
    systemctl("daemon-reload")
    if systemctl("enable", "vmlord-agent.service") != 0:
        fail("the VMLord agent unit could not be enabled")


def verify_agent_files():
    for path, mode in ((AGENT, 0o755), (AGENT_SECRET, 0o600), (AGENT_UNIT, 0o644)):
        if not os.path.isfile(path):
            fail("%s is missing" % path)
        if os.stat(path).st_mode & 0o777 != mode:
            fail("%s does not have the permissions VMLord installed it with" % path)
    if systemctl("is-enabled", "--quiet", "vmlord-agent.service") != 0:
        fail("vmlord-agent.service is not enabled")


def install_display_payload():
    install_payload("display", "display_payload_id")


def verify_display_payload():
    check_payload("display", "display_payload_id")


def install_gpu_payload():
    install_payload("gpu", "gpu_payload_id")


def verify_gpu_payload():
    check_payload("gpu", "gpu_payload_id")


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
    verify_display_payload()
    verify_gpu_payload()
    if subprocess.call(["systemd-analyze", "verify", AGENT_UNIT]) != 0:
        fail("systemd does not accept %s" % AGENT_UNIT)


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
    "install-display-payload": install_display_payload,
    "verify-display-payload": verify_display_payload,
    "install-gpu-payload": install_gpu_payload,
    "verify-gpu-payload": verify_gpu_payload,
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
