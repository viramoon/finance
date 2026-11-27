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
PUBLISHED = ".data-guard/published.json"
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

    def test_ib_review_export_needs_only_the_checked_out_commit(self):
        script = "scripts/export-ib-review-sources.sh"
        _, tree = data_guard.staged_trees(cwd=REPO)
        sources = data_guard.load_sources(tree, required=True, cwd=REPO)
        digests = {path: row["sha256"] for path, row in sources.items() if row["generator"] == script}
        self.assertEqual(len(digests), 3)

        with tempfile.TemporaryDirectory() as directory:
            # An independent root commit has no other refs, parents or shared objects.
            repo = Path(directory) / "checkout"
            data_guard.git("init", "-q", "--template=", repo)
            for path in (script, SOURCES, *digests):
                target = repo / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(data_guard.blob(tree[path], cwd=REPO))
            data_guard.git("add", "--", script, SOURCES, *digests, cwd=repo)
            data_guard.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                           "commit", "-qm", "Isolated review tree", cwd=repo)
            for path in digests:
                (repo / path).write_bytes(b"uncommitted edit\n")

            output = subprocess.check_output(["sh", repo / script], cwd=directory, env=data_guard.GIT_ENV)
            with tarfile.open(fileobj=io.BytesIO(output), mode="r:") as archive:
                files = [member for member in archive if not member.isdir()]
                self.assertCountEqual([member.name for member in files], digests)
                for member in files:
                    self.assertTrue(member.isfile())
                    digest = hashlib.sha256(archive.extractfile(member).read()).hexdigest()
                    self.assertEqual(digest, digests[member.name])

    def test_option_flow_review_export_from_an_isolated_commit(self):
        script = "scripts/export-option-flow-review-sources.sh"
        fixture = "crates/finance-collector/src/option_flow/observation/tests.rs"
        _, tree = data_guard.staged_trees(cwd=REPO)
        sources = data_guard.load_sources(tree, required=True, cwd=REPO)
        self.assertEqual([path for path, row in sources.items() if row["generator"] == script], [fixture])
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "checkout"
            data_guard.git("init", "-q", "--template=", repo)
            for path in (script, fixture):
                target = repo / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(data_guard.blob(tree[path], cwd=REPO))
            data_guard.git("add", "--", script, fixture, cwd=repo)
            data_guard.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                           "commit", "-qm", "Isolated option flow source", cwd=repo)
            (repo / fixture).write_bytes(b"uncommitted edit\n")

            output = subprocess.check_output(["sh", repo / script], cwd=directory, env=data_guard.GIT_ENV)
            with tarfile.open(fileobj=io.BytesIO(output), mode="r:") as archive:
                member, = (entry for entry in archive if not entry.isdir())
                self.assertEqual(member.name, fixture)
                self.assertTrue(member.isfile())
                digest = hashlib.sha256(archive.extractfile(member).read()).hexdigest()
                self.assertEqual(digest, sources[fixture]["sha256"])

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
        _, tree = data_guard.staged_trees(cwd=repo)
        sources = data_guard.load_sources(tree, required=True, cwd=repo)
        required = set(sources) | {info["generator"] for info in sources.values()} | {GENERATOR}
        for path in required:
            entry = tree.get(path)
            self.assertTrue(entry is not None and entry.mode in data_guard.REGULAR_MODES,
                            "fixture or generator must be a regular Git blob")

        with tempfile.TemporaryDirectory() as directory:
            generator = Path(directory) / "generate_synthetic_fixtures.py"
            generator.write_bytes(data_guard.blob(tree[GENERATOR], cwd=repo))
            output = Path(directory) / "output"
            subprocess.run([sys.executable, "-I", "-B", generator, output], cwd=directory,
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            produced = {
                path.relative_to(output).as_posix() for path in output.rglob("*")
                if path.is_symlink() or path.is_file()
            }
            expected = {path for path, info in sources.items() if info["generator"] == GENERATOR}
            self.assertTrue(produced == expected, "generated fixture inventory mismatch")
            for path, info in sources.items():
                original = data_guard.blob(tree[path], cwd=repo)
                self.assertTrue(hashlib.sha256(original).hexdigest() == info["sha256"],
                                "fixture digest mismatch")
                if path in produced:
                    self.assertTrue(original == self.fixture_file(output, path).read_bytes(),
                                    "generated fixture bytes mismatch")

    def init_fixture_repo(self, repo):
        data_guard.git("init", "-q", "--template=", repo)
        generator = repo / GENERATOR
        generator.parent.mkdir()
        generator.write_text(
            "from pathlib import Path\nimport sys\n"
            f"out = Path(sys.argv[1])\nout.mkdir()\n(out / 'synthetic.csv').write_bytes({MARKER!r})\n",
            encoding="utf-8",
        )
        data_guard.git("add", "--", GENERATOR, cwd=repo)

    def stage_manifest(self, repo, path, data=MARKER):
        manifest = repo / SOURCES
        manifest.parent.mkdir(parents=True, exist_ok=True)
        manifest.write_text(json.dumps({"version": 1, "fixtures": {path: {
            "sha256": hashlib.sha256(data).hexdigest(),
            "generator": GENERATOR,
            "seed": "fixed",
            "source": "Synthetic fixture for the verifier regression.",
        }}}), encoding="utf-8")
        data_guard.git("add", "--", SOURCES, cwd=repo)

    def assert_manifest_rejected_before_reading_fixtures(self, repo, path, error, message):
        self.stage_manifest(repo, path)
        _, tree = data_guard.staged_trees(cwd=repo)
        with patch.object(data_guard, "blob", wraps=data_guard.blob) as read_blob:
            with patch.object(Path, "read_bytes", side_effect=AssertionError("worktree read")) as read_file:
                with self.assertRaises(error) as failure:
                    self.verify_fixtures(repo)
            self.assertTrue(str(failure.exception).endswith(message))
            read_file.assert_not_called()
            read_blob.assert_called_once_with(tree[SOURCES], cwd=repo)

    def test_fixture_verifier_rejects_absolute_and_parent_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "work"
            self.init_fixture_repo(repo)
            for path in (str(repo.parent / "synthetic.csv"), "../synthetic.csv", "..\\synthetic.csv"):
                with self.subTest(path=path):
                    self.assert_manifest_rejected_before_reading_fixtures(
                        repo, path, data_guard.GuardError, "invalid-sources"
                    )

    def test_fixture_verifier_rejects_symlink_escape(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = root / "work"
            self.init_fixture_repo(repo)
            (root / "synthetic.csv").write_bytes(MARKER)
            (repo / "link").symlink_to(root, target_is_directory=True)
            (repo / "link.csv").symlink_to(root / "synthetic.csv")
            data_guard.git("add", "--", "link", "link.csv", cwd=repo)
            for path in ("link/synthetic.csv", "link.csv"):
                with self.subTest(path=path):
                    self.assert_manifest_rejected_before_reading_fixtures(
                        repo, path, AssertionError, "fixture or generator must be a regular Git blob"
                    )

    def test_fixture_verifier_rejects_files_outside_the_index_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "work"
            self.init_fixture_repo(repo)
            paths = (".git/synthetic-local-metadata", "untracked.csv", "intent.csv")
            for path in paths:
                (repo / path).write_bytes(MARKER)
            data_guard.git("add", "-N", "--", "intent.csv", cwd=repo)
            for path in paths:
                with self.subTest(path=path):
                    self.assert_manifest_rejected_before_reading_fixtures(
                        repo, path, AssertionError, "fixture or generator must be a regular Git blob"
                    )

    def test_fixture_verifier_reads_staged_blobs_with_dirty_worktree(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "work"
            self.init_fixture_repo(repo)
            (repo / "synthetic.csv").write_bytes(MARKER)
            data_guard.git("add", "--", "synthetic.csv", cwd=repo)
            self.stage_manifest(repo, "synthetic.csv")
            (repo / SOURCES).write_bytes(b"unstaged manifest")
            (repo / GENERATOR).write_bytes(b"raise RuntimeError('unstaged generator')\n")
            (repo / "synthetic.csv").write_bytes(b"unstaged fixture")
            self.verify_fixtures(repo)

    def test_fixture_verifier_redacts_digest_mismatches(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "work"
            self.init_fixture_repo(repo)
            (repo / "synthetic.csv").write_bytes(MARKER)
            data_guard.git("add", "--", "synthetic.csv", cwd=repo)
            different = b"synthetic nonmatching fixture"
            self.stage_manifest(repo, "synthetic.csv", different)
            with self.assertRaises(AssertionError) as failure:
                self.verify_fixtures(repo)
            diagnostic = str(failure.exception)
            self.assertTrue(diagnostic.endswith("fixture digest mismatch"))
            for data in (MARKER, different):
                self.assertFalse(hashlib.sha256(data).hexdigest() in diagnostic)
                self.assertFalse(data.decode() in diagnostic)


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

    def head(self):
        return self.git("rev-parse", "HEAD").stdout.strip().decode("ascii")

    def stage_published(self, *commits):
        self.write_json(PUBLISHED, {"version": 1, "commits": {
            commit: "Synthetic history published before enforcement." for commit in commits
        }})
        self.git("add", PUBLISHED)

    def approve_on_default_branch(self, *commits):
        self.git("switch", "-q", "main")
        self.stage_published(*commits)
        self.git("commit", "-qm", "Approve the synthetic published history")
        self.git("push", "-q", "origin", "main")
        return self.head()

    def stage_rules(self, **changes):
        rules = json.loads(self.git("show", f"{self.base}:{RULES}").stdout)
        rules.update(changes)
        self.write_json(RULES, rules)
        self.git("add", RULES)

    def stage_relaxed_rules(self):
        self.stage_rules(max_blob_bytes=524288, blocked_path_components=[], blocked_suffixes=[],
                         blocked_magic=[], fixture_paths=[], fixture_suffixes=[])

    def stage_rule_change_examples(self):
        # Fixed synthetic bytes. The JSON text exercises content detection even
        # after removing every path and suffix that requires fixture provenance.
        examples = tuple(item for item in forbidden_examples() if item[0] in {
            "oversize.txt", "nested/LoGs/hint.txt", "archive.ZIP", "renamed-parquet.txt", "quotes.txt",
        }) + (
            ("plain.csv", MARKER, b"fixture-source-required"),
            ("tests/fixtures/plain.json", b'{"synthetic":true}', b"fixture-source-required"),
        )
        for path, data, _ in examples:
            self.stage(path, data)
        return examples

    def assert_rule_change_rejected(self, result, examples):
        for path, _, reason in examples:
            self.rejected(result, reason + b" " + ascii(path).encode("ascii"))

    def test_pre_commit_enforces_head_rules_over_relaxed_index(self):
        self.stage_relaxed_rules()
        self.check("staged")
        self.write(RULES, b"Unstaged policy bytes.\n")
        examples = self.stage_rule_change_examples()
        result = self.git("commit", "-qm", "Try to loosen staged checks", success=False)
        self.assert_rule_change_rejected(result, examples)
        self.assertEqual(self.head(), self.base)

    def test_unborn_pre_commit_enforces_builtin_rules(self):
        self.git("switch", "--orphan", "initial")
        for path in (GUARD, SOURCES):
            self.stage(path, self.git("show", f"{self.base}:{path}").stdout)
        self.stage_relaxed_rules()
        self.check("staged")
        examples = self.stage_rule_change_examples()
        result = self.git("commit", "-qm", "Try to loosen initial checks", success=False)
        self.assert_rule_change_rejected(result, examples)

    def test_pre_push_enforces_tracking_default_rules_over_candidate_and_advertised_ref(self):
        self.git("switch", "-qc", "candidate")
        self.stage_relaxed_rules()
        self.git("commit", "-qm", "Propose rule changes on a topic branch")
        self.git("push", "-q", "origin", "candidate")
        advertised = self.head()
        examples = self.stage_rule_change_examples()
        self.git("commit", "--no-verify", "-qm", "Simulate synthetic violations on the topic branch")
        for destination in ("candidate", "candidate:new-candidate"):
            with self.subTest(destination=destination):
                result = self.git("push", "-q", "origin", destination, success=False)
                self.assert_rule_change_rejected(result, examples)
        refs = self.git("--git-dir", self.remote, "for-each-ref", "--format=%(objectname) %(refname)").stdout
        self.assertEqual(refs.decode("ascii").splitlines(), [
            f"{advertised} refs/heads/candidate", f"{self.base} refs/heads/main",
        ])

    def test_ci_enforces_pr_base_rules_in_merge_tree_and_branch_history(self):
        self.git("switch", "-qc", "candidate")
        self.stage_relaxed_rules()
        examples = self.stage_rule_change_examples()
        self.git("commit", "--no-verify", "-qm", "Simulate a PR with relaxed rules and synthetic violations")
        self.git("switch", "-q", "main")
        self.git("merge", "--no-ff", "-qm", "Simulate CI merge commit", "candidate")
        result = self.check("range", self.base, "HEAD", success=False)
        for path, _, reason in examples:
            self.rejected(result, reason)
            self.assertNotIn(path.encode("ascii"), result.stderr)
        self.assertIn(f"rejected ({len(examples) * 3})".encode("ascii"), result.stderr)

        self.git("switch", "-q", "candidate")
        self.git("rm", "-q", "--", *(path for path, _, _ in examples))
        self.git("commit", "-qm", "Remove the synthetic violations before review")
        result = self.check("range", self.base, "HEAD", success=False)
        for _, _, reason in examples:
            self.rejected(result, reason)
        self.assertIn(f"rejected ({len(examples)})".encode("ascii"), result.stderr)

    def test_rule_schema_is_checked_by_all_entries_and_in_intermediate_commits(self):
        invalid_json = b'{"version":1,"version":' + MARKER + b"}"
        original = self.git("show", f"{self.base}:{RULES}").stdout
        for case in ("malformed", "duplicate", "unknown-text-switch", "oversize", "deleted", "symlink", "directory"):
            with self.subTest(case=case):
                self.git("reset", "--hard", self.base)
                if case == "malformed":
                    self.stage(RULES, invalid_json)
                elif case == "duplicate":
                    self.stage(RULES, b'{"version":1,' + original[1:])
                elif case == "unknown-text-switch":
                    self.stage_rules(blocked_text_features=[])
                elif case == "oversize":
                    self.stage(RULES, original.ljust(data_guard.MAX_POLICY_BYTES + 1))
                else:
                    self.git("rm", "-q", RULES)
                    if case == "symlink":
                        (self.root / RULES).symlink_to("sources.json")
                        self.git("add", RULES)
                    elif case == "directory":
                        self.stage(RULES + "/nested.txt", b"A rules file must be a regular blob.\n")
                self.rejected(self.git("commit", "-qm", "Try invalid rules", success=False), b"invalid-rules")
                self.git("commit", "--no-verify", "-qm", "Simulate unchecked rules")
                self.rejected(self.git("push", "-q", "origin", "HEAD:candidate", success=False), b"invalid-rules")
                self.rejected(self.check("range", self.base, "HEAD", success=False), b"invalid-rules")

        self.git("restore", f"--source={self.base}", "--staged", "--worktree", RULES)
        self.git("commit", "--no-verify", "-qm", "Restore valid rules at the tip")
        self.rejected(self.check("range", self.base, "HEAD", success=False), b"invalid-rules")
        self.rejected(self.git("push", "-q", "origin", "HEAD:candidate", success=False), b"invalid-rules")

    def test_reviewed_rules_apply_to_followup_changes_after_merge(self):
        self.git("switch", "-qc", "policy")
        suffixes = json.loads(self.git("show", f"{self.base}:{RULES}").stdout)["blocked_suffixes"]
        self.stage_rules(blocked_suffixes=[*suffixes, ".sample"])
        self.git("commit", "-qm", "Propose a narrower policy")
        self.git("push", "-q", "origin", "policy")
        self.git("switch", "-q", "main")
        self.git("merge", "--no-ff", "-qm", "Simulate review and merge", "policy")
        self.check("range", self.base, "HEAD")
        self.git("push", "-q", "origin", "HEAD:reviewed")
        self.git("--git-dir", self.remote, "symbolic-ref", "HEAD", "refs/heads/reviewed")
        self.git("remote", "set-head", "origin", "reviewed")
        reviewed = self.head()

        self.stage("generated.sample", MARKER)
        self.rejected(self.git("commit", "-qm", "Try a newly disallowed suffix", success=False),
                      b"blocked-extension")
        self.git("commit", "--no-verify", "-qm", "Simulate a later PR using that suffix")
        self.check("range", self.base, "HEAD")
        self.rejected(self.check("range", reviewed, "HEAD", success=False), b"blocked-extension")
        self.rejected(self.git("push", "-q", "origin", "HEAD:candidate", success=False), b"blocked-extension")

    def test_pre_push_rejects_clean_push_when_configured_default_is_missing(self):
        self.git("switch", "-qc", "candidate")
        self.stage("safe.md", b"Ordinary synthetic content.\n")
        self.git("commit", "-qm", "Add a safe document")
        self.git("remote", "set-head", "origin", "-d")
        self.git("push", "-q", "origin", "candidate")  # Exact main fallback for an unset remote HEAD.
        self.git("symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/missing")
        self.rejected(self.git("push", "-q", "origin", "HEAD:new-candidate", success=False),
                      b"missing-trusted-ref")

    def test_pre_commit_requires_rules_when_head_exists_even_if_tree_is_empty(self):
        self.git("switch", "--orphan", "initial")
        self.git("commit", "--no-verify", "--allow-empty", "-qm", "Simulate a HEAD predating rules")
        for path in (GUARD, SOURCES, RULES):
            self.stage(path, self.git("show", f"{self.base}:{path}").stdout)
        self.rejected(self.git("commit", "-qm", "Try to shadow missing HEAD rules", success=False),
                      b"invalid-rules")

    def test_valid_candidate_cannot_replace_invalid_trusted_rules(self):
        self.stage_rules(version=2)
        self.git("commit", "--no-verify", "-qm", "Simulate malformed rules in a trusted tree")
        self.git("push", "--no-verify", "-q", "origin", "main")
        trusted = self.head()
        self.stage_rules()
        self.rejected(self.git("commit", "-qm", "Try to shadow malformed trusted rules", success=False),
                      b"invalid-rules")
        self.git("commit", "--no-verify", "-qm", "Simulate a candidate repairing the trusted rules")
        self.rejected(self.check("range", trusted, "HEAD", success=False), b"invalid-rules")
        self.rejected(self.git("push", "-q", "origin", "HEAD:candidate", success=False), b"invalid-rules")

    def test_pr_cannot_approve_its_own_history(self):
        self.git("switch", "-qc", "candidate")
        self.stage("temporary.txt", b"PAR1" + MARKER)
        self.git("commit", "--no-verify", "-qm", "Add a synthetic violation")
        violation = self.head()
        self.git("rm", "-q", "temporary.txt")
        self.stage_published(violation)
        self.git("commit", "-qm", "Try to approve history in the same PR")
        self.git("switch", "-q", "main")
        self.git("merge", "--no-ff", "-qm", "Simulate CI merge commit", "candidate")

        self.rejected(self.check("range", self.base, "HEAD", success=False), b"blocked-magic")

    def test_pre_push_trusts_default_branch_instead_of_advertised_ref(self):
        self.git("switch", "-qc", "legacy")
        self.stage("temporary.txt", b"PAR1" + MARKER)
        self.git("commit", "--no-verify", "-qm", "Add a synthetic violation")
        violation = self.head()
        self.git("rm", "-q", "temporary.txt")
        self.git("commit", "-qm", "Remove the violation")

        self.git("switch", "-qc", "candidate", self.base)
        self.stage_published(violation)
        self.git("commit", "-qm", "Approve history on a non-default branch")
        self.git("push", "-q", "origin", "candidate")
        self.git("merge", "--no-ff", "-qm", "Include the legacy branch", "legacy")
        for refspec in ("candidate", "candidate:refs/heads/new-candidate"):
            with self.subTest(refspec=refspec):
                self.rejected(self.git("push", "-q", "origin", refspec, success=False), b"blocked-magic")

        self.approve_on_default_branch(violation)
        self.git("switch", "-q", "candidate")
        self.git("push", "-q", "origin", "candidate", "candidate:refs/heads/new-candidate")

    def test_pre_commit_reads_head_and_checks_candidate_even_with_an_exemption(self):
        self.stage(PUBLISHED, b'{"version":2,"commits":{}}\n')
        self.git("commit", "--no-verify", "-qm", "Simulate malformed trusted metadata")
        self.stage_published(self.head())
        self.rejected(self.git("commit", "-qm", "Try to shadow HEAD metadata", success=False),
                      b"invalid-published")

        self.git("reset", "--hard", self.base)
        self.stage_published(self.head())
        self.stage("tests/fixtures/unregistered.csv", b"price,volume\n1.00,2\n")
        self.rejected(self.git("commit", "-qm", "Try to exempt staged content", success=False),
                      b"fixture-source-required")

    def test_published_ancestry_is_exempt_but_new_transient_violations_are_checked(self):
        self.git("switch", "-qc", "legacy")
        self.git("rm", "-q", SOURCES)
        self.stage("historical.txt", b"PAR1" + MARKER)
        self.git("commit", "--no-verify", "-qm", "Add synthetic content before enforcement")
        self.git("rm", "-q", "historical.txt")
        self.git("commit", "--no-verify", "-qm", "Remove the historical violation")
        base = self.approve_on_default_branch(self.head())
        self.git("switch", "-q", "legacy")
        self.git("restore", "--source=main", "--staged", "--worktree", SOURCES)
        self.git("commit", "-qm", "Restore the required fixture manifest")
        self.check("range", base, "HEAD")

        self.stage("new.txt", b"PAR1" + MARKER)
        self.git("commit", "--no-verify", "-qm", "Add a new synthetic violation")
        self.git("rm", "-q", "new.txt")
        self.git("commit", "-qm", "Remove the new violation from the tip")
        self.rejected(self.check("range", base, "HEAD", success=False), b"blocked-magic")
        self.rejected(self.git("push", "-q", "origin", "legacy", success=False), b"blocked-magic")

    def test_net_diff_checks_all_remaining_paths_even_when_tip_is_exempt(self):
        self.git("switch", "-qc", "legacy")
        examples = forbidden_examples()
        for path, data, _ in examples:
            self.stage(path, data)
        self.git("commit", "--no-verify", "-qm", "Add synthetic violations before enforcement")
        legacy = self.head()
        base = self.approve_on_default_branch(legacy)

        result = self.check("range", base, legacy, success=False)
        for _, _, reason in examples:
            self.rejected(result, reason)
        self.assertIn(f"rejected ({len(examples)})".encode(), result.stderr)
        push = self.git("push", "-q", "origin", "legacy", success=False)
        self.rejected(push, b"fixture-source-required")
        self.rejected(push, b"blocked-magic")

    def test_net_diff_requires_target_sources_and_checks_registered_digest(self):
        self.git("switch", "-qc", "legacy")
        self.git("rm", "-q", SOURCES)
        self.git("commit", "--no-verify", "-qm", "Simulate a branch without fixture sources")
        no_sources = self.head()
        path = "tests/fixtures/legacy.csv"
        self.stage(path, b"price,volume\n1.00,2\n")
        self.stage("make_fixture.py", b'print("price,volume\\n1.00,3")\n')
        self.register(path, b"price,volume\n1.00,3\n")
        self.git("commit", "--no-verify", "-qm", "Add a fixture with an incorrect digest")
        wrong_digest = self.head()
        base = self.approve_on_default_branch(wrong_digest)
        self.rejected(self.check("range", base, no_sources, success=False), b"invalid-sources")
        self.rejected(self.check("range", base, wrong_digest, success=False), b"fixture-digest")

        self.git("switch", "-q", "legacy")
        self.stage(path, self.run_command(sys.executable, "make_fixture.py").stdout)
        self.git("commit", "-qm", "Regenerate the fixture with its reviewed recipe")
        self.check("range", base, "HEAD")

    def test_missing_and_non_commit_objects_grant_no_exemption(self):
        self.git("switch", "-qc", "candidate")
        self.stage("temporary.txt", b"PAR1" + MARKER)
        self.git("commit", "--no-verify", "-qm", "Add a synthetic violation")
        blob = self.git("rev-parse", "HEAD:temporary.txt").stdout.strip().decode("ascii")
        self.git("rm", "-q", "temporary.txt")
        self.git("commit", "-qm", "Remove the violation")
        base = self.approve_on_default_branch("f" * 40, "f" * 64, blob)

        self.check("range", base, base)
        result = self.check("range", base, "candidate", success=False)
        self.rejected(result, b"blocked-magic")
        self.assertNotIn(b"git-read-failed", result.stderr)

    def test_published_schema_rejects_invalid_candidates_and_oversize_blobs(self):
        sha = b"a" * 40
        valid = b'{"version":1,"commits":{"' + sha + b'":"Synthetic reason"}}'
        cases = {
            "unknown key": b'{"version":1,"commits":{},"other":0}',
            "duplicate key": b'{"version":1,"commits":{},"commits":{}}',
            "duplicate sha": valid[:-2] + b',"' + sha + b'":"Duplicate"}}',
            "missing commits": b'{"version":1}',
            "wrong version": valid.replace(b'"version":1', b'"version":2'),
            "boolean version": valid.replace(b'"version":1', b'"version":true'),
            "floating version": valid.replace(b'"version":1', b'"version":1.0'),
            "non-object root": b"[]",
            "non-object commits": b'{"version":1,"commits":[]}',
            "abbreviated sha": valid.replace(sha, sha[:-1]),
            "uppercase sha": valid.replace(sha, sha.upper()),
            "non-hex sha": valid.replace(sha, b"g" * 40),
            "empty reason": valid.replace(b"Synthetic reason", b""),
            "blank reason": valid.replace(b"Synthetic reason", b" \\t"),
            "non-string reason": valid.replace(b'"Synthetic reason"', b"null"),
            "oversize blob": valid.ljust(data_guard.MAX_POLICY_BYTES + 1),
        }
        for case, data in cases.items():
            with self.subTest(case=case):
                self.stage(PUBLISHED, data)
                self.rejected(self.git("commit", "-qm", "Try invalid metadata", success=False),
                              b"invalid-published")

        self.stage(PUBLISHED, valid.ljust(data_guard.MAX_POLICY_BYTES))
        self.check("staged")
        self.unstage(PUBLISHED)
        (self.root / PUBLISHED).symlink_to("sources.json")
        self.git("add", PUBLISHED)
        self.rejected(self.check("staged", success=False), b"invalid-published")
        self.unstage(PUBLISHED)
        self.stage(PUBLISHED + "/nested.txt", b"A directory cannot be the exemption list.\n")
        self.rejected(self.check("staged", success=False), b"invalid-published")
        self.git("commit", "--no-verify", "-qm", "Simulate invalid metadata in a PR")
        self.rejected(self.check("range", self.base, "HEAD", success=False), b"invalid-published")

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
