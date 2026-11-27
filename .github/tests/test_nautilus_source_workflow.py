import subprocess
import unittest
from pathlib import Path

import yaml


WORKFLOW = Path(__file__).resolve().parents[1] / "workflows" / "nautilus-source.yml"
UBUNTU_SOURCES = "/etc/apt/sources.list.d/ubuntu.sources"
APT_OPTIONS = (
    f"-o Dir::Etc::sourcelist={UBUNTU_SOURCES} "
    "-o Dir::Etc::sourceparts=- -o Acquire::Retries=5"
)
UPDATE = f"{APT_OPTIONS} -o APT::Update::Error-Mode=any update"
INSTALL = f"{APT_OPTIONS} install -y libssl-dev pkg-config libtinfo6 libxml2 libz3-4"

# Synthetic runner. Without both source overrides, apt would contact the
# preinstalled Microsoft repository, which returns 403 in this fixture.
RUNNER = r"""
test() {
  [[ "$*" == "-s $UBUNTU_SOURCES" && "$HAS_UBUNTU_SOURCES" == 1 ]]
}
df() { :; }
sudo() {
  case "$1" in
    rm) return 0 ;;
    apt-get) shift ;;
    *) return 125 ;;
  esac
  local args
  printf -v args '%q ' "$@"
  printf '%s\n' "${args% }"
  case " $args" in
    *" -o Dir::Etc::sourcelist=$UBUNTU_SOURCES -o Dir::Etc::sourceparts=- "*)
      ;;
    *)
      echo "HTTP 403 from packages.microsoft.com" >&2
      return 100
      ;;
  esac
  case " $args" in
    *" update "*) return "$UPDATE_STATUS" ;;
    *" install "*) return "$INSTALL_STATUS" ;;
    *) return 125 ;;
  esac
}
"""


class NautilusSourceWorkflowTests(unittest.TestCase):
    def run_dependency_step(self, *, has_ubuntu_sources=True, update_status=0, install_status=0):
        workflow = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
        job = workflow["jobs"]["source"]
        self.assertEqual(job["runs-on"], "ubuntu-24.04")
        step, = [step for step in job["steps"] if step.get("name") == "系统编译依赖"]
        self.assertEqual(step["shell"], "bash")
        self.assertNotIn("if", step)
        self.assertNotIn("continue-on-error", step)
        return subprocess.run(
            ["/bin/bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", RUNNER + step["run"]],
            env={
                "PATH": "/dev/null",
                "RUNNER_TEMP": "/tmp",
                "UBUNTU_SOURCES": UBUNTU_SOURCES,
                "HAS_UBUNTU_SOURCES": str(int(has_ubuntu_sources)),
                "UPDATE_STATUS": str(update_status),
                "INSTALL_STATUS": str(install_status),
            },
            capture_output=True,
            text=True,
            timeout=10,
        )

    def test_third_party_403_does_not_block_ubuntu_install(self):
        result = self.run_dependency_step()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [UPDATE, INSTALL])

    def test_missing_ubuntu_sources_stop_before_apt(self):
        result = self.run_dependency_step(has_ubuntu_sources=False)

        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            result.stdout.splitlines(),
            [f"::error::Ubuntu apt sources missing or empty: {UBUNTU_SOURCES}"],
        )

    def test_failed_ubuntu_update_stops_install(self):
        result = self.run_dependency_step(update_status=100)

        self.assertEqual(result.returncode, 100)
        self.assertEqual(result.stdout.splitlines(), [UPDATE])

    def test_failed_ubuntu_install_fails_step(self):
        result = self.run_dependency_step(install_status=100)

        self.assertEqual(result.returncode, 100)
        self.assertEqual(result.stdout.splitlines(), [UPDATE, INSTALL])


if __name__ == "__main__":
    unittest.main()
