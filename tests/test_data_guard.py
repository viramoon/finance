import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile
import zlib


REPO = Path(__file__).resolve().parents[1]
RULES = ".data-guard/rules.json"
SOURCES = ".data-guard/sources.json"
GUARD = "scripts/data_guard.py"
INSTALLER = "scripts/install-data-guard-hooks.sh"
HOOKS = (".githooks/pre-commit", ".githooks/pre-push")
GENERATOR = "scripts/generate_synthetic_fixtures.py"
MARKER = b"SYNTHETIC_BODY_MUST_STAY_PRIVATE"

with patch.object(sys, "path", [str(REPO), *sys.path]):
    from scripts import data_guard


def png():
    def chunk(kind, data):
        return (struct.pack(">I", len(data)) + kind + data
                + struct.pack(">I", zlib.crc32(kind + data)))

    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0))
            + chunk(b"tEXt", b"Comment\0" + MARKER)
            + chunk(b"IDAT", zlib.compress(b"\0\0\0\0"))
            + chunk(b"IEND", b""))


def forbidden_examples():
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w") as handle:
        handle.writestr("synthetic.txt", MARKER)
    tar = io.BytesIO()
    with tarfile.open(fileobj=tar, mode="w", format=tarfile.USTAR_FORMAT) as handle:
        entry = tarfile.TarInfo("synthetic.txt")
        entry.size = len(MARKER)
        handle.addfile(entry, io.BytesIO(MARKER))
    return (
        ("missing.csv", b"symbol,price\nDEMO,12.34\n" + MARKER, b"fixture-source-required"),
        ("table.md", b"time,price,volume\n2025-01-01,1.00,2\n2025-01-02,2.00,3\n" + MARKER,
         b"fixture-source-required"),
        ("renamed-quotes.txt", b"symbol,venue,event_time,bid,ask,bid_size,ask_size\n"
         b"DEMO,SIM,2025-01-01T00:00:00Z,11.00,11.25,2,3\n"
         + MARKER + b",SIM,2025-01-01T00:00:01Z,11.25,11.50,2,3\n", b"fixture-source-required"),
        ("quotes.txt", b'{"bars":[{"open":"1","high":"2","low":"1","close":"2"}],"note":"'
         + MARKER + b'"}', b"fixture-source-required"),
        ("looks-like-csv.csv", b"PAR1" + MARKER + b"PAR1", b"blocked-magic"),
        ("renamed-parquet.txt", b"PAR1" + MARKER + b"PAR1", b"blocked-magic"),
        ("renamed-zip.txt", archive.getvalue(), b"blocked-magic"),
        ("renamed-gzip.txt", gzip.compress(MARKER, mtime=0), b"blocked-magic"),
        ("renamed-tar.txt", tar.getvalue(), b"blocked-magic"),
        ("renamed-screenshot.txt", png(), b"blocked-magic"),
        ("renamed-db.txt", b"SQLite format 3\0" + MARKER, b"blocked-magic"),
        ("renamed-dump.txt", b"PGDMP" + MARKER, b"blocked-magic"),
        ("renamed-sql.txt", b"--\n-- PostgreSQL database dump\n--\n" + MARKER, b"blocked-magic"),
        ("renamed-lfs.txt", b"version https://git-lfs.github.com/spec/v1\n" + MARKER, b"blocked-magic"),
        ("archive.ZIP", MARKER, b"blocked-extension"),
        ("nested/LoGs/hint.txt", MARKER, b"blocked-path"),
        ("payload.txt", b"\0" + MARKER, b"non-text"),
        ("oversize.txt", MARKER.ljust(262145, b"z"), b"oversize"),
    )


class FixtureGenerationTest(unittest.TestCase):
    def test_committed_synthetic_fixtures_are_reproducible_and_registered(self):
        self.verify_fixtures(REPO)

    def fixture_file(self, root, name):
        candidate = root / name
        self.assertTrue(candidate.is_relative_to(root), "fixture path escapes its root")
        path = root
        for part in candidate.relative_to(root).parts:
            path /= part
            self.assertFalse(path.is_symlink(), "fixture path uses a symbolic link")
        self.assertTrue(path.is_file(), "fixture or generator is missing")
        return path

    def verify_fixtures(self, repo):
        sources = data_guard.parse_sources(self.fixture_file(repo, SOURCES).read_bytes())
        fixtures = {path: self.fixture_file(repo, path) for path in sources}
        generator = self.fixture_file(repo, GENERATOR)
        with tempfile.TemporaryDirectory() as directory:
            subprocess.run([sys.executable, generator, directory],
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            output = Path(directory)
            produced = {
                path.relative_to(output).as_posix() for path in output.rglob("*")
                if path.is_symlink() or path.is_file()
            }
            self.assertEqual(produced, {path for path, info in sources.items() if info["generator"] == GENERATOR})
            for path, info in sources.items():
                original = fixtures[path].read_bytes()
                self.assertEqual(hashlib.sha256(original).hexdigest(), info["sha256"], path)
                if path in produced:
                    self.assertTrue(original == self.fixture_file(output, path).read_bytes(), path)

    def assert_manifest_rejected_before_reading_fixtures(self, repo, path, error, message):
        manifest = repo / SOURCES
        manifest.parent.mkdir(parents=True, exist_ok=True)
        manifest.write_text(json.dumps({"version": 1, "fixtures": {path: {
            "sha256": hashlib.sha256(MARKER).hexdigest(),
            "generator": GENERATOR,
            "seed": "fixed",
            "source": "Synthetic fixture for the verifier regression.",
        }}}), encoding="utf-8")
        contents = manifest.read_bytes()
        with patch.object(Path, "read_bytes", autospec=True, return_value=contents) as read:
            with patch.object(subprocess, "run") as run:
                with self.assertRaisesRegex(error, message):
                    self.verify_fixtures(repo)
            read.assert_called_once_with(manifest)
            run.assert_not_called()

    def test_fixture_verifier_rejects_absolute_and_parent_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "work"
            for path in (str(repo.parent / "synthetic.csv"), "../synthetic.csv", "..\\synthetic.csv"):
                with self.subTest(path=path):
                    self.assert_manifest_rejected_before_reading_fixtures(
                        repo, path, data_guard.GuardError, "^invalid-sources$"
                    )

    def test_fixture_verifier_rejects_symlink_escape(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = root / "work"
            repo.mkdir()
            (root / "synthetic.csv").write_bytes(MARKER)
            (repo / "link").symlink_to(root, target_is_directory=True)
            self.assert_manifest_rejected_before_reading_fixtures(
                repo, "link/synthetic.csv", AssertionError, "fixture path uses a symbolic link"
            )


class GitGuardTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name) / "work"
        self.root.mkdir()
        self.remote = Path(tmp.name) / "remote.git"
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        self.env.update(
            GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1", GIT_ALLOW_PROTOCOL="file",
            GIT_AUTHOR_NAME="Fixture Test", GIT_AUTHOR_EMAIL="fixture@example.invalid",
            GIT_COMMITTER_NAME="Fixture Test", GIT_COMMITTER_EMAIL="fixture@example.invalid",
        )
        for name in (RULES, GUARD, INSTALLER, *HOOKS):
            dest = self.root / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPO / name, dest)
        self.write_json(SOURCES, {"version": 1, "fixtures": {}})
        self.write("README.md", b"Isolated guard test repository.\n")
        self.git("init", "-q", "--initial-branch=main")
        self.git("add", ".")
        self.git("commit", "-qm", "Published baseline")
        self.base = self.git("rev-parse", "HEAD").stdout.strip().decode("ascii")
        self.git("init", "-q", "--bare", "--initial-branch=main", self.remote)
        self.git("remote", "add", "origin", self.remote)
        self.git("push", "-q", "origin", "main")
        self.git("remote", "set-head", "origin", "main")
        self.install()

    def run_command(self, *args, success=True, input=None):
        result = subprocess.run(args, cwd=self.root, env=self.env, capture_output=True,
                                input=input, timeout=20)
        if success:
            self.assertEqual(result.returncode, 0, f"command failed: {args[:3]}")
        return result

    def git(self, *args, **kwargs):
        return self.run_command("git", *args, **kwargs)

    def write(self, path, data):
        dest = self.root / path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(data)

    def write_json(self, path, value):
        self.write(path, (json.dumps(value, indent=2) + "\n").encode())

    def install(self, success=True):
        return self.run_command("sh", INSTALLER, success=success)

    def check(self, *args, **kwargs):
        return self.run_command(sys.executable, GUARD, *args, **kwargs)

    def stage(self, path, data):
        self.write(path, data)
        self.git("add", "--", path)

    def unstage(self, path):
        self.git("reset", "-q", "HEAD", "--", path)
        (self.root / path).unlink()

    def rejected(self, result, reason):
        self.assertNotEqual(result.returncode, 0)
        output = result.stdout + result.stderr
        self.assertTrue(reason in output, f"missing diagnostic: {reason!r}")
        self.assertFalse(MARKER in output, "diagnostics disclosed synthetic content or redacted path")

    def register(self, path, data, generator="make_fixture.py"):
        self.write_json(SOURCES, {"version": 1, "fixtures": {path: {
            "sha256": hashlib.sha256(data).hexdigest(),
            "generator": generator,
            "seed": "31",
            "source": "Generated from the seed and fixed inputs without external data.",
        }}})
        self.git("add", SOURCES)

    def test_generated_fixture_passes_commit_push_and_ci(self):
        recipe = (
            b'import random\n'
            b'print("price,volume")\n'
            b'rng = random.Random(31)\n'
            b'print(f"100.25,{rng.randrange(10)}")\n'
        )
        self.stage("make_fixture.py", recipe)
        data = self.run_command(sys.executable, "make_fixture.py").stdout
        self.stage("tests/fixtures/generated.csv", data)
        self.register("tests/fixtures/generated.csv", data)
        self.git("switch", "-qc", "fixture")
        self.git("commit", "-qm", "Add a seeded fixture")
        self.git("push", "-q", "origin", "fixture")
        self.check("range", self.base, "HEAD")

    def test_pre_commit_sees_index_and_ignores_unstaged_or_intent_to_add(self):
        self.stage("review.md", b"PAR1" + MARKER)
        self.write("review.md", b"An innocuous unstaged document.\n")
        self.rejected(self.git("commit", "-qm", "Attempt", success=False), b"blocked-magic")
        self.git("add", "review.md")
        self.write("review.md", b"PAR1" + MARKER)
        self.write("not-staged.csv", MARKER)
        self.git("add", "-N", "not-staged.csv")
        self.git("commit", "-qm", "Only the staged document")

    def test_pre_commit_rejects_disguised_formats_and_size_or_path_violations(self):
        for path, data, reason in forbidden_examples():
            with self.subTest(path=path):
                self.stage(path, data)
                self.rejected(self.git("commit", "-qm", "Attempt", success=False), reason)
                self.unstage(path)

    def test_source_code_and_json_schema_can_be_committed(self):
        self.stage("make_fixture.py", b'quotes = {\n    "open": "1, 2, 3", "high": "2, 3, 4",\n'
                   b'    "low": "1, 2, 3", "close": "2, 3, 4",\n}\n')
        self.write_json("bars.schema.json", {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "properties": {field: {"type": "string"} for field in ("open", "high", "low", "close")},
        })
        self.git("add", "bars.schema.json")
        self.git("commit", "-qm", "Add a recipe and schema as source code")

    def test_pre_push_blocks_new_refs_and_all_commits_of_existing_refs(self):
        self.git("switch", "-qc", "feature")
        for path, data, reason in forbidden_examples():
            with self.subTest(path=path):
                self.stage(path, data)
                self.git("commit", "--no-verify", "-qm", "Introduce a violation")
                self.git("rm", "-q", path)
                self.git("commit", "-qm", "Remove the violation from the tip")
                self.rejected(self.git("push", "-q", "origin", "feature", success=False), reason)
                self.rejected(
                    self.git("push", "-q", "origin", "feature:main", success=False), reason
                )
                self.git("reset", "--hard", self.base)
        advertised = self.git("--git-dir", self.remote, "for-each-ref", "--format=%(refname)").stdout
        self.assertEqual(advertised, b"refs/heads/main\n")
        self.assertEqual(self.git("--git-dir", self.remote, "rev-parse", "main").stdout.strip().decode(), self.base)

    def test_ci_rejects_merge_history_without_reporting_names_or_contents(self):
        self.git("switch", "-qc", "side")
        paths = []
        for index, (path, data, _) in enumerate(forbidden_examples()):
            name = f"{index}/{MARKER.decode()}/{path}"
            self.stage(name, data)
            paths.append(name)
        self.git("commit", "--no-verify", "-qm", "Introduce synthetic forbidden blobs")
        self.git("rm", "-q", "--", *paths)
        self.git("commit", "-qm", "Remove the blobs before merging")
        self.git("switch", "-q", "main")
        self.git("merge", "--no-ff", "-qm", "Merge synthetic violation history", "side")
        result = self.check("range", self.base, "HEAD", success=False)
        for _, _, reason in forbidden_examples():
            self.rejected(result, reason)
        self.assertTrue(f"rejected ({len(paths)})".encode() in result.stderr)

    def test_manifest_uses_exact_paths_and_hashes_and_requires_staged_generator(self):
        path = "tests/fixtures/exact.csv"
        data = b"price,volume\n1.00,2\n"
        self.stage(path, data)
        self.register(path, data)
        self.write("make_fixture.py", b"# Unstaged generator.\n")
        self.rejected(self.check("staged", success=False), b"fixture-source-missing")
        self.git("add", "make_fixture.py")
        self.check("staged")
        self.stage("tests/fixtures/copy.csv", data)
        self.rejected(self.check("staged", success=False), b"fixture-source-required")
        self.unstage("tests/fixtures/copy.csv")
        self.stage(path, data + MARKER)
        self.rejected(self.check("staged", success=False), b"fixture-digest")
        self.register(path, b"PAR1" + MARKER)
        self.stage(path, b"PAR1" + MARKER)
        self.rejected(self.check("staged", success=False), b"blocked-magic")

    def test_pre_commit_requires_generator_content_to_be_staged(self):
        fixture = "tests/fixtures/intent.csv"
        generator = "make_fixture.py"
        data = b"price,volume\n1.00,2\n"
        self.stage(fixture, data)
        self.write(generator, b'print("price,volume\\n1.00,2")\n')
        self.register(fixture, data, generator)
        self.git("add", "-N", generator)
        self.rejected(self.git("commit", "-qm", "Attempt", success=False), b"fixture-source-missing")
        self.git("add", generator)
        self.git("commit", "-qm", "Commit the fixture with its generator")
        self.git("rm", "--cached", generator)
        self.git("add", "-N", generator)
        self.rejected(self.git("commit", "-qm", "Attempt", success=False), b"fixture-source-missing")

    def test_missing_or_malformed_policy_is_rejected_without_details(self):
        self.stage(RULES, b'{"version":1,"version":' + MARKER + b"}")
        self.rejected(self.check("staged", success=False), b"invalid-rules")
        self.git("restore", "--source=HEAD", "--staged", "--worktree", RULES)
        self.write_json(SOURCES, {"version": 2, "fixtures": {}})
        self.git("add", SOURCES)
        self.rejected(self.check("staged", success=False), b"invalid-sources")
        self.git("rm", "-f", SOURCES)
        self.rejected(self.check("staged", success=False), b"invalid-sources")

    def test_registration_cannot_be_removed_while_fixture_stays(self):
        path = "tests/fixtures/fixed.csv"
        data = b"price,volume\n1.00,2\n"
        self.stage(path, data)
        self.stage("make_fixture.py", b'print("price,volume\\n1.00,2")\n')
        self.register(path, data)
        self.git("commit", "-qm", "Commit fixed synthetic fixture")
        self.write_json(SOURCES, {"version": 1, "fixtures": {}})
        self.git("add", SOURCES)
        self.rejected(self.check("staged", success=False), b"fixture-source-required")
        self.git("rm", "-q", path)
        self.check("staged")

    def test_pre_push_checks_other_branch_and_rejects_non_commit_refs(self):
        self.git("switch", "-qc", "unsafe")
        self.stage("unreviewed.txt", png())
        self.git("commit", "--no-verify", "-qm", "Introduce a synthetic image")
        self.git("switch", "-qc", "safe", self.base)
        self.stage("docs.md", b"An ordinary document.\n")
        self.git("commit", "-qm", "Add an ordinary document")
        self.rejected(self.git("push", "-q", "origin", "safe", "unsafe", success=False), b"blocked-magic")
        self.git("tag", "image", "unsafe:unreviewed.txt")
        self.rejected(self.git("push", "-q", "origin", "refs/tags/image", success=False), b"git-read-failed")
        advertised = self.git("--git-dir", self.remote, "for-each-ref", "--format=%(refname)").stdout
        self.assertEqual(advertised, b"refs/heads/main\n")

    def test_installer_is_idempotent_and_preserves_config_and_existing_hooks(self):
        config = (self.root / ".git/config").read_bytes()
        self.install()
        self.assertTrue((self.root / ".git/config").read_bytes() == config)
        pre_commit, pre_push = (self.root / ".git/hooks" / Path(item).name for item in HOOKS)
        pre_commit.unlink()
        pre_push.write_bytes(b"#!/bin/sh\nexit 0\n")
        self.rejected(self.install(success=False), b"existing pre-push hook")
        self.assertFalse(pre_commit.exists())
        self.assertTrue(pre_push.read_bytes() == b"#!/bin/sh\nexit 0\n")
        self.git("config", "--local", "core.hooksPath", "other-hooks")
        self.rejected(self.install(success=False), b"requires the default hook directory")

    def test_new_ref_without_remote_tracking_scans_full_history(self):
        empty = self.remote.with_name("empty.git")
        self.git("init", "-q", "--bare", "--initial-branch=main", empty)
        self.git("remote", "add", "empty", empty)
        self.git("switch", "--orphan", "orphan")
        for path in (RULES, GUARD, INSTALLER, SOURCES, *HOOKS):
            self.stage(path, self.git("show", f"{self.base}:{path}").stdout)
        self.stage("historical.md", b"PAR1" + MARKER)
        self.git("commit", "--no-verify", "-qm", "Initial commit with a forbidden blob")
        self.git("rm", "-q", "historical.md")
        self.git("commit", "--no-verify", "-qm", "Remove the forbidden blob")
        self.rejected(self.git("push", "-q", "empty", "orphan", success=False), b"blocked-magic")


if __name__ == "__main__":
    unittest.main()
