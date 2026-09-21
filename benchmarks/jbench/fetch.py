"""Downloading corpora, and pinning what was downloaded.

A benchmark that silently scores against a different version of a corpus than
it did last month is worse than no benchmark. But the checksums for these
archives are published on web pages rather than in a machine-readable place, and
hard-coding values nobody verified is its own kind of lie.

So: first download records what it got in `corpora.lock.json`, which is
committed. Every later download on every machine is checked against it, and a
mismatch is an error rather than a warning. The lock also stores the MD5 that
openslr.org prints beside each archive, so a human can confirm the first
download once and the lock carries that confirmation forever after.
"""

from __future__ import annotations

import hashlib
import json
import shutil
import tarfile
import urllib.request
import zipfile
from pathlib import Path

LOCK_PATH = Path(__file__).with_name("corpora.lock.json")

#: OpenAI's British-to-American spelling map, used by Whisper's normaliser.
#: Pinned to a tag rather than a branch so the contents cannot move.
SPELLING_URL = (
    "https://raw.githubusercontent.com/openai/whisper/v20231117/whisper/normalizers/english.json"
)

_CHUNK = 1 << 20


def download(url: str, destination: Path, key: str | None = None) -> Path:
    """Download `url` to `destination`, verifying it against the lock.

    Resumes nothing and caches everything: an archive already on disk that
    matches the lock is left alone, because these are gigabytes over a link
    that may be metered.
    """
    key = key or destination.name
    destination.parent.mkdir(parents=True, exist_ok=True)

    if destination.exists():
        digests = _digests(destination)
        _check(key, digests, url)
        return destination

    # Into a temporary name first, so an interrupted download cannot be
    # mistaken for a complete one on the next run.
    partial = destination.with_suffix(destination.suffix + ".part")
    print(f"fetching {url}")
    with urllib.request.urlopen(url) as response, partial.open("wb") as out:
        total = int(response.headers.get("Content-Length") or 0)
        seen = 0
        while chunk := response.read(_CHUNK):
            out.write(chunk)
            seen += len(chunk)
            if total:
                print(f"\r  {seen / total:6.1%} of {total / 1e6:.0f} MB", end="", flush=True)
        print()

    partial.replace(destination)
    digests = _digests(destination)
    _check(key, digests, url)
    return destination


def _digests(path: Path) -> dict[str, str]:
    sha, md5 = hashlib.sha256(), hashlib.md5()
    with path.open("rb") as handle:
        while chunk := handle.read(_CHUNK):
            sha.update(chunk)
            md5.update(chunk)
    return {"sha256": sha.hexdigest(), "md5": md5.hexdigest()}


def _check(key: str, digests: dict[str, str], url: str) -> None:
    lock = json.loads(LOCK_PATH.read_text()) if LOCK_PATH.exists() else {}
    recorded = lock.get(key)

    if recorded is None:
        lock[key] = {"url": url, **digests}
        LOCK_PATH.write_text(json.dumps(lock, indent=2, sort_keys=True) + "\n")
        print(
            f"  pinned {key}\n"
            f"    sha256 {digests['sha256']}\n"
            f"    md5    {digests['md5']}\n"
            f"  Check that md5 against the one published beside the archive, then "
            f"commit {LOCK_PATH.name}."
        )
        return

    if recorded["sha256"] != digests["sha256"]:
        raise RuntimeError(
            f"{key} does not match {LOCK_PATH.name}:\n"
            f"  expected sha256 {recorded['sha256']}\n"
            f"  got      sha256 {digests['sha256']}\n"
            "The corpus changed, or the download is corrupt. Scores from this and "
            "from an earlier run are not comparable until this is resolved."
        )


def extract(archive: Path, into: Path, marker: str | None = None) -> Path:
    """Unpack `archive` into `into`, skipping the work if `marker` is there."""
    into.mkdir(parents=True, exist_ok=True)
    if marker and (into / marker).exists():
        return into

    print(f"extracting {archive.name}")
    if archive.suffixes[-2:] == [".tar", ".gz"] or archive.suffix in {".tgz", ".tar"}:
        with tarfile.open(archive) as tar:
            _safe_extract(tar, into)
    elif archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as zf:
            zf.extractall(into)
    else:
        raise ValueError(f"do not know how to extract {archive.name}")
    return into


def _safe_extract(tar: tarfile.TarFile, into: Path) -> None:
    """Refuse members that would escape the destination.

    These archives come from trusted hosts, but an extraction that can write
    outside its directory is a foot-gun regardless of who served it.
    """
    root = into.resolve()
    for member in tar.getmembers():
        target = (root / member.name).resolve()
        if not target.is_relative_to(root):
            raise RuntimeError(f"{member.name} would extract outside {root}")
    # `filter="data"` additionally drops device nodes and absolute links.
    tar.extractall(into, filter="data")


def fetch_normalizer(spelling_path: Path, lock_path: Path) -> Path:
    """Fetch Whisper's spelling map, pinning it the same way corpora are."""
    if spelling_path.exists():
        print(f"{spelling_path.name} already present")
    else:
        print(f"fetching {SPELLING_URL}")
        with urllib.request.urlopen(SPELLING_URL) as response:
            spelling_path.write_bytes(response.read())

    mapping = json.loads(spelling_path.read_text())
    if not isinstance(mapping, dict) or len(mapping) < 100:
        spelling_path.unlink()
        raise RuntimeError("that did not look like english.json — deleted it")

    digest = hashlib.sha256(spelling_path.read_bytes()).hexdigest()
    recorded = json.loads(lock_path.read_text()) if lock_path.exists() else {}
    if recorded.get("english_json_sha256") not in (None, digest):
        raise RuntimeError(
            f"english.json does not match {lock_path.name} — scores would shift "
            "under you. Delete the lock only if you mean to change normalisation."
        )

    lock_path.write_text(
        json.dumps(
            {"english_json_sha256": digest, "url": SPELLING_URL, "entries": len(mapping)},
            indent=2,
        )
        + "\n"
    )
    print(f"  pinned english.json sha256:{digest[:12]} ({len(mapping)} entries)")
    return spelling_path


def require(tool: str) -> str:
    path = shutil.which(tool)
    if path is None:
        raise RuntimeError(f"{tool} is not on PATH")
    return path
