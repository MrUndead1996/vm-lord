"""Lays out a prepared payload tree and writes its two provenance documents.

`cargo xtask gpu-payload pack` reads provenance twice: from the recipe it is
given, and from the `sources.json` inside the tree it is packing, and it
refuses the pair unless they agree field for field. Writing both by hand is a
drift waiting to happen, so both are generated here from one spec, and the
overlay digests they carry are measured from the files that were just copied
rather than transcribed.

Invoked by prepare.sh, which is what fetches the pinned upstream checkout.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import shutil
from pathlib import Path

SCHEMA_VERSION = 1


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--spec", type=Path, required=True)
    parser.add_argument("--overlays", type=Path, required=True)
    parser.add_argument("--licenses", type=Path, required=True)
    parser.add_argument("--checkout", type=Path, required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()

    spec = json.loads(arguments.spec.read_text())
    if spec["schema_version"] != SCHEMA_VERSION:
        raise SystemExit(f"unknown payload spec schema version: {spec['schema_version']}")
    if len(arguments.revision) != 40 or not all(c in "0123456789abcdef" for c in arguments.revision):
        raise SystemExit(f"vmlord revision is not a full commit: {arguments.revision}")

    prepared = arguments.output / "prepared"
    if prepared.exists():
        shutil.rmtree(prepared)
    prepared.mkdir(parents=True)

    copy_upstream(spec, arguments.checkout, prepared)
    copy_files(arguments.overlays, prepared, [(o["file"], o["path"]) for o in spec["overlays"]])
    copy_files(arguments.licenses, prepared, [(l["path"], l["path"]) for l in spec["licenses"]])

    provenance = {
        "target": spec["target"],
        "mesa_policy": spec["mesa_policy"],
        "vmlord_revision": arguments.revision,
        "builder_version": spec["builder_version"],
        "sources": [source_record(spec, arguments.checkout)],
        "overlays": [overlay_record(overlay, prepared) for overlay in spec["overlays"]],
    }

    write_json(prepared / "sources.json", {"schema_version": SCHEMA_VERSION, **provenance})
    write_json(
        arguments.output / "recipe.json",
        {
            "schema_version": SCHEMA_VERSION,
            "payload_id": spec["payload_id"],
            "required_renderers": spec["required_renderers"],
            **provenance,
            "licenses": [{"spdx": l["spdx"], "path": l["path"]} for l in spec["licenses"]],
        },
    )

    files = sorted(path for path in prepared.rglob("*") if path.is_file())
    total = sum(path.stat().st_size for path in files)
    print(f"prepared {len(files)} files ({total} bytes) in {prepared}")
    print(f"recipe written to {arguments.output / 'recipe.json'}")


def copy_upstream(spec: dict, checkout: Path, prepared: Path) -> None:
    """Copies the selected upstream paths into the payload's layout."""
    for selection in spec["source"]["paths"]:
        destination = prepared / selection["destination"]
        if selection["kind"] == "file":
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(checkout / selection["path"], destination)
            continue
        destination.mkdir(parents=True, exist_ok=True)
        for item in selected_files(selection, checkout):
            shutil.copyfile(item, destination / item.name)


def selected_files(selection: dict, checkout: Path) -> list[Path]:
    """The upstream files one selection takes, sorted by name."""
    source = checkout / selection["path"]
    if selection["kind"] == "file":
        return [source]
    patterns = selection["files"]
    chosen = sorted(
        item
        for item in source.iterdir()
        if item.is_file() and any(fnmatch.fnmatch(item.name, p) for p in patterns)
    )
    if not chosen:
        raise SystemExit(f"no upstream file under {source} matched {patterns}")
    return chosen


def copy_files(root: Path, prepared: Path, pairs: list[tuple[str, str]]) -> None:
    for name, path in pairs:
        destination = prepared / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(root / name, destination)


def source_record(spec: dict, checkout: Path) -> dict:
    """The upstream record, with its paths sorted as the manifest requires."""
    source = spec["source"]
    selections = sorted(source["paths"], key=lambda selection: selection["path"])
    return {
        "url": source["url"],
        "commit": source["commit"],
        "version": source["version"],
        "paths": [selection["path"] for selection in selections],
        "licenses": [
            {"path": selection["path"], "spdx": selection["spdx"]} for selection in selections
        ],
        "sha256": upstream_digest(selections, checkout),
    }


def upstream_digest(selections: list[dict], checkout: Path) -> str:
    """Digests the upstream material this payload takes, and only that.

    The commit already names the whole tree, and a whole tree is not what
    travels in the payload. This covers exactly the selected files, so a later
    audit can recompute it from a fresh checkout of the same commit and see
    whether the bytes that shipped are the bytes upstream published: for each
    file, sorted by its upstream path, the path, a NUL, and its contents.
    """
    digest = hashlib.sha256()
    for selection in selections:
        for path in sorted(selected_files(selection, checkout)):
            digest.update(path.relative_to(checkout).as_posix().encode())
            digest.update(b"\0")
            digest.update(path.read_bytes())
    return digest.hexdigest()


def overlay_record(overlay: dict, prepared: Path) -> dict:
    return {
        "path": overlay["path"],
        "sha256": sha256(prepared / overlay["path"]),
        "license": overlay["license"],
        "author": overlay["author"],
    }


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def write_json(path: Path, document: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(document, indent=2) + "\n")


if __name__ == "__main__":
    main()
