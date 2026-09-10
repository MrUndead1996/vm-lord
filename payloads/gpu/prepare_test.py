"""Holds `prepare.py`'s digest rule to the one the Rust packer implements.

`tree_digest` here and `built_output_digest` in `crates/gpu-payload/src/builder.rs`
are one rule written twice: every file under the built output, sorted by its
payload-relative POSIX path *string*, contributed as path, NUL, contents. If they
ever disagree, `pack` refuses a tree it just built with a message that names
neither the rule nor the file.

The tie between them is the literal below. `EXPECTED_ADVERSARIAL_DIGEST` also
appears in `builder.rs`'s `the_built_output_digest_matches_the_python_golden_vector`
test, over a tree with the same three members and the same bytes, so changing
either implementation's rule turns exactly one of the two red.

The tree is adversarial on purpose. `lib-extra`, `lib.conf` and `lib/dri.so`
sort one way as joined strings (`-` < `.` < `/`) and the other way as `Path`
objects, which compare component by component -- so the natural-looking
`sorted(p for p in tree.rglob('*') if p.is_file())` that the `tree_digest`
docstring warns against fails here, rather than in six months when a Mesa bump
happens to install such a pair.

Run it directly: `python3 prepare_test.py`. The Dockerfile beside this file runs
it in the same stage that runs `prepare.py`, so every payload build executes it
and no host needs a test framework, or a python3, to get the check.
"""

from __future__ import annotations

import hashlib
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import prepare  # noqa: E402

EXPECTED_ADVERSARIAL_DIGEST = "033ae129cd90239804e4a42ba7b80063e4fdcf85d5b2a634eb9dc32acdc3a034"

# path under the payload root -> contents. Kept as data because the Rust test
# lays out the same three members, and a reader comparing the two should be able
# to compare two lists rather than two programs.
ADVERSARIAL_TREE = {
    "content/mesa/lib/dri.so": b"dri\n",
    "content/mesa/lib-extra": b"extra\n",
    "content/mesa/lib.conf": b"conf\n",
}


def test_tree_digest_matches_the_shared_golden_vector() -> None:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        for relative, contents in ADVERSARIAL_TREE.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(contents)

        digest = prepare.tree_digest(root, root / "content/mesa")

    if digest != EXPECTED_ADVERSARIAL_DIGEST:
        raise AssertionError(
            "tree_digest no longer computes the rule the Rust packer implements.\n"
            f"  expected {EXPECTED_ADVERSARIAL_DIGEST}\n"
            f"  measured {digest}\n"
            "Members are sorted by the joined POSIX path string, not by Path, which\n"
            "orders component by component. See tree_digest in prepare.py and\n"
            "built_output_digest in crates/gpu-payload/src/builder.rs; if the rule\n"
            "changed on purpose, both sides and this literal move together."
        )


def test_the_vector_would_catch_a_path_sort() -> None:
    """The vector is only worth having if the wrong rule gives a different answer."""
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        for relative, contents in ADVERSARIAL_TREE.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(contents)

        tree = root / "content/mesa"
        digest = hashlib.sha256()
        for path in sorted(item for item in tree.rglob("*") if item.is_file()):
            digest.update(path.relative_to(root).as_posix().encode())
            digest.update(b"\0")
            digest.update(path.read_bytes())

    if digest.hexdigest() == EXPECTED_ADVERSARIAL_DIGEST:
        raise AssertionError(
            "the adversarial tree no longer distinguishes a Path sort from a string\n"
            "sort, so this file proves nothing. Pick members whose two orders differ."
        )


def test_an_empty_output_is_refused() -> None:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        tree = root / "content/mesa"
        tree.mkdir(parents=True)
        try:
            prepare.tree_digest(root, tree)
        except SystemExit:
            return
    raise AssertionError("tree_digest accepted a built tree with no files in it")


def main() -> int:
    tests = [value for name, value in sorted(globals().items()) if name.startswith("test_")]
    failures = 0
    for test in tests:
        try:
            test()
        except AssertionError as error:
            failures += 1
            print(f"FAIL {test.__name__}: {error}", file=sys.stderr)
        else:
            print(f"ok   {test.__name__}")
    if failures:
        print(f"{failures} of {len(tests)} checks failed", file=sys.stderr)
        return 1
    print(f"{len(tests)} checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
